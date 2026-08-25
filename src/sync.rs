//! Mirror commits to the remotes of mirror-enabled repositories.
//!
//! Mirroring is opt-in per repo, toggled with `workset mirror init` or
//! `Ctrl+R` in the TUI. When enabled, all of the repo's remotes are mirror
//! targets: for every branch and tag, the newest id published to at least one
//! remote is propagated to the remotes that are behind or missing it — even
//! when the local checkout is behind or doesn't have the ref at all. Local
//! refs are never modified, and commits that exist only locally are never
//! pushed automatically. Repos with mirroring disabled are skipped entirely.
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
    /// Local commit (or tag object) id, if the ref exists locally
    pub local: Option<String>,
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
        /// Object id to push; not necessarily the local ref's id
        from: String,
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
    /// True when the repo has mirroring disabled and was not touched
    pub skipped: bool,
    /// True when no remote could be fetched, so the mirror state is unknown
    pub offline: bool,
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
/// The mirror-only rule: only ids already present on at least one remote are
/// propagated. Refs the user never pushed anywhere are left alone, and the
/// local ref is never modified — the newest published id flows between
/// remotes even when the local checkout is behind or lacks the ref.
pub fn plan_ref_sync(
    state: &RefState,
    ancestry: &mut dyn FnMut(&str, &str) -> Ancestry,
) -> Vec<RefDecision> {
    let refname = state.refname();
    let mut published = state.remotes.iter().filter_map(|(_, id)| id.as_deref());
    let Some(first_published) = published.next() else {
        // Never-published ref: never touched
        return Vec::new();
    };

    let mut decisions = Vec::new();
    match state.kind {
        RefKind::Branch => {
            // The candidate is the newest published id, preferring the local
            // id when it is itself published
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
                    && ancestry(candidate, id) == Ancestry::LocalBehind
                {
                    candidate = id;
                }
            }
            for (remote, id) in &state.remotes {
                match id.as_deref() {
                    None => decisions.push(RefDecision::Push {
                        remote: remote.clone(),
                        refname: refname.clone(),
                        from: candidate.to_string(),
                    }),
                    Some(id) if id == candidate => {}
                    Some(id) => match ancestry(candidate, id) {
                        Ancestry::LocalAhead => decisions.push(RefDecision::Push {
                            remote: remote.clone(),
                            refname: refname.clone(),
                            from: candidate.to_string(),
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
            // Tags are compared by identity only and never rewritten. A local
            // tag takes precedence; otherwise the first remote's id is the
            // one the others must match.
            let candidate = state.local.as_deref().unwrap_or(first_published);
            let published = state
                .remotes
                .iter()
                .any(|(_, id)| id.as_deref() == Some(candidate));
            for (remote, id) in &state.remotes {
                match id.as_deref() {
                    None if published => decisions.push(RefDecision::Push {
                        remote: remote.clone(),
                        refname: refname.clone(),
                        from: candidate.to_string(),
                    }),
                    None => {}
                    Some(id) if id == candidate => {}
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

/// Fetch all remotes, then mirror published refs to every remote of a
/// mirror-enabled repo. Repos with mirroring disabled are skipped without
/// touching the network.
///
/// Blocking; intended to run on a background thread. `interrupt` is checked
/// between git invocations and aborts the ones in flight.
pub fn sync_repo(repo_path: &Path, interrupt: &AtomicBool, dry_run: bool) -> Result<SyncOutcome> {
    let mut outcome = SyncOutcome::default();

    if !mirror_enabled(repo_path, interrupt)? {
        outcome.skipped = true;
        return Ok(outcome);
    }

    let remotes = list_remotes(repo_path, interrupt)?;
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
        // repo as failed in that case, but don't claim it's in sync either
        if fetched.is_empty() {
            outcome.fetch_errors.clear();
            outcome.offline = true;
        } else {
            mirror_refs(repo_path, &fetched, interrupt, dry_run, &mut outcome)?;
        }
    }

    let (status, modification_time) = crate::check_repo_status_and_modification_time(repo_path)
        .unwrap_or((crate::RepoStatus::NoCommits, None));
    outcome.status = Some(status);
    outcome.modification_time = modification_time;
    Ok(outcome)
}

/// Plan mirror pushes using the state of all fetched remotes, then execute
/// the resulting decisions
fn mirror_refs(
    repo_path: &Path,
    remotes: &[String],
    interrupt: &AtomicBool,
    dry_run: bool,
    outcome: &mut SyncOutcome,
) -> Result<()> {
    let ref_states = collect_ref_states(repo_path, remotes, interrupt)?;

    // Where each published id can be fetched from, for ids (remote-only tags)
    // that the regular fetch didn't bring into the local object database
    let mut sources: BTreeMap<String, (String, String)> = BTreeMap::new();
    for state in &ref_states {
        for (remote, id) in &state.remotes {
            if let Some(id) = id {
                sources
                    .entry(id.clone())
                    .or_insert_with(|| (remote.clone(), state.refname()));
            }
        }
    }

    let mut ancestry =
        |local: &str, other: &str| compare_ancestry(repo_path, local, other, interrupt);
    let mut pushes: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    for state in &ref_states {
        for decision in plan_ref_sync(state, &mut ancestry) {
            match decision {
                RefDecision::Push {
                    remote,
                    refname,
                    from,
                } => pushes.entry(remote).or_default().push((from, refname)),
                RefDecision::Conflict {
                    remote,
                    refname,
                    reason,
                } => outcome.conflicts.push((remote, refname, reason)),
            }
        }
    }

    for (remote, refspecs) in pushes {
        if dry_run {
            for (_, refname) in refspecs {
                outcome.would_push.push((remote.clone(), refname));
            }
            continue;
        }
        if interrupt.load(Ordering::Relaxed) {
            bail!("interrupted");
        }
        let mut ready = Vec::new();
        for (from, refname) in refspecs {
            match ensure_object_local(repo_path, &from, &sources, interrupt) {
                Ok(()) => ready.push((from, refname)),
                Err(e) => outcome
                    .push_errors
                    .push((remote.clone(), refname, e.to_string())),
            }
        }
        if !ready.is_empty() {
            push_refs(repo_path, &remote, &ready, interrupt, outcome);
        }
    }
    Ok(())
}

/// Make sure `id` exists in the local object database, fetching it from a
/// remote that has it if necessary. The fetch uses a refspec without a
/// destination so no local ref is created.
fn ensure_object_local(
    repo_path: &Path,
    id: &str,
    sources: &BTreeMap<String, (String, String)>,
    interrupt: &AtomicBool,
) -> Result<()> {
    let out = run_git(
        repo_path,
        &["cat-file", "-e", id],
        interrupt,
        LOCAL_TIMEOUT,
    )?;
    if out.status.success() {
        return Ok(());
    }
    let Some((remote, refname)) = sources.get(id) else {
        bail!("object {} not found locally or on any remote", id);
    };
    let out = run_git(
        repo_path,
        &["fetch", "--quiet", remote, refname],
        interrupt,
        NETWORK_TIMEOUT,
    )?;
    if !out.status.success() {
        bail!("fetch {} from {}: {}", refname, remote, stderr_summary(&out));
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

    // Branch names come from local branches and tracking refs alike, so a
    // branch that only exists on remotes still gets mirrored
    let mut branch_names: std::collections::BTreeSet<String> = branches.keys().cloned().collect();
    branch_names.extend(tracking.keys().map(|(_, name)| name.clone()));

    let mut states = Vec::new();
    for name in branch_names {
        let remote_ids = remotes
            .iter()
            .map(|r| (r.clone(), tracking.get(&(r.clone(), name.clone())).cloned()))
            .collect();
        states.push(RefState {
            local: branches.get(&name).cloned(),
            name,
            kind: RefKind::Branch,
            remotes: remote_ids,
        });
    }

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
    let mut tag_names: std::collections::BTreeSet<String> = tags.keys().cloned().collect();
    tag_names.extend(remote_tags.keys().map(|(_, name)| name.clone()));
    for name in tag_names {
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
            local: tags.get(&name).cloned(),
            name,
            kind: RefKind::Tag,
            remotes: remote_ids,
        });
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

/// Push a batch of (object id, destination refname) pairs to one remote,
/// classifying per-ref results
fn push_refs(
    repo_path: &Path,
    remote: &str,
    refs: &[(String, String)],
    interrupt: &AtomicBool,
    outcome: &mut SyncOutcome,
) {
    let refspecs: Vec<String> = refs
        .iter()
        .map(|(from, refname)| format!("{}:{}", from, refname))
        .collect();
    let mut args = vec!["push", "--porcelain", remote];
    args.extend(refspecs.iter().map(|s| s.as_str()));

    let out = match run_git(repo_path, &args, interrupt, NETWORK_TIMEOUT) {
        Ok(out) => out,
        Err(e) => {
            for (_, refname) in refs {
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
        for (_, refname) in refs {
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

/// Whether mirroring is enabled for this repo, via the boolean local git
/// config key `workset.mirror`. Unset or unparsable values read as disabled.
pub fn mirror_enabled(repo_path: &Path, interrupt: &AtomicBool) -> Result<bool> {
    let out = run_git(
        repo_path,
        &["config", "--local", "--get", "--type=bool", "workset.mirror"],
        interrupt,
        LOCAL_TIMEOUT,
    )?;
    Ok(out.status.success() && String::from_utf8_lossy(&out.stdout).trim() == "true")
}

/// Enable or disable mirroring for this repo. Disabling removes the config
/// key entirely; absent means disabled.
pub fn set_mirror_enabled(repo_path: &Path, enabled: bool) -> Result<()> {
    let interrupt = AtomicBool::new(false);
    if enabled {
        let out = run_git(
            repo_path,
            &["config", "--local", "--replace-all", "workset.mirror", "true"],
            &interrupt,
            LOCAL_TIMEOUT,
        )?;
        if !out.status.success() {
            bail!("git config --replace-all failed: {}", stderr_summary(&out));
        }
    } else {
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

    fn branch(local: Option<&str>, remotes: &[(&str, Option<&str>)]) -> RefState {
        RefState {
            name: "main".to_string(),
            kind: RefKind::Branch,
            local: local.map(|s| s.to_string()),
            remotes: remotes
                .iter()
                .map(|(r, id)| (r.to_string(), id.map(|s| s.to_string())))
                .collect(),
        }
    }

    fn tag(local: Option<&str>, remotes: &[(&str, Option<&str>)]) -> RefState {
        RefState {
            name: "v1".to_string(),
            kind: RefKind::Tag,
            ..branch(local, remotes)
        }
    }

    fn push(remote: &str, refname: &str, from: &str) -> RefDecision {
        RefDecision::Push {
            remote: remote.to_string(),
            refname: refname.to_string(),
            from: from.to_string(),
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
        let state = branch(Some("b"), &[("a", Some("b")), ("mirror", Some("a"))]);
        let decisions = plan_ref_sync(&state, &mut stub_ancestry(&["ab"]));
        assert_eq!(decisions, vec![push("mirror", "refs/heads/main", "b")]);
    }

    #[test]
    fn in_sync_branch_does_nothing() {
        let state = branch(Some("b"), &[("a", Some("b")), ("mirror", Some("b"))]);
        assert!(plan_ref_sync(&state, &mut stub_ancestry(&[])).is_empty());
    }

    #[test]
    fn local_only_branch_untouched() {
        let state = branch(Some("b"), &[("a", None), ("mirror", None)]);
        assert!(plan_ref_sync(&state, &mut stub_ancestry(&[])).is_empty());
    }

    #[test]
    fn unpushed_commits_never_auto_pushed() {
        // Local is ahead of every remote: the newest published id still
        // propagates, but the local-only commit "c" is never pushed
        let state = branch(Some("c"), &[("a", Some("b")), ("mirror", Some("a"))]);
        let decisions = plan_ref_sync(&state, &mut stub_ancestry(&["ab", "bc", "ac"]));
        assert_eq!(decisions, vec![push("mirror", "refs/heads/main", "b")]);
    }

    #[test]
    fn newer_id_on_one_remote_mirrored_to_others() {
        // A push from another machine left "a" ahead of both the local
        // branch and mirror: mirror receives it, local is left alone
        let state = branch(Some("b"), &[("a", Some("c")), ("mirror", Some("b"))]);
        let decisions = plan_ref_sync(&state, &mut stub_ancestry(&["bc"]));
        assert_eq!(decisions, vec![push("mirror", "refs/heads/main", "c")]);
    }

    #[test]
    fn remote_only_branch_propagated() {
        let state = branch(None, &[("a", Some("b")), ("mirror", None)]);
        let decisions = plan_ref_sync(&state, &mut stub_ancestry(&[]));
        assert_eq!(decisions, vec![push("mirror", "refs/heads/main", "b")]);
    }

    #[test]
    fn diverged_remote_reports_conflict() {
        let state = branch(Some("b"), &[("a", Some("b")), ("mirror", Some("x"))]);
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
    fn diverged_published_ids_conflict() {
        // Two remotes diverged from each other; the local id isn't published
        let state = branch(Some("a"), &[("a", Some("b")), ("mirror", Some("x"))]);
        let decisions = plan_ref_sync(&state, &mut stub_ancestry(&["ab"]));
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
        let state = branch(Some("b"), &[("a", Some("b")), ("mirror", None)]);
        let decisions = plan_ref_sync(&state, &mut stub_ancestry(&[]));
        assert_eq!(decisions, vec![push("mirror", "refs/heads/main", "b")]);
    }

    #[test]
    fn published_tag_mirrored_to_missing_remote() {
        let state = tag(Some("t"), &[("a", Some("t")), ("mirror", None)]);
        let decisions = plan_ref_sync(&state, &mut stub_ancestry(&[]));
        assert_eq!(decisions, vec![push("mirror", "refs/tags/v1", "t")]);
    }

    #[test]
    fn remote_only_tag_propagated() {
        let state = tag(None, &[("a", Some("t")), ("mirror", None)]);
        let decisions = plan_ref_sync(&state, &mut stub_ancestry(&[]));
        assert_eq!(decisions, vec![push("mirror", "refs/tags/v1", "t")]);
    }

    #[test]
    fn tag_mismatch_reports_conflict() {
        let state = tag(Some("t"), &[("a", Some("t")), ("mirror", Some("x"))]);
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
    fn tag_disagreement_between_remotes_conflicts() {
        let state = tag(None, &[("a", Some("t")), ("mirror", Some("x"))]);
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
        let state = tag(Some("t"), &[("a", None), ("mirror", None)]);
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

}
