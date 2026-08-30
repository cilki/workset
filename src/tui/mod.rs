mod app;
mod details;
mod metadata;
mod tree;
mod watcher;

use app::{App, AppMode, Section};
use details::{DetailsLoader, MirrorConfig, MirrorState, mirror_rows};
use crate::get_repo_modification_time;
use metadata::{format_size, format_time_ago_verbose, get_repo_size};
use tree::{RepoInfo, RepoOperationStatus, TreeNode};
use watcher::FileWatcher;

use crate::{RepoPattern, Workspace, find_git_repositories};
use anyhow::{Result, anyhow};
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
    widgets::{Block, Borders, List, ListItem, Paragraph},
};
use std::collections::{HashMap, HashSet, VecDeque};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

const SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// How many repos may sync (fetch/push) concurrently
const MAX_CONCURRENT_SYNCS: usize = 4;
/// How often all repos are re-checked against their remotes
const SYNC_INTERVAL: Duration = Duration::from_secs(300);
/// Ignore watcher-triggered sync requests this soon after a sync finished,
/// since the sync's own fetch writes the tracking refs the watcher observes
const WATCHER_SYNC_COOLDOWN: Duration = Duration::from_secs(2);

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
/// Rows the scan hasn't reached yet are shown with a "scanning" status:
/// seeded from the app's previous data on a refresh (keeping their last known
/// status), or as bare placeholders on the initial load.
struct RepoLoader {
    rx: mpsc::Receiver<LoadEvent>,
    scanned_workspace: Vec<RepoInfo>,
    scanned_library: Vec<RepoInfo>,
    pending_workspace: Vec<RepoInfo>,
    pending_library: Vec<RepoInfo>,
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
            scanned_workspace: Vec::new(),
            scanned_library: Vec::new(),
            pending_workspace: mark_scanning(seed_workspace),
            pending_library: mark_scanning(seed_library),
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
                    merge_discovered(&mut self.pending_workspace, workspace);
                    merge_discovered(&mut self.pending_library, library);
                    received = true;
                }
                Ok(LoadEvent::Workspace(infos)) => {
                    self.pending_workspace
                        .retain(|p| !infos.iter().any(|i| i.display_name == p.display_name));
                    self.scanned_workspace.extend(infos);
                    self.done += 1;
                    received = true;
                }
                Ok(LoadEvent::Library(info)) => {
                    self.pending_library
                        .retain(|p| p.display_name != info.display_name);
                    self.scanned_library.push(info);
                    self.done += 1;
                    received = true;
                }
                Err(mpsc::TryRecvError::Empty) => break false,
                Err(mpsc::TryRecvError::Disconnected) => break true,
            }
        };

        if finished {
            app.update_repos(
                std::mem::take(&mut self.scanned_workspace),
                std::mem::take(&mut self.scanned_library),
            );
            if self.progressive {
                app.loading_progress = None;
            }
        } else if received {
            let workspace = self
                .scanned_workspace
                .iter()
                .chain(&self.pending_workspace)
                .cloned()
                .collect();
            let library = self
                .scanned_library
                .iter()
                .chain(&self.pending_library)
                .cloned()
                .collect();
            app.update_repos(workspace, library);
            if self.progressive {
                let spinner = SPINNER_FRAMES[self.done % SPINNER_FRAMES.len()];
                app.loading_progress = Some(format!("{} {}/{}", spinner, self.done, self.total));
            }
        }

        !finished
    }
}

/// Reconcile seeded pending rows with the freshly enumerated repo set: rows
/// that no longer exist are dropped, newly appeared repos get a placeholder.
/// Submodule rows are kept as-is; they ride along with their parent's scan.
fn merge_discovered(pending: &mut Vec<RepoInfo>, discovered: Vec<RepoInfo>) {
    pending
        .retain(|p| p.is_submodule || discovered.iter().any(|d| d.display_name == p.display_name));
    for repo in discovered {
        if !pending.iter().any(|p| p.display_name == repo.display_name) {
            pending.push(repo);
        }
    }
}

/// Result of a background sync job for one repo
enum SyncEvent {
    Started,
    /// The repo has mirroring disabled; nothing was done
    Skipped,
    Finished(crate::sync::SyncOutcome),
    Failed(String),
}

/// Schedules background jobs that mirror each repo's commits across its
/// remotes. Jobs run on their own threads (up to `MAX_CONCURRENT_SYNCS`) and
/// report back through a channel drained by the event loop.
struct SyncManager {
    tx: mpsc::Sender<(PathBuf, SyncEvent)>,
    rx: mpsc::Receiver<(PathBuf, SyncEvent)>,
    queue: VecDeque<PathBuf>,
    in_flight: HashSet<PathBuf>,
    /// Repos that were requested again while already syncing; re-queued once
    /// the running job finishes
    rerun_after: HashSet<PathBuf>,
    recently_synced: HashMap<PathBuf, Instant>,
    last_periodic: Instant,
    /// Sync every repo once the initial scan completes. Because the outer TUI
    /// loop rebuilds everything after an interactive shell exits, this covers
    /// both startup and returning from a shell.
    startup_pending: bool,
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
            last_periodic: Instant::now(),
            startup_pending: true,
            interrupt: Arc::new(AtomicBool::new(false)),
            workspace_path,
        }
    }

    /// Queue a repo for syncing, deduplicating against queued and running jobs
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

    /// Queue every syncable workspace repo
    fn request_sync_all(&mut self, app: &App) {
        for repo in app.syncable_repo_paths() {
            self.request_sync(repo, false);
        }
    }

    /// Re-check all repos on a timer; skipped while a scan is loading since
    /// the repo list may be incomplete
    fn maybe_periodic(&mut self, app: &App, loader_active: bool) {
        if loader_active || self.last_periodic.elapsed() < SYNC_INTERVAL {
            return;
        }
        self.last_periodic = Instant::now();
        self.request_sync_all(app);
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
                // Check the mirror config before reporting Started so disabled
                // repos never flash a "syncing" status
                match crate::sync::mirror_patterns(&repo, &interrupt) {
                    Ok(patterns) if !patterns.enabled() => {
                        let _ = tx.send((repo, SyncEvent::Skipped));
                        return;
                    }
                    Err(e) => {
                        let _ = tx.send((repo, SyncEvent::Failed(e.to_string())));
                        return;
                    }
                    Ok(_) => {}
                }
                let _ = tx.send((repo.clone(), SyncEvent::Started));
                let event = match crate::sync::sync_repo(&repo, &interrupt, false) {
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
                    app.syncing_repos.insert(repo);
                    app.set_sync_status(&display_name, RepoOperationStatus::Syncing);
                }
                SyncEvent::Skipped => {
                    self.finish(&repo);
                    // Clears a stale SyncFailed overlay if the user just
                    // disabled mirroring for the repo
                    app.clear_sync_status(&display_name);
                    app.syncing_repos.remove(&repo);
                    app.sync_outcomes.remove(&repo);
                    app.details.remove(&repo);
                }
                SyncEvent::Finished(outcome) => {
                    self.finish(&repo);
                    if let Some(status) = outcome.status {
                        app.apply_scan_result(&display_name, status, outcome.modification_time);
                    }
                    match outcome.error_summary() {
                        Some(err) => {
                            app.set_sync_status(&display_name, RepoOperationStatus::SyncFailed(err))
                        }
                        None => app.clear_sync_status(&display_name),
                    }
                    app.syncing_repos.remove(&repo);
                    // The fetch may have changed what the info panel shows
                    app.details.remove(&repo);
                    // Keep the full outcome for the panel's per-mirror rows
                    app.sync_outcomes.insert(repo, outcome);
                }
                SyncEvent::Failed(err) => {
                    self.finish(&repo);
                    app.set_sync_status(&display_name, RepoOperationStatus::SyncFailed(err));
                    app.syncing_repos.remove(&repo);
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
    /// Mirrors commits across each repo's remotes
    sync: SyncManager,
    /// Computes info-panel details for the selected repo
    details: DetailsLoader,
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
                            let Ok(pattern) = repo_path.parse::<RepoPattern>();
                            workspace.drop(&pattern, false, false)
                        },
                    )?;
                    let (seed_workspace, seed_library) = app.repo_snapshot();
                    background.loader = Some(RepoLoader::start(
                        workspace,
                        seed_workspace,
                        seed_library,
                        false,
                    ));
                }
                Action::RestoreFromLibrary(repo_paths) => {
                    run_repo_operation(
                        &mut terminal,
                        &mut app,
                        &repo_paths,
                        RepoOperationStatus::Restoring,
                        |repo_path| workspace.restore_from_library(repo_path),
                    )?;
                    let (seed_workspace, seed_library) = app.repo_snapshot();
                    background.loader = Some(RepoLoader::start(
                        workspace,
                        seed_workspace,
                        seed_library,
                        false,
                    ));
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
                    let (seed_workspace, seed_library) = app.repo_snapshot();
                    background.loader = Some(RepoLoader::start(
                        workspace,
                        seed_workspace,
                        seed_library,
                        false,
                    ));
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
            // Check every repo against its remotes once the first scan lands
            if background.sync.startup_pending {
                background.sync.startup_pending = false;
                background.sync.request_sync_all(app);
            }
        }
        let loader_active = background.loader.is_some();
        background.sync.poll(app);
        background.sync.maybe_periodic(app, loader_active);
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
            match app.mode {
                AppMode::Normal => match key.code {
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        return Ok(Action::None);
                    }
                    KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        // Ctrl+D = drop workspace repo(s) to library
                        if app.active_section == Section::Workspace
                            && let Some(node) = app.selected_node()
                        {
                            let repo_paths = node.collect_repo_paths();
                            if !repo_paths.is_empty() {
                                return Ok(Action::DropToLibrary(repo_paths));
                            }
                        }
                    }
                    KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        // Ctrl+R = toggle mirroring for the selected repo
                        if app.active_section == Section::Workspace
                            && let Some(repo_path) = app
                                .selected_node()
                                .and_then(|node| node.repo_info.as_ref())
                                .filter(|repo| !repo.is_submodule)
                                .map(|repo| repo.path.clone())
                        {
                            let interrupt = AtomicBool::new(false);
                            if let Ok(patterns) =
                                crate::sync::mirror_patterns(&repo_path, &interrupt)
                                && crate::sync::set_mirror_patterns(
                                    &repo_path,
                                    &if patterns.enabled() {
                                        crate::sync::MirrorPatterns::default()
                                    } else {
                                        crate::sync::MirrorPatterns {
                                            branches: vec![crate::sync::default_branch(
                                                &repo_path, &interrupt,
                                            )],
                                            tags: vec!["*".to_string()],
                                        }
                                    },
                                )
                                .is_ok()
                            {
                                // Re-read the info panel's mirror status
                                app.details.remove(&repo_path);
                                // Apply the new config right away; a Skipped
                                // event also clears any stale sync-failed
                                // overlay
                                background.sync.request_sync(repo_path, false);
                            }
                        }
                    }
                    KeyCode::Right | KeyCode::Left => {
                        app.toggle_expand();
                    }
                    KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        // Ctrl+A = clone repo dialog; suggestions arrive from a
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
                AppMode::CloneRepo => match key.code {
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
    if app.mode == AppMode::CloneRepo {
        render_clone_repo_dialog(f, app);
        return;
    }

    // Split vertically into rows; the search box only appears while a query
    // is being typed
    let show_search = !app.search_query.is_empty();
    let mut constraints = vec![
        Constraint::Length(1), // Help (at top)
        Constraint::Min(0),    // Main area (workspace + library side by side)
    ];
    if show_search {
        constraints.push(Constraint::Length(3)); // Search box
    }
    let vertical_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(f.area());

    // Split the main area horizontally into workspace (left) and library (right)
    let horizontal_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(50), // Workspace (left)
            Constraint::Percentage(50), // Library (right)
        ])
        .split(vertical_chunks[1]);

    render_help_line(f, app, vertical_chunks[0]);
    render_tree_panel(f, app, horizontal_chunks[0], Section::Workspace);

    render_tree_panel(f, app, horizontal_chunks[1], Section::Library);

    if show_search {
        let search_text = format!("{}_", app.search_query);
        let search_style = Style::default().fg(Color::Yellow);
        let search = Paragraph::new(search_text)
            .style(search_style)
            .alignment(Alignment::Left)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(search_style)
                    .title("Search"),
            );
        f.render_widget(search, vertical_chunks[2]);
    }
}

/// Render the key-binding help line at the top of the screen
fn render_help_line(f: &mut Frame, app: &App, area: Rect) {
    let enter_action = match app.active_section {
        Section::Workspace => " open  ",
        Section::Library => " restore  ",
    };

    let mut bindings: Vec<(&str, Color, &str)> = vec![
        ("Tab", Color::Cyan, " switch  "),
        ("↑/↓", Color::Cyan, " navigate  "),
        ("←/→", Color::Cyan, " expand/collapse  "),
        ("Enter", Color::Green, enter_action),
    ];
    if app.active_section == Section::Workspace {
        bindings.push(("Ctrl+D", Color::Yellow, " drop  "));
        bindings.push(("Ctrl+R", Color::Cyan, " mirror on/off  "));
    }
    bindings.push(("Ctrl+A", Color::Magenta, " clone  "));
    bindings.push(("Esc", Color::Red, " quit"));

    let mut help_spans = Vec::new();
    for (key, color, description) in bindings {
        help_spans.push(Span::styled(
            key,
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ));
        help_spans.push(Span::raw(description));
    }

    let help = Paragraph::new(Line::from(help_spans)).alignment(Alignment::Center);
    f.render_widget(help, area);
}

/// Detail rows rendered under the selected repo: size, worktree changes
/// (workspace only — library repos are always clean), and one status row per
/// mirror remote. Fields still being computed by the details loader show an
/// ellipsis.
fn repo_detail_lines(app: &App, repo: &RepoInfo, section: Section, depth: usize) -> Vec<Line<'static>> {
    let dim = Style::default().fg(Color::DarkGray);
    let details = app.details.get(&repo.path).cloned().unwrap_or_default();
    let indent = "  ".repeat(depth + 2);

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
        let changes = match details.change_count {
            Some(0) => Span::styled("clean", Style::default().fg(Color::Green)),
            Some(count) => Span::styled(
                format!("{} outstanding", count),
                Style::default().fg(Color::Yellow),
            ),
            None => Span::styled("…", dim),
        };
        lines.push(Line::from(vec![
            Span::raw(indent.clone()),
            Span::styled("Changes: ", dim),
            changes,
        ]));
    }

    match &details.mirrors {
        None => lines.push(Line::from(vec![
            Span::raw(indent.clone()),
            Span::styled("Mirrors: ", dim),
            Span::styled("…", dim),
        ])),
        Some(MirrorConfig::Disabled) => {
            // Mirroring can only be toggled on workspace repos
            let hint = if section == Section::Workspace && !repo.is_submodule {
                "off (Ctrl+R to enable)"
            } else {
                "off"
            };
            lines.push(Line::from(vec![
                Span::raw(indent.clone()),
                Span::styled("Mirrors: ", dim),
                Span::styled(hint, dim),
            ]));
        }
        Some(MirrorConfig::Enabled { remotes, .. }) if remotes.is_empty() => {
            lines.push(Line::from(vec![
                Span::raw(indent.clone()),
                Span::styled("Mirrors: ", dim),
                Span::styled("no remotes", dim),
            ]));
        }
        Some(MirrorConfig::Enabled { remotes, patterns }) => {
            lines.push(Line::from(vec![
                Span::raw(indent.clone()),
                Span::styled("Mirrors:", dim),
            ]));
            lines.push(Line::from(vec![
                Span::raw(indent.clone()),
                Span::styled(
                    format!(
                        "  branches: {}  tags: {}",
                        crate::sync::display_patterns(&patterns.branches),
                        crate::sync::display_patterns(&patterns.tags),
                    ),
                    dim,
                ),
            ]));
            let outcome = app.sync_outcomes.get(&repo.path);
            let syncing = app.syncing_repos.contains(&repo.path);
            for (name, state) in mirror_rows(remotes, outcome, syncing) {
                let mut line = mirror_status_line(name, state);
                line.spans.insert(0, Span::raw(indent.clone()));
                lines.push(line);
            }
        }
    }
    lines
}

/// One detail row showing a mirror remote and its sync state
fn mirror_status_line(name: String, state: MirrorState) -> Line<'static> {
    let status = match state {
        MirrorState::Syncing => Span::styled("syncing…", Style::default().fg(Color::Cyan)),
        MirrorState::InSync => Span::styled("✓ in sync", Style::default().fg(Color::Green)),
        MirrorState::Pushed(count) => Span::styled(
            format!(
                "↑ pushed {} ref{}",
                count,
                if count == 1 { "" } else { "s" }
            ),
            Style::default().fg(Color::Green),
        ),
        MirrorState::Conflict(msg) | MirrorState::PushError(msg) | MirrorState::FetchError(msg) => {
            Span::styled(format!("⚠ {}", msg), Style::default().fg(Color::Red))
        }
        MirrorState::Pending => {
            Span::styled("not synced yet", Style::default().fg(Color::DarkGray))
        }
    };
    Line::from(vec![
        Span::raw("  "),
        Span::raw(name),
        Span::raw("  "),
        status,
    ])
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
            RepoOperationStatus::Syncing => ("syncing".to_string(), Color::Cyan),
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

/// Render highlighted name with character-by-character fuzzy match highlighting
fn render_highlighted_name<'a>(
    node_name: &'a str,
    full_path: &str,
    should_highlight_dir: bool,
    indices: Option<Vec<usize>>,
) -> Vec<Span<'a>> {
    let mut spans = vec![];

    if let Some(indices) = indices {
        let mut last_pos = 0;
        let chars: Vec<(usize, char)> = node_name.char_indices().collect();

        // Find which indices apply to just the node name (not full path)
        let path_offset = full_path.len() - node_name.len();

        for &match_idx in &indices {
            if match_idx < path_offset {
                continue; // Skip matches in path prefix
            }
            let local_idx = match_idx - path_offset;

            if local_idx >= chars.len() {
                continue;
            }

            // Add unmatched text before this character
            if local_idx > last_pos {
                let start_byte = chars[last_pos].0;
                let end_byte = chars[local_idx].0;
                spans.push(Span::raw(&node_name[start_byte..end_byte]));
            }

            // Add highlighted character
            let char_byte_start = chars[local_idx].0;
            let char_byte_end = if local_idx + 1 < chars.len() {
                chars[local_idx + 1].0
            } else {
                node_name.len()
            };
            spans.push(Span::styled(
                &node_name[char_byte_start..char_byte_end],
                Style::default().fg(Color::Black).bg(Color::Yellow),
            ));

            last_pos = local_idx + 1;
        }

        // Add remaining text
        if last_pos < chars.len() {
            let start_byte = chars[last_pos].0;
            spans.push(Span::raw(&node_name[start_byte..]));
        } else if last_pos == 0 {
            // No matches in name portion, show normally
            spans.push(Span::raw(node_name));
        }
    } else if should_highlight_dir {
        // Directory is part of search path, highlight entire name
        spans.push(Span::styled(
            node_name,
            Style::default().fg(Color::Black).bg(Color::Yellow),
        ));
    } else {
        spans.push(Span::raw(node_name));
    }

    spans
}

/// Scan a single workspace repository, returning it and any submodules
/// Workspace-relative display name for a repo path
fn workspace_display_name(workspace_path: &str, path: &Path) -> String {
    path.strip_prefix(workspace_path)
        .unwrap_or(path)
        .display()
        .to_string()
        .trim_start_matches('/')
        .to_string()
}

fn scan_workspace_repo(workspace_path: &str, path: PathBuf) -> Vec<RepoInfo> {
    let display_name = workspace_display_name(workspace_path, &path);

    // Check repo status and get modification time in a single repo open for performance
    let (status, modification_time) = crate::check_repo_status_and_modification_time(&path)
        .unwrap_or((crate::RepoStatus::NoCommits, None));

    let mut infos = vec![RepoInfo {
        path: path.clone(),
        display_name: display_name.clone(),
        status: Some(status),
        modification_time,
        // Size not computed for workspace repos to save time
        ..Default::default()
    }];

    // Find and add submodules
    if let Ok(submodules) = crate::find_submodules_in_repo(&path) {
        for submodule in submodules {
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
    }

    infos
}

/// Scan a single library repository for its metadata
fn scan_library_repo(library_path: &str, repo_path: String) -> RepoInfo {
    let full_path = PathBuf::from(library_path).join(&repo_path);
    RepoInfo {
        modification_time: get_repo_modification_time(&full_path).ok(),
        size_bytes: get_repo_size(&full_path).ok(),
        path: full_path,
        display_name: repo_path,
        status: Some(crate::RepoStatus::Clean), // Library repos are always clean
        ..Default::default()
    }
}

/// Enumerate and scan all workspace and library repositories on worker
/// threads, streaming results to the UI thread. Runs on a background thread;
/// exits early if the receiver is dropped.
fn scan_all_repos(workspace: &Workspace, tx: mpsc::Sender<LoadEvent>) {
    enum ScanTask {
        Workspace(PathBuf),
        Library(String),
    }

    let workspace_paths = find_git_repositories(Path::new(&workspace.path)).unwrap_or_default();
    let library_paths = workspace.list_library().unwrap_or_default();
    let library_path = workspace.library_path();

    // Announce the full repo set before any git work so every row can render
    // with a "scanning" status right away
    let discovered_workspace = workspace_paths
        .iter()
        .map(|path| RepoInfo {
            path: path.clone(),
            display_name: workspace_display_name(&workspace.path, path),
            operation_status: RepoOperationStatus::Scanning,
            ..Default::default()
        })
        .collect();
    let discovered_library = library_paths
        .iter()
        .map(|repo_path| RepoInfo {
            path: PathBuf::from(&library_path).join(repo_path),
            display_name: repo_path.clone(),
            status: Some(crate::RepoStatus::Clean),
            operation_status: RepoOperationStatus::Scanning,
            ..Default::default()
        })
        .collect();
    if tx
        .send(LoadEvent::Discovered {
            workspace: discovered_workspace,
            library: discovered_library,
        })
        .is_err()
    {
        return;
    }

    let tasks: Mutex<Vec<ScanTask>> = Mutex::new(
        workspace_paths
            .into_iter()
            .map(ScanTask::Workspace)
            .chain(library_paths.into_iter().map(ScanTask::Library))
            .collect(),
    );

    let workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .min(8);

    std::thread::scope(|scope| {
        for _ in 0..workers {
            let tx = tx.clone();
            let tasks = &tasks;
            let workspace_path = workspace.path.as_str();
            let library_path = library_path.as_str();
            scope.spawn(move || {
                loop {
                    let task = tasks.lock().unwrap().pop();
                    let Some(task) = task else {
                        break;
                    };
                    let event = match task {
                        ScanTask::Workspace(path) => {
                            LoadEvent::Workspace(scan_workspace_repo(workspace_path, path))
                        }
                        ScanTask::Library(repo_path) => {
                            LoadEvent::Library(scan_library_repo(library_path, repo_path))
                        }
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
    use details::RepoDetails;

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
                change_count: Some(3),
                mirrors: Some(MirrorConfig::Disabled),
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
        assert!(workspace.contains("Changes: 3 outstanding"));
        assert!(workspace.contains("Mirrors:"));

        let library = render(Section::Library);
        assert!(library.contains("Size:"));
        assert!(!library.contains("Changes:"));
        assert!(library.contains("Mirrors:"));
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
}
