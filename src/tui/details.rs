use super::app::{App, Section};
use crate::RepoStatus;
use crate::sync::SyncOutcome;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

/// How long the selection must rest on a repo before its details are
/// computed, so scrolling through the list doesn't spawn a job per row
const DETAILS_DEBOUNCE: Duration = Duration::from_millis(250);

/// Facts about one repo shown in the info panel, filled in asynchronously.
/// `None` fields are still being computed.
#[derive(Clone, Default)]
pub struct RepoDetails {
    pub size_bytes: Option<u64>,
    /// Lines (added, removed) across uncommitted changes and untracked files;
    /// (0, 0) = clean
    pub line_changes: Option<(usize, usize)>,
    /// The repo's remotes and their sync state
    pub remotes: Option<RemotesDetail>,
}

/// The repo's remotes as shown in the detail rows
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemotesDetail {
    pub remotes: Vec<RemoteInfo>,
}

/// One remote as shown in the repo detail
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteInfo {
    pub name: String,
    /// Commits this remote is missing from the newest published id across the
    /// repo's branches, per the local tracking refs; 0 = up to date
    pub behind: usize,
}

impl RepoDetails {
    fn is_complete(&self) -> bool {
        self.size_bytes.is_some() && self.line_changes.is_some() && self.remotes.is_some()
    }
}

/// Display state of one remote in the info panel
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RemoteSyncState {
    Fetching,
    InSync,
    FetchError(String),
    /// No sync outcome recorded yet
    Pending,
}

/// Merge the remotes with the last sync outcome and whether a fetch is
/// currently running into one display row per remote
pub fn remote_rows(
    remotes: &[String],
    outcome: Option<&SyncOutcome>,
    fetching: bool,
) -> Vec<(String, RemoteSyncState)> {
    remotes
        .iter()
        .map(|name| {
            let state = if fetching {
                RemoteSyncState::Fetching
            } else {
                match outcome {
                    None => RemoteSyncState::Pending,
                    Some(o) => remote_state_from_outcome(name, o),
                }
            };
            (name.clone(), state)
        })
        .collect()
}

fn remote_state_from_outcome(remote: &str, outcome: &SyncOutcome) -> RemoteSyncState {
    if let Some((_, error)) = outcome.fetch_errors.iter().find(|(r, _)| r == remote) {
        return RemoteSyncState::FetchError(error.clone());
    }
    if outcome.offline {
        return RemoteSyncState::FetchError("unreachable".to_string());
    }
    RemoteSyncState::InSync
}

/// Results streamed from a background details job. Events are keyed by repo
/// path, so results from a job for a previously selected repo land harmlessly
/// in the cache.
enum DetailsEvent {
    Size(PathBuf, u64),
    Changes(PathBuf, (usize, usize)),
    Remotes(PathBuf, RemotesDetail),
    /// The job for the given repo finished
    Done(PathBuf),
}

/// Computes info-panel details for the selected repo on a background thread,
/// debounced so scrolling stays free of git subprocesses. Drained into the
/// `App` from the event loop via `poll`.
pub struct DetailsLoader {
    tx: mpsc::Sender<DetailsEvent>,
    rx: mpsc::Receiver<DetailsEvent>,
    /// Selection waiting out the debounce window
    pending: Option<(PathBuf, Instant)>,
    in_flight: Option<PathBuf>,
    pub interrupt: Arc<AtomicBool>,
}

impl DetailsLoader {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            tx,
            rx,
            pending: None,
            in_flight: None,
            interrupt: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Track the current selection, seeding cheap fields from data the scan
    /// already produced. Called every event-loop iteration.
    pub fn note_selection(&mut self, app: &mut App) {
        let selected = app
            .selected_node()
            .and_then(|node| node.repo_info.as_ref())
            .map(|repo| (repo.path.clone(), repo.size_bytes, repo.status));
        let is_library = app.active_section == Section::Library;

        let Some((path, size_bytes, status)) = selected else {
            self.pending = None;
            return;
        };

        let entry = app.details.entry(path.clone()).or_default();
        if entry.size_bytes.is_none() && size_bytes.is_some() {
            entry.size_bytes = size_bytes;
        }
        // Library repos have no worktree, and a repo the scan saw as Clean or
        // Unpushed has no changes to count
        if entry.line_changes.is_none()
            && (is_library || matches!(status, Some(RepoStatus::Clean | RepoStatus::Unpushed)))
        {
            entry.line_changes = Some((0, 0));
        }

        if entry.is_complete() {
            self.pending = None;
        } else if !self.pending.as_ref().is_some_and(|(p, _)| *p == path) {
            self.pending = Some((path, Instant::now()));
        }
    }

    /// Spawn a details job once the selection has rested past the debounce
    pub fn pump(&mut self, app: &App) {
        if self.in_flight.is_some() {
            return;
        }
        let Some((path, since)) = &self.pending else {
            return;
        };
        if since.elapsed() < DETAILS_DEBOUNCE {
            return;
        }
        let path = path.clone();
        self.pending = None;

        let current = app.details.get(&path).cloned().unwrap_or_default();
        if current.is_complete() {
            return;
        }
        self.in_flight = Some(path.clone());
        let tx = self.tx.clone();
        let interrupt = self.interrupt.clone();
        // Fastest first so the panel fills in progressively; failures send
        // fallback values so the entry completes instead of respawning forever
        std::thread::spawn(move || {
            if current.remotes.is_none() {
                let _ = tx.send(DetailsEvent::Remotes(
                    path.clone(),
                    load_remotes(&path, &interrupt),
                ));
            }
            if current.line_changes.is_none() {
                let counts = crate::sync::count_diff_lines(&path, &interrupt).unwrap_or((0, 0));
                let _ = tx.send(DetailsEvent::Changes(path.clone(), counts));
            }
            if current.size_bytes.is_none() {
                let size = super::metadata::get_repo_size(&path).unwrap_or(0);
                let _ = tx.send(DetailsEvent::Size(path.clone(), size));
            }
            let _ = tx.send(DetailsEvent::Done(path));
        });
    }

    /// Drain finished results into the app without blocking
    pub fn poll(&mut self, app: &mut App) {
        while let Ok(event) = self.rx.try_recv() {
            match event {
                DetailsEvent::Size(path, size) => {
                    app.details.entry(path).or_default().size_bytes = Some(size);
                }
                DetailsEvent::Changes(path, counts) => {
                    app.details.entry(path).or_default().line_changes = Some(counts);
                }
                DetailsEvent::Remotes(path, remotes) => {
                    app.details.entry(path).or_default().remotes = Some(remotes);
                }
                DetailsEvent::Done(path) => {
                    if self.in_flight.as_ref() == Some(&path) {
                        self.in_flight = None;
                    }
                }
            }
        }
    }
}

/// List the repo's remotes, annotating each with how far behind it is
fn load_remotes(path: &Path, interrupt: &AtomicBool) -> RemotesDetail {
    let names = crate::sync::list_remotes(path, interrupt).unwrap_or_default();
    let behind = crate::sync::behind_counts(path, &names, interrupt).unwrap_or_default();
    RemotesDetail {
        remotes: names
            .into_iter()
            .map(|name| RemoteInfo {
                behind: behind.get(&name).copied().unwrap_or(0),
                name,
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn remotes(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| n.to_string()).collect()
    }

    #[test]
    fn no_outcome_reports_pending() {
        let rows = remote_rows(&remotes(&["a", "b"]), None, false);
        assert_eq!(
            rows,
            vec![
                ("a".to_string(), RemoteSyncState::Pending),
                ("b".to_string(), RemoteSyncState::Pending),
            ]
        );
    }

    #[test]
    fn fetching_overrides_outcome() {
        let outcome = SyncOutcome::default();
        let rows = remote_rows(&remotes(&["a"]), Some(&outcome), true);
        assert_eq!(rows, vec![("a".to_string(), RemoteSyncState::Fetching)]);
    }

    #[test]
    fn offline_outcome_not_reported_as_in_sync() {
        let outcome = SyncOutcome {
            offline: true,
            ..Default::default()
        };
        let rows = remote_rows(&remotes(&["a", "b"]), Some(&outcome), false);
        assert_eq!(
            rows,
            vec![
                (
                    "a".to_string(),
                    RemoteSyncState::FetchError("unreachable".to_string())
                ),
                (
                    "b".to_string(),
                    RemoteSyncState::FetchError("unreachable".to_string())
                ),
            ]
        );
    }

    #[test]
    fn fetch_error_reported_per_remote() {
        let outcome = SyncOutcome {
            fetch_errors: vec![("b".to_string(), "could not resolve host".to_string())],
            ..Default::default()
        };
        let rows = remote_rows(&remotes(&["a", "b"]), Some(&outcome), false);
        assert_eq!(
            rows,
            vec![
                ("a".to_string(), RemoteSyncState::InSync),
                (
                    "b".to_string(),
                    RemoteSyncState::FetchError("could not resolve host".to_string())
                ),
            ]
        );
    }

    #[test]
    fn clean_outcome_reports_in_sync() {
        let outcome = SyncOutcome::default();
        let rows = remote_rows(&remotes(&["a"]), Some(&outcome), false);
        assert_eq!(rows, vec![("a".to_string(), RemoteSyncState::InSync)]);
    }
}
