//! Fetch a repository's remotes and report their status.
//!
//! A sync fetches every remote (keeping tracking refs fresh), recomputes the
//! repo's status, and counts how many commits each remote is behind the
//! newest id published to any remote for each branch. Nothing is ever pushed
//! and local refs are never modified — workset only observes remotes.
//!
//! All network operations shell out to the `git` CLI (consistent with the
//! existing `gh`/`glab` shell-outs).

use anyhow::{Result, bail};
use std::collections::BTreeMap;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Timeout for git commands that hit the network (fetch)
const NETWORK_TIMEOUT: Duration = Duration::from_secs(60);
/// Timeout for purely local git commands
const LOCAL_TIMEOUT: Duration = Duration::from_secs(10);
/// How much of one untracked file is read to count its lines. A worktree can
/// hold a file of any size — a core dump, a VM image, a dataset nobody
/// ignored — and the info panel wants the number of lines, not the file, so
/// the bytes read have to stop somewhere. Past this point the count is
/// whatever the bytes up to it held.
const UNTRACKED_SCAN_LIMIT: u64 = 16 * 1024 * 1024;

/// Local and per-remote state of one branch, input to the behind-count
/// computation
#[derive(Debug, Clone)]
pub struct RefState {
    /// Local commit id, if the branch exists locally
    pub local: Option<String>,
    /// (remote name, id of this branch on that remote, if it exists there)
    pub remotes: Vec<(String, Option<String>)>,
}

/// Result of syncing one repository
#[derive(Debug, Default)]
pub struct SyncOutcome {
    /// (remote, error) for remotes that could not be fetched
    pub fetch_errors: Vec<(String, String)>,
    /// Fresh status computed after fetching (tracking refs are up to date)
    pub status: Option<crate::RepoStatus>,
    pub modification_time: Option<std::time::SystemTime>,
    /// True when no remote could be fetched, so the remote state is unknown
    pub offline: bool,
}

impl SyncOutcome {
    /// Short description of the most relevant problem, if any
    pub fn error_summary(&self) -> Option<String> {
        self.fetch_errors
            .first()
            .map(|(remote, error)| format!("fetch {}: {}", remote, error))
    }
}

/// The newest published id of a branch: the newest id on any remote,
/// preferring the local id when it is itself published. None when the ref
/// was never pushed anywhere.
fn newest_published(
    state: &RefState,
    is_ancestor: &mut dyn FnMut(&str, &str) -> bool,
) -> Option<String> {
    let first_published = state.remotes.iter().find_map(|(_, id)| id.as_deref())?;
    let local_published = state.local.as_deref().filter(|local| {
        state
            .remotes
            .iter()
            .any(|(_, id)| id.as_deref() == Some(local))
    });
    let mut candidate = local_published.unwrap_or(first_published);
    for (_, id) in &state.remotes {
        if let Some(id) = id.as_deref()
            && id != candidate
            && is_ancestor(candidate, id)
        {
            candidate = id;
        }
    }
    Some(candidate.to_string())
}

/// Sum how many commits each remote is behind the newest published id of the
/// given branch states. Remotes missing a ref or diverged contribute nothing
/// — only plain fast-forward distance is counted.
///
/// `is_ancestor(a, b)` answers whether `a` is reachable from `b`, which is
/// the only question the plan asks about history: ids that differ are
/// distinct commits, so one being an ancestor of the other already rules out
/// the reverse, and a pair related in neither direction has diverged.
pub fn plan_behind_counts(
    states: &[RefState],
    is_ancestor: &mut dyn FnMut(&str, &str) -> bool,
    count_range: &mut dyn FnMut(&str, &str) -> usize,
) -> BTreeMap<String, usize> {
    let mut behind: BTreeMap<String, usize> = BTreeMap::new();
    for state in states {
        let Some(candidate) = newest_published(state, is_ancestor) else {
            continue;
        };
        for (remote, id) in &state.remotes {
            if let Some(id) = id.as_deref()
                && id != candidate
                && is_ancestor(id, &candidate)
            {
                let count = count_range(id, &candidate);
                if count > 0 {
                    *behind.entry(remote.clone()).or_default() += count;
                }
            }
        }
    }
    behind
}

/// Per-remote counts of commits missing from each remote, computed from the
/// local tracking refs of the repo's branches (no network — counts are
/// stale until the next fetch, which is fine for a UI indicator).
pub fn behind_counts(
    repo_path: &Path,
    remotes: &[String],
    interrupt: &AtomicBool,
) -> Result<BTreeMap<String, usize>> {
    let states = collect_ref_states(repo_path, remotes, interrupt)?;
    let mut is_ancestor =
        |ancestor: &str, descendant: &str| is_ancestor(repo_path, ancestor, descendant, interrupt);
    let mut count_range = |behind_id: &str, candidate: &str| {
        let range = format!("{}..{}", behind_id, candidate);
        run_git(
            repo_path,
            &["rev-list", "--count", &range],
            interrupt,
            LOCAL_TIMEOUT,
        )
        .ok()
        .filter(|out| out.status.success())
        .and_then(|out| String::from_utf8_lossy(&out.stdout).trim().parse().ok())
        .unwrap_or(0)
    };
    Ok(plan_behind_counts(&states, &mut is_ancestor, &mut count_range))
}

/// Fetch all remotes to refresh tracking refs, then recompute the repo's
/// status. Never pushes or modifies refs.
///
/// Blocking; intended to run on a background thread. `interrupt` is checked
/// between git invocations and aborts the ones in flight.
pub fn sync_repo(repo_path: &Path, interrupt: &AtomicBool) -> Result<SyncOutcome> {
    let mut outcome = SyncOutcome::default();

    let remotes = list_remotes(repo_path, interrupt)?;
    if !remotes.is_empty() {
        let mut fetched = Vec::new();
        for remote in &remotes {
            if interrupt.load(Ordering::Relaxed) {
                bail!("interrupted");
            }
            // The remote name comes out of the repository's own config, which
            // is part of whatever the repo was when it arrived. `--` keeps a
            // name that happens to begin with a dash from being read as an
            // option: `git fetch --upload-pack=<cmd>` runs <cmd>, so without
            // the separator a hand-written `[remote "--upload-pack=..."]`
            // section turns a background fetch into arbitrary code execution.
            match run_git(
                repo_path,
                &["fetch", "--prune", "--quiet", "--", remote],
                interrupt,
                NETWORK_TIMEOUT,
            ) {
                Ok(out) if out.status.success() => fetched.push(remote.clone()),
                Ok(out) => outcome
                    .fetch_errors
                    .push((remote.clone(), stderr_summary(&out))),
                Err(e) => outcome.fetch_errors.push((remote.clone(), e.to_string())),
            }
        }

        // Every fetch failing usually means we're offline; don't flag each
        // repo as failed in that case, but don't claim it's in sync either
        if fetched.is_empty() {
            outcome.fetch_errors.clear();
            outcome.offline = true;
        }
    }

    let (status, modification_time) = crate::check_repo_status_and_modification_time(repo_path)
        .unwrap_or((crate::RepoStatus::NoCommits, None));
    outcome.status = Some(status);
    outcome.modification_time = modification_time;
    Ok(outcome)
}

/// Gather the local and per-remote state of every branch, from local branch
/// heads and tracking refs alone — this never touches the network.
fn collect_ref_states(
    repo_path: &Path,
    remotes: &[String],
    interrupt: &AtomicBool,
) -> Result<Vec<RefState>> {
    let out = run_git(
        repo_path,
        &["for-each-ref", "--format=%(objectname) %(refname)"],
        interrupt,
        LOCAL_TIMEOUT,
    )?;
    if !out.status.success() {
        bail!("git for-each-ref failed: {}", stderr_summary(&out));
    }

    let mut branches: BTreeMap<String, String> = BTreeMap::new();
    let mut tracking: BTreeMap<(String, String), String> = BTreeMap::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let Some((id, refname)) = line.split_once(' ') else {
            continue;
        };
        if let Some(name) = refname.strip_prefix("refs/heads/") {
            branches.insert(name.to_string(), id.to_string());
        } else if let Some(rest) = refname.strip_prefix("refs/remotes/") {
            // Remote names are matched against the known list because branch
            // names may themselves contain slashes
            for remote in remotes {
                if let Some(branch) = rest
                    .strip_prefix(remote.as_str())
                    .and_then(|r| r.strip_prefix('/'))
                    && branch != "HEAD"
                {
                    tracking.insert((remote.clone(), branch.to_string()), id.to_string());
                    break;
                }
            }
        }
    }

    // Branch names come from local branches and tracking refs alike, so a
    // branch that only exists on remotes is still counted
    let mut branch_names: std::collections::BTreeSet<String> = branches.keys().cloned().collect();
    branch_names.extend(tracking.keys().map(|(_, name)| name.clone()));

    let mut states = Vec::new();
    for name in branch_names {
        states.push(RefState {
            local: branches.get(&name).cloned(),
            remotes: remotes
                .iter()
                .map(|r| (r.clone(), tracking.get(&(r.clone(), name.clone())).cloned()))
                .collect(),
        });
    }

    Ok(states)
}

/// Whether `ancestor` is reachable from `descendant`. Both ids must exist
/// locally, which tracking refs do after a fetch. A git call that fails or is
/// interrupted reads as "not an ancestor", so an unanswerable comparison
/// leaves the remote out of the behind counts rather than inventing one.
fn is_ancestor(
    repo_path: &Path,
    ancestor: &str,
    descendant: &str,
    interrupt: &AtomicBool,
) -> bool {
    run_git(
        repo_path,
        &["merge-base", "--is-ancestor", ancestor, descendant],
        interrupt,
        LOCAL_TIMEOUT,
    )
    .map(|out| out.status.success())
    .unwrap_or(false)
}

/// List the repository's configured remotes
pub fn list_remotes(repo_path: &Path, interrupt: &AtomicBool) -> Result<Vec<String>> {
    let out = run_git(repo_path, &["remote"], interrupt, LOCAL_TIMEOUT)?;
    if !out.status.success() {
        bail!("git remote failed: {}", stderr_summary(&out));
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect())
}

/// Lines (added, removed) across the worktree and index relative to HEAD,
/// with untracked file contents counted as additions; see `count_lines` for
/// how much of one untracked file is read.
pub fn count_diff_lines(repo_path: &Path, interrupt: &AtomicBool) -> Result<(usize, usize)> {
    let mut added = 0;
    let mut removed = 0;

    // Binary files show "-" in numstat and count as 0 lines; a failing diff
    // (no commits yet, so no HEAD) just means there is no diff to count
    if let Ok(out) = run_git(
        repo_path,
        &["diff", "HEAD", "--numstat"],
        interrupt,
        LOCAL_TIMEOUT,
    ) && out.status.success()
    {
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            let mut fields = line.split('\t');
            let count = |field: Option<&str>| field.and_then(|f| f.parse().ok()).unwrap_or(0);
            added += count(fields.next());
            removed += count(fields.next());
        }
    }

    let out = run_git(
        repo_path,
        &["ls-files", "--others", "--exclude-standard", "-z"],
        interrupt,
        LOCAL_TIMEOUT,
    )?;
    if !out.status.success() {
        bail!("git ls-files failed: {}", stderr_summary(&out));
    }
    for file in out.stdout.split(|b| *b == 0).filter(|f| !f.is_empty()) {
        if interrupt.load(Ordering::Relaxed) {
            bail!("interrupted");
        }
        let path = repo_path.join(String::from_utf8_lossy(file).as_ref());
        added += count_lines(&path, UNTRACKED_SCAN_LIMIT, interrupt).unwrap_or(0);
    }
    Ok((added, removed))
}

/// Lines in `path`: its newlines, plus one for a last line that doesn't end in
/// one. At most `limit` bytes are read, through a buffer of a fixed size, so
/// neither the memory nor the work this takes follows the size of the file.
/// `None` when the path holds no lines to count.
///
/// Only a regular file is read. A symlink's bytes live wherever it points,
/// which is not the worktree: a path outside the repo, a directory, or a
/// character device that yields bytes forever and so is never finished with.
/// `git ls-files --others` hands back untracked symlinks along with everything
/// else, so the check has to happen here.
fn count_lines(path: &Path, limit: u64, interrupt: &AtomicBool) -> Option<usize> {
    use std::io::Read;

    if !std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_file()) {
        return None;
    }
    let mut reader = std::fs::File::open(path).ok()?.take(limit);

    let mut buf = [0u8; 64 * 1024];
    let mut lines = 0;
    // An empty file has no last line to make up for
    let mut last = b'\n';
    loop {
        if interrupt.load(Ordering::Relaxed) {
            return None;
        }
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(read) => {
                lines += buf[..read].iter().filter(|byte| **byte == b'\n').count();
                last = buf[read - 1];
            }
            Err(_) => return None,
        }
    }
    if last != b'\n' {
        lines += 1;
    }
    Some(lines)
}

fn stderr_summary(out: &Output) -> String {
    let stderr = String::from_utf8_lossy(&out.stderr);
    stderr
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("unknown error")
        .trim()
        .to_string()
}

/// Run a git command with output capture, a timeout, and interrupt support.
/// Credential prompts are disabled so background tasks fail instead of
/// hanging or corrupting the terminal.
fn run_git(
    repo_path: &Path,
    args: &[&str],
    interrupt: &AtomicBool,
    timeout: Duration,
) -> Result<Output> {
    let mut child = Command::new("git")
        .args(args)
        .current_dir(repo_path)
        .env("GIT_TERMINAL_PROMPT", "0")
        // BatchMode alone rejects hosts not yet in known_hosts, since there is
        // no terminal to confirm on; accept-new trusts a host's key on first
        // contact but still fails hard if a known key ever changes
        .env(
            "GIT_SSH_COMMAND",
            "ssh -oBatchMode=yes -oStrictHostKeyChecking=accept-new",
        )
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    // Drain pipes on separate threads so a chatty child can't fill the pipe
    // buffer and deadlock against try_wait polling
    let mut stdout_pipe = child.stdout.take().expect("stdout was piped");
    let mut stderr_pipe = child.stderr.take().expect("stderr was piped");
    let stdout_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = std::io::Read::read_to_end(&mut stdout_pipe, &mut buf);
        buf
    });
    let stderr_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = std::io::Read::read_to_end(&mut stderr_pipe, &mut buf);
        buf
    });

    let start = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if interrupt.load(Ordering::Relaxed) {
            let _ = child.kill();
            let _ = child.wait();
            bail!("git {} interrupted", args.first().unwrap_or(&""));
        }
        if start.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait();
            bail!("git {} timed out", args.first().unwrap_or(&""));
        }
        std::thread::sleep(Duration::from_millis(50));
    };

    Ok(Output {
        status,
        stdout: stdout_reader.join().unwrap_or_default(),
        stderr: stderr_reader.join().unwrap_or_default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn branch(local: Option<&str>, remotes: &[(&str, Option<&str>)]) -> RefState {
        RefState {
            local: local.map(|s| s.to_string()),
            remotes: remotes
                .iter()
                .map(|(r, id)| (r.to_string(), id.map(|s| s.to_string())))
                .collect(),
        }
    }

    /// Ancestor stub: ids are single letters, and "ab" means a precedes b in
    /// history, so a is an ancestor of b; anything unlisted has diverged
    fn stub_is_ancestor(order: &'static [&'static str]) -> impl FnMut(&str, &str) -> bool {
        move |ancestor, descendant| order.contains(&format!("{}{}", ancestor, descendant).as_str())
    }

    #[test]
    fn behind_counts_summed_across_branches() {
        let states = [
            branch(Some("b"), &[("a", Some("b")), ("backup", Some("a"))]),
            branch(Some("d"), &[("a", Some("d")), ("backup", Some("c"))]),
        ];
        let mut is_ancestor = stub_is_ancestor(&["ab", "cd"]);
        let mut count = |behind: &str, candidate: &str| -> usize {
            match (behind, candidate) {
                ("a", "b") => 3,
                ("c", "d") => 2,
                other => panic!("unexpected range {:?}", other),
            }
        };
        let counts = plan_behind_counts(&states, &mut is_ancestor, &mut count);
        assert_eq!(counts, BTreeMap::from([("backup".to_string(), 5)]));
    }

    #[test]
    fn missing_diverged_and_unpublished_refs_not_counted() {
        let states = [
            // Remote missing the ref: nothing published there to lag behind
            branch(Some("b"), &[("a", Some("b")), ("backup", None)]),
            // Diverged remote: neither id is reachable from the other, so
            // there is no fast-forward distance to report
            branch(Some("x"), &[("a", Some("x")), ("backup", Some("y"))]),
            // Never-published ref: no published id to lag behind
            branch(Some("z"), &[("a", None), ("backup", None)]),
        ];
        let mut is_ancestor = stub_is_ancestor(&[]);
        let mut count = |_: &str, _: &str| -> usize { panic!("no range should be counted") };
        assert!(plan_behind_counts(&states, &mut is_ancestor, &mut count).is_empty());
    }

    /// End-to-end over a real repository: `collect_ref_states` reads the
    /// branch heads and tracking refs, and `behind_counts` turns them into a
    /// per-remote commit distance.
    #[test]
    fn behind_counts_read_from_real_tracking_refs() {
        let temp = tempfile::TempDir::new().unwrap();
        let repo = temp.path().join("work");
        std::fs::create_dir(&repo).unwrap();
        let git = |dir: &Path, args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(dir)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {:?}: {}",
                args,
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };

        git(&repo, &["init", "--quiet", "--initial-branch=main"]);
        git(&repo, &["config", "user.name", "workset"]);
        git(&repo, &["config", "user.email", "workset@example.com"]);
        git(&repo, &["commit", "--quiet", "--allow-empty", "-m", "one"]);
        let first = git(&repo, &["rev-parse", "HEAD"]);
        git(&repo, &["commit", "--quiet", "--allow-empty", "-m", "two"]);

        // "current" has both commits, "stale" only the first, so it is one
        // commit behind the newest published id of main
        for (name, tip) in [("current", "HEAD"), ("stale", first.as_str())] {
            let remote = temp.path().join(name);
            git(temp.path(), &["init", "--quiet", "--bare", name]);
            git(&repo, &["remote", "add", name, &remote.to_string_lossy()]);
            git(
                &repo,
                &["push", "--quiet", name, &format!("{}:refs/heads/main", tip)],
            );
        }
        git(&repo, &["fetch", "--quiet", "--all"]);

        let interrupt = AtomicBool::new(false);
        let remotes = list_remotes(&repo, &interrupt).unwrap();
        assert_eq!(remotes, vec!["current".to_string(), "stale".to_string()]);
        assert_eq!(
            behind_counts(&repo, &remotes, &interrupt).unwrap(),
            BTreeMap::from([("stale".to_string(), 1)]),
        );
    }

    #[test]
    fn error_summary_reports_first_fetch_error() {
        let outcome = SyncOutcome {
            fetch_errors: vec![("origin".to_string(), "unreachable".to_string())],
            ..Default::default()
        };
        assert_eq!(outcome.error_summary().unwrap(), "fetch origin: unreachable");
    }

    #[test]
    fn all_remotes_unreachable_reported_offline() {
        let temp = tempfile::TempDir::new().unwrap();
        let repo = temp.path();
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(repo)
                .output()
                .unwrap();
            assert!(out.status.success(), "git {:?} failed", args);
        };
        git(&["init", "--quiet"]);
        git(&["remote", "add", "origin", "/nonexistent/workset-test-remote"]);

        let interrupt = AtomicBool::new(false);
        let outcome = sync_repo(repo, &interrupt).unwrap();
        assert!(outcome.offline);
        assert!(outcome.fetch_errors.is_empty());
        assert!(outcome.error_summary().is_none());
    }

    /// A remote name is config the repository carries with it, and a repo can
    /// arrive from anywhere (a tarball, a shared directory, a copy someone
    /// handed over). `git remote add` refuses a name beginning with a dash,
    /// but a hand-written config can hold one, and the TUI fetches every
    /// remote of every repo in the workspace on its own. Passed as a bare
    /// argument, `--upload-pack=<cmd>` makes git run <cmd>.
    #[cfg(unix)]
    #[test]
    fn dashed_remote_name_is_not_read_as_a_fetch_option() {
        let temp = tempfile::TempDir::new().unwrap();
        let repo = temp.path().join("work");
        std::fs::create_dir(&repo).unwrap();
        let git = |dir: &Path, args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(dir)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {:?}: {}",
                args,
                String::from_utf8_lossy(&out.stderr)
            );
        };

        git(&repo, &["init", "--quiet", "--initial-branch=main"]);
        git(&repo, &["config", "user.name", "workset"]);
        git(&repo, &["config", "user.email", "workset@example.com"]);
        git(&repo, &["commit", "--quiet", "--allow-empty", "-m", "one"]);
        git(temp.path(), &["init", "--quiet", "--bare", "upstream"]);
        let upstream = temp.path().join("upstream");
        let upstream = upstream.to_string_lossy().to_string();
        git(&repo, &["remote", "add", "origin", &upstream]);
        git(&repo, &["push", "--quiet", "origin", "main"]);

        // Written straight into the config, the way an untrusted repo would
        // carry it, because git won't create a remote with this name itself
        let marker = temp.path().join("pwned");
        let name = format!("--upload-pack=touch {}", marker.display());
        let mut config = std::fs::read_to_string(repo.join(".git/config")).unwrap();
        config.push_str(&format!(
            "[remote \"{}\"]\n\turl = {}\n\tfetch = +refs/heads/*:refs/remotes/evil/*\n",
            name, upstream
        ));
        std::fs::write(repo.join(".git/config"), config).unwrap();
        assert!(
            list_remotes(&repo, &AtomicBool::new(false))
                .unwrap()
                .contains(&name)
        );

        let outcome = sync_repo(&repo, &AtomicBool::new(false)).unwrap();

        assert!(
            !marker.exists(),
            "fetching the repo ran the remote name's embedded command"
        );
        // Behind the separator the name is still just a remote name, so the
        // fetch resolves the section's url and succeeds like any other
        assert!(
            outcome.fetch_errors.is_empty(),
            "{:?}",
            outcome.fetch_errors
        );
        assert!(!outcome.offline);
        assert!(repo.join(".git/refs/remotes/evil/main").exists());
    }

    /// `git ls-files --others` reports an untracked symlink as a path like
    /// any other, so counting the file "at" that path reads whatever it
    /// points at: bytes that live outside the worktree, or a character device
    /// that never reaches an end. Reading a 4 GiB artifact nobody ignored
    /// costs just as much, and a line count doesn't need the file in memory.
    #[cfg(unix)]
    #[test]
    fn untracked_symlinks_and_oversized_files_do_not_become_the_line_count() {
        let temp = tempfile::TempDir::new().unwrap();
        let repo = temp.path().join("work");
        std::fs::create_dir(&repo).unwrap();
        let out = std::process::Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(&repo)
            .output()
            .unwrap();
        assert!(out.status.success());

        // Three lines of the repo's own, which are the only ones to find
        std::fs::write(repo.join("notes.txt"), "a\nb\nc\n").unwrap();

        // Bytes that belong to something else, reachable only through a link
        let outside = temp.path().join("outside.txt");
        std::fs::write(&outside, "x\n".repeat(500)).unwrap();
        std::os::unix::fs::symlink(&outside, repo.join("linked")).unwrap();
        // A link to a directory, and one to a stream with no end at all:
        // following either is wrong, but the second never even returns
        std::os::unix::fs::symlink(temp.path(), repo.join("linked-dir")).unwrap();
        std::os::unix::fs::symlink("/dev/zero", repo.join("endless")).unwrap();

        let interrupt = AtomicBool::new(false);
        assert_eq!(count_diff_lines(&repo, &interrupt).unwrap(), (3, 0));
    }

    /// A file is counted through a fixed buffer, so its size is not the amount
    /// of memory the count takes, and the scan gives up on one file rather
    /// than reading an unbounded number of bytes to label a panel.
    #[test]
    fn line_count_reads_at_most_the_scan_limit() {
        let temp = tempfile::TempDir::new().unwrap();
        let file = temp.path().join("many.txt");
        std::fs::write(&file, "x\n".repeat(100)).unwrap();
        let interrupt = AtomicBool::new(false);

        assert_eq!(count_lines(&file, u64::MAX, &interrupt), Some(100));
        // Only the first 10 bytes are read, which hold five lines
        assert_eq!(count_lines(&file, 10, &interrupt), Some(5));

        // A line without a trailing newline is still a line
        std::fs::write(&file, "one\ntwo").unwrap();
        assert_eq!(count_lines(&file, u64::MAX, &interrupt), Some(2));
        std::fs::write(&file, "").unwrap();
        assert_eq!(count_lines(&file, u64::MAX, &interrupt), Some(0));

        // Nothing is read once the user has moved on
        std::fs::write(&file, "one\ntwo\n").unwrap();
        assert_eq!(count_lines(&file, u64::MAX, &AtomicBool::new(true)), None);
    }

    #[test]
    fn repo_without_remotes_not_offline() {
        let temp = tempfile::TempDir::new().unwrap();
        let out = std::process::Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(temp.path())
            .output()
            .unwrap();
        assert!(out.status.success());

        let interrupt = AtomicBool::new(false);
        let outcome = sync_repo(temp.path(), &interrupt).unwrap();
        assert!(!outcome.offline);
        assert!(outcome.fetch_errors.is_empty());
    }
}
