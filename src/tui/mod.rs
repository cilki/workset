mod app;
mod details;
mod metadata;
mod tree;
mod watcher;

use app::{App, AppMode, Section};
use details::{DetailsLoader, RemoteInfo, RemoteSyncState, remote_sync_state};
use crate::get_repo_modification_time;
use metadata::{format_size, format_time_ago_verbose, get_repo_size};
use tree::{RepoInfo, RepoOperationStatus, TreeNode};
use watcher::FileWatcher;

use crate::{RepoPattern, Workspace, find_git_repositories};
use anyhow::{Result, anyhow, bail};
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use fuzzy_matcher::FuzzyMatcher;
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph},
};
use ratatui_image::{StatefulImage, picker::Picker, protocol::StatefulProtocol};
use std::collections::{HashMap, HashSet, VecDeque};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

const SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// How many repos may fetch concurrently
const MAX_CONCURRENT_SYNCS: usize = 4;
/// How long the selection must rest on a repo before it is fetched, so
/// scrolling through the list doesn't fetch every row passed over. Longer
/// than the details debounce because a fetch hits the network.
const SELECTION_SYNC_DEBOUNCE: Duration = Duration::from_millis(500);
/// Don't refetch a repo the selection returns to within this window
const SELECTION_SYNC_COOLDOWN: Duration = Duration::from_secs(60);
/// Ignore watcher-triggered sync requests this soon after a sync finished,
/// since the sync's own fetch writes the tracking refs the watcher observes
const WATCHER_SYNC_COOLDOWN: Duration = Duration::from_secs(2);
/// Below this terminal width only the active panel is shown (Tab toggles)
const SINGLE_PANEL_THRESHOLD: u16 = 80;

enum Action {
    None,
    OpenShell(PathBuf),
    DropToLibrary(Vec<String>),
    RestoreFromLibrary(Vec<String>),
    CloneRepo(String),
    RefreshData,
}

/// Results streamed from the background repo scan
enum LoadEvent {
    /// Fast filesystem enumeration finished: placeholder rows for every repo,
    /// sent before any git work starts
    Discovered {
        workspace: Vec<RepoInfo>,
        library: Vec<RepoInfo>,
    },
    Workspace(Vec<RepoInfo>),
    Library(RepoInfo),
}

/// A repo scan running on background threads. Results are drained into the
/// `App` from the event loop via `poll`, so the UI stays responsive.
///
/// `workspace` and `library` each hold one row per repo for the whole scan.
/// A row starts out as a placeholder carrying the "scanning" status — seeded
/// from the app's previous data on a refresh, so it keeps its last known
/// status, or bare on the initial load — and is replaced by the scanned row
/// once a worker gets to that repo.
struct RepoLoader {
    rx: mpsc::Receiver<LoadEvent>,
    workspace: Vec<RepoInfo>,
    library: Vec<RepoInfo>,
    done: usize,
    total: usize,
    /// Show a spinner with scan progress in the title (initial load)
    progressive: bool,
}

impl RepoLoader {
    fn start(
        workspace: &Workspace,
        seed_workspace: Vec<RepoInfo>,
        seed_library: Vec<RepoInfo>,
        progressive: bool,
    ) -> Self {
        let (tx, rx) = mpsc::channel();
        let workspace = workspace.clone();
        std::thread::spawn(move || scan_all_repos(&workspace, tx));
        let mark_scanning = |mut repos: Vec<RepoInfo>| {
            for repo in &mut repos {
                repo.operation_status = RepoOperationStatus::Scanning;
            }
            repos
        };
        Self {
            rx,
            workspace: mark_scanning(seed_workspace),
            library: mark_scanning(seed_library),
            done: 0,
            total: 0,
            progressive,
        }
    }

    /// Drain any newly scanned repos into the app without blocking.
    /// Returns false once the scan has finished.
    fn poll(&mut self, app: &mut App) -> bool {
        let mut received = false;
        let finished = loop {
            match self.rx.try_recv() {
                Ok(LoadEvent::Discovered { workspace, library }) => {
                    self.total = workspace.len() + library.len();
                    merge_discovered(&mut self.workspace, workspace);
                    merge_discovered(&mut self.library, library);
                    received = true;
                }
                Ok(LoadEvent::Workspace(infos)) => {
                    replace_scanned(&mut self.workspace, infos);
                    self.done += 1;
                    received = true;
                }
                Ok(LoadEvent::Library(info)) => {
                    replace_scanned(&mut self.library, vec![info]);
                    self.done += 1;
                    received = true;
                }
                Err(mpsc::TryRecvError::Empty) => break false,
                Err(mpsc::TryRecvError::Disconnected) => break true,
            }
        };

        if finished {
            // Every worker is done, so a row still marked "scanning" names a
            // repo nothing scanned: it was seeded from the app's previous data
            // and is no longer there. Dropping it is how a repo that left the
            // workspace leaves the list.
            let scanned = |repo: &RepoInfo| repo.operation_status != RepoOperationStatus::Scanning;
            self.workspace.retain(scanned);
            self.library.retain(scanned);
            app.update_repos(
                std::mem::take(&mut self.workspace),
                std::mem::take(&mut self.library),
            );
            if self.progressive {
                app.loading_progress = None;
            }
        } else if received {
            app.update_repos(self.workspace.clone(), self.library.clone());
            if self.progressive {
                let spinner = SPINNER_FRAMES[self.done % SPINNER_FRAMES.len()];
                app.loading_progress = Some(format!("{} {}/{}", spinner, self.done, self.total));
            }
        }

        !finished
    }
}

/// Reconcile seeded placeholder rows with the freshly enumerated repo set:
/// rows that no longer exist are dropped, newly appeared repos get a
/// placeholder. Submodule rows are kept as-is; they ride along with their
/// parent's scan.
fn merge_discovered(rows: &mut Vec<RepoInfo>, discovered: Vec<RepoInfo>) {
    rows.retain(|row| {
        row.is_submodule || display_names(&discovered).any(|name| name == row.display_name)
    });
    for repo in discovered {
        if !display_names(rows).any(|name| name == repo.display_name) {
            rows.push(repo);
        }
    }
}

/// Replace the rows a worker just produced, so each repo stays listed exactly
/// once: the placeholder it had goes, the scanned row takes its place. A
/// workspace repo's scan also yields a row per submodule, each replacing any
/// seeded row for that submodule.
fn replace_scanned(rows: &mut Vec<RepoInfo>, scanned: Vec<RepoInfo>) {
    rows.retain(|row| !display_names(&scanned).any(|name| name == row.display_name));
    rows.extend(scanned);
}

/// The display names of the given rows, which are what identifies a repo
fn display_names(repos: &[RepoInfo]) -> impl Iterator<Item = &str> {
    repos.iter().map(|repo| repo.display_name.as_str())
}

/// Result of a background sync job for one repo
enum SyncEvent {
    Started,
    Finished(crate::sync::SyncOutcome),
    Failed(String),
}

/// Schedules background jobs that fetch a repo's remotes and refresh its
/// status — the repo the selection rests on, plus any repo whose tracking
/// refs the watcher saw change. Jobs run on their own threads (up to
/// `MAX_CONCURRENT_SYNCS`) and report back through a channel drained by the
/// event loop.
struct SyncManager {
    tx: mpsc::Sender<(PathBuf, SyncEvent)>,
    rx: mpsc::Receiver<(PathBuf, SyncEvent)>,
    queue: VecDeque<PathBuf>,
    in_flight: HashSet<PathBuf>,
    /// Repos that were requested again while already syncing; re-queued once
    /// the running job finishes
    rerun_after: HashSet<PathBuf>,
    recently_synced: HashMap<PathBuf, Instant>,
    /// Selection waiting out the debounce window before being fetched
    selection_pending: Option<(PathBuf, Instant)>,
    /// Last selection acted on (fetched or skipped by cooldown), so a resting
    /// selection is handled once rather than re-queued every loop iteration
    last_selection: Option<PathBuf>,
    interrupt: Arc<AtomicBool>,
    workspace_path: String,
}

impl SyncManager {
    fn new(workspace_path: String) -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            tx,
            rx,
            queue: VecDeque::new(),
            in_flight: HashSet::new(),
            rerun_after: HashSet::new(),
            recently_synced: HashMap::new(),
            selection_pending: None,
            last_selection: None,
            interrupt: Arc::new(AtomicBool::new(false)),
            workspace_path,
        }
    }

    /// Queue a repo for syncing, deduplicating against queued and running
    /// jobs
    fn request_sync(&mut self, repo: PathBuf, from_watcher: bool) {
        if from_watcher
            && self
                .recently_synced
                .get(&repo)
                .is_some_and(|t| t.elapsed() < WATCHER_SYNC_COOLDOWN)
        {
            return;
        }
        if self.in_flight.contains(&repo) {
            self.rerun_after.insert(repo);
            return;
        }
        if !self.queue.contains(&repo) {
            self.queue.push_back(repo);
        }
    }

    /// Track the selected repo; a changed selection re-arms the debounce.
    /// Called every event-loop iteration.
    fn note_selection(&mut self, selected: Option<PathBuf>) {
        let Some(path) = selected else {
            self.selection_pending = None;
            return;
        };
        if self.last_selection.as_ref() == Some(&path) {
            // Already handled; also drop any pending fetch left by a row the
            // cursor only passed over on the way back here
            self.selection_pending = None;
            return;
        }
        if !self.selection_pending.as_ref().is_some_and(|(p, _)| *p == path) {
            self.selection_pending = Some((path, Instant::now()));
        }
    }

    /// Queue a fetch for the selection once it has rested past the debounce,
    /// unless the repo was fetched recently (returning to a row shouldn't
    /// refetch it). Held while a scan is loading so results don't race the
    /// loader; the initial selection therefore fires right after the first
    /// scan lands, which is what used to be the startup sync-all.
    fn pump_selection(&mut self, loader_active: bool) {
        if loader_active {
            return;
        }
        let Some((path, since)) = &self.selection_pending else {
            return;
        };
        if since.elapsed() < SELECTION_SYNC_DEBOUNCE {
            return;
        }
        let path = path.clone();
        self.selection_pending = None;
        self.last_selection = Some(path.clone());
        if self
            .recently_synced
            .get(&path)
            .is_some_and(|t| t.elapsed() < SELECTION_SYNC_COOLDOWN)
        {
            return;
        }
        self.request_sync(path, false);
    }

    /// Spawn queued jobs up to the concurrency cap
    fn pump(&mut self) {
        while self.in_flight.len() < MAX_CONCURRENT_SYNCS {
            let Some(repo) = self.queue.pop_front() else {
                break;
            };
            self.in_flight.insert(repo.clone());
            let tx = self.tx.clone();
            let interrupt = self.interrupt.clone();
            std::thread::spawn(move || {
                let _ = tx.send((repo.clone(), SyncEvent::Started));
                let event = match crate::sync::sync_repo(&repo, &interrupt) {
                    Ok(outcome) => SyncEvent::Finished(outcome),
                    Err(e) => SyncEvent::Failed(e.to_string()),
                };
                let _ = tx.send((repo, event));
            });
        }
    }

    /// Drain finished jobs into the app without blocking
    fn poll(&mut self, app: &mut App) {
        while let Ok((repo, event)) = self.rx.try_recv() {
            let display_name = workspace_display_name(&self.workspace_path, &repo);
            match event {
                SyncEvent::Started => {
                    app.fetching.insert(repo);
                    app.set_sync_status(&display_name, RepoOperationStatus::Fetching);
                }
                SyncEvent::Finished(outcome) => {
                    self.finish(&repo);
                    if let Some(status) = outcome.status {
                        app.apply_scan_result(&display_name, status, outcome.modification_time);
                    }
                    if let Some(err) = outcome.error_summary() {
                        app.set_sync_status(&display_name, RepoOperationStatus::SyncFailed(err));
                    } else {
                        app.clear_sync_status(&display_name);
                    }
                    app.fetching.remove(&repo);
                    // The fetch may have changed what the info panel shows
                    app.details.remove(&repo);
                    // Keep the full outcome for the panel's per-remote rows
                    app.sync_outcomes.insert(repo, outcome);
                }
                SyncEvent::Failed(err) => {
                    self.finish(&repo);
                    app.set_sync_status(&display_name, RepoOperationStatus::SyncFailed(err));
                    app.fetching.remove(&repo);
                }
            }
        }
    }

    fn finish(&mut self, repo: &PathBuf) {
        self.in_flight.remove(repo);
        self.recently_synced.insert(repo.clone(), Instant::now());
        if self.rerun_after.remove(repo) {
            self.queue.push_back(repo.clone());
        }
    }
}

/// Handles to work running on background threads, drained by the event loop
struct BackgroundTasks {
    loader: Option<RepoLoader>,
    suggestions: Option<mpsc::Receiver<Vec<String>>>,
    /// Delivers the error (if any) once the background clone of the given
    /// repo pattern finishes
    clone_result: Option<(String, mpsc::Receiver<Option<String>>)>,
    /// Delivers the file watcher once its (potentially slow) recursive
    /// registration of the workspace tree completes
    watcher: Option<mpsc::Receiver<Result<FileWatcher, notify::Error>>>,
    /// Fetches each repo's remotes and refreshes its status
    sync: SyncManager,
    /// Computes info-panel details for the selected repo
    details: DetailsLoader,
}

impl BackgroundTasks {
    /// Kick off a non-progressive background rescan, seeding it with the app's
    /// current rows so they keep their data while being refreshed.
    fn reload(&mut self, app: &App, workspace: &Workspace) {
        let (seed_workspace, seed_library) = app.repo_snapshot();
        self.loader = Some(RepoLoader::start(
            workspace,
            seed_workspace,
            seed_library,
            false,
        ));
    }
}

/// Detect the parent shell by reading /proc/self/status
fn detect_parent_shell() -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        // Read parent PID from /proc/self/status
        let status = std::fs::read_to_string("/proc/self/status").ok()?;
        let ppid_line = status.lines().find(|line| line.starts_with("PPid:"))?;
        let ppid: u32 = ppid_line.split_whitespace().nth(1)?.parse().ok()?;

        // Read the command name of the parent process
        let cmdline = std::fs::read_to_string(format!("/proc/{}/comm", ppid)).ok()?;
        let shell_name = cmdline.trim();

        // Check if it's a known shell
        if matches!(shell_name, "fish" | "bash" | "zsh" | "sh" | "dash" | "ksh") {
            // Find the full path to this shell
            if let Ok(output) = std::process::Command::new("which").arg(shell_name).output()
                && output.status.success()
            {
                return Some(String::from_utf8_lossy(&output.stdout).trim().to_string());
            }
            // Fallback to just the shell name
            return Some(shell_name.to_string());
        }
        None
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

const WORKSET_LOGO_SVG: &[u8] = include_bytes!("../../assets/workset.svg");

/// Rasterize the vendored logo SVG and prepare it for the terminal's graphics
/// protocol. Must run while raw mode is active but before the event loop
/// starts reading stdin, since the protocol query does a terminal roundtrip.
/// Any failure just means the help overlay renders without a logo.
fn build_help_image() -> Option<StatefulProtocol> {
    let tree =
        resvg::usvg::Tree::from_data(WORKSET_LOGO_SVG, &resvg::usvg::Options::default()).ok()?;

    // Rasterize at 3x for crispness, composited onto black to match the help
    // overlay's background since the SVG itself is transparent
    let scale = 3.0;
    let width = (tree.size().width() * scale).ceil() as u32;
    let height = (tree.size().height() * scale).ceil() as u32;
    let mut pixmap = resvg::tiny_skia::Pixmap::new(width, height)?;
    pixmap.fill(resvg::tiny_skia::Color::from_rgba8(0, 0, 0, 255));
    resvg::render(
        &tree,
        resvg::tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );

    // tiny-skia pixels are premultiplied RGBA
    let pixels = pixmap
        .pixels()
        .iter()
        .flat_map(|pixel| {
            let pixel = pixel.demultiply();
            [pixel.red(), pixel.green(), pixel.blue(), pixel.alpha()]
        })
        .collect();
    let image = image::DynamicImage::ImageRgba8(image::RgbaImage::from_raw(width, height, pixels)?);

    let picker = Picker::from_query_stdio().unwrap_or_else(|_| Picker::halfblocks());
    Some(picker.new_resize_protocol(image))
}

pub fn run_tui(workspace: &Workspace) -> Result<()> {
    loop {
        // Setup terminal FIRST so we can show progress
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
        let backend = CrosstermBackend::new(stdout);
        let mut terminal = Terminal::new(backend)?;

        // Create app with empty data; repos stream in from the background loader
        let mut app = App::new(workspace.path.clone(), Vec::new(), Vec::new());
        app.loading_progress = Some("loading...".to_string());
        // Query the graphics protocol now, before the event loop starts
        // reading stdin
        app.help_image = build_help_image();

        // Setup the debounced filesystem watcher on a background thread, since
        // recursively registering a large workspace can take a while and would
        // delay the first frame
        let workspace_path = PathBuf::from(&workspace.path);
        let (watcher_tx, watcher_rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = watcher_tx.send(FileWatcher::new(
                &workspace_path,
                Duration::from_millis(500),
            ));
        });
        let mut file_watcher: Option<FileWatcher> = None;

        let mut background = BackgroundTasks {
            loader: Some(RepoLoader::start(workspace, Vec::new(), Vec::new(), true)),
            suggestions: None,
            clone_result: None,
            watcher: Some(watcher_rx),
            sync: SyncManager::new(workspace.path.clone()),
            details: DetailsLoader::new(),
        };

        // Inner loop to handle actions without tearing down terminal
        loop {
            let action = run_app(&mut terminal, &mut app, &mut file_watcher, &mut background)?;

            // Handle the action
            match action {
                Action::None => {
                    // Stop in-flight sync jobs between git invocations
                    background.sync.interrupt.store(true, Ordering::Relaxed);
                    background.details.interrupt.store(true, Ordering::Relaxed);

                    // Drain any pending events before cleanup to avoid issues
                    while event::poll(Duration::from_millis(0))? {
                        let _ = event::read()?;
                    }

                    // Restore terminal before exiting
                    disable_raw_mode()?;
                    execute!(
                        terminal.backend_mut(),
                        LeaveAlternateScreen,
                        DisableMouseCapture
                    )?;
                    terminal.show_cursor()?;
                    return Ok(()); // Exit completely
                }
                Action::OpenShell(path) => {
                    // Restore terminal before opening shell
                    disable_raw_mode()?;
                    execute!(
                        terminal.backend_mut(),
                        LeaveAlternateScreen,
                        DisableMouseCapture
                    )?;
                    terminal.show_cursor()?;

                    // Use $SHELL or try to detect the actual parent shell
                    let shell = std::env::var("SHELL").unwrap_or_else(|_| {
                        detect_parent_shell().unwrap_or_else(|| "/bin/sh".to_string())
                    });

                    // Spawn an interactive shell in the repository directory
                    std::process::Command::new(&shell)
                        .current_dir(&path)
                        .status()?;

                    // After shell exits, break inner loop to restart outer loop (recreate terminal)
                    break;
                }
                Action::DropToLibrary(repo_paths) => {
                    run_repo_operation(
                        &mut terminal,
                        &mut app,
                        &repo_paths,
                        RepoOperationStatus::Dropping,
                        |repo_path| {
                            // Tree paths are already workspace-relative
                            let Ok(pattern) = repo_path.parse::<RepoPattern>();
                            // A repo left in place (uncommitted or unpushed
                            // changes) is a failure as far as the UI is
                            // concerned, so the row says why instead of
                            // silently reporting success
                            let report = workspace.drop(&pattern, false, false)?;
                            match report.skipped.first() {
                                Some((_, blocker)) => bail!("{blocker}"),
                                None => Ok(()),
                            }
                        },
                    )?;
                    background.reload(&app, workspace);
                }
                Action::RestoreFromLibrary(repo_paths) => {
                    run_repo_operation(
                        &mut terminal,
                        &mut app,
                        &repo_paths,
                        RepoOperationStatus::Restoring,
                        |repo_path| workspace.restore_from_library(repo_path),
                    )?;
                    background.reload(&app, workspace);
                }
                Action::CloneRepo(repo_pattern) => {
                    // Show a placeholder repo row while the clone runs
                    app.add_pending_clone(repo_pattern.clone());

                    // Clone on a background thread; the file watcher picks up
                    // the new repo and triggers a refresh when it lands
                    let (tx, rx) = mpsc::channel();
                    background.clone_result = Some((repo_pattern.clone(), rx));
                    let workspace = workspace.clone();
                    std::thread::spawn(move || {
                        let Ok(pattern) = repo_pattern.parse::<RepoPattern>();
                        let error = workspace.open(&pattern).err().map(|e| e.to_string());
                        let _ = tx.send(error);
                    });
                }
                Action::RefreshData => {
                    // Filesystem changed - reload repository data in the background
                    background.reload(&app, workspace);
                    if let Some(watcher) = file_watcher.as_mut() {
                        watcher.drain_pending();
                    }
                }
            }
        }
    }
}

/// Run a drop/restore operation over the given repos, updating each repo's
/// status in the UI as it progresses
fn run_repo_operation<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    app: &mut App,
    repo_paths: &[String],
    in_progress: RepoOperationStatus,
    mut operation: impl FnMut(&str) -> Result<()>,
) -> Result<()> {
    let draw = |terminal: &mut Terminal<B>, app: &mut App| -> Result<()> {
        terminal
            .draw(|f| ui(f, app))
            .map_err(|e| anyhow!("Failed to render frame: {}", e))?;
        Ok(())
    };

    for repo_path in repo_paths {
        app.update_repo_status(repo_path, in_progress.clone());
        draw(terminal, app)?;

        // Small delay so user can see the status change
        std::thread::sleep(Duration::from_millis(100));

        match operation(repo_path) {
            Ok(_) => app.update_repo_status(repo_path, RepoOperationStatus::Success),
            Err(e) => app.update_repo_status(repo_path, RepoOperationStatus::Failed(e.to_string())),
        }
        draw(terminal, app)?;
    }

    // Wait a moment for user to see the result
    std::thread::sleep(Duration::from_millis(500));

    Ok(())
}

/// What the event loop should do with a key before any mode looks at it
#[derive(Debug, PartialEq, Eq)]
enum KeyBinding {
    /// Ctrl+C, which quits from every mode
    Quit,
    /// A character pressed with Ctrl/Alt/Super. The TUI binds bare characters
    /// only, so these have no binding and must not be mistaken for the
    /// unmodified key: Ctrl+D is not the 'd' that drops the selected repos,
    /// and terminals do send it (it's what a terminal delivers for EOF).
    Ignored,
    /// Dispatch to the current mode
    Key(KeyCode),
}

fn classify_key(key: &event::KeyEvent) -> KeyBinding {
    // Shift is how a terminal reports '?' and other shifted characters, so it
    // doesn't make a character modified
    const MODIFIED: KeyModifiers = KeyModifiers::CONTROL
        .union(KeyModifiers::ALT)
        .union(KeyModifiers::SUPER);

    match key.code {
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => KeyBinding::Quit,
        KeyCode::Char(_) if key.modifiers.intersects(MODIFIED) => KeyBinding::Ignored,
        code => KeyBinding::Key(code),
    }
}

fn run_app<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    app: &mut App,
    file_watcher: &mut Option<FileWatcher>,
    background: &mut BackgroundTasks,
) -> Result<Action> {
    loop {
        // Apply results from background work before drawing
        if let Some(rx) = &background.watcher {
            match rx.try_recv() {
                Ok(Ok(watcher)) => {
                    *file_watcher = Some(watcher);
                    background.watcher = None;
                }
                Ok(Err(_)) => {
                    app.watch_disabled = true;
                    background.watcher = None;
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => background.watcher = None,
            }
        }
        if let Some(loader) = background.loader.as_mut()
            && !loader.poll(app)
        {
            background.loader = None;
            // Drain events generated by the scan to prevent a feedback loop
            if let Some(watcher) = file_watcher.as_mut() {
                watcher.drain_pending();
            }
        }
        let loader_active = background.loader.is_some();
        background.sync.poll(app);
        background.sync.note_selection(app.selected_syncable_repo_path());
        background.sync.pump_selection(loader_active);
        background.sync.pump();
        background.details.poll(app);
        background.details.note_selection(app);
        background.details.pump(app);
        poll_suggestions(app, background);
        if let Some((pattern, rx)) = &background.clone_result {
            match rx.try_recv() {
                Ok(error) => {
                    let pattern = pattern.clone();
                    app.finish_pending_clone(&pattern, error);
                    background.clone_result = None;
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => {
                    let pattern = pattern.clone();
                    app.finish_pending_clone(&pattern, Some("clone interrupted".to_string()));
                    background.clone_result = None;
                }
            }
        }
        app.expire_failed_clones(Duration::from_secs(5));

        terminal
            .draw(|f| ui(f, app))
            .map_err(|e| anyhow!("Failed to render frame: {}", e))?;

        // Check for filesystem changes. Remote-tracking ref updates (e.g. a
        // push from another terminal) trigger a sync check for that repo;
        // worktree changes trigger a reload unless one is already running
        if let Some(watcher) = file_watcher.as_mut() {
            let signals = watcher.poll();
            for repo in signals.refs_changed {
                background.sync.request_sync(repo, true);
            }
            if signals.refresh && background.loader.is_none() {
                return Ok(Action::RefreshData);
            }
        }

        // Use poll with timeout to allow checking for filesystem updates periodically
        if event::poll(Duration::from_millis(100))? {
            // Ignore other event types (Mouse, Resize, etc.)
            let Event::Key(key) = event::read()? else {
                continue;
            };

            let code = match classify_key(&key) {
                // Ctrl+C quits from any mode, including the help overlay and
                // the search and clone prompts
                KeyBinding::Quit => return Ok(Action::None),
                // Every other binding is the bare character, so a modified one
                // is not it: Ctrl+D is not 'd' and must not drop the selection
                KeyBinding::Ignored => continue,
                KeyBinding::Key(code) => code,
            };

            // While the help overlay is open, it swallows all input except
            // '?'/Esc to close
            if app.help_visible {
                match code {
                    KeyCode::Char('?') | KeyCode::Esc => app.help_visible = false,
                    _ => {}
                }
                continue;
            }

            match app.mode {
                AppMode::Normal => match code {
                    KeyCode::Char('d') => {
                        // Drop workspace repo(s) to library
                        if app.active_section == Section::Workspace
                            && let Some(node) = app.selected_node()
                        {
                            let repo_paths = node.collect_repo_paths();
                            if !repo_paths.is_empty() {
                                return Ok(Action::DropToLibrary(repo_paths));
                            }
                        }
                    }
                    KeyCode::Right | KeyCode::Left => {
                        app.toggle_expand();
                    }
                    KeyCode::Char('c') => {
                        // Clone repo dialog; suggestions arrive from a
                        // background thread since gh/glab may hit the network
                        app.mode = AppMode::CloneRepo;
                        app.clone_repo_input.clear();
                        app.clone_repo_suggestions.clear();
                        app.clone_repo_state.select(None);
                        app.suggestions_loading = true;

                        let (tx, rx) = mpsc::channel();
                        background.suggestions = Some(rx);
                        std::thread::spawn(move || {
                            let mut suggestions = get_github_suggestions();
                            suggestions.extend(get_gitlab_suggestions());
                            suggestions.sort();
                            suggestions.dedup();
                            let _ = tx.send(suggestions);
                        });
                    }
                    KeyCode::Esc => {
                        // Return None to exit - cleanup happens in the outer loop
                        return Ok(Action::None);
                    }
                    KeyCode::Tab => {
                        // Tab switches between workspace and library
                        app.switch_section();
                    }
                    KeyCode::Down => app.next(),
                    KeyCode::Up => app.previous(),
                    KeyCode::Enter => {
                        // Enter on workspace repo = open shell
                        // Enter on library repo = restore from library
                        // Enter on a directory node = toggle expansion
                        let selected = app.selected_node().map(|node| {
                            (
                                node.repo_info.as_ref().map(|r| r.path.clone()),
                                node.collect_repo_paths(),
                            )
                        });

                        if let Some((repo_dir, repo_paths)) = selected {
                            match app.active_section {
                                Section::Workspace => match repo_dir {
                                    Some(path) => return Ok(Action::OpenShell(path)),
                                    None => app.toggle_expand(),
                                },
                                Section::Library => {
                                    if !repo_paths.is_empty() {
                                        return Ok(Action::RestoreFromLibrary(repo_paths));
                                    }
                                    app.toggle_expand();
                                }
                            }
                        }
                    }
                    KeyCode::Char('?') => {
                        app.help_visible = true;
                    }
                    KeyCode::Char('/') => {
                        // Keep any existing query so '/' resumes editing an
                        // active filter
                        app.mode = AppMode::Search;
                    }
                    _ => {}
                },
                AppMode::Search => match code {
                    KeyCode::Esc => {
                        app.mode = AppMode::Normal;
                        app.clear_search();
                    }
                    KeyCode::Enter => {
                        // Confirm the filter and return to normal mode
                        app.mode = AppMode::Normal;
                    }
                    KeyCode::Down => app.next(),
                    KeyCode::Up => app.previous(),
                    KeyCode::Char(c) => {
                        app.search_query.push(c);
                        app.filter_repos();
                    }
                    KeyCode::Backspace => {
                        app.search_query.pop();
                        app.filter_repos();
                    }
                    _ => {}
                },
                AppMode::CloneRepo => match code {
                    KeyCode::Esc => {
                        app.mode = AppMode::Normal;
                        app.clone_repo_input.clear();
                    }
                    KeyCode::Enter => {
                        // Use selected suggestion or manual input
                        let repo = app
                            .clone_repo_state
                            .selected()
                            .and_then(|idx| {
                                app.filtered_suggestions().get(idx).map(|s| s.to_string())
                            })
                            .unwrap_or_else(|| app.clone_repo_input.clone());

                        if !repo.is_empty() {
                            app.mode = AppMode::Normal;
                            return Ok(Action::CloneRepo(repo));
                        }
                    }
                    KeyCode::Down => {
                        let len = app.filtered_suggestions().len();
                        if len > 0 {
                            let next = match app.clone_repo_state.selected() {
                                Some(i) if i + 1 >= len => 0,
                                Some(i) => i + 1,
                                None => 0,
                            };
                            app.clone_repo_state.select(Some(next));
                        }
                    }
                    KeyCode::Up => {
                        let len = app.filtered_suggestions().len();
                        if len > 0 {
                            let prev = match app.clone_repo_state.selected() {
                                Some(0) => len - 1,
                                Some(i) => i - 1,
                                None => 0,
                            };
                            app.clone_repo_state.select(Some(prev));
                        }
                    }
                    KeyCode::Char(c) => {
                        app.clone_repo_input.push(c);
                        app.clone_repo_state.select(Some(0));
                    }
                    KeyCode::Backspace => {
                        app.clone_repo_input.pop();
                    }
                    _ => {}
                },
            }
        }
    }
}

/// Apply clone-dialog suggestions once the background fetch completes
fn poll_suggestions(app: &mut App, background: &mut BackgroundTasks) {
    let Some(rx) = &background.suggestions else {
        return;
    };
    match rx.try_recv() {
        Ok(mut suggestions) => {
            // Filter out repos that already exist in workspace or library
            let existing_repos: std::collections::HashSet<String> = app
                .get_flattened_workspace()
                .iter()
                .chain(app.get_flattened_library().iter())
                .filter_map(|(node, _, _, _)| {
                    node.repo_info.as_ref().map(|r| r.display_name.clone())
                })
                .collect();
            suggestions.retain(|s| !existing_repos.contains(s));

            app.clone_repo_suggestions = suggestions;
            if !app.clone_repo_suggestions.is_empty() && app.clone_repo_state.selected().is_none() {
                app.clone_repo_state.select(Some(0));
            }
            app.suggestions_loading = false;
            background.suggestions = None;
        }
        Err(mpsc::TryRecvError::Empty) => {}
        Err(mpsc::TryRecvError::Disconnected) => {
            app.suggestions_loading = false;
            background.suggestions = None;
        }
    }
}

fn ui(f: &mut Frame, app: &mut App) {
    app.single_panel = f.area().width < SINGLE_PANEL_THRESHOLD;

    if app.mode == AppMode::CloneRepo {
        render_clone_repo_dialog(f, app);
        return;
    }

    // Split vertically into rows; the search box appears while search mode is
    // active or a filter is applied
    let show_search = app.mode == AppMode::Search || !app.search_query.is_empty();
    let mut constraints = vec![
        Constraint::Min(0), // Main area (workspace + library side by side)
    ];
    if show_search {
        constraints.push(Constraint::Length(3)); // Search box
    }
    let vertical_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(f.area());

    if app.single_panel {
        // Too narrow for the side-by-side layout; show only the active panel
        render_tree_panel(f, app, vertical_chunks[0], app.active_section);
    } else {
        // Split the main area horizontally into workspace (left) and library (right)
        let horizontal_chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Percentage(50), // Workspace (left)
                Constraint::Percentage(50), // Library (right)
            ])
            .split(vertical_chunks[0]);

        render_tree_panel(f, app, horizontal_chunks[0], Section::Workspace);

        render_tree_panel(f, app, horizontal_chunks[1], Section::Library);
    }

    if show_search {
        // Yellow with a cursor while editing; dimmed when the filter is
        // merely applied
        let (search_text, search_style, title) = if app.mode == AppMode::Search {
            (
                format!("{}_", app.search_query),
                Style::default().fg(Color::Yellow),
                "Search",
            )
        } else {
            (
                app.search_query.clone(),
                Style::default().fg(Color::DarkGray),
                "Filter (/ to edit)",
            )
        };
        let search = Paragraph::new(search_text)
            .style(search_style)
            .alignment(Alignment::Left)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(search_style)
                    .title(title),
            );
        f.render_widget(search, vertical_chunks[1]);
    }

    if app.help_visible {
        render_help_dialog(f, app);
    }
}

/// The keybindings shown in the help overlay
fn help_bindings(app: &App) -> Vec<(&'static str, Color, &'static str)> {
    let enter_action = match app.active_section {
        Section::Workspace => "open",
        Section::Library => "restore",
    };

    let tab_action = if app.single_panel {
        match app.active_section {
            Section::Workspace => "show library",
            Section::Library => "show workspace",
        }
    } else {
        "switch section"
    };

    let mut bindings: Vec<(&'static str, Color, &'static str)> = vec![
        ("Tab", Color::Cyan, tab_action),
        ("↑/↓", Color::Cyan, "navigate"),
        ("←/→", Color::Cyan, "expand/collapse"),
        ("Enter", Color::Green, enter_action),
    ];
    if app.active_section == Section::Workspace {
        bindings.push(("d", Color::Yellow, "drop"));
    }
    bindings.push(("c", Color::Magenta, "clone"));
    bindings.push(("/", Color::Yellow, "search"));
    bindings.push(("?", Color::Cyan, "help"));
    bindings.push(("Esc", Color::Red, "quit"));
    bindings
}

/// Render the keybindings overlay, shown while '?' is held
fn render_help_dialog(f: &mut Frame, app: &mut App) {
    let bindings = help_bindings(app);
    let image_rows: u16 = if app.help_image.is_some() { 7 } else { 0 };

    let area = f.area();
    let dialog_width = area.width.min(56);
    let dialog_height = area.height.min(image_rows + bindings.len() as u16 + 2);
    let dialog_area = Rect::new(
        (area.width.saturating_sub(dialog_width)) / 2,
        (area.height.saturating_sub(dialog_height)) / 2,
        dialog_width,
        dialog_height,
    );

    f.render_widget(Clear, dialog_area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .style(Style::default().bg(Color::Black));
    let inner = block.inner(dialog_area);
    f.render_widget(block, dialog_area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(image_rows), Constraint::Min(0)])
        .split(inner);

    if let Some(protocol) = app.help_image.as_mut() {
        f.render_stateful_widget(StatefulImage::default(), chunks[0], protocol);
    }

    // Two aligned columns, centered as a block (centering each line
    // individually would break the key column)
    let key_width = bindings
        .iter()
        .map(|(key, _, _)| key.chars().count())
        .max()
        .unwrap_or(0);
    let block_width = key_width
        + 2
        + bindings
            .iter()
            .map(|(_, _, description)| description.chars().count())
            .max()
            .unwrap_or(0);
    let indent = " ".repeat((chunks[1].width as usize).saturating_sub(block_width) / 2);
    let lines: Vec<Line> = bindings
        .iter()
        .map(|(key, color, description)| {
            let pad = " ".repeat(key_width - key.chars().count());
            Line::from(vec![
                Span::raw(format!("{indent}{pad}")),
                Span::styled(
                    *key,
                    Style::default().fg(*color).add_modifier(Modifier::BOLD),
                ),
                Span::raw("  "),
                Span::raw(*description),
            ])
        })
        .collect();
    let help = Paragraph::new(lines);
    f.render_widget(help, chunks[1]);
}

/// Detail rows rendered under the selected repo: size, worktree line changes
/// (workspace only — library repos are always clean), and one row per remote
/// with its sync state. Fields still being computed by the details loader
/// show an ellipsis.
fn repo_detail_lines(app: &App, repo: &RepoInfo, section: Section, depth: usize) -> Vec<Line<'static>> {
    let dim = Style::default().fg(Color::DarkGray);
    let details = app.details.get(&repo.path).cloned().unwrap_or_default();
    let indent = "  ".repeat(depth + 3);

    let mut lines = Vec::new();

    let size = match details.size_bytes.or(repo.size_bytes) {
        Some(bytes) => Span::raw(format_size(bytes)),
        None => Span::styled("…", dim),
    };
    lines.push(Line::from(vec![
        Span::raw(indent.clone()),
        Span::styled("Size: ", dim),
        size,
    ]));

    if section == Section::Workspace {
        let mut line = vec![Span::raw(indent.clone()), Span::styled("Changes: ", dim)];
        match details.line_changes {
            Some((0, 0)) => line.push(Span::styled("clean", Style::default().fg(Color::Green))),
            Some((added, removed)) => {
                line.push(Span::styled(
                    format!("+{}", added),
                    Style::default().fg(Color::Green),
                ));
                line.push(Span::raw(" "));
                line.push(Span::styled(
                    format!("-{}", removed),
                    Style::default().fg(Color::Red),
                ));
            }
            None => line.push(Span::styled("…", dim)),
        }
        lines.push(Line::from(line));
    }

    match &details.remotes {
        None => lines.push(Line::from(vec![
            Span::raw(indent.clone()),
            Span::styled("Remotes: ", dim),
            Span::styled("…", dim),
        ])),
        Some(remotes) if remotes.is_empty() => {
            lines.push(Line::from(vec![
                Span::raw(indent.clone()),
                Span::styled("Remotes: ", dim),
                Span::styled("none", dim),
            ]));
        }
        Some(remotes) => {
            lines.push(Line::from(vec![
                Span::raw(indent.clone()),
                Span::styled("Remotes:", dim),
            ]));
            let outcome = app.sync_outcomes.get(&repo.path);
            let fetching = app.fetching.contains(&repo.path);
            for remote in remotes {
                let state = remote_sync_state(&remote.name, outcome, fetching);
                let mut line = remote_status_line(remote, state);
                line.spans.insert(0, Span::raw(indent.clone()));
                lines.push(line);
            }
        }
    }
    lines
}

/// One detail row showing a remote, the id its tracking ref holds for the
/// checked-out branch, its sync state, and how many commits it is behind
/// (when it is)
fn remote_status_line(remote: &RemoteInfo, state: RemoteSyncState) -> Line<'static> {
    // While fetching, the behind count is about to be superseded. The commit
    // id stays: it states what the local tracking ref holds, which remains
    // true until the fetch rewrites it, and hiding it would make the line
    // jump on every fetch.
    let show_behind = remote.behind > 0 && state != RemoteSyncState::Fetching;
    let status = match state {
        RemoteSyncState::Fetching => Some(Span::styled(
            "fetching…",
            Style::default().fg(Color::DarkGray),
        )),
        // A behind remote isn't "in sync"; the behind span below says it all
        RemoteSyncState::InSync if show_behind => None,
        RemoteSyncState::InSync => Some(Span::styled(
            "✓ in sync",
            Style::default().fg(Color::Green),
        )),
        RemoteSyncState::FetchError(msg) => Some(Span::styled(
            format!("⚠ {}", msg),
            Style::default().fg(Color::Red),
        )),
        RemoteSyncState::Pending => Some(Span::styled(
            "not synced yet",
            Style::default().fg(Color::DarkGray),
        )),
    };
    let mut spans = vec![Span::raw("  "), Span::raw(remote.name.clone())];
    if let Some(commit) = &remote.commit {
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            commit.clone(),
            Style::default().fg(Color::DarkGray),
        ));
    }
    if let Some(status) = status {
        spans.push(Span::raw(" "));
        spans.push(status);
    }
    if show_behind {
        spans.push(Span::styled(
            format!(" ↓ {} behind", remote.behind),
            Style::default().fg(Color::Yellow),
        ));
    }
    Line::from(spans)
}

/// Render the workspace or library tree panel
fn render_tree_panel(f: &mut Frame, app: &App, area: Rect, section: Section) {
    // Account for: 2 for borders, 2 for highlight symbol ">> ", 1 for padding on right
    let available_width = area.width.saturating_sub(5) as usize;

    // Workspace repos show status icons and modification time; library repos show size
    type IdleMetadata = fn(&RepoInfo) -> String;
    let (items, state, title, show_status_icons, idle_metadata) = match section {
        Section::Workspace => (
            app.get_flattened_workspace(),
            &app.workspace_state,
            workspace_panel_title(app, area.width),
            true,
            (|repo| {
                repo.modification_time
                    .map(format_time_ago_verbose)
                    .unwrap_or_default()
            }) as IdleMetadata,
        ),
        Section::Library => (
            app.get_flattened_library(),
            &app.library_state,
            Line::from(format!("Library ({})", app.count_library_repos())),
            false,
            (|repo| repo.size_bytes.map(format_size).unwrap_or_default()) as IdleMetadata,
        ),
    };

    let mut list_items: Vec<ListItem> = items
        .iter()
        .map(|(node, depth, _, full_path)| {
            tree_list_item(
                node,
                *depth,
                full_path,
                app,
                available_width,
                show_status_icons,
                idle_metadata,
            )
        })
        .collect();

    // Splice detail rows in below the highlighted repo. They sit strictly
    // after the selected index, so the selection model and highlight are
    // unaffected and the rows can't be selected. Rows may be clipped when the
    // selection is at the bottom of the viewport, since the list keeps only
    // the selected row visible.
    if app.active_section == section
        && let Some(selected) = state.selected()
        && let Some((node, depth, _, _)) = items.get(selected)
        && let Some(repo) = node.repo_info.as_ref()
    {
        let detail_items = repo_detail_lines(app, repo, section, *depth)
            .into_iter()
            .map(ListItem::new);
        list_items.splice(selected + 1..selected + 1, detail_items);
    }

    let list = List::new(list_items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(title)
                .border_style(if app.active_section == section {
                    Style::default().fg(Color::Cyan)
                } else {
                    Style::default()
                }),
        )
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol(">> ");

    // Create a custom state wrapper for rendering
    let mut list_state = ratatui::widgets::ListState::default();
    list_state.select(state.selected());
    f.render_stateful_widget(list, area, &mut list_state);
}

/// Title for the workspace panel: the workspace path (truncated to fit),
/// repo count, and any transient loading/watcher markers
fn workspace_panel_title(app: &App, panel_width: u16) -> Line<'static> {
    let suffix = format!(" ({})", app.count_workspace_repos());

    let mut markers = String::new();
    if let Some(ref progress) = app.loading_progress {
        markers.push_str(&format!(" {}", progress));
    }
    if app.watch_disabled {
        markers.push_str(" [watch disabled]");
    }

    // Fit the path into what's left after borders, the count, and the markers
    let max_path_width = (panel_width.saturating_sub(2) as usize)
        .saturating_sub(suffix.chars().count())
        .saturating_sub(markers.chars().count());
    let path = truncate_start(&shorten_home(&app.workspace_path), max_path_width);

    let mut spans = vec![Span::raw(format!("{}{}", path, suffix))];
    if !markers.is_empty() {
        spans.push(Span::styled(markers, Style::default().fg(Color::DarkGray)));
    }
    Line::from(spans)
}

/// Replace a home directory prefix with `~`
fn shorten_home(path: &str) -> String {
    if let Ok(home) = std::env::var("HOME")
        && !home.is_empty()
        && let Some(rest) = path.strip_prefix(&home)
        && (rest.is_empty() || rest.starts_with('/'))
    {
        return format!("~{}", rest);
    }
    path.to_string()
}

/// Truncate the front of a string with a leading ellipsis so it fits in
/// `max_chars` characters
fn truncate_start(s: &str, max_chars: usize) -> String {
    let len = s.chars().count();
    if len <= max_chars {
        return s.to_string();
    }
    let Some(keep) = max_chars.checked_sub(1) else {
        return String::new();
    };
    let tail: String = s.chars().skip(len - keep).collect();
    format!("…{}", tail)
}

/// Render a single tree node as a list item
fn tree_list_item<'a>(
    node: &'a TreeNode,
    depth: usize,
    full_path: &str,
    app: &App,
    available_width: usize,
    show_status_icons: bool,
    idle_metadata: fn(&RepoInfo) -> String,
) -> ListItem<'a> {
    let mut spans = vec![];

    // Add tree structure indicators
    if depth > 0 {
        spans.push(Span::raw("  ".repeat(depth)));
    }

    // Add expand/collapse indicator
    if !node.children.is_empty() {
        let is_git_submodule = node
            .repo_info
            .as_ref()
            .map(|r| r.is_submodule)
            .unwrap_or(false);
        let indicator = match (is_git_submodule, node.expanded) {
            (true, true) => "◇ ",
            (true, false) => "◆ ",
            (false, true) => "▼ ",
            (false, false) => "▶ ",
        };
        spans.push(Span::styled(indicator, Style::default().fg(Color::Cyan)));
    } else if depth > 0 {
        spans.push(Span::raw("  "));
    }

    // Add status icon for repos only
    if show_status_icons && let Some(ref repo) = node.repo_info {
        if repo.is_submodule {
            // Submodule indicator
            if repo.submodule_initialized {
                spans.push(Span::styled("S ", Style::default().fg(Color::Magenta)));
            } else {
                spans.push(Span::styled("S ", Style::default().fg(Color::DarkGray)));
                spans.push(Span::styled(
                    "(uninit) ",
                    Style::default().fg(Color::DarkGray),
                ));
            }
        } else {
            let (icon, color) = match repo.status {
                // Not scanned yet (includes placeholder rows for clones)
                None => ("· ", Color::DarkGray),
                Some(crate::RepoStatus::Clean) => ("✓ ", Color::Green),
                Some(crate::RepoStatus::Dirty) => ("* ", Color::Yellow),
                Some(crate::RepoStatus::Unpushed) => ("↑ ", Color::Yellow),
                Some(crate::RepoStatus::NoCommits) => ("· ", Color::DarkGray),
                // Not "empty": workset couldn't read this repo at all
                Some(crate::RepoStatus::Unknown) => ("? ", Color::Red),
            };
            spans.push(Span::styled(icon, Style::default().fg(color)));
        }
    }

    // Add name with search highlighting
    if !app.search_query.is_empty() {
        // For directory nodes, check if the search query contains this directory as a path component
        let should_highlight_dir =
            node.repo_info.is_none() && app.search_query.contains(&format!("{}/", node.name));

        // Try to match against the full path for this node
        let indices = app
            .matcher
            .fuzzy_indices(full_path, &app.search_query)
            .map(|(_, indices)| indices);

        spans.extend(render_highlighted_name(
            &node.name,
            full_path,
            should_highlight_dir,
            indices,
        ));
    } else {
        spans.push(Span::raw(&node.name));
    }

    // Add right-aligned operation status or idle metadata for repos
    if let Some(ref repo) = node.repo_info {
        // Current text width (accounting for unicode characters)
        let text_width: usize = spans.iter().map(|s| s.content.chars().count()).sum();

        let (status_text, status_color) = match &repo.operation_status {
            RepoOperationStatus::None => (idle_metadata(repo), Color::DarkGray),
            RepoOperationStatus::Scanning => ("scanning".to_string(), Color::DarkGray),
            RepoOperationStatus::Fetching => ("fetching".to_string(), Color::DarkGray),
            RepoOperationStatus::SyncFailed(err) => (format!("sync failed: {}", err), Color::Red),
            RepoOperationStatus::Cloning => ("cloning...".to_string(), Color::Magenta),
            RepoOperationStatus::Dropping => ("dropping...".to_string(), Color::Yellow),
            RepoOperationStatus::Restoring => ("restoring...".to_string(), Color::Cyan),
            RepoOperationStatus::Success => ("done".to_string(), Color::Green),
            RepoOperationStatus::Failed(err) => (format!("failed: {}", err), Color::Red),
        };

        spans.extend(render_metadata_span(
            text_width,
            available_width,
            status_text,
            status_color,
        ));
    }

    ListItem::new(Line::from(spans))
}

fn render_clone_repo_dialog(f: &mut Frame, app: &App) {
    // Create a centered dialog
    let area = f.area();
    let dialog_width = area.width.min(80);
    let dialog_height = area.height.min(20);

    let dialog_area = Rect {
        x: (area.width - dialog_width) / 2,
        y: (area.height - dialog_height) / 2,
        width: dialog_width,
        height: dialog_height,
    };

    // Clear the background
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title("Clone Repository")
        .style(Style::default().bg(Color::Black));
    f.render_widget(block, dialog_area);

    // Split into input and suggestions
    let inner = dialog_area.inner(ratatui::layout::Margin {
        horizontal: 1,
        vertical: 1,
    });

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // Input box
            Constraint::Min(0),    // Suggestions
        ])
        .split(inner);

    // Input box
    let input_text = format!("{}_", app.clone_repo_input);
    let input = Paragraph::new(input_text)
        .style(Style::default().fg(Color::Yellow))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("Repository (e.g. github.com/user/repo)"),
        );
    f.render_widget(input, chunks[0]);

    // Suggestions list
    let filtered_suggestions = app.filtered_suggestions();
    let suggestion_items: Vec<ListItem> = filtered_suggestions
        .iter()
        .map(|s| ListItem::new(*s))
        .collect();

    let suggestions_title = if app.suggestions_loading {
        format!("Suggestions ({}) - loading...", filtered_suggestions.len())
    } else {
        format!("Suggestions ({})", filtered_suggestions.len())
    };
    let suggestions = List::new(suggestion_items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(suggestions_title),
        )
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol(">> ");

    let mut state = ratatui::widgets::ListState::default();
    state.select(app.clone_repo_state.selected());
    f.render_stateful_widget(suggestions, chunks[1], &mut state);
}

/// Render right-aligned metadata (status or size) with padding
fn render_metadata_span<'a>(
    text_width: usize,
    available_width: usize,
    metadata_text: String,
    metadata_color: Color,
) -> Vec<Span<'a>> {
    let mut spans = vec![];

    if !metadata_text.is_empty() {
        let metadata_width = metadata_text.chars().count();
        // Calculate padding needed to right-align (ensure at least 1 space)
        let padding_needed = available_width
            .saturating_sub(text_width)
            .saturating_sub(metadata_width)
            .max(1);
        spans.push(Span::raw(" ".repeat(padding_needed)));
        spans.push(Span::styled(
            metadata_text,
            Style::default().fg(metadata_color),
        ));
    }

    spans
}

/// Render the node's name with the characters the search matched highlighted.
///
/// `indices` are character positions in `full_path`, the whole path of the row
/// (see [`TreeNode::flatten`](tree::TreeNode::flatten)), whose tail is this
/// node's name — a match on an ancestor's characters belongs to that
/// ancestor's row, not this one.
fn render_highlighted_name<'a>(
    node_name: &'a str,
    full_path: &str,
    should_highlight_dir: bool,
    indices: Option<Vec<usize>>,
) -> Vec<Span<'a>> {
    let span = |text: &'a str, matched: bool| {
        if matched {
            Span::styled(text, Style::default().fg(Color::Black).bg(Color::Yellow))
        } else {
            Span::raw(text)
        }
    };

    // Character positions within the name itself
    let matched: HashSet<usize> = match indices {
        Some(indices) => {
            let Some(offset) = full_path
                .chars()
                .count()
                .checked_sub(node_name.chars().count())
            else {
                return vec![span(node_name, false)];
            };
            indices
                .into_iter()
                .filter_map(|index| index.checked_sub(offset))
                .collect()
        }
        // Nothing in this row's path matched. A directory the query names
        // outright ("<name>/") is still highlighted whole.
        None => return vec![span(node_name, should_highlight_dir)],
    };

    // One span per run of consecutively matched (or unmatched) characters
    let mut spans = Vec::new();
    let mut run: Option<(usize, bool)> = None;
    for (position, (byte, _)) in node_name.char_indices().enumerate() {
        let is_match = matched.contains(&position);
        match run {
            Some((start, was_match)) if was_match != is_match => {
                spans.push(span(&node_name[start..byte], was_match));
                run = Some((byte, is_match));
            }
            Some(_) => {}
            None => run = Some((byte, is_match)),
        }
    }
    if let Some((start, was_match)) = run {
        spans.push(span(&node_name[start..], was_match));
    }
    spans
}

/// Workspace-relative display name for a repo path
fn workspace_display_name(workspace_path: &str, path: &Path) -> String {
    path.strip_prefix(workspace_path)
        .unwrap_or(path)
        .display()
        .to_string()
        .trim_start_matches('/')
        .to_string()
}

/// Fill in a discovered workspace repo's status and modification time, and
/// return it followed by a row for each of its submodules
fn scan_workspace_repo(mut repo: RepoInfo) -> Vec<RepoInfo> {
    // Check repo status and get modification time in a single repo open for performance
    let (status, modification_time) = crate::check_repo_status_and_modification_time(&repo.path)
        .unwrap_or((crate::RepoStatus::Unknown, None));
    repo.status = Some(status);
    repo.modification_time = modification_time;
    repo.operation_status = RepoOperationStatus::None;
    // Size not computed for workspace repos to save time

    let path = repo.path.clone();
    let display_name = repo.display_name.clone();
    let mut infos = vec![repo];

    // Find and add submodules
    for submodule in crate::find_submodules_in_repo(&path).unwrap_or_default() {
        let submodule_display_name = if display_name.is_empty() {
            submodule.path.display().to_string()
        } else {
            format!("{}/{}", display_name, submodule.path.display())
        };

        infos.push(RepoInfo {
            path: path.join(&submodule.path),
            display_name: submodule_display_name,
            status: Some(crate::RepoStatus::Clean), // Submodule status computed separately
            is_submodule: true,
            submodule_initialized: submodule.initialized,
            parent_repo_path: Some(path.clone()),
            ..Default::default()
        });
    }

    infos
}

/// Fill in a discovered library repo's metadata
fn scan_library_repo(mut repo: RepoInfo) -> RepoInfo {
    repo.modification_time = get_repo_modification_time(&repo.path).ok();
    repo.size_bytes = get_repo_size(&repo.path).ok();
    repo.operation_status = RepoOperationStatus::None;
    repo
}

/// Enumerate and scan all workspace and library repositories on worker
/// threads, streaming results to the UI thread. Runs on a background thread;
/// exits early if the receiver is dropped.
fn scan_all_repos(workspace: &Workspace, tx: mpsc::Sender<LoadEvent>) {
    enum ScanTask {
        Workspace(RepoInfo),
        Library(RepoInfo),
    }

    let library_path = workspace.library_path();

    // Enumerate the repo set — and derive each row's tree path, which needs a
    // repo open — before any status work, so every row can render with a
    // "scanning" status right away. These rows are also the scan's task list,
    // so each repo is placed in the tree exactly once.
    let discovered_workspace: Vec<RepoInfo> = find_git_repositories(Path::new(&workspace.path))
        .unwrap_or_default()
        .into_iter()
        .map(|path| RepoInfo {
            display_name: workspace_display_name(&workspace.path, &path),
            tree_path: crate::remote_tree_path(&path),
            path,
            operation_status: RepoOperationStatus::Scanning,
            ..Default::default()
        })
        .collect();
    let discovered_library: Vec<RepoInfo> = workspace
        .list_library()
        .unwrap_or_default()
        .into_iter()
        .map(|repo_path| {
            let path = PathBuf::from(&library_path).join(&repo_path);
            RepoInfo {
                tree_path: crate::remote_tree_path(&path),
                path,
                display_name: repo_path,
                status: Some(crate::RepoStatus::Clean), // Library repos are always clean
                operation_status: RepoOperationStatus::Scanning,
                ..Default::default()
            }
        })
        .collect();
    if tx
        .send(LoadEvent::Discovered {
            workspace: discovered_workspace.clone(),
            library: discovered_library.clone(),
        })
        .is_err()
    {
        return;
    }

    let tasks: Mutex<Vec<ScanTask>> = Mutex::new(
        discovered_workspace
            .into_iter()
            .map(ScanTask::Workspace)
            .chain(discovered_library.into_iter().map(ScanTask::Library))
            .collect(),
    );

    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    let workers = cores.min(8);

    std::thread::scope(|scope| {
        for _ in 0..workers {
            let tx = tx.clone();
            let tasks = &tasks;
            scope.spawn(move || {
                // Each worker walks a repo of its own, so the cores are shared
                // between the walks instead of each walk claiming all of them
                crate::limit_status_walk_threads(cores / workers);
                loop {
                    let task = tasks.lock().unwrap().pop();
                    let Some(task) = task else {
                        break;
                    };
                    let event = match task {
                        ScanTask::Workspace(repo) => {
                            LoadEvent::Workspace(scan_workspace_repo(repo))
                        }
                        ScanTask::Library(repo) => LoadEvent::Library(scan_library_repo(repo)),
                    };
                    if tx.send(event).is_err() {
                        break;
                    }
                }
            });
        }
    });
}

/// Get the configured GitHub hostname from gh CLI
fn get_github_hostname() -> String {
    if let Ok(output) = std::process::Command::new("gh")
        .args(["auth", "status", "--active", "--json", "hosts"])
        .output()
        && output.status.success()
        && let Ok(json) = serde_json::from_slice::<serde_json::Value>(&output.stdout)
        && let Some(hosts) = json.get("hosts").and_then(|h| h.as_object())
        && let Some(hostname) = hosts.keys().next()
    {
        return hostname.clone();
    }
    // Default to github.com if we can't determine the hostname
    "github.com".to_string()
}

/// Fetch repository suggestions from GitHub CLI for TUI autocomplete
fn get_github_suggestions() -> Vec<String> {
    if let Ok(output) = std::process::Command::new("gh")
        .args([
            "repo",
            "list",
            "--limit",
            "100",
            "--json",
            "nameWithOwner",
            "-q",
            ".[].nameWithOwner",
        ])
        .output()
        && output.status.success()
    {
        let hostname = get_github_hostname();
        return String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(|line| format!("{}/{}", hostname, line.trim()))
            .collect();
    }
    Vec::new()
}

/// Get the configured GitLab hostname from glab CLI
fn get_gitlab_hostname() -> String {
    if let Ok(output) = std::process::Command::new("glab")
        .args(["config", "get", "host"])
        .output()
        && output.status.success()
    {
        let hostname = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !hostname.is_empty() {
            return hostname;
        }
    }
    // Default to gitlab.com if we can't determine the hostname
    "gitlab.com".to_string()
}

/// Fetch repository suggestions from GitLab CLI for TUI autocomplete
fn get_gitlab_suggestions() -> Vec<String> {
    if let Ok(output) = std::process::Command::new("glab")
        .args(["repo", "list", "--all", "--per-page", "100"])
        .output()
        && output.status.success()
    {
        let hostname = get_gitlab_hostname();
        return String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| {
                // glab output format is: "namespace/project"
                let parts: Vec<&str> = line.split_whitespace().collect();
                parts.first().map(|repo| format!("{}/{}", hostname, repo))
            })
            .collect();
    }
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEvent, KeyEventKind, KeyEventState};
    use details::RepoDetails;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent {
            code,
            modifiers,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    /// The rows discovery announces are the scan's input, so everything
    /// discovery derived — notably the tree path the repo is grouped under,
    /// which costs a repo open — has to survive the scan, and the
    /// placeholder "scanning" marker must not.
    #[test]
    fn scanning_a_workspace_repo_builds_on_its_discovered_row() {
        let temp = tempfile::TempDir::new().unwrap();
        let repo_path = temp.path().join("work/app");
        std::fs::create_dir_all(&repo_path).unwrap();
        gix::init(&repo_path).unwrap();
        std::fs::write(repo_path.join("notes.txt"), "unfinished").unwrap();
        std::fs::write(
            repo_path.join(".gitmodules"),
            "[submodule \"dep\"]\n\tpath = vendor/dep\n\turl = https://example.com/dep.git\n",
        )
        .unwrap();

        let scanned = scan_workspace_repo(RepoInfo {
            path: repo_path.clone(),
            display_name: "work/app".to_string(),
            tree_path: Some("github.com/fossable/app".to_string()),
            operation_status: RepoOperationStatus::Scanning,
            ..Default::default()
        });

        let repo = &scanned[0];
        assert_eq!(repo.display_name, "work/app");
        assert_eq!(repo.tree_path.as_deref(), Some("github.com/fossable/app"));
        assert_eq!(repo.status, Some(crate::RepoStatus::Dirty));
        assert_eq!(repo.operation_status, RepoOperationStatus::None);

        // Submodules ride along with their parent's scan
        let submodule = &scanned[1];
        assert_eq!(submodule.display_name, "work/app/vendor/dep");
        assert!(submodule.is_submodule);
        assert_eq!(submodule.parent_repo_path.as_deref(), Some(&*repo_path));
    }

    /// A submodule row's path is its parent's joined with whatever
    /// `.gitmodules` declares, and that file comes from the repo — so a cloned
    /// repo could hand the scan a path outside itself. Everything the TUI does
    /// to a row works from this path: the size walk, the git calls behind the
    /// info panel, the shell Enter opens. An absolute path is the worst of it,
    /// since joining replaces the parent's path outright.
    #[test]
    fn scanning_a_repo_yields_no_row_outside_it() {
        let temp = tempfile::TempDir::new().unwrap();
        let repo_path = temp.path().join("work/app");
        std::fs::create_dir_all(&repo_path).unwrap();
        gix::init(&repo_path).unwrap();
        std::fs::write(
            repo_path.join(".gitmodules"),
            "[submodule \"root\"]\n\tpath = /\n\turl = https://example.com/a.git\n\
             [submodule \"up\"]\n\tpath = ../../..\n\turl = https://example.com/b.git\n",
        )
        .unwrap();

        let scanned = scan_workspace_repo(RepoInfo {
            path: repo_path.clone(),
            display_name: "work/app".to_string(),
            ..Default::default()
        });

        // Only the repo itself, and nothing claiming to be a submodule of it
        assert_eq!(scanned.len(), 1, "expected no submodule rows");
        assert_eq!(scanned[0].path, repo_path);
    }

    /// Every repo discovery announces comes back scanned exactly once, from
    /// both the workspace and the library.
    #[test]
    fn scan_all_repos_scans_every_discovered_row_once() {
        let temp = tempfile::TempDir::new().unwrap();
        let workspace = Workspace {
            path: temp.path().to_string_lossy().to_string(),
        };

        let in_workspace = temp.path().join("github.com/fossable/alpha");
        std::fs::create_dir_all(&in_workspace).unwrap();
        gix::init(&in_workspace).unwrap();

        let in_library = PathBuf::from(workspace.library_path()).join("github.com/fossable/beta");
        std::fs::create_dir_all(&in_library).unwrap();
        gix::init_bare(&in_library).unwrap();

        // Blocks until every worker is done, then drops the sender
        let (tx, rx) = mpsc::channel();
        scan_all_repos(&workspace, tx);

        let mut discovered = Vec::new();
        let mut scanned_workspace = Vec::new();
        let mut scanned_library = Vec::new();
        for event in rx {
            match event {
                LoadEvent::Discovered { workspace, library } => {
                    discovered = workspace.into_iter().chain(library).collect()
                }
                LoadEvent::Workspace(infos) => scanned_workspace.extend(infos),
                LoadEvent::Library(info) => scanned_library.push(info),
            }
        }

        let mut names: Vec<&str> = discovered.iter().map(|r| &*r.display_name).collect();
        names.sort_unstable();
        assert_eq!(
            names,
            ["github.com/fossable/alpha", "github.com/fossable/beta"]
        );
        assert!(
            discovered
                .iter()
                .all(|r| r.operation_status == RepoOperationStatus::Scanning)
        );

        assert_eq!(scanned_workspace.len(), 1);
        assert_eq!(
            scanned_workspace[0].display_name,
            "github.com/fossable/alpha"
        );
        assert_eq!(
            scanned_workspace[0].operation_status,
            RepoOperationStatus::None
        );

        assert_eq!(scanned_library.len(), 1);
        assert_eq!(scanned_library[0].display_name, "github.com/fossable/beta");
        assert_eq!(
            scanned_library[0].operation_status,
            RepoOperationStatus::None
        );
        assert!(scanned_library[0].size_bytes.is_some());
    }

    /// A refresh seeds the loader with the rows the app already shows, so they
    /// keep their data while being rescanned. Each repo must end up listed
    /// exactly once — placeholder replaced by the scanned row — and a repo
    /// that left the workspace since the last scan must drop out of the list.
    #[test]
    fn loader_replaces_seeded_rows_and_drops_vanished_ones() {
        let temp = tempfile::TempDir::new().unwrap();
        let workspace = Workspace {
            path: temp.path().to_string_lossy().to_string(),
        };
        std::fs::create_dir_all(workspace.library_path()).unwrap();

        let still_here = temp.path().join("github.com/fossable/alpha");
        std::fs::create_dir_all(&still_here).unwrap();
        gix::init(&still_here).unwrap();

        let seed = |display_name: &str, path: PathBuf| RepoInfo {
            path,
            display_name: display_name.to_string(),
            status: Some(crate::RepoStatus::Clean),
            ..Default::default()
        };
        let mut app = App::new(workspace.path.clone(), Vec::new(), Vec::new());
        let mut loader = RepoLoader::start(
            &workspace,
            vec![
                seed("github.com/fossable/alpha", still_here),
                // Dropped from the workspace since the rows were built
                seed(
                    "github.com/fossable/gone",
                    temp.path().join("github.com/fossable/gone"),
                ),
            ],
            Vec::new(),
            false,
        );

        while loader.poll(&mut app) {
            std::thread::sleep(Duration::from_millis(5));
        }

        let rows: Vec<String> = app
            .get_flattened_workspace()
            .iter()
            .filter(|(node, _, _, _)| node.repo_info.is_some())
            .map(|(_, _, _, path)| path.clone())
            .collect();
        assert_eq!(rows, vec!["github.com/fossable/alpha".to_string()]);
    }

    #[test]
    fn ctrl_c_quits_from_every_mode() {
        assert_eq!(
            classify_key(&key(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            KeyBinding::Quit
        );
    }

    #[test]
    fn modified_characters_have_no_binding() {
        // Ctrl+D is what a terminal sends for EOF; taken for a bare 'd' it
        // used to drop the selected repos to the library
        for modifiers in [
            KeyModifiers::CONTROL,
            KeyModifiers::ALT,
            KeyModifiers::SUPER,
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        ] {
            for code in [KeyCode::Char('d'), KeyCode::Char('/'), KeyCode::Char('?')] {
                assert_eq!(
                    classify_key(&key(code, modifiers)),
                    KeyBinding::Ignored,
                    "{code:?} with {modifiers:?}"
                );
            }
        }

        // Only Ctrl+C quits; Alt+c is as unbound as the rest
        assert_eq!(
            classify_key(&key(KeyCode::Char('c'), KeyModifiers::ALT)),
            KeyBinding::Ignored
        );
    }

    #[test]
    fn bare_characters_reach_their_binding() {
        for (code, modifiers) in [
            (KeyCode::Char('d'), KeyModifiers::NONE),
            (KeyCode::Char('c'), KeyModifiers::NONE),
            // Shift is how a terminal reports a shifted character
            (KeyCode::Char('?'), KeyModifiers::SHIFT),
        ] {
            assert_eq!(classify_key(&key(code, modifiers)), KeyBinding::Key(code));
        }
    }

    #[test]
    fn non_character_keys_keep_their_modifiers_binding() {
        // Ctrl+Enter still reaches the mode that handles Enter
        assert_eq!(
            classify_key(&key(KeyCode::Enter, KeyModifiers::CONTROL)),
            KeyBinding::Key(KeyCode::Enter)
        );
        assert_eq!(
            classify_key(&key(KeyCode::Esc, KeyModifiers::NONE)),
            KeyBinding::Key(KeyCode::Esc)
        );
    }

    /// Every row's path ends with that row's own name, which is what lets the
    /// highlighter tell the node's characters from its ancestors'. A repo node
    /// used to report its on-disk display name here instead: once repos are
    /// grouped by remote the two differ, and a repo cloned into a directory
    /// whose name is shorter than its remote's last component made the
    /// highlighter subtract its way out of the string.
    #[test]
    fn a_row_path_ends_with_the_row_name() {
        let mut app = App::new(
            "/ws".to_string(),
            vec![RepoInfo {
                path: PathBuf::from("/ws/a"),
                display_name: "a".to_string(),
                tree_path: Some("github.com/fossable/longname".to_string()),
                ..Default::default()
            }],
            Vec::new(),
        );
        app.search_query = "a".to_string();
        app.filter_repos();

        let rows = app.get_flattened_workspace();
        assert_eq!(
            rows.iter()
                .map(|(_, _, _, path)| path.as_str())
                .collect::<Vec<_>>(),
            [
                "github.com",
                "github.com/fossable",
                "github.com/fossable/longname",
            ]
        );
        for (node, depth, _, full_path) in &rows {
            assert!(
                full_path.ends_with(&node.name),
                "{full_path} / {}",
                node.name
            );
            // Rendering the row with a search active is what used to panic
            let _ = tree_list_item(node, *depth, full_path, &app, 40, true, |_| String::new());
        }
    }

    #[test]
    fn search_highlights_the_matched_characters_of_the_name() {
        let render = |name: &str, full_path: &str, indices: Option<Vec<usize>>| {
            render_highlighted_name(name, full_path, false, indices)
                .iter()
                .map(|span| {
                    (
                        span.content.to_string(),
                        span.style.bg == Some(Color::Yellow),
                    )
                })
                .collect::<Vec<_>>()
        };

        // Consecutive matches collapse into one span, not one per character
        assert_eq!(
            render("workset", "github.com/workset", Some(vec![11, 12, 13, 14])),
            [("work".to_string(), true), ("set".to_string(), false)]
        );

        // Characters matched on an ancestor belong to that ancestor's row
        assert_eq!(
            render("workset", "github.com/workset", Some(vec![0, 1, 2])),
            [("workset".to_string(), false)]
        );

        // Indices are character positions, so a multibyte path keeps its
        // alignment; byte offsets used to shift the highlight off the end
        assert_eq!(
            render("wörld", "héllo/wörld", Some(vec![6, 7, 8, 9, 10])),
            [("wörld".to_string(), true)]
        );

        // With nothing matched, a directory the query names outright is still
        // highlighted whole
        assert_eq!(
            render_highlighted_name("fossable", "github.com/fossable", true, None)[0]
                .style
                .bg,
            Some(Color::Yellow)
        );
    }

    #[test]
    fn detail_lines_omit_changes_for_library() {
        let mut app = App::new("ws".to_string(), Vec::new(), Vec::new());
        let repo = RepoInfo {
            path: PathBuf::from("ws/repo"),
            display_name: "repo".to_string(),
            ..Default::default()
        };
        app.details.insert(
            repo.path.clone(),
            RepoDetails {
                size_bytes: Some(1024),
                line_changes: Some((531, 95)),
                remotes: Some(vec![RemoteInfo {
                    name: "origin".to_string(),
                    behind: 0,
                    commit: None,
                }]),
            },
        );

        let render = |section| {
            repo_detail_lines(&app, &repo, section, 0)
                .iter()
                .map(|line| line.to_string())
                .collect::<Vec<_>>()
                .join("\n")
        };

        let workspace = render(Section::Workspace);
        assert!(workspace.contains("Size:"));
        assert!(workspace.contains("Changes: +531 -95"));
        assert!(workspace.contains("origin"));

        let library = render(Section::Library);
        assert!(library.contains("Size:"));
        assert!(!library.contains("Changes:"));
        assert!(library.contains("origin"));
    }

    #[test]
    fn detail_lines_show_remote_status_and_behind_counts() {
        let mut app = App::new("ws".to_string(), Vec::new(), Vec::new());
        let repo = RepoInfo {
            path: PathBuf::from("ws/repo"),
            display_name: "repo".to_string(),
            ..Default::default()
        };
        app.details.insert(
            repo.path.clone(),
            RepoDetails {
                size_bytes: Some(1024),
                line_changes: Some((0, 0)),
                remotes: Some(vec![
                    RemoteInfo {
                        name: "origin".to_string(),
                        behind: 0,
                        commit: Some("a1b2c3d".to_string()),
                    },
                    RemoteInfo {
                        name: "backup".to_string(),
                        behind: 3,
                        commit: None,
                    },
                ]),
            },
        );

        let text = repo_detail_lines(&app, &repo, Section::Workspace, 0)
            .iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("Changes: clean"));
        assert!(text.contains("Remotes:"));
        // The checked-out branch's id follows the remote name when the
        // remote tracks the branch, and is omitted when it doesn't
        assert!(text.contains("origin a1b2c3d not synced yet"));
        assert!(text.contains("backup not synced yet"));
        assert!(text.contains("↓ 3 behind"));
        assert!(!text.contains("↓ 0"));
        assert!(!text.contains("branches:"));
    }

    #[test]
    fn sync_manager_dedups_and_requeues() {
        let mut mgr = SyncManager::new("ws".to_string());
        let repo = PathBuf::from("ws/repo");

        mgr.request_sync(repo.clone(), false);
        mgr.request_sync(repo.clone(), false);
        assert_eq!(mgr.queue.len(), 1);

        // Simulate the job being picked up
        mgr.queue.pop_front();
        mgr.in_flight.insert(repo.clone());

        // Requests during a running sync are deferred, not duplicated
        mgr.request_sync(repo.clone(), false);
        assert!(mgr.queue.is_empty());
        assert!(mgr.rerun_after.contains(&repo));

        // Finishing re-queues the deferred request and stamps the cooldown
        mgr.finish(&repo);
        assert!(!mgr.in_flight.contains(&repo));
        assert_eq!(mgr.queue.len(), 1);

        // Watcher-triggered requests inside the cooldown are dropped
        mgr.queue.clear();
        mgr.request_sync(repo.clone(), true);
        assert!(mgr.queue.is_empty());

        // Explicit (non-watcher) requests ignore the cooldown
        mgr.request_sync(repo.clone(), false);
        assert_eq!(mgr.queue.len(), 1);
    }

    #[test]
    fn sync_manager_selection_fetches_once_with_cooldown() {
        let backdate = |by: Duration| Instant::now().checked_sub(by).expect("clock too young");
        let mut mgr = SyncManager::new("ws".to_string());
        let repo = PathBuf::from("ws/repo");
        let other = PathBuf::from("ws/other");

        // A fresh selection arms the debounce but queues nothing yet
        mgr.note_selection(Some(repo.clone()));
        assert!(mgr.selection_pending.is_some());
        mgr.pump_selection(false);
        assert!(mgr.queue.is_empty());

        // Past the debounce, but held while a scan is loading
        mgr.selection_pending = Some((repo.clone(), backdate(SELECTION_SYNC_DEBOUNCE)));
        mgr.pump_selection(true);
        assert!(mgr.queue.is_empty());
        mgr.pump_selection(false);
        assert_eq!(mgr.queue.len(), 1);
        assert_eq!(mgr.last_selection.as_ref(), Some(&repo));

        // A parked cursor is handled once, not re-armed every iteration
        mgr.note_selection(Some(repo.clone()));
        assert!(mgr.selection_pending.is_none());

        // Simulate the queued job running to completion
        mgr.queue.pop_front();
        mgr.in_flight.insert(repo.clone());
        mgr.finish(&repo);

        // Returning to the row inside the cooldown doesn't refetch
        mgr.note_selection(Some(other.clone()));
        mgr.note_selection(Some(repo.clone()));
        mgr.selection_pending = Some((repo.clone(), backdate(SELECTION_SYNC_DEBOUNCE)));
        mgr.pump_selection(false);
        assert!(mgr.queue.is_empty());

        // Past the cooldown the same return fetches again
        mgr.recently_synced
            .insert(repo.clone(), backdate(SELECTION_SYNC_COOLDOWN));
        mgr.note_selection(Some(other.clone()));
        mgr.note_selection(Some(repo.clone()));
        mgr.selection_pending = Some((repo.clone(), backdate(SELECTION_SYNC_DEBOUNCE)));
        mgr.pump_selection(false);
        assert_eq!(mgr.queue.len(), 1);
    }

    #[test]
    fn sync_manager_drops_pending_fetch_of_a_row_passed_over() {
        let mut mgr = SyncManager::new("ws".to_string());
        let repo = PathBuf::from("ws/repo");
        let other = PathBuf::from("ws/other");
        mgr.last_selection = Some(repo.clone());

        // The cursor passes over another row and returns before its debounce
        // elapses; the passed-over row must not be fetched
        mgr.note_selection(Some(other.clone()));
        assert!(mgr.selection_pending.is_some());
        mgr.note_selection(Some(repo.clone()));
        assert!(mgr.selection_pending.is_none());
        mgr.pump_selection(false);
        assert!(mgr.queue.is_empty());
    }
}
