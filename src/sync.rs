//! Mirror commits to a repository's opted-in mirror remotes.
//!
//! Mirroring is opt-in per repo: remotes listed in the multi-valued local git
//! config key `workset.mirror` receive refs that the user has already
//! published to at least one remote. Repos without any `workset.mirror`
//! entries are skipped entirely. Commits that exist only locally are never
//! pushed automatically.
//!
//! gix has no push support yet, so all network operations shell out to the
//! `git` CLI (consistent with the existing `gh`/`glab` shell-outs).

use anyhow::{Result, bail};
use std::collections::BTreeMap;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Timeout for git commands that hit the network (fetch, push, ls-remote)
const NETWORK_TIMEOUT: Duration = Duration::from_secs(60);
/// Timeout for purely local git commands
const LOCAL_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefKind {
    Branch,
    Tag,
}

/// Local and per-remote state of one ref, input to the sync planner
#[derive(Debug, Clone)]
pub struct RefState {
    /// Short name, e.g. "main" or "v1.0"
    pub name: String,
    pub kind: RefKind,
    /// Local commit (or tag object) id
    pub local: String,
    /// (remote name, id of this ref on that remote, if it exists there)
    pub remotes: Vec<(String, Option<String>)>,
}

impl RefState {
    /// Full refname, e.g. "refs/heads/main" or "refs/tags/v1.0"
    pub fn refname(&self) -> String {
        match self.kind {
            RefKind::Branch => format!("refs/heads/{}", self.name),
            RefKind::Tag => format!("refs/tags/{}", self.name),
        }
    }
}

/// Relationship between the local id and a remote's id for the same ref
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ancestry {
    Equal,
    /// The remote id is an ancestor of the local id
    LocalAhead,
    /// The local id is an ancestor of the remote id
    LocalBehind,
    Diverged,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefDecision {
    Push {
        remote: String,
        refname: String,
    },
    Conflict {
        remote: String,
        refname: String,
        reason: String,
    },
}

/// Result of syncing one repository
#[derive(Debug, Default)]
pub struct SyncOutcome {
    /// (remote, error) for remotes that could not be fetched
    pub fetch_errors: Vec<(String, String)>,
    /// (remote, refname) successfully mirrored
    pub pushed: Vec<(String, String)>,
    /// (remote, refname) that a dry run would have pushed
    pub would_push: Vec<(String, String)>,
    /// (remote, refname, reason) for diverged refs and rejected pushes
    pub conflicts: Vec<(String, String, String)>,
    /// (remote, refname, error) for pushes that failed for other reasons
    pub push_errors: Vec<(String, String, String)>,
    /// Fresh status computed after fetching (tracking refs are up to date)
    pub status: Option<crate::RepoStatus>,
    pub modification_time: Option<std::time::SystemTime>,
    /// True when the repo has no workset.mirror entries and was not touched
    pub skipped: bool,
    /// Configured mirror names that don't match any git remote
    pub config_errors: Vec<String>,
}

impl SyncOutcome {
    /// Short description of the most relevant problem, if any
    pub fn error_summary(&self) -> Option<String> {
        if let Some((remote, refname, reason)) = self.conflicts.first() {
            return Some(format!("{} on {}: {}", short_ref(refname), remote, reason));
        }
        if let Some((remote, refname, error)) = self.push_errors.first() {
            return Some(format!(
                "push {} to {}: {}",
                short_ref(refname),
                remote,
                error
            ));
        }
        if let Some((remote, error)) = self.fetch_errors.first() {
            return Some(format!("fetch {}: {}", remote, error));
        }
        if let Some(error) = self.config_errors.first() {
            return Some(error.clone());
        }
        None
    }
}

pub(crate) fn short_ref(refname: &str) -> &str {
    refname
        .strip_prefix("refs/heads/")
        .or_else(|| refname.strip_prefix("refs/tags/"))
        .unwrap_or(refname)
}

/// Decide what to do for one ref across all remotes.
///
/// The mirror-only rule: a ref is only propagated if its exact local id is
/// already present on at least one remote. Refs the user never pushed
/// anywhere are left alone.
pub fn plan_ref_sync(
    state: &RefState,
    ancestry: &mut dyn FnMut(&str, &str) -> Ancestry,
) -> Vec<RefDecision> {
    let refname = state.refname();
    let exists_somewhere = state.remotes.iter().any(|(_, id)| id.is_some());
    if !exists_somewhere {
        // Local-only ref: never touched
        return Vec::new();
    }
    let published = state
        .remotes
        .iter()
        .any(|(_, id)| id.as_deref() == Some(state.local.as_str()));

    let mut decisions = Vec::new();
    match state.kind {
        RefKind::Branch => {
            if !published {
                // Local id isn't on any remote: unpushed, behind, or diverged.
                // Nothing is pushed and no error is raised.
                return Vec::new();
            }
            for (remote, id) in &state.remotes {
                match id.as_deref() {
                    None => decisions.push(RefDecision::Push {
                        remote: remote.clone(),
                        refname: refname.clone(),
                    }),
                    Some(id) if id == state.local => {}
                    Some(id) => match ancestry(&state.local, id) {
                        Ancestry::LocalAhead => decisions.push(RefDecision::Push {
                            remote: remote.clone(),
                            refname: refname.clone(),
                        }),
                        Ancestry::Equal | Ancestry::LocalBehind => {}
                        Ancestry::Diverged => decisions.push(RefDecision::Conflict {
                            remote: remote.clone(),
                            refname: refname.clone(),
                            reason: "diverged (non-fast-forward)".to_string(),
                        }),
                    },
                }
            }
        }
        RefKind::Tag => {
            // Tags are compared by identity only and never rewritten
            for (remote, id) in &state.remotes {
                match id.as_deref() {
                    None if published => decisions.push(RefDecision::Push {
                        remote: remote.clone(),
                        refname: refname.clone(),
                    }),
                    None => {}
                    Some(id) if id == state.local => {}
                    Some(_) => decisions.push(RefDecision::Conflict {
                        remote: remote.clone(),
                        refname: refname.clone(),
                        reason: "tag exists with different id".to_string(),
                    }),
                }
            }
        }
    }
    decisions
}

/// Fetch all remotes, then mirror published refs to the repo's configured
/// mirror remotes (`workset.mirror`). Repos without any mirror remotes are
/// skipped without touching the network.
///
/// Blocking; intended to run on a background thread. `interrupt` is checked
/// between git invocations and aborts the ones in flight.
pub fn sync_repo(repo_path: &Path, interrupt: &AtomicBool, dry_run: bool) -> Result<SyncOutcome> {
    let mut outcome = SyncOutcome::default();

    let mirrors = mirror_remotes(repo_path, interrupt)?;
    if mirrors.is_empty() {
        outcome.skipped = true;
        return Ok(outcome);
    }

    let remotes = list_remotes(repo_path, interrupt)?;
    for mirror in &mirrors {
        if !remotes.contains(mirror) {
            outcome
                .config_errors
                .push(format!("mirror remote '{}' not found", mirror));
        }
    }

    if !remotes.is_empty() {
        // All remotes are fetched, not just mirrors: the planner needs to see
        // where a ref is published, and the status recompute below needs
        // fresh tracking refs
        let mut fetched = Vec::new();
        for remote in &remotes {
            if interrupt.load(Ordering::Relaxed) {
                bail!("interrupted");
            }
            match run_git(
                repo_path,
                &["fetch", "--prune", "--quiet", remote],
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
        // repo as failed in that case
        if fetched.is_empty() {
            outcome.fetch_errors.clear();
        } else if fetched.iter().any(|r| mirrors.contains(r)) {
            mirror_refs(repo_path, &fetched, &mirrors, interrupt, dry_run, &mut outcome)?;
        }
    }

    let (status, modification_time) = crate::check_repo_status_and_modification_time(repo_path)
        .unwrap_or((crate::RepoStatus::NoCommits, None));
    outcome.status = Some(status);
    outcome.modification_time = modification_time;
    Ok(outcome)
}

/// Keep only decisions that target a declared mirror remote
pub fn filter_mirror_decisions(
    decisions: Vec<RefDecision>,
    mirrors: &[String],
) -> Vec<RefDecision> {
    decisions
        .into_iter()
        .filter(|decision| {
            let remote = match decision {
                RefDecision::Push { remote, .. } => remote,
                RefDecision::Conflict { remote, .. } => remote,
            };
            mirrors.contains(remote)
        })
        .collect()
}

/// Plan mirror pushes using the state of all fetched remotes, then execute
/// only the decisions targeting mirror remotes
fn mirror_refs(
    repo_path: &Path,
    remotes: &[String],
    mirrors: &[String],
    interrupt: &AtomicBool,
    dry_run: bool,
    outcome: &mut SyncOutcome,
) -> Result<()> {
    let ref_states = collect_ref_states(repo_path, remotes, interrupt)?;

    let mut ancestry =
        |local: &str, other: &str| compare_ancestry(repo_path, local, other, interrupt);
    let mut pushes: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for state in &ref_states {
        for decision in filter_mirror_decisions(plan_ref_sync(state, &mut ancestry), mirrors) {
            match decision {
                RefDecision::Push { remote, refname } => {
                    pushes.entry(remote).or_default().push(refname)
                }
                RefDecision::Conflict {
                    remote,
                    refname,
                    reason,
                } => outcome.conflicts.push((remote, refname, reason)),
            }
        }
    }

    for (remote, refnames) in pushes {
        if dry_run {
            for refname in refnames {
                outcome.would_push.push((remote.clone(), refname));
            }
            continue;
        }
        if interrupt.load(Ordering::Relaxed) {
            bail!("interrupted");
        }
        push_refs(repo_path, &remote, &refnames, interrupt, outcome);
    }
    Ok(())
}

/// Gather the local and per-remote state of every branch and tag
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
    let mut tags: BTreeMap<String, String> = BTreeMap::new();
    let mut tracking: BTreeMap<(String, String), String> = BTreeMap::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let Some((id, refname)) = line.split_once(' ') else {
            continue;
        };
        if let Some(name) = refname.strip_prefix("refs/heads/") {
            branches.insert(name.to_string(), id.to_string());
        } else if let Some(name) = refname.strip_prefix("refs/tags/") {
            tags.insert(name.to_string(), id.to_string());
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

    let mut states = Vec::new();
    for (name, local) in branches {
        let remote_ids = remotes
            .iter()
            .map(|r| (r.clone(), tracking.get(&(r.clone(), name.clone())).cloned()))
            .collect();
        states.push(RefState {
            name,
            kind: RefKind::Branch,
            local,
            remotes: remote_ids,
        });
    }

    if !tags.is_empty() {
        // Tracking refs don't cover tags, so ask each remote directly
        let mut remote_tags: BTreeMap<(String, String), String> = BTreeMap::new();
        for remote in remotes {
            let out = run_git(
                repo_path,
                &["ls-remote", "--tags", remote],
                interrupt,
                NETWORK_TIMEOUT,
            )?;
            if !out.status.success() {
                bail!(
                    "git ls-remote --tags {} failed: {}",
                    remote,
                    stderr_summary(&out)
                );
            }
            for line in String::from_utf8_lossy(&out.stdout).lines() {
                let Some((id, refname)) = line.split_once('\t') else {
                    continue;
                };
                // Skip peeled entries; tag object ids match for-each-ref output
                if let Some(name) = refname.strip_prefix("refs/tags/")
                    && !name.ends_with("^{}")
                {
                    remote_tags.insert((remote.clone(), name.to_string()), id.to_string());
                }
            }
        }
        for (name, local) in tags {
            let remote_ids = remotes
                .iter()
                .map(|r| {
                    (
                        r.clone(),
                        remote_tags.get(&(r.clone(), name.clone())).cloned(),
                    )
                })
                .collect();
            states.push(RefState {
                name,
                kind: RefKind::Tag,
                local,
                remotes: remote_ids,
            });
        }
    }

    Ok(states)
}

/// Compare two ids that both exist locally (tracking refs after a fetch)
fn compare_ancestry(
    repo_path: &Path,
    local: &str,
    other: &str,
    interrupt: &AtomicBool,
) -> Ancestry {
    if local == other {
        return Ancestry::Equal;
    }
    let is_ancestor = |a: &str, b: &str| {
        run_git(
            repo_path,
            &["merge-base", "--is-ancestor", a, b],
            interrupt,
            LOCAL_TIMEOUT,
        )
        .map(|out| out.status.success())
        .unwrap_or(false)
    };
    match (is_ancestor(other, local), is_ancestor(local, other)) {
        (true, true) => Ancestry::Equal,
        (true, false) => Ancestry::LocalAhead,
        (false, true) => Ancestry::LocalBehind,
        (false, false) => Ancestry::Diverged,
    }
}

/// Push a batch of refs to one remote, classifying per-ref results
fn push_refs(
    repo_path: &Path,
    remote: &str,
    refnames: &[String],
    interrupt: &AtomicBool,
    outcome: &mut SyncOutcome,
) {
    let refspecs: Vec<String> = refnames.iter().map(|r| format!("{}:{}", r, r)).collect();
    let mut args = vec!["push", "--porcelain", remote];
    args.extend(refspecs.iter().map(|s| s.as_str()));

    let out = match run_git(repo_path, &args, interrupt, NETWORK_TIMEOUT) {
        Ok(out) => out,
        Err(e) => {
            for refname in refnames {
                outcome
                    .push_errors
                    .push((remote.to_string(), refname.clone(), e.to_string()));
            }
            return;
        }
    };

    let mut accounted = std::collections::HashSet::new();
    for (flag, to_ref, summary) in parse_push_porcelain(&String::from_utf8_lossy(&out.stdout)) {
        accounted.insert(to_ref.clone());
        match flag {
            '!' => outcome
                .conflicts
                .push((remote.to_string(), to_ref, summary)),
            _ => outcome.pushed.push((remote.to_string(), to_ref)),
        }
    }
    if !out.status.success() {
        let error = stderr_summary(&out);
        for refname in refnames {
            if !accounted.contains(refname) {
                outcome
                    .push_errors
                    .push((remote.to_string(), refname.clone(), error.clone()));
            }
        }
    }
}

/// Parse `git push --porcelain` output into (flag, destination ref, summary)
fn parse_push_porcelain(stdout: &str) -> Vec<(char, String, String)> {
    let mut results = Vec::new();
    for line in stdout.lines() {
        if line.starts_with("To ") || line == "Done" {
            continue;
        }
        let mut chars = line.chars();
        let Some(flag) = chars.next() else { continue };
        let rest = chars.as_str();
        let mut parts = rest.trim_start_matches('\t').splitn(2, '\t');
        let Some(refspec) = parts.next() else {
            continue;
        };
        let summary = parts.next().unwrap_or("").to_string();
        let Some((_, to_ref)) = refspec.split_once(':') else {
            continue;
        };
        results.push((flag, to_ref.to_string(), summary));
    }
    results
}

/// Remote names declared as replication targets via the multi-valued local
/// git config key `workset.mirror`. Empty when the key is unset.
pub fn mirror_remotes(repo_path: &Path, interrupt: &AtomicBool) -> Result<Vec<String>> {
    let out = run_git(
        repo_path,
        &["config", "--local", "--get-all", "workset.mirror"],
        interrupt,
        LOCAL_TIMEOUT,
    )?;
    // Exit code 1 means the key is unset
    if !out.status.success() {
        if out.status.code() == Some(1) {
            return Ok(Vec::new());
        }
        bail!("git config failed: {}", stderr_summary(&out));
    }
    let mut mirrors: Vec<String> = Vec::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let name = line.trim();
        if !name.is_empty() && !mirrors.iter().any(|m| m == name) {
            mirrors.push(name.to_string());
        }
    }
    Ok(mirrors)
}

/// Replace the repo's `workset.mirror` entries with the given set
pub fn set_mirror_remotes(repo_path: &Path, mirrors: &[String]) -> Result<()> {
    let interrupt = AtomicBool::new(false);
    let out = run_git(
        repo_path,
        &["config", "--local", "--unset-all", "workset.mirror"],
        &interrupt,
        LOCAL_TIMEOUT,
    )?;
    // Exit code 5 means the key didn't exist
    if !out.status.success() && out.status.code() != Some(5) {
        bail!("git config --unset-all failed: {}", stderr_summary(&out));
    }
    for mirror in mirrors {
        let out = run_git(
            repo_path,
            &["config", "--local", "--add", "workset.mirror", mirror],
            &interrupt,
            LOCAL_TIMEOUT,
        )?;
        if !out.status.success() {
            bail!("git config --add failed: {}", stderr_summary(&out));
        }
    }
    Ok(())
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
        .env("GIT_SSH_COMMAND", "ssh -oBatchMode=yes")
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

    fn branch(local: &str, remotes: &[(&str, Option<&str>)]) -> RefState {
        RefState {
            name: "main".to_string(),
            kind: RefKind::Branch,
            local: local.to_string(),
            remotes: remotes
                .iter()
                .map(|(r, id)| (r.to_string(), id.map(|s| s.to_string())))
                .collect(),
        }
    }

    fn tag(local: &str, remotes: &[(&str, Option<&str>)]) -> RefState {
        RefState {
            name: "v1".to_string(),
            kind: RefKind::Tag,
            ..branch(local, remotes)
        }
    }

    /// Ancestry stub: ids are single letters, and "ab" means a precedes b in
    /// history (b is a descendant of a); anything unlisted diverged
    fn stub_ancestry(order: &'static [&'static str]) -> impl FnMut(&str, &str) -> Ancestry {
        move |local, other| {
            let ahead = format!("{}{}", other, local);
            let behind = format!("{}{}", local, other);
            if local == other {
                Ancestry::Equal
            } else if order.iter().any(|s| *s == ahead) {
                Ancestry::LocalAhead
            } else if order.iter().any(|s| *s == behind) {
                Ancestry::LocalBehind
            } else {
                Ancestry::Diverged
            }
        }
    }

    #[test]
    fn published_branch_pushed_to_remote_behind() {
        let state = branch("b", &[("a", Some("b")), ("mirror", Some("a"))]);
        let decisions = plan_ref_sync(&state, &mut stub_ancestry(&["ab"]));
        assert_eq!(
            decisions,
            vec![RefDecision::Push {
                remote: "mirror".to_string(),
                refname: "refs/heads/main".to_string(),
            }]
        );
    }

    #[test]
    fn in_sync_branch_does_nothing() {
        let state = branch("b", &[("a", Some("b")), ("mirror", Some("b"))]);
        assert!(plan_ref_sync(&state, &mut stub_ancestry(&[])).is_empty());
    }

    #[test]
    fn local_only_branch_untouched() {
        let state = branch("b", &[("a", None), ("mirror", None)]);
        assert!(plan_ref_sync(&state, &mut stub_ancestry(&[])).is_empty());
    }

    #[test]
    fn unpushed_commits_never_auto_pushed() {
        // Local is ahead of every remote: the user hasn't pushed anywhere
        let state = branch("c", &[("a", Some("b")), ("mirror", Some("a"))]);
        assert!(plan_ref_sync(&state, &mut stub_ancestry(&["ab", "bc", "ac"])).is_empty());
    }

    #[test]
    fn remote_ahead_of_local_skipped() {
        // Published on mirror, but "a" has newer commits: don't touch it
        let state = branch("b", &[("a", Some("c")), ("mirror", Some("b"))]);
        assert!(plan_ref_sync(&state, &mut stub_ancestry(&["bc"])).is_empty());
    }

    #[test]
    fn diverged_remote_reports_conflict() {
        let state = branch("b", &[("a", Some("b")), ("mirror", Some("x"))]);
        let decisions = plan_ref_sync(&state, &mut stub_ancestry(&[]));
        assert_eq!(
            decisions,
            vec![RefDecision::Conflict {
                remote: "mirror".to_string(),
                refname: "refs/heads/main".to_string(),
                reason: "diverged (non-fast-forward)".to_string(),
            }]
        );
    }

    #[test]
    fn published_branch_created_on_remote_missing_it() {
        let state = branch("b", &[("a", Some("b")), ("mirror", None)]);
        let decisions = plan_ref_sync(&state, &mut stub_ancestry(&[]));
        assert_eq!(
            decisions,
            vec![RefDecision::Push {
                remote: "mirror".to_string(),
                refname: "refs/heads/main".to_string(),
            }]
        );
    }

    #[test]
    fn published_tag_mirrored_to_missing_remote() {
        let state = tag("t", &[("a", Some("t")), ("mirror", None)]);
        let decisions = plan_ref_sync(&state, &mut stub_ancestry(&[]));
        assert_eq!(
            decisions,
            vec![RefDecision::Push {
                remote: "mirror".to_string(),
                refname: "refs/tags/v1".to_string(),
            }]
        );
    }

    #[test]
    fn tag_mismatch_reports_conflict() {
        let state = tag("t", &[("a", Some("t")), ("mirror", Some("x"))]);
        let decisions = plan_ref_sync(&state, &mut stub_ancestry(&[]));
        assert_eq!(
            decisions,
            vec![RefDecision::Conflict {
                remote: "mirror".to_string(),
                refname: "refs/tags/v1".to_string(),
                reason: "tag exists with different id".to_string(),
            }]
        );
    }

    #[test]
    fn local_only_tag_untouched() {
        let state = tag("t", &[("a", None), ("mirror", None)]);
        assert!(plan_ref_sync(&state, &mut stub_ancestry(&[])).is_empty());
    }

    #[test]
    fn porcelain_rejection_parsed() {
        let stdout = "To git@example.com:foo/bar.git\n \trefs/heads/main:refs/heads/main\t1234..5678\n!\trefs/heads/dev:refs/heads/dev\t[rejected] (non-fast-forward)\nDone\n";
        let parsed = parse_push_porcelain(stdout);
        assert_eq!(
            parsed,
            vec![
                (' ', "refs/heads/main".to_string(), "1234..5678".to_string()),
                (
                    '!',
                    "refs/heads/dev".to_string(),
                    "[rejected] (non-fast-forward)".to_string()
                ),
            ]
        );
    }

    #[test]
    fn error_summary_prefers_conflicts() {
        let outcome = SyncOutcome {
            fetch_errors: vec![("a".to_string(), "unreachable".to_string())],
            conflicts: vec![(
                "mirror".to_string(),
                "refs/heads/main".to_string(),
                "diverged (non-fast-forward)".to_string(),
            )],
            ..Default::default()
        };
        assert_eq!(
            outcome.error_summary().unwrap(),
            "main on mirror: diverged (non-fast-forward)"
        );
    }

    #[test]
    fn filter_keeps_only_mirror_decisions() {
        let decisions = vec![
            RefDecision::Push {
                remote: "mirror".to_string(),
                refname: "refs/heads/main".to_string(),
            },
            RefDecision::Push {
                remote: "upstream".to_string(),
                refname: "refs/heads/main".to_string(),
            },
            RefDecision::Conflict {
                remote: "upstream".to_string(),
                refname: "refs/heads/dev".to_string(),
                reason: "diverged (non-fast-forward)".to_string(),
            },
            RefDecision::Conflict {
                remote: "mirror".to_string(),
                refname: "refs/heads/dev".to_string(),
                reason: "diverged (non-fast-forward)".to_string(),
            },
        ];
        let filtered = filter_mirror_decisions(decisions, &["mirror".to_string()]);
        assert_eq!(
            filtered,
            vec![
                RefDecision::Push {
                    remote: "mirror".to_string(),
                    refname: "refs/heads/main".to_string(),
                },
                RefDecision::Conflict {
                    remote: "mirror".to_string(),
                    refname: "refs/heads/dev".to_string(),
                    reason: "diverged (non-fast-forward)".to_string(),
                },
            ]
        );
    }

    #[test]
    fn error_summary_reports_config_errors_last() {
        let mut outcome = SyncOutcome {
            config_errors: vec!["mirror remote 'nosuch' not found".to_string()],
            ..Default::default()
        };
        assert_eq!(
            outcome.error_summary().unwrap(),
            "mirror remote 'nosuch' not found"
        );

        outcome
            .fetch_errors
            .push(("a".to_string(), "unreachable".to_string()));
        assert_eq!(outcome.error_summary().unwrap(), "fetch a: unreachable");
    }
}
