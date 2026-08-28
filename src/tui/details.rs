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
    /// Number of uncommitted changes and untracked files; 0 = clean
    pub change_count: Option<usize>,
    /// The repo's mirror configuration; when enabled, carries the remotes
    /// that act as mirror targets
    pub mirrors: Option<MirrorConfig>,
}

/// Whether mirroring is enabled for a repo, and if so the remotes it covers
/// and the branch/tag patterns it mirrors
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MirrorConfig {
    Disabled,
    Enabled {
        remotes: Vec<String>,
        patterns: crate::sync::MirrorPatterns,
    },
}

impl RepoDetails {
    fn is_complete(&self) -> bool {
        self.size_bytes.is_some() && self.change_count.is_some() && self.mirrors.is_some()
    }
}

/// Display state of one mirror remote in the info panel
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MirrorState {
    Syncing,
    InSync,
    /// Refs pushed to this remote during the last sync
    Pushed(usize),
    Conflict(String),
    PushError(String),
    FetchError(String),
    /// No sync outcome recorded yet
    Pending,
}

/// Merge the mirror remotes with the last sync outcome and the live syncing
/// flag into one display row per remote
pub fn mirror_rows(
    mirrors: &[String],
    outcome: Option<&SyncOutcome>,
    syncing: bool,
) -> Vec<(String, MirrorState)> {
    mirrors
        .iter()
        .map(|name| {
            let state = if syncing {
                MirrorState::Syncing
            } else {
                match outcome {
                    None => MirrorState::Pending,
                    Some(o) => mirror_state_from_outcome(name, o),
                }
            };
            (name.clone(), state)
        })
        .collect()
}

fn mirror_state_from_outcome(remote: &str, outcome: &SyncOutcome) -> MirrorState {
    if let Some((_, refname, reason)) = outcome.conflicts.iter().find(|(r, ..)| r == remote) {
        return MirrorState::Conflict(format!("{}: {}", crate::sync::short_ref(refname), reason));
    }
    if let Some((_, refname, error)) = outcome.push_errors.iter().find(|(r, ..)| r == remote) {
        return MirrorState::PushError(format!(
            "push {}: {}",
            crate::sync::short_ref(refname),
            error
        ));
    }
    if let Some((_, error)) = outcome.fetch_errors.iter().find(|(r, _)| r == remote) {
        return MirrorState::FetchError(error.clone());
    }
    if outcome.offline {
        return MirrorState::FetchError("unreachable".to_string());
    }
    let pushed = outcome.pushed.iter().filter(|(r, _)| r == remote).count();
    if pushed > 0 {
        MirrorState::Pushed(pushed)
    } else {
        MirrorState::InSync
    }
}

/// Results streamed from a background details job. Events are keyed by repo
/// path, so results from a job for a previously selected repo land harmlessly
/// in the cache.
enum DetailsEvent {
    Size(PathBuf, u64),
    Changes(PathBuf, usize),
    Mirrors(PathBuf, MirrorConfig),
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
        if entry.change_count.is_none()
            && (is_library || matches!(status, Some(RepoStatus::Clean | RepoStatus::Unpushed)))
        {
            entry.change_count = Some(0);
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
            if current.mirrors.is_none() {
                let _ = tx.send(DetailsEvent::Mirrors(
                    path.clone(),
                    load_mirrors(&path, &interrupt),
                ));
            }
            if current.change_count.is_none() {
                let count = crate::count_worktree_changes(&path).unwrap_or(0);
                let _ = tx.send(DetailsEvent::Changes(path.clone(), count));
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
                DetailsEvent::Changes(path, count) => {
                    app.details.entry(path).or_default().change_count = Some(count);
                }
                DetailsEvent::Mirrors(path, mirrors) => {
                    app.details.entry(path).or_default().mirrors = Some(mirrors);
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

/// Read the repo's mirror patterns; when enabled, the mirror targets are all
/// of the repo's remotes
fn load_mirrors(path: &Path, interrupt: &AtomicBool) -> MirrorConfig {
    let patterns = crate::sync::mirror_patterns(path, interrupt).unwrap_or_default();
    if patterns.enabled() {
        MirrorConfig::Enabled {
            remotes: crate::sync::list_remotes(path, interrupt).unwrap_or_default(),
            patterns,
        }
    } else {
        MirrorConfig::Disabled
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mirrors(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| n.to_string()).collect()
    }

    #[test]
    fn no_outcome_reports_pending() {
        let rows = mirror_rows(&mirrors(&["a", "b"]), None, false);
        assert_eq!(
            rows,
            vec![
                ("a".to_string(), MirrorState::Pending),
                ("b".to_string(), MirrorState::Pending),
            ]
        );
    }

    #[test]
    fn syncing_overrides_outcome() {
        let outcome = SyncOutcome {
            pushed: vec![("a".to_string(), "refs/heads/main".to_string())],
            ..Default::default()
        };
        let rows = mirror_rows(&mirrors(&["a"]), Some(&outcome), true);
        assert_eq!(rows, vec![("a".to_string(), MirrorState::Syncing)]);
    }

    #[test]
    fn two_mirrors_get_independent_states() {
        let outcome = SyncOutcome {
            pushed: vec![
                ("a".to_string(), "refs/heads/main".to_string()),
                ("a".to_string(), "refs/tags/v1".to_string()),
            ],
            conflicts: vec![(
                "b".to_string(),
                "refs/heads/main".to_string(),
                "diverged".to_string(),
            )],
            ..Default::default()
        };
        let rows = mirror_rows(&mirrors(&["a", "b"]), Some(&outcome), false);
        assert_eq!(
            rows,
            vec![
                ("a".to_string(), MirrorState::Pushed(2)),
                (
                    "b".to_string(),
                    MirrorState::Conflict("main: diverged".to_string())
                ),
            ]
        );
    }

    #[test]
    fn conflict_takes_priority_over_push_on_same_remote() {
        let outcome = SyncOutcome {
            pushed: vec![("a".to_string(), "refs/tags/v1".to_string())],
            conflicts: vec![(
                "a".to_string(),
                "refs/heads/main".to_string(),
                "non-fast-forward".to_string(),
            )],
            ..Default::default()
        };
        let rows = mirror_rows(&mirrors(&["a"]), Some(&outcome), false);
        assert_eq!(
            rows,
            vec![(
                "a".to_string(),
                MirrorState::Conflict("main: non-fast-forward".to_string())
            )]
        );
    }

    #[test]
    fn offline_outcome_not_reported_as_in_sync() {
        let outcome = SyncOutcome {
            offline: true,
            ..Default::default()
        };
        let rows = mirror_rows(&mirrors(&["a", "b"]), Some(&outcome), false);
        assert_eq!(
            rows,
            vec![
                (
                    "a".to_string(),
                    MirrorState::FetchError("unreachable".to_string())
                ),
                (
                    "b".to_string(),
                    MirrorState::FetchError("unreachable".to_string())
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
        let rows = mirror_rows(&mirrors(&["a", "b"]), Some(&outcome), false);
        assert_eq!(
            rows,
            vec![
                ("a".to_string(), MirrorState::InSync),
                (
                    "b".to_string(),
                    MirrorState::FetchError("could not resolve host".to_string())
                ),
            ]
        );
    }

    #[test]
    fn clean_outcome_reports_in_sync() {
        let outcome = SyncOutcome::default();
        let rows = mirror_rows(&mirrors(&["a"]), Some(&outcome), false);
        assert_eq!(rows, vec![("a".to_string(), MirrorState::InSync)]);
    }
}
