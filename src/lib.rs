use anyhow::{Result, bail};
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;
use std::str::FromStr;
use tracing::{debug, warn};

pub mod sync;
#[cfg(feature = "tui")]
pub mod tui;

/// Represents a pattern that matches one or more repositories. It has the
/// format: [provider]/<path>.
#[derive(Debug, Eq, PartialEq)]
pub struct RepoPattern {
    /// The provider (e.g., "github.com", "gitlab.com")
    pub provider: Option<String>,

    /// The repo path
    pub path: String,
}

/// Rewrite the clone URL a forge hands out into the `[provider]/<path>` form a
/// pattern is written in.
///
/// Every forge's "clone" button offers a URL, so a URL is what gets pasted onto
/// the command line, and the pattern it means is spelled out inside it. Taking
/// it literally instead left `workset clone https://github.com/jqlang/jq`
/// failing with "No provider specified" while the provider sat in plain sight,
/// and turned the SSH form into the clone URL
/// `https://git@github.com:jqlang/jq.git`, which git then rejected for having an
/// invalid port number.
///
/// The transport (scheme, credentials, and the `:` the SSH form puts between
/// host and path) says nothing a pattern records, so it is dropped here rather
/// than becoming part of a repo's name in the workspace. A port is not
/// transport: it belongs to the provider, and the clone URL needs it back.
fn strip_url_syntax(pattern: &str) -> String {
    let pattern = pattern.trim();
    // https://, ssh://, git://, git+ssh://
    let pattern = pattern.split_once("://").map_or(pattern, |(_, rest)| rest);

    // The host is everything up to the first '/' — or, in the SSH form
    // `git@host:owner/repo`, up to the ':' inside that first component
    let (host, path) = pattern.split_once('/').unwrap_or((pattern, ""));
    let host = host.rsplit_once('@').map_or(host, |(_, host)| host);
    let host = match host.split_once(':') {
        // `host:1234` is a port and stays with the host; `host:owner` is the
        // SSH form's separator and becomes the '/' a pattern uses
        Some((host, rest)) if !rest.is_empty() && !rest.bytes().all(|b| b.is_ascii_digit()) => {
            format!("{}/{}", host, rest)
        }
        _ => host.to_string(),
    };

    let joined = if path.is_empty() {
        host
    } else {
        format!("{}/{}", host, path)
    };
    joined.trim_end_matches('/').to_string()
}

impl FromStr for RepoPattern {
    type Err = std::convert::Infallible;

    fn from_str(pattern: &str) -> std::result::Result<Self, Self::Err> {
        let pattern = strip_url_syntax(pattern);

        // If the first component looks like a domain (contains '.'), it's a provider
        Ok(match pattern.split_once('/') {
            Some((first, rest)) if first.contains('.') => Self {
                provider: Some(first.to_string()),
                // A clone URL's ".git" suffix names the repository on the
                // remote, not the directory it is checked out into. Left on, it
                // put `jq.git` in the workspace beside the `jq` the same repo
                // gets without the suffix, so the two spellings of one URL
                // cloned, dropped and restored as two unrelated repos. Only a
                // pattern that names a provider is URL-shaped; a bare local one
                // keeps whatever name it was given, so `drop` can still name a
                // directory that really does end in ".git".
                path: rest
                    .strip_suffix(".git")
                    .filter(|path| !path.is_empty())
                    .unwrap_or(rest)
                    .to_string(),
            },
            _ => Self {
                provider: None,
                path: pattern,
            },
        })
    }
}

impl RepoPattern {
    /// Get the provider and path as a tuple if provider exists
    pub fn provider_and_path(&self) -> Option<(&str, &str)> {
        self.provider
            .as_ref()
            .map(|p| (p.as_str(), self.path.as_str()))
    }

    /// Get the full path including provider if it exists
    pub fn full_path(&self) -> String {
        match &self.provider {
            Some(provider) => format!("{}/{}", provider, self.path),
            None => self.path.clone(),
        }
    }
}

/// Represents a git submodule within a repository
#[derive(Debug, Clone)]
pub struct SubmoduleInfo {
    /// Relative path within parent repo
    pub path: PathBuf,
    /// Whether submodule is checked out
    pub initialized: bool,
}

/// Whether this directory is a git directory itself rather than a worktree
/// with one in it: a bare repository, or the `.git` directory of a non-bare
/// one. Uses the same cheap test git does — a `HEAD`, an `objects` directory
/// and a `refs` directory — and stats `HEAD` first, because almost nothing
/// else has one and the other two cost nothing once it's missing.
fn is_git_dir(path: &Path) -> bool {
    path.join("HEAD").is_file() && path.join("objects").is_dir() && path.join("refs").is_dir()
}

/// Recursively find "top-level" git repositories.
/// This function will not traverse into .git directories or nested git repositories.
///
/// A directory that can't be read is skipped rather than reported, so the walk
/// always yields whatever it could reach.
pub fn find_git_repositories(path: &Path) -> Vec<PathBuf> {
    debug!(path = %path.display(), "Recursively searching for git repositories");

    // Check if this path itself is a git repository
    if path.join(".git").exists() {
        return vec![path.to_path_buf()]; // Don't traverse into git repositories
    }

    // A bare repository is a repository too, so the search stops here as
    // well. It isn't reported, because every caller wants the working set:
    // repos with a worktree to show a status for, to drop, to open a shell
    // in. A bare repo has none, which is why it was never reported before
    // either — but the search used to walk straight through it.
    //
    // Walking through one is what made every command pay for the library.
    // `drop` stores each repo as a bare git directory under `.workset/`, and
    // `.workset/` lives in the workspace, so the walk that enumerates the
    // working set descended through every library entry's whole object store
    // — a 256-way fanout per repo, thousands of directories that cannot hold
    // a workspace repo — to find nothing in any of them. The cost grew with
    // the library, which is the part of a workspace this tool exists to let
    // you grow.
    if is_git_dir(path) {
        return Vec::new();
    }

    let mut found: Vec<PathBuf> = Vec::new();

    // Otherwise, recursively search subdirectories. `read_dir` hands entries
    // back in whatever order the filesystem keeps them, which is neither
    // alphabetical nor the same on two machines holding the same repos. Every
    // caller shows this list to someone — `workset list`, the drop report, the
    // completion candidates — so walking in name order is what makes their
    // output the same everywhere, and a depth-first walk of sorted entries
    // comes out sorted by path.
    if let Ok(entries) = std::fs::read_dir(path) {
        let mut entries: Vec<_> = entries.filter_map(|e| e.ok()).collect();
        entries.sort_by_key(|entry| entry.file_name());

        for entry in entries {
            // Only traverse real directories. `file_type` doesn't follow
            // symlinks, so a symlinked directory is skipped: whatever it
            // points at lives somewhere else, and a repo found through one
            // would be reported under a workspace-relative path that doesn't
            // name where it actually is. Acting on that path — dropping it
            // into the library, deleting it — reaches back out through the
            // link and touches a repo the workspace never contained.
            if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                continue;
            }

            found.append(&mut find_git_repositories(&entry.path()));
        }
    }

    found
}

/// Whether a path read out of `.gitmodules` names a place inside the
/// repository.
///
/// `.gitmodules` is part of the repo's content, so whoever wrote the repo
/// decides what it says — cloning one is enough to get their text. Every
/// submodule it declares becomes a repo-shaped row in the TUI whose path is
/// the parent's joined with this one, and everything workset does to such a
/// row follows that path: the size walk, the `git diff`/`ls-files` the info
/// panel runs (which then reads each untracked file whole), the shell Enter
/// opens there. An absolute path replaces the parent's entirely when joined,
/// so a single `path = /` aims all of that at the whole filesystem, and `..`
/// walks anywhere a relative path can reach.
///
/// A path that merely spells out as relative proves nothing either: a symlink
/// in the worktree points wherever it likes, so where the path lands decides.
/// Nothing on disk yet means an uninitialized submodule, which has no link to
/// follow and is fine. git refuses out-of-tree submodule paths as well.
fn submodule_path_is_contained(repo_path: &Path, path: &Path) -> bool {
    if path.as_os_str().is_empty() {
        return false;
    }
    if !path
        .components()
        .all(|component| matches!(component, Component::Normal(_) | Component::CurDir))
    {
        return false;
    }
    match (
        std::fs::canonicalize(repo_path),
        std::fs::canonicalize(repo_path.join(path)),
    ) {
        (Ok(root), Ok(resolved)) => resolved
            .strip_prefix(&root)
            // An empty remainder is the repo root itself, not something in it
            .is_ok_and(|relative| !relative.as_os_str().is_empty()),
        // Not checked out, or the repo is gone: no link to follow
        _ => true,
    }
}

/// Find all submodules in a git repository by parsing the .gitmodules file.
///
/// Declared paths that don't stay inside the repository are skipped; see
/// [`submodule_path_is_contained`].
pub fn find_submodules_in_repo(repo_path: &Path) -> Result<Vec<SubmoduleInfo>> {
    let gitmodules_path = repo_path.join(".gitmodules");

    // If .gitmodules doesn't exist, return empty vec
    if !gitmodules_path.exists() {
        return Ok(Vec::new());
    }

    let content = std::fs::read_to_string(&gitmodules_path)?;
    let mut submodules = Vec::new();

    // Simple parser for .gitmodules INI format
    let mut current_path: Option<PathBuf> = None;

    // Emit a submodule once its section has yielded a path
    let mut flush = |path: &mut Option<PathBuf>| {
        if let Some(path) = path.take() {
            if !submodule_path_is_contained(repo_path, &path) {
                warn!(
                    repo = %repo_path.display(),
                    submodule = %path.display(),
                    "Ignoring submodule declared outside the repository"
                );
                return;
            }
            let initialized = repo_path.join(&path).join(".git").exists();
            submodules.push(SubmoduleInfo { path, initialized });
        }
    };

    for line in content.lines() {
        let line = line.trim();

        // Skip empty lines and comments
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }

        // Parse [submodule "name"] section headers
        if line.starts_with('[') && line.ends_with(']') {
            // Save the previous submodule before starting a new section
            flush(&mut current_path);
            continue;
        }

        // Parse key = value lines
        if let Some(eq_pos) = line.find('=') {
            let key = line[..eq_pos].trim();
            let value = line[eq_pos + 1..].trim();

            if key == "path" {
                current_path = Some(PathBuf::from(value));
            }
        }
    }

    // Don't forget the last submodule
    flush(&mut current_path);

    Ok(submodules)
}

/// Return the config text with core.bare set to the given value, adding the
/// [core] section or the bare entry if missing
fn set_core_bare(config: &str, bare: bool) -> String {
    if config.contains("[core]") {
        if config.contains("bare =") || config.contains("bare=") {
            config
                .lines()
                .map(|line| {
                    if line.trim().starts_with("bare") {
                        format!("\tbare = {}", bare)
                    } else {
                        line.to_string()
                    }
                })
                .collect::<Vec<_>>()
                .join("\n")
        } else {
            config.replacen("[core]", &format!("[core]\n\tbare = {}", bare), 1)
        }
    } else {
        format!("{}\n[core]\n\tbare = {}\n", config, bare)
    }
}

/// Clone a repository (from a remote URL or a local path) into the given
/// destination directory, creating parent directories as needed
pub fn gix_clone(url: &str, dest: &Path) -> Result<gix::Repository> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let mut prepare_fetch = gix::clone::PrepareFetch::new(
        url,
        dest,
        gix::create::Kind::WithWorktree,
        gix::create::Options::default(),
        gix::open::Options::isolated(),
    )?;
    let should_interrupt = std::sync::atomic::AtomicBool::new(false);
    let (mut prepare_checkout, _) =
        prepare_fetch.fetch_then_checkout(gix::progress::Discard, &should_interrupt)?;
    let (repo, _) = prepare_checkout.main_worktree(gix::progress::Discard, &should_interrupt)?;

    Ok(repo)
}

/// Tree-grouping path derived from a git remote URL, e.g.
/// "git@github.com:fossable/workset.git" -> "github.com/fossable/workset".
/// None for URLs without a host (local paths, file://).
pub fn url_tree_path(url: &str) -> Option<String> {
    let url = url.trim();
    let (host, path) = if let Some((scheme, rest)) = url.split_once("://") {
        if !matches!(scheme, "http" | "https" | "ssh" | "git") {
            return None;
        }
        rest.split_once('/')?
    } else {
        // scp-like: [user@]host:path — the colon must come before any slash
        let (host, path) = url.split_once(':')?;
        if host.contains('/') {
            return None;
        }
        (host, path)
    };

    let host = host.rsplit_once('@').map_or(host, |(_, h)| h);
    let host = host
        .rsplit_once(':')
        .filter(|(_, port)| !port.is_empty() && port.chars().all(|c| c.is_ascii_digit()))
        .map_or(host, |(h, _)| h);

    let path = path.trim_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let path = path.trim_end_matches('/');
    if host.is_empty() || path.is_empty() {
        return None;
    }
    Some(format!("{}/{}", host, path))
}

/// Tree-grouping path from the repo's origin remote (or its alphabetically
/// first remote when origin is absent), read via gix without spawning git.
/// None when the repo has no remotes or the URL has no host.
pub fn remote_tree_path(repo_path: &Path) -> Option<String> {
    let repo = gix::open_opts(repo_path, gix::open::Options::isolated()).ok()?;
    let config = repo.config_snapshot();
    let url = config
        .string("remote.origin.url")
        .or_else(|| {
            let name = repo.remote_names().into_iter().next()?;
            config.string(format!("remote.{}.url", name).as_str())
        })?
        .to_string();
    url_tree_path(&url)
}

/// Repository status information
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepoStatus {
    /// Repository is clean (has commits, no changes, no unpushed)
    Clean,
    /// Repository has uncommitted changes or untracked files
    Dirty,
    /// Repository has no commits yet, and nothing uncommitted either: there is
    /// no work in it that only exists here. A commit-less repo that does hold
    /// uncommitted work is [`Dirty`](Self::Dirty), because that is the fact
    /// that matters to anything destructive.
    NoCommits,
    /// Repository has unpushed commits (but is otherwise clean)
    Unpushed,
    /// The repository couldn't be read, so what it holds is unknown. It may be
    /// full of work that exists nowhere else.
    ///
    /// gix doesn't read every repository git can write: a repo created with
    /// `--object-format=sha256` has a config gix refuses outright, and one
    /// created with `--ref-format=reftable` (git 2.45+) opens but yields
    /// nothing for HEAD or for the worktree status. Both are ordinary
    /// repositories to git, and both land here.
    ///
    /// This is deliberately *not* [`NoCommits`](Self::NoCommits): reporting an
    /// unreadable repo as empty told `drop` that moving its git directory
    /// away and deleting its worktree lost nothing, and it deleted
    /// uncommitted work that existed nowhere else.
    Unknown,
}

/// Why a repository was left in the workspace instead of being dropped
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropBlocker {
    /// Work that exists nowhere but this worktree, which `--force` discards
    Outstanding(RepoStatus),

    /// The repo doesn't own its git directory: `.git` is a file holding
    /// `gitdir: <path>`, which is how git writes a linked worktree (`git
    /// worktree add`) and a submodule. The library holds git directories, so
    /// there is none here to store, and deleting the directory would leave the
    /// repository that does own it holding a registration for a worktree
    /// that's gone.
    BorrowedGitDir,

    /// The main worktree of this many live linked worktrees. Its git directory
    /// is the one they all point at, so moving it into the library (or
    /// deleting it) leaves every one of them pointing at a gitdir that isn't
    /// there any more.
    MainWorktree(usize),
}

impl DropBlocker {
    /// What the user should do about it, if anything can be done at all
    pub fn remedy(&self) -> &'static str {
        match self {
            Self::Outstanding(_) => "use --force to drop anyway",
            Self::BorrowedGitDir => "drop that repository instead",
            Self::MainWorktree(1) => "remove it with 'git worktree remove' first",
            Self::MainWorktree(_) => "remove them with 'git worktree remove' first",
        }
    }
}

impl std::fmt::Display for DropBlocker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Outstanding(RepoStatus::Dirty) => f.write_str("uncommitted changes"),
            Self::Outstanding(RepoStatus::Unpushed) => f.write_str("unpushed commits"),
            Self::Outstanding(RepoStatus::Unknown) => f.write_str("unreadable repository"),
            // drop only ever blocks on the three statuses above
            Self::Outstanding(_) => f.write_str("outstanding changes"),
            Self::BorrowedGitDir => f.write_str("git directory belongs to another repository"),
            Self::MainWorktree(1) => f.write_str("git directory shared with 1 linked worktree"),
            Self::MainWorktree(count) => {
                write!(f, "git directory shared with {} linked worktrees", count)
            }
        }
    }
}

/// The outcome of a drop, so the caller can report it to the user
#[derive(Debug, Default)]
pub struct DropReport {
    /// Workspace-relative paths of the repos that were dropped
    pub dropped: Vec<String>,

    /// Repos left where they are, each with what blocked the drop
    pub skipped: Vec<(String, DropBlocker)>,
}

impl DropReport {
    /// Whether nothing was dropped and nothing was skipped, which means the
    /// pattern matched no repository at all
    pub fn is_empty(&self) -> bool {
        self.dropped.is_empty() && self.skipped.is_empty()
    }
}

/// Whether `git worktree` has left this repo in a shape a drop would damage,
/// either because its git directory isn't its own or because other worktrees
/// are using it.
///
/// `find_git_repositories` reports anything with a `.git` in it, and the two
/// ends of a `git worktree add` both qualify, so both end up as repos of the
/// workspace that `drop` will act on. Neither is a repo the library can hold
/// on its own: one half has no git directory and the other half's git
/// directory is shared.
fn worktree_blocker(repo: &Path) -> Option<DropBlocker> {
    // A repo that owns its history has a `.git` directory. A linked worktree
    // and a submodule have a `.git` *file* holding `gitdir: <path>` instead,
    // and that path is inside the repository the git directory belongs to.
    // Storing one in the library reads `<worktree>/.git/config` through that
    // file and fails with a bare ENOTDIR, which in a `workset drop` with no
    // pattern abandoned the whole request; `--delete` skips the library and
    // deletes the worktree while the owner keeps its registration.
    let git_path = repo.join(".git");
    if git_path.is_file() {
        return Some(DropBlocker::BorrowedGitDir);
    }

    // The other half: the git directory under a main worktree is the one every
    // linked worktree reads its objects and refs from. Moving it into the
    // library breaks each of them in place — `git status` there reports "not a
    // git repository", and because nothing can be read out of them any more
    // they turn into "no commits" rows, the status that tells `drop` a repo is
    // empty and safe to consume.
    match live_linked_worktrees(&git_path) {
        0 => None,
        count => Some(DropBlocker::MainWorktree(count)),
    }
}

/// How many linked worktrees are still using the given git directory.
///
/// git registers each one as `<git-dir>/worktrees/<name>/`, whose `gitdir`
/// file holds the path of that worktree's own `.git` file. Once the worktree
/// is gone, so is the path in it: that's the stale registration `git worktree
/// prune` collects, and nothing points here through it, so only the live ones
/// are counted.
fn live_linked_worktrees(git_dir: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(git_dir.join("worktrees")) else {
        return 0;
    };
    entries
        .filter_map(|entry| entry.ok())
        .filter(|entry| {
            std::fs::read_to_string(entry.path().join("gitdir"))
                .is_ok_and(|gitdir| Path::new(gitdir.trim()).exists())
        })
        .count()
}

/// Scan every repo in `repos` with `scan` on a pool of worker threads, and
/// return what `scan` produced in the order the repos were given.
///
/// Scanning a repo means walking its whole worktree: the index-to-worktree
/// comparison every status check starts with stats each tracked file, and the
/// directory walk that finds untracked files reads each directory. That is the
/// bulk of the work behind `workset list`, `workset status` and the shell
/// completions, and done one repo after another it takes the sum of every
/// repo's walk while the rest of the machine idles. The walks don't depend on
/// each other at all — each one reads one repo and writes nothing.
///
/// Each worker is also told how many threads it may use for its own walk, so
/// the walks share the cores rather than each one claiming all of them: gix
/// spreads a single repo's comparison over every core by default, which is
/// right for the one-repo workspace and, with a worker per core, means cores²
/// threads spawned for the work of one.
pub fn scan_repos<T, F>(repos: &[PathBuf], scan: F) -> Vec<T>
where
    F: Fn(&Path) -> T + Send + Sync,
    T: Send,
{
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
    let workers = cores.min(repos.len());
    if workers <= 1 {
        return repos.iter().map(|repo| scan(repo)).collect();
    }

    let next = std::sync::atomic::AtomicUsize::new(0);
    let scan = &scan;
    let mut scanned: Vec<(usize, T)> = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..workers)
            .map(|_| {
                let next = &next;
                scope.spawn(move || {
                    limit_status_walk_threads(cores / workers);
                    let mut scanned = Vec::new();
                    loop {
                        let index = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let Some(repo) = repos.get(index) else {
                            break;
                        };
                        scanned.push((index, scan(repo)));
                    }
                    scanned
                })
            })
            .collect();
        workers
            .into_iter()
            .flat_map(|worker| match worker.join() {
                Ok(scanned) => scanned,
                // A worker that panicked scanned an unknown subset, so there is
                // no partial answer to report: fail the way the serial scan
                // this replaced would have.
                Err(panic) => std::panic::resume_unwind(panic),
            })
            .collect()
    });

    // Callers show this list to someone, and the order they gave is the order
    // they want it back in — the walks finish in whatever order the repos
    // happen to take
    scanned.sort_by_key(|(index, _)| *index);
    scanned.into_iter().map(|(_, scanned)| scanned).collect()
}

thread_local! {
    /// How many threads a status walk on this thread may use, or 0 to let gix
    /// decide (one per core). See [`scan_repos`].
    static STATUS_WALK_THREADS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Limit the status walks run on this thread to `threads` threads of their
/// own, because this thread is sharing the machine with other repo scans.
pub fn limit_status_walk_threads(threads: usize) {
    STATUS_WALK_THREADS.set(threads);
}

/// Check repository status (commits, changes, unpushed) in a single pass
pub fn check_repo_status(repo_path: &Path) -> Result<RepoStatus> {
    let Some(repo) = open_repo(repo_path) else {
        return Ok(RepoStatus::Unknown);
    };
    // Dirtiness is checked before the first commit is looked for, because a
    // repo without commits still holds whatever is staged or untracked in its
    // worktree, and that work exists nowhere but here: no commit, no remote,
    // nothing the library would keep. Reporting it as NoCommits told `drop` it
    // was safe to move the git directory away and delete the worktree.
    match worktree_is_dirty(&repo, repo_path) {
        Some(true) => return Ok(RepoStatus::Dirty),
        // A worktree that couldn't be read is not a worktree without changes
        None => return Ok(RepoStatus::Unknown),
        Some(false) => {}
    }
    Ok(match head_state(&repo) {
        HeadState::Unborn => RepoStatus::NoCommits,
        HeadState::Unreadable => RepoStatus::Unknown,
        HeadState::Detached => RepoStatus::Clean,
        HeadState::Branch(head_ref) => check_unpushed_status(&repo, head_ref),
    })
}

/// Check repository status and get modification time in a single repo open
/// and a single worktree scan
pub fn check_repo_status_and_modification_time(
    repo_path: &Path,
) -> Result<(RepoStatus, Option<std::time::SystemTime>)> {
    let Some(repo) = open_repo(repo_path) else {
        return Ok((RepoStatus::Unknown, None));
    };

    let (readable, dirty_time) = dirty_files_time(&repo, repo_path);
    if !readable {
        // Nothing may conclude from an unreadable worktree that it holds no
        // work, least of all `drop`
        return Ok((RepoStatus::Unknown, None));
    }

    if let Some(dirty_time) = dirty_time {
        // For dirty repos, use the max of last commit time and dirty file
        // times. A repo without commits has no commit time, only the files'.
        let commit_time = get_last_commit_time(&repo).unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        return Ok((RepoStatus::Dirty, Some(commit_time.max(dirty_time))));
    }

    let status = match head_state(&repo) {
        HeadState::Unborn => {
            // No commits and a clean worktree: no timestamp to report at all,
            // so callers don't render a spurious "56y ago" from the UNIX epoch.
            return Ok((RepoStatus::NoCommits, None));
        }
        HeadState::Unreadable => return Ok((RepoStatus::Unknown, None)),
        HeadState::Detached => RepoStatus::Clean,
        HeadState::Branch(head_ref) => check_unpushed_status(&repo, head_ref),
    };
    Ok((status, get_last_commit_time(&repo).ok()))
}

/// Open a repository, logging and returning None when it can't be opened
fn open_repo(repo_path: &Path) -> Option<gix::Repository> {
    match gix::open(repo_path) {
        Ok(repo) => Some(repo),
        Err(e) => {
            warn!(
                path = %repo_path.display(),
                error = %e,
                "Failed to open repository"
            );
            None
        }
    }
}

/// Where a repository's HEAD points
enum HeadState<'repo> {
    /// At a branch, which may have an upstream to compare against
    Branch(gix::Reference<'repo>),
    /// Straight at a commit, as during a bisect, a rebase or after an
    /// explicit `git checkout --detach`. There is history, but no branch and
    /// therefore no upstream.
    Detached,
    /// At a branch that doesn't exist yet, because the repo has no commits
    Unborn,
    /// HEAD couldn't be read, so where it points is unknown. A repo whose refs
    /// gix can't read (reftable) lands here, and it is no more "no commits"
    /// than it is clean.
    Unreadable,
}

/// Classify what HEAD points at. Detached is kept apart from Unborn because
/// the two look alike through `try_into_referent` — it yields no reference
/// either way — and calling a repo full of history "no commits" is a lie the
/// status line, the summary counts and the TUI all repeat.
fn head_state(repo: &gix::Repository) -> HeadState<'_> {
    let Ok(head) = repo.head() else {
        return HeadState::Unreadable;
    };
    if head.is_unborn() {
        return HeadState::Unborn;
    }
    match head.try_into_referent() {
        Some(head_ref) => HeadState::Branch(head_ref),
        None => HeadState::Detached,
    }
}

/// Classify a repository with no uncommitted changes as Clean or Unpushed
fn check_unpushed_status(repo: &gix::Repository, head_ref: gix::Reference<'_>) -> RepoStatus {
    let local_branch = head_ref.name();
    let remote_ref_name =
        match repo.branch_remote_tracking_ref_name(local_branch, gix::remote::Direction::Fetch) {
            Some(Ok(name)) => name,
            Some(Err(e)) => {
                debug!(error = %e, "Failed to get remote tracking ref");
                return RepoStatus::Clean;
            }
            None => {
                debug!("No upstream branch configured");
                return RepoStatus::Clean;
            }
        };

    // Try to find the remote ref
    let has_unpushed = match repo.find_reference(remote_ref_name.as_ref()) {
        Ok(remote_ref) => {
            let local_commit = match head_ref.id().object() {
                Ok(obj) => obj.id,
                Err(e) => {
                    warn!(error = %e, "Failed to get local commit");
                    return RepoStatus::Clean;
                }
            };

            let remote_commit = match remote_ref.id().object() {
                Ok(obj) => obj.id,
                Err(e) => {
                    warn!(error = %e, "Failed to get remote commit");
                    return RepoStatus::Clean;
                }
            };

            local_commit != remote_commit
        }
        Err(_) => {
            debug!("Remote ref not found, assuming no unpushed commits");
            false
        }
    };

    if has_unpushed {
        RepoStatus::Unpushed
    } else {
        RepoStatus::Clean
    }
}

/// Format a SystemTime as a human-readable "time ago" string
pub fn format_time_ago(time: std::time::SystemTime) -> String {
    let elapsed = match std::time::SystemTime::now().duration_since(time) {
        Ok(d) => d,
        Err(_) => {
            // Time is in the future, should not happen
            return "just now".to_string();
        }
    };

    let seconds = elapsed.as_secs();

    if seconds < 60 {
        format!("{}s", seconds)
    } else if seconds < 3600 {
        // Under 1 hour: show minutes (rounded)
        let minutes = (seconds + 30) / 60; // Round to nearest minute
        format!("{}m", minutes)
    } else if seconds < 86400 {
        // Under 1 day: show hours (rounded)
        let hours = (seconds + 1800) / 3600; // Round to nearest hour
        format!("{}h", hours)
    } else if seconds < 2_592_000 {
        // Under 30 days: show days (rounded)
        let days = (seconds + 43200) / 86400; // Round to nearest day
        format!("{}d", days)
    } else if seconds < 31_536_000 {
        // Under 1 year: show months (rounded)
        let months = (seconds + 1_296_000) / 2_592_000; // Round to nearest month
        format!("{}mo", months)
    } else {
        // Over 1 year: show years (rounded)
        let years = (seconds + 15_768_000) / 31_536_000; // Round to nearest year
        format!("{}y", years)
    }
}

/// Get the last modification time for a repository (its last commit time).
/// Use check_repo_status_and_modification_time to also account for dirty files.
pub fn get_repo_modification_time(repo_path: &Path) -> Result<std::time::SystemTime> {
    let repo = gix::open(repo_path)?;
    get_last_commit_time(&repo)
}

/// Get the last commit time using gix
fn get_last_commit_time(repo: &gix::Repository) -> Result<std::time::SystemTime> {
    if matches!(head_state(repo), HeadState::Unborn) {
        bail!("Repository has no commits");
    }

    // head_commit resolves a detached HEAD too, so a repo mid-bisect or
    // mid-rebase still reports when it last changed
    let commit = repo.head_commit()?;
    let timestamp = commit.time()?.seconds;

    Ok(std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(timestamp as u64))
}

/// Walk the repository's changed and untracked files in a single status pass,
/// calling `visit` with the worktree path of each, or None when git reports a
/// path this platform can't represent. Stops as soon as `visit` returns false,
/// so callers that only need to know whether the worktree is dirty don't pay
/// for the rest of the walk.
///
/// Returns false when the walk couldn't be completed, in which case `visit`
/// never saw the changes the worktree may well hold. Callers must not read
/// that as a clean worktree: that is what let `drop` delete work it couldn't
/// see.
#[must_use]
fn walk_worktree_changes(
    repo: &gix::Repository,
    repo_path: &Path,
    mut visit: impl FnMut(Option<PathBuf>) -> bool,
) -> bool {
    let platform = match repo.status(gix::progress::Discard) {
        Ok(p) => p,
        Err(e) => {
            warn!(
                path = %repo_path.display(),
                error = %e,
                "Failed to create status platform"
            );
            return false;
        }
    };

    // One walk per core is right when this is the only walk running; it isn't
    // when a pool of workers is scanning a repo each, and then each walk takes
    // the share of the cores its worker was given.
    let platform = match STATUS_WALK_THREADS.get() {
        0 => platform,
        threads => platform.index_worktree_options_mut(|options| {
            options.thread_limit = Some(threads);
        }),
    };

    // Tracked changes and untracked files come from the same iterator
    let iter = match platform
        .untracked_files(gix::status::UntrackedFiles::Files)
        .into_index_worktree_iter(Vec::new())
    {
        Ok(iter) => iter,
        Err(e) => {
            warn!(
                path = %repo_path.display(),
                error = %e,
                "Failed to check for changes"
            );
            return false;
        }
    };

    for item in iter {
        // An item that failed is a file whose state we don't know, so the walk
        // can no longer vouch for the whole worktree
        let item = match item {
            Ok(item) => item,
            Err(e) => {
                warn!(
                    path = %repo_path.display(),
                    error = %e,
                    "Failed to check a worktree entry for changes"
                );
                return false;
            }
        };

        // An entry whose path this platform can't represent is still a change,
        // so hand it to `visit` unnamed rather than dropping it and risking a
        // dirty worktree reported as clean
        let path = match gix::path::from_bstr(item.rela_path()) {
            Ok(rela_path) => Some(repo_path.join(rela_path)),
            Err(e) => {
                warn!(
                    path = %repo_path.display(),
                    rela_path = %item.rela_path(),
                    error = %e,
                    "Changed file has a path that can't be represented"
                );
                None
            }
        };

        if !visit(path) {
            break;
        }
    }
    true
}

/// Whether the worktree holds any uncommitted change or untracked file, or
/// None when it couldn't be read and the answer is unknown
fn worktree_is_dirty(repo: &gix::Repository, repo_path: &Path) -> Option<bool> {
    let mut dirty = false;
    let readable = walk_worktree_changes(repo, repo_path, |_| {
        dirty = true;
        false
    });
    readable.then_some(dirty)
}

/// The most recent modification time among the changed and untracked files,
/// or None when the worktree is clean. Files that can't be stat'd count as the
/// UNIX epoch, so a dirty worktree always yields some time.
///
/// The flag is false when the worktree couldn't be read, in which case the
/// time says nothing about what the worktree holds.
fn dirty_files_time(
    repo: &gix::Repository,
    repo_path: &Path,
) -> (bool, Option<std::time::SystemTime>) {
    let mut latest = None;
    let readable = walk_worktree_changes(repo, repo_path, |file_path| {
        let modified = file_path
            .and_then(|file_path| {
                std::fs::metadata(&file_path)
                    .and_then(|metadata| metadata.modified())
                    .ok()
            })
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        latest = latest.max(Some(modified));
        true
    });
    (readable, latest)
}

/// Delete the directories a repo's workspace path leaves behind once the repo
/// itself is gone, walking up from `repo` and stopping at the workspace root,
/// at the library, or at the first directory that still holds something.
///
/// A repo sits at the path its remote dictates, so the directories above it
/// exist only to hold it: dropping `github.com/user/repo` left `github.com` and
/// `github.com/user` behind with nothing in them, and a clone that failed left
/// the pair it had just made for a repo that never arrived. Nothing ever
/// collected those, so a workspace accumulated an empty tree mirroring every
/// repo it had held and every name the user had mistyped — the clutter a drop
/// exists to clear.
///
/// Tidying up is not worth pulling the ground out from under the caller's
/// shell, so the current directory and everything above it is left alone:
/// `cd github.com/user && workset drop project` empties the directory it is
/// standing in rather than removing it.
fn prune_empty_dirs(root: &Path, repo: &Path) {
    let library = root.join(".workset");
    let cwd = std::env::current_dir().ok();
    let mut candidate = Some(repo);

    while let Some(dir) = candidate {
        // Never the root or above it, and never the library: an empty library
        // is still the directory that makes this a workspace
        if dir == root || dir == library.as_path() || !dir.starts_with(root) {
            break;
        }

        // Nor the current directory or any of its parents, which the caller is
        // still standing in
        if cwd.as_deref().is_some_and(|cwd| cwd.starts_with(dir)) {
            break;
        }

        match std::fs::remove_dir(dir) {
            Ok(()) => {}
            // Already gone, which the repo's own directory normally is by the
            // time a drop gets here: keep walking up to the parents it emptied
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            // A directory that still holds something, or one that isn't ours
            // to remove, is where the walk ends
            Err(_) => break,
        }

        candidate = dir.parent();
    }
}

/// A `Workspace` is filesystem directory containing git repositories checked out
/// from one or more providers. Each repository's path matches the remote's path,
/// for example:
///     <workspace path>/github.com/fossable/workset
///
/// Workspace root is identified by the presence of a .workset/ directory.
#[derive(Clone, Debug)]
pub struct Workspace {
    /// The workspace directory's filesystem path
    pub path: String,
}

impl Workspace {
    /// Get the library path for this workspace
    pub fn library_path(&self) -> String {
        format!("{}/.workset", self.path)
    }

    /// Get the library path as a PathBuf (avoids allocation)
    fn library_path_buf(&self) -> PathBuf {
        PathBuf::from(&self.path).join(".workset")
    }

    /// Render a repo's path relative to the workspace root as a display string,
    /// falling back to the full path if it isn't under the workspace.
    pub fn relative_name(&self, repo: &Path) -> String {
        repo.strip_prefix(&self.path)
            .unwrap_or(repo)
            .display()
            .to_string()
    }

    /// Locate the workspace containing the current directory without validating
    /// it or touching the filesystem beyond the directory search.
    ///
    /// Suitable for read-only, side-effect-free callers such as shell
    /// completion, which must be fast and infallible. Use [`load`](Self::load)
    /// when the workspace is about to be operated on.
    pub fn discover() -> Option<Self> {
        let mut workspace_root = std::env::current_dir().ok()?;

        // Search up for a .workset/ directory
        loop {
            if workspace_root.join(".workset").is_dir() {
                let workspace = Workspace {
                    path: workspace_root.display().to_string(),
                };
                debug!(workspace_path = %workspace.path, "Found workspace");
                return Some(workspace);
            }

            // Try parent directory
            workspace_root = workspace_root.parent()?.to_path_buf();
        }
    }

    /// Load workspace from current directory, validating it and ensuring the
    /// library directory exists.
    pub fn load() -> Result<Option<Self>> {
        match Self::discover() {
            Some(workspace) => {
                // Validate the workspace configuration
                workspace.validate()?;

                // Make sure library directory exists
                std::fs::create_dir_all(workspace.library_path_buf())
                    .map_err(|e| anyhow::anyhow!("Failed to create library directory: {}", e))?;

                Ok(Some(workspace))
            }
            None => Ok(None),
        }
    }

    /// Validate the workspace configuration
    fn validate(&self) -> Result<()> {
        // Check if workspace path exists
        if !Path::new(&self.path).exists() {
            bail!("Workspace path does not exist: {}", self.path);
        }

        Ok(())
    }

    /// The path a pattern names inside the workspace, or an error when the
    /// pattern doesn't name anything the workspace contains.
    ///
    /// Patterns are workspace-relative by definition, so an absolute one, or
    /// one with a `..` component, points outside the workspace. Joining those
    /// onto the root anyway escaped it: `workset drop ../repo` matched a repo
    /// the workspace never contained and consumed it, deleting the worktree
    /// and moving the git directory somewhere `restore` could never find it.
    pub fn repo_path(&self, pattern: &RepoPattern) -> Result<PathBuf> {
        let relative = PathBuf::from(pattern.full_path());
        if relative.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        }) {
            bail!("'{}' is outside the workspace", pattern.full_path());
        }
        Ok(Path::new(&self.path).join(relative))
    }

    /// Search the workspace for local repos matching the given pattern.
    ///
    /// Patterns are always interpreted relative to the workspace root; see
    /// [`resolve_pattern`](Self::resolve_pattern) for turning a pattern the
    /// user typed relative to their current directory into one of these.
    pub fn search(&self, pattern: &RepoPattern) -> Result<Vec<PathBuf>> {
        Ok(find_git_repositories(&self.repo_path(pattern)?))
    }

    /// Rewrite a pattern that names a path under `cwd` into a workspace-relative
    /// one.
    ///
    /// The CLI documents patterns as relative to the current directory
    /// (`workset drop ./repo`), but [`search`](Self::search) only ever looks
    /// workspace-relative, so `./repo` — and a bare `repo` — matched nothing
    /// whenever the current directory wasn't the workspace root. Patterns that
    /// don't name an existing path inside the workspace are returned unchanged,
    /// so a full workspace-relative pattern keeps working from any directory.
    pub fn resolve_pattern(&self, cwd: &Path, pattern: &str) -> String {
        let resolved = Path::new(&self.path)
            .canonicalize()
            .and_then(|root| Ok((root, cwd.join(pattern).canonicalize()?)))
            .ok()
            .and_then(|(root, target)| {
                let relative = target.strip_prefix(root).ok()?;
                // The workspace root itself isn't a pattern for any repo
                (!relative.as_os_str().is_empty()).then(|| relative.display().to_string())
            });

        resolved.unwrap_or_else(|| pattern.to_string())
    }

    /// Clone/open a repository in this workspace
    pub fn open(&self, pattern: &RepoPattern) -> Result<PathBuf> {
        debug!(pattern = ?pattern, "Opening repos");

        // First check if repository already exists locally
        let local_repos = self.search(pattern)?;

        if !local_repos.is_empty() {
            return Ok(local_repos[0].clone());
        }

        // Check library and restore if found
        let relative_path = pattern.full_path();
        let repo_path = format!("{}/{}", self.path, relative_path);

        if self.library_contains(&relative_path) {
            self.restore_from_library(&relative_path)?;
            // TODO: fetch latest changes from upstream once the gix API is clearer
            return Ok(PathBuf::from(repo_path));
        }

        // Try to clone from remotes
        let repo_path = self.clone_from_remote(pattern)?;
        Ok(repo_path)
    }

    /// Drop the repositories matching a pattern from this workspace
    pub fn drop(&self, pattern: &RepoPattern, delete: bool, force: bool) -> Result<DropReport> {
        debug!("Drop requested for pattern: {:?}", pattern);

        let mut report = DropReport::default();
        for repo in self.search(pattern)? {
            self.drop_repo(&repo, delete, force, &mut report)?;
        }
        Ok(report)
    }

    /// Drop all repositories in the current directory
    pub fn drop_all(&self, delete: bool, force: bool) -> Result<DropReport> {
        debug!("Drop all requested in current directory");

        let cwd = std::env::current_dir()?;
        let mut report = DropReport::default();
        for repo in find_git_repositories(&cwd) {
            self.drop_repo(&repo, delete, force, &mut report)?;
        }
        Ok(report)
    }

    /// Drop a single repository: store it in the library (unless deleting) and
    /// remove it from the workspace, recording the outcome in `report`. A repo
    /// with uncommitted or unpushed changes is left alone unless `force`.
    fn drop_repo(
        &self,
        repo: &Path,
        delete: bool,
        force: bool,
        report: &mut DropReport,
    ) -> Result<()> {
        // Everything below moves or deletes the directory, so a path that
        // isn't under the workspace root must never get this far: the library
        // can only hold repos the workspace contains, and workset has no
        // business removing anything else.
        //
        // Containment is decided on canonical paths. A path that merely spells
        // out as workspace-relative proves nothing: `..` walks straight out,
        // and a symlink anywhere along the way points wherever it likes while
        // still reading as a path under the root. Resolving both ends settles
        // where the directory about to be moved or deleted really is.
        let root = std::fs::canonicalize(&self.path).unwrap_or_else(|_| PathBuf::from(&self.path));
        let relative = std::fs::canonicalize(repo)
            .ok()
            .and_then(|resolved| Some(resolved.strip_prefix(&root).ok()?.to_path_buf()))
            // An empty relative path is the workspace root itself, which is
            // not a repo to drop even when it happens to contain a `.git`
            .filter(|relative| !relative.as_os_str().is_empty());
        let Some(relative) = relative else {
            bail!("{} is outside the workspace", repo.display());
        };
        let relative_path = relative.to_string_lossy().to_string();

        // What a drop does is move a git directory into the library and delete
        // the worktree around it, which only makes sense for a repo that owns
        // its git directory and is the only worktree using it. The two ways
        // `git worktree` breaks that assumption are checked before the status
        // is, and ahead of `force`: neither is outstanding work the user can
        // decide to discard, so there is nothing for `--force` to mean here.
        if let Some(blocker) = worktree_blocker(repo) {
            debug!(repo = %repo.display(), %blocker, "Refusing to drop repository");
            report.skipped.push((relative_path, blocker));
            return Ok(());
        }

        // Check for uncommitted or unpushed changes unless --force is given.
        // A repo whose status couldn't be read at all is kept as well: this
        // step exists to find work that would be lost, and a repo workset
        // can't see into may be full of it.
        if !force {
            let status = check_repo_status(repo)?;
            if matches!(
                status,
                RepoStatus::Dirty | RepoStatus::Unpushed | RepoStatus::Unknown
            ) {
                debug!(repo = %repo.display(), ?status, "Refusing to drop repository");
                report
                    .skipped
                    .push((relative_path, DropBlocker::Outstanding(status)));
                return Ok(());
            }
        }

        if !delete {
            // Store the repository in the library using workspace-relative path
            self.store_in_library(&relative_path)?;
        }

        // Remove the directory
        debug!(path = ?repo, "Removing directory");
        std::fs::remove_dir_all(repo)?;
        prune_empty_dirs(&root, &root.join(&relative));
        report.dropped.push(relative_path);
        Ok(())
    }

    /// Clone `url` into `dest`, which must be a path inside this workspace.
    ///
    /// A clone has to make the directories leading to the repo before it knows
    /// whether there is a repo to put in them, so a clone that fails — a
    /// mistyped pattern, a private repo, a remote that is down — has to take
    /// them back out on the way, or a typo leaves a piece of workspace behind
    /// for good.
    pub fn clone_into(&self, url: &str, dest: &Path) -> Result<()> {
        gix_clone(url, dest)
            .map(|_| ())
            .inspect_err(|_| prune_empty_dirs(Path::new(&self.path), dest))
    }

    /// Attempt to clone a repository from configured remotes or infer the clone URL
    fn clone_from_remote(&self, pattern: &RepoPattern) -> Result<PathBuf> {
        // Try to infer the git URL from the pattern
        // Pattern could be:
        // - github.com/user/repo (with provider)
        // - user/repo (without provider, check configured remotes)
        if let Some((provider, repo_path)) = pattern.provider_and_path() {
            // Has provider like github.com/user/repo
            let clone_url = format!("https://{}/{}", provider, repo_path);
            let dest_path = self.repo_path(pattern)?;

            self.clone_into(&clone_url, &dest_path)?;
            return Ok(dest_path);
        }

        // No provider specified, would need to check configured remotes
        bail!("No provider specified. Use full path like github.com/user/repo")
    }

    /// Check if a repository exists in the library
    pub fn library_contains(&self, repo_path: &str) -> bool {
        std::fs::metadata(format!("{}/{}", self.library_path(), repo_path)).is_ok()
    }

    /// Move the given repository into the library.
    /// relative_path: the relative path of the repo within the workspace (e.g. "github.com/user/repo")
    pub fn store_in_library(&self, relative_path: &str) -> Result<()> {
        let library_path = self.library_path();

        // Make sure the library directory exists first
        std::fs::create_dir_all(&library_path).map_err(|e| {
            anyhow::anyhow!("Failed to create library directory {}: {}", library_path, e)
        })?;

        let source = format!("{}/{}/.git", self.path, relative_path);
        let dest = format!("{}/{}", library_path, relative_path);

        // Verify the source .git directory exists
        if std::fs::metadata(&source).is_err() {
            bail!("Repository .git directory not found: {}", source);
        }

        // Set core.bare=true by modifying the config file directly
        let config_path = std::path::Path::new(&source).join("config");
        let config_content = std::fs::read_to_string(&config_path)?;
        std::fs::write(&config_path, set_core_bare(&config_content, true))?;

        debug!(source = %source, dest = %dest, "Storing repository in library");

        // Create parent directories in library if needed
        if let Some(parent) = Path::new(&dest).parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| anyhow::anyhow!("Failed to create library parent directory: {}", e))?;
        }

        // Clear the library entry if it exists (for re-storing)
        if std::fs::metadata(&dest).is_ok() {
            debug!("Removing existing library entry: {}", dest);
            std::fs::remove_dir_all(&dest)
                .map_err(|e| anyhow::anyhow!("Failed to remove existing library entry: {}", e))?;
        }

        // Move the repository to the library
        std::fs::rename(&source, &dest)
            .map_err(|e| anyhow::anyhow!("Failed to move repository to library: {}", e))?;

        Ok(())
    }

    /// Restore a repository from the library to the workspace, the exact
    /// inverse of `store_in_library`: the git directory moves back out of the
    /// library whole, so every local branch, tag, reflog and config key the
    /// repo had when it was dropped comes back with it.
    ///
    /// relative_path: the relative path of the repo within the workspace (e.g. "github.com/user/repo")
    pub fn restore_from_library(&self, relative_path: &str) -> Result<()> {
        let library_path = self.library_path();
        let source = PathBuf::from(&library_path).join(relative_path);
        let dest = PathBuf::from(&self.path).join(relative_path);

        // Verify the library entry exists
        if std::fs::metadata(&source).is_err() {
            bail!(
                "Repository not found in library for path: {}",
                relative_path
            );
        }

        // Cloning out of the library is what materializes the worktree; its
        // git directory is thrown away again right below. A clone only
        // creates a local branch for HEAD, so keeping it would silently drop
        // every other branch (and any commit only reachable from one).
        self.clone_into(&source.to_string_lossy(), &dest)?;

        let git_dir = dest.join(".git");
        let discarded = dest.join(".git.workset-restore");
        std::fs::rename(&git_dir, &discarded)?;
        if let Err(e) = std::fs::rename(&source, &git_dir) {
            // Put the clone back so the worktree isn't left without a repo
            let _ = std::fs::rename(&discarded, &git_dir);
            return Err(anyhow::anyhow!(
                "Failed to move repository out of the library: {}",
                e
            ));
        }

        // The index the repo had when it was dropped describes the worktree as
        // it was then, which the fresh checkout only matches if the drop was
        // clean. The clone's index always matches what it just wrote, so adopt
        // that one and let everything else come from the library. A library
        // entry without commits checks out nothing and so has no index to
        // adopt, in which case the empty worktree matches no index at all.
        let checked_out_index = discarded.join("index");
        if checked_out_index.exists() {
            std::fs::rename(&checked_out_index, git_dir.join("index"))?;
        } else {
            let _ = std::fs::remove_file(git_dir.join("index"));
        }
        std::fs::remove_dir_all(&discarded)?;

        // The library keeps its copy bare; a workspace checkout is not
        let config_path = git_dir.join("config");
        let config = std::fs::read_to_string(&config_path)?;
        std::fs::write(&config_path, set_core_bare(&config, false))?;

        Ok(())
    }

    /// List all repositories in the library
    pub fn list_library(&self) -> Result<Vec<String>> {
        let library_path = self.library_path();
        if !Path::new(&library_path).exists() {
            return Ok(Vec::new());
        }

        let mut repos = Vec::new();

        // Recursively find all git repositories in the library
        fn find_repos(base_path: &str, current_path: &Path, repos: &mut Vec<String>) -> Result<()> {
            if current_path.is_dir() {
                // Is this directory a repository? `gix::open` answers by
                // building a whole Repository — locating the git directory,
                // reading and parsing the repo's config and every file it
                // includes, resolving the object store — and all of it is
                // thrown away here, where only the yes or no is wanted. The
                // stats git itself uses answer the same question for a
                // fraction of the cost, and this walk is not a rare one: it
                // runs before the TUI's first paint, on every `workset
                // status`, before every restore, and on every TAB press
                // through the shell completions. Its cost grows with the
                // library, which is the part of a workspace this tool exists
                // to let you grow.
                if is_git_dir(current_path) || current_path.join(".git").exists() {
                    // Get the relative path from the library base
                    if let Ok(rel_path) = current_path.strip_prefix(base_path) {
                        let repo_path = rel_path.to_string_lossy().to_string();
                        if !repo_path.is_empty() {
                            repos.push(repo_path);
                        }
                    }
                    return Ok(()); // Don't recurse into git repos
                }

                // Recursively search subdirectories
                if let Ok(entries) = std::fs::read_dir(current_path) {
                    for entry in entries.filter_map(|e| e.ok()) {
                        let path = entry.path();
                        find_repos(base_path, &path, repos)?;
                    }
                }
            }
            Ok(())
        }

        find_repos(&library_path, Path::new(&library_path), &mut repos)?;

        debug!(count = repos.len(), "Found repositories in library");
        repos.sort();
        Ok(repos)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn url_tree_path_normalizes_remote_urls() {
        let cases = [
            (
                "https://github.com/fossable/workset.git",
                Some("github.com/fossable/workset"),
            ),
            (
                "https://github.com/fossable/workset",
                Some("github.com/fossable/workset"),
            ),
            (
                "https://gitlab.com/group/sub/project.git",
                Some("gitlab.com/group/sub/project"),
            ),
            ("http://example.com/a/b/", Some("example.com/a/b")),
            ("https://user:pass@host/a/b", Some("host/a/b")),
            (
                "git@github.com:fossable/workset.git",
                Some("github.com/fossable/workset"),
            ),
            (
                "ssh://git@host.example:2222/org/repo.git",
                Some("host.example/org/repo"),
            ),
            ("git://host/a/b", Some("host/a/b")),
            ("file:///srv/git/repo.git", None),
            ("/home/user/.workset/github.com/foo/bar", None),
            ("../relative/repo", None),
            ("~/repos/foo", None),
            ("", None),
        ];
        for (url, expected) in cases {
            assert_eq!(url_tree_path(url).as_deref(), expected, "url: {url}");
        }
    }

    #[test]
    fn set_core_bare_replaces_existing_entry() {
        let config = "[core]\n\tbare = false\n[remote \"origin\"]\n\turl = x\n";
        let updated = set_core_bare(config, true);
        assert!(updated.contains("\tbare = true"));
        assert!(!updated.contains("bare = false"));
        assert!(updated.contains("[remote \"origin\"]"));

        let reverted = set_core_bare(&updated, false);
        assert!(reverted.contains("\tbare = false"));
        assert!(!reverted.contains("bare = true"));
    }

    #[test]
    fn set_core_bare_inserts_under_existing_core_section() {
        let config = "[core]\n\tfilemode = true\n";
        let updated = set_core_bare(config, true);
        assert!(updated.contains("[core]\n\tbare = true"));
        assert!(updated.contains("filemode = true"));
    }

    #[test]
    fn set_core_bare_appends_missing_core_section() {
        let config = "[user]\n\tname = example\n";
        let updated = set_core_bare(config, false);
        assert!(updated.contains("[core]\n\tbare = false"));
        assert!(updated.contains("name = example"));
    }

    #[test]
    fn test_library_contains() {
        let temp_dir = TempDir::new().unwrap();
        let workspace = Workspace {
            path: temp_dir.path().to_string_lossy().to_string(),
        };

        let repo_path = "test/repo";
        assert!(!workspace.library_contains(repo_path));

        // Create the library directory with a test repo
        let library_path = format!("{}/{}", workspace.library_path(), repo_path);
        fs::create_dir_all(&library_path).unwrap();

        assert!(workspace.library_contains(repo_path));
    }

    #[test]
    fn resolve_pattern_is_relative_to_the_current_directory() {
        let temp_dir = TempDir::new().unwrap();
        let workspace = Workspace {
            path: temp_dir.path().to_string_lossy().to_string(),
        };
        let parent = temp_dir.path().join("github.com/user");
        fs::create_dir_all(parent.join("project")).unwrap();

        // From the repo's parent, both the documented './repo' form and a bare
        // name resolve to the repo's workspace-relative path
        assert_eq!(
            workspace.resolve_pattern(&parent, "./project"),
            "github.com/user/project"
        );
        assert_eq!(
            workspace.resolve_pattern(&parent, "project"),
            "github.com/user/project"
        );

        // A full workspace-relative pattern names nothing under the parent, so
        // it is left alone and still matches from there
        assert_eq!(
            workspace.resolve_pattern(&parent, "github.com/user/project"),
            "github.com/user/project"
        );

        // From the workspace root, a workspace-relative pattern is unchanged
        assert_eq!(
            workspace.resolve_pattern(temp_dir.path(), "github.com/user/project"),
            "github.com/user/project"
        );
    }

    #[test]
    fn resolve_pattern_keeps_patterns_it_cannot_place_in_the_workspace() {
        let temp_dir = TempDir::new().unwrap();
        let workspace = Workspace {
            path: temp_dir.path().join("ws").to_string_lossy().to_string(),
        };
        let root = temp_dir.path().join("ws");
        fs::create_dir_all(root.join("repo")).unwrap();
        fs::create_dir_all(temp_dir.path().join("outside")).unwrap();

        // Nonexistent paths can't be resolved, so the pattern passes through
        assert_eq!(workspace.resolve_pattern(&root, "missing"), "missing");

        // Neither are paths that escape the workspace
        assert_eq!(workspace.resolve_pattern(&root, "../outside"), "../outside");

        // Nor the workspace root itself, which names no repo
        assert_eq!(workspace.resolve_pattern(&root, "."), ".");
    }

    #[test]
    fn search_refuses_patterns_that_point_outside_the_workspace() {
        let temp_dir = TempDir::new().unwrap();
        let workspace = Workspace {
            path: temp_dir.path().join("ws").to_string_lossy().to_string(),
        };
        fs::create_dir_all(temp_dir.path().join("ws/github.com/user/project/.git")).unwrap();
        let outside = temp_dir.path().join("outside/repo");
        fs::create_dir_all(outside.join(".git")).unwrap();

        // A pattern naming a repo the workspace contains still resolves
        let inside = "github.com/user/project".parse::<RepoPattern>().unwrap();
        assert_eq!(workspace.search(&inside).unwrap().len(), 1);

        // Everything that leaves the workspace is refused, however it gets out
        let escaping = [
            "../outside/repo".to_string(),
            "github.com/../../outside/repo".to_string(),
            outside.to_string_lossy().to_string(),
        ];
        for pattern in escaping {
            let error = workspace
                .search(&pattern.parse::<RepoPattern>().unwrap())
                .unwrap_err()
                .to_string();
            assert!(
                error.contains("outside the workspace"),
                "pattern {pattern}: {error}"
            );
        }
    }

    #[test]
    fn drop_leaves_repos_outside_the_workspace_alone() {
        let temp_dir = TempDir::new().unwrap();
        let workspace = Workspace {
            path: temp_dir.path().join("ws").to_string_lossy().to_string(),
        };
        fs::create_dir_all(workspace.library_path()).unwrap();
        let outside = temp_dir.path().join("outside/repo");
        fs::create_dir_all(&outside).unwrap();
        gix::init(&outside).unwrap();
        fs::write(outside.join("file.txt"), "important").unwrap();

        let pattern = "../outside/repo".parse::<RepoPattern>().unwrap();
        // Deleting and storing in the library are both destructive, and the
        // repo belongs to neither the workspace nor its library
        for delete in [false, true] {
            assert!(workspace.drop(&pattern, delete, true).is_err());
            assert!(outside.join("file.txt").exists());
            assert!(outside.join(".git").exists());
        }

        // Nor was the repo smuggled into the workspace on the way out
        assert!(find_git_repositories(Path::new(&workspace.path)).is_empty());
    }

    #[test]
    fn commit_less_repo_holding_work_is_dirty_not_empty() {
        let temp_dir = TempDir::new().unwrap();
        let repo_path = temp_dir.path();
        gix::init(repo_path).unwrap();

        // Freshly initialized and genuinely empty
        assert_eq!(
            check_repo_status(repo_path).unwrap(),
            RepoStatus::NoCommits,
            "an empty repo has no work to lose"
        );

        // A file in the worktree is work that exists nowhere else yet, which
        // the commit-less case used to hide behind NoCommits
        fs::write(repo_path.join("notes.txt"), "unfinished").unwrap();
        assert_eq!(check_repo_status(repo_path).unwrap(), RepoStatus::Dirty);
    }

    #[test]
    fn drop_refuses_a_commit_less_repo_holding_uncommitted_work() {
        let temp_dir = TempDir::new().unwrap();
        let workspace = Workspace {
            path: temp_dir.path().join("ws").to_string_lossy().to_string(),
        };
        fs::create_dir_all(workspace.library_path()).unwrap();
        let repo = PathBuf::from(&workspace.path).join("fresh");
        fs::create_dir_all(&repo).unwrap();
        gix::init(&repo).unwrap();
        fs::write(repo.join("notes.txt"), "unfinished").unwrap();

        let pattern = "fresh".parse::<RepoPattern>().unwrap();
        let report = workspace.drop(&pattern, false, false).unwrap();

        // Nothing but the worktree holds this file, so the drop leaves it be
        assert!(report.dropped.is_empty());
        assert_eq!(
            report.skipped,
            vec![(
                "fresh".to_string(),
                DropBlocker::Outstanding(RepoStatus::Dirty)
            )]
        );
        assert_eq!(
            fs::read_to_string(repo.join("notes.txt")).unwrap(),
            "unfinished"
        );

        // An empty one has nothing to lose and still drops
        let empty = PathBuf::from(&workspace.path).join("empty");
        fs::create_dir_all(&empty).unwrap();
        gix::init(&empty).unwrap();
        let report = workspace
            .drop(&"empty".parse::<RepoPattern>().unwrap(), false, false)
            .unwrap();
        assert_eq!(report.dropped, vec!["empty".to_string()]);
        assert!(!empty.exists());
    }

    /// Lay out a linked worktree the way `git worktree add` does: the worktree
    /// keeps a `.git` *file* naming a registration directory under the main
    /// repo's git directory, and that registration's `gitdir` file names the
    /// file pointing at it.
    fn add_linked_worktree(main_repo: &Path, worktree: &Path, name: &str) {
        let registration = main_repo.join(".git").join("worktrees").join(name);
        fs::create_dir_all(&registration).unwrap();
        fs::create_dir_all(worktree).unwrap();
        fs::write(
            worktree.join(".git"),
            format!("gitdir: {}\n", registration.display()),
        )
        .unwrap();
        fs::write(
            registration.join("gitdir"),
            format!("{}\n", worktree.join(".git").display()),
        )
        .unwrap();
    }

    /// A linked worktree has no git directory of its own — the one it uses
    /// belongs to the repo it was added from. There is nothing here the
    /// library can hold, and taking the directory anyway leaves that repo
    /// registering a worktree that no longer exists.
    #[test]
    fn drop_refuses_a_linked_worktree() {
        let temp_dir = TempDir::new().unwrap();
        let workspace = Workspace {
            path: temp_dir.path().join("ws").to_string_lossy().to_string(),
        };
        fs::create_dir_all(workspace.library_path()).unwrap();

        let main_repo = PathBuf::from(&workspace.path).join("main");
        fs::create_dir_all(&main_repo).unwrap();
        gix::init(&main_repo).unwrap();
        let worktree = PathBuf::from(&workspace.path).join("feature");
        add_linked_worktree(&main_repo, &worktree, "feature");
        fs::write(worktree.join("work.txt"), "only here").unwrap();

        let pattern = "feature".parse::<RepoPattern>().unwrap();
        // Nothing the user can decide to discard, so neither flag gets past it
        for (delete, force) in [(false, false), (false, true), (true, true)] {
            let report = workspace.drop(&pattern, delete, force).unwrap();
            assert!(report.dropped.is_empty());
            assert_eq!(
                report.skipped,
                vec![("feature".to_string(), DropBlocker::BorrowedGitDir)]
            );
            assert_eq!(
                fs::read_to_string(worktree.join("work.txt")).unwrap(),
                "only here"
            );
            assert!(worktree.join(".git").is_file());
        }
    }

    /// The git directory under a main worktree is the one every linked
    /// worktree reads through. Moving it into the library breaks each of them
    /// where it stands, and because nothing can be read out of them afterwards
    /// they report as having no commits — the status that tells a later drop
    /// they are empty and safe to consume.
    #[test]
    fn drop_refuses_the_main_worktree_of_a_live_linked_worktree() {
        let temp_dir = TempDir::new().unwrap();
        let workspace = Workspace {
            path: temp_dir.path().join("ws").to_string_lossy().to_string(),
        };
        fs::create_dir_all(workspace.library_path()).unwrap();

        let main_repo = PathBuf::from(&workspace.path).join("main");
        fs::create_dir_all(&main_repo).unwrap();
        gix::init(&main_repo).unwrap();
        // One inside the workspace and one outside it: either way the drop is
        // what breaks them, so where they sit doesn't decide anything
        add_linked_worktree(
            &main_repo,
            &PathBuf::from(&workspace.path).join("feature"),
            "feature",
        );
        add_linked_worktree(&main_repo, &temp_dir.path().join("elsewhere"), "elsewhere");

        let pattern = "main".parse::<RepoPattern>().unwrap();
        for (delete, force) in [(false, false), (false, true), (true, true)] {
            let report = workspace.drop(&pattern, delete, force).unwrap();
            assert!(report.dropped.is_empty());
            assert_eq!(
                report.skipped,
                vec![("main".to_string(), DropBlocker::MainWorktree(2))]
            );
            assert!(main_repo.join(".git").is_dir());
        }

        // Taking the worktrees away leaves a repo nothing else is using
        fs::remove_dir_all(PathBuf::from(&workspace.path).join("feature")).unwrap();
        fs::remove_dir_all(temp_dir.path().join("elsewhere")).unwrap();
        let report = workspace.drop(&pattern, false, false).unwrap();
        assert_eq!(report.dropped, vec!["main".to_string()]);
    }

    /// `git worktree` leaves the registration behind when the worktree itself
    /// is deleted rather than removed; `git worktree prune` is what collects
    /// it. Nothing points at the git directory through a registration like
    /// that, so it is no reason to keep a repo in the workspace.
    #[test]
    fn a_pruned_linked_worktree_does_not_block_a_drop() {
        let temp_dir = TempDir::new().unwrap();
        let workspace = Workspace {
            path: temp_dir.path().join("ws").to_string_lossy().to_string(),
        };
        fs::create_dir_all(workspace.library_path()).unwrap();

        let main_repo = PathBuf::from(&workspace.path).join("main");
        fs::create_dir_all(&main_repo).unwrap();
        gix::init(&main_repo).unwrap();
        let worktree = temp_dir.path().join("gone");
        add_linked_worktree(&main_repo, &worktree, "gone");
        fs::remove_dir_all(&worktree).unwrap();

        let report = workspace
            .drop(&"main".parse::<RepoPattern>().unwrap(), false, false)
            .unwrap();
        assert_eq!(report.dropped, vec!["main".to_string()]);
        assert!(workspace.library_contains("main"));
    }

    #[test]
    fn drop_repo_refuses_a_path_outside_the_workspace() {
        let temp_dir = TempDir::new().unwrap();
        let workspace = Workspace {
            path: temp_dir.path().join("ws").to_string_lossy().to_string(),
        };
        fs::create_dir_all(workspace.library_path()).unwrap();
        let outside = temp_dir.path().join("outside/repo");
        fs::create_dir_all(&outside).unwrap();
        gix::init(&outside).unwrap();

        // The last gate before a directory is moved or deleted holds on its
        // own, for a path that never went through a pattern
        let mut report = DropReport::default();
        let through_the_root = PathBuf::from(&workspace.path).join("../outside/repo");
        for repo in [outside.clone(), through_the_root] {
            assert!(workspace.drop_repo(&repo, true, true, &mut report).is_err());
        }
        assert!(outside.join(".git").exists());
        assert!(report.is_empty());
    }

    /// A repo's path is its remote's path, so the directories above it are
    /// workset's bookkeeping rather than anything the user put there. Leaving
    /// them behind empty meant a workspace emptied of every repo still held a
    /// directory tree naming each one.
    #[test]
    fn drop_takes_the_directories_it_leaves_empty_with_it() {
        let temp_dir = TempDir::new().unwrap();
        let workspace = Workspace {
            path: temp_dir.path().join("ws").to_string_lossy().to_string(),
        };
        fs::create_dir_all(workspace.library_path()).unwrap();
        let root = PathBuf::from(&workspace.path);
        repo_with_a_commit(&root.join("github.com/user/project"));

        let report = workspace
            .drop(
                &"github.com/user/project".parse::<RepoPattern>().unwrap(),
                false,
                false,
            )
            .unwrap();
        assert_eq!(report.dropped, vec!["github.com/user/project".to_string()]);

        // Nothing is left of the repo's path, but the workspace itself and the
        // library holding the repo are untouched
        assert!(!root.join("github.com").exists());
        assert!(root.is_dir());
        assert!(workspace.library_contains("github.com/user/project"));

        // And a restore builds the path back, so the pruning costs nothing
        workspace
            .restore_from_library("github.com/user/project")
            .unwrap();
        assert!(root.join("github.com/user/project/.git").is_dir());
    }

    /// Only the directories that the dropped repo was the last thing in are
    /// workset's to remove: anything still holding something stays, whether
    /// that something is another repo or a file the user left there.
    #[test]
    fn drop_keeps_a_directory_that_still_holds_something() {
        let temp_dir = TempDir::new().unwrap();
        let workspace = Workspace {
            path: temp_dir.path().join("ws").to_string_lossy().to_string(),
        };
        fs::create_dir_all(workspace.library_path()).unwrap();
        let root = PathBuf::from(&workspace.path);
        repo_with_a_commit(&root.join("github.com/user/first"));
        repo_with_a_commit(&root.join("github.com/user/second"));
        repo_with_a_commit(&root.join("gitlab.com/group/only"));
        fs::write(root.join("gitlab.com/notes.txt"), "mine").unwrap();

        let report = workspace
            .drop(
                &"github.com/user/first".parse::<RepoPattern>().unwrap(),
                false,
                false,
            )
            .unwrap();
        assert_eq!(report.dropped, vec!["github.com/user/first".to_string()]);

        // The sibling repo is still in the workspace, so its parents are too
        assert!(!root.join("github.com/user/first").exists());
        assert!(root.join("github.com/user/second/.git").is_dir());

        let report = workspace
            .drop(
                &"gitlab.com/group/only".parse::<RepoPattern>().unwrap(),
                false,
                false,
            )
            .unwrap();
        assert_eq!(report.dropped, vec!["gitlab.com/group/only".to_string()]);

        // The empty group directory goes; the one with the user's file in it
        // is not workset's to clean up
        assert!(!root.join("gitlab.com/group").exists());
        assert_eq!(
            fs::read_to_string(root.join("gitlab.com/notes.txt")).unwrap(),
            "mine"
        );
    }

    /// The directories leading to a repo are made before the clone knows
    /// whether there is a repo to put in them, so a mistyped pattern used to
    /// leave a piece of workspace behind that nothing would ever fill or
    /// remove.
    #[test]
    fn a_failed_clone_takes_the_directories_it_made_with_it() {
        let temp_dir = TempDir::new().unwrap();
        let workspace = Workspace {
            path: temp_dir.path().join("ws").to_string_lossy().to_string(),
        };
        fs::create_dir_all(workspace.library_path()).unwrap();
        let root = PathBuf::from(&workspace.path);

        // A local path holding no repository stands in for every way a clone
        // can fail, and needs no network to do it
        let nowhere = temp_dir.path().join("nowhere");
        assert!(
            workspace
                .clone_into(
                    &nowhere.to_string_lossy(),
                    &root.join("github.com/user/typo")
                )
                .is_err()
        );

        assert!(!root.join("github.com").exists());
        assert!(root.is_dir());
        assert!(Path::new(&workspace.library_path()).is_dir());
    }

    /// The walk upward has to stop somewhere: the workspace root and the
    /// library are what make the directory a workspace at all, and a path that
    /// isn't under the root was never workset's to touch.
    #[test]
    fn pruning_stops_at_the_root_the_library_and_the_workspace_edge() {
        let temp_dir = TempDir::new().unwrap();
        let root = temp_dir.path().join("ws");
        let library = root.join(".workset");
        fs::create_dir_all(&library).unwrap();

        // A repo directly under the root, already removed: there is nothing
        // above it to prune but the root
        prune_empty_dirs(&root, &root.join("repo"));
        assert!(root.is_dir());
        assert!(library.is_dir());

        // An empty library is still the workspace's library
        prune_empty_dirs(&root, &library.join("github.com/user/project"));
        assert!(library.is_dir());

        // Nothing outside the workspace is removed, however empty
        let outside = temp_dir.path().join("outside/dir");
        fs::create_dir_all(&outside).unwrap();
        prune_empty_dirs(&root, &outside);
        assert!(outside.is_dir());
    }

    /// A symlinked directory in the workspace is a path that reads as
    /// workspace-relative while pointing anywhere at all. Reporting what is
    /// behind it as a repo of this workspace is what lets `drop` move its git
    /// directory into the library and delete a worktree that was never here.
    #[cfg(unix)]
    #[test]
    fn find_git_repositories_does_not_follow_symlinks() {
        let temp_dir = TempDir::new().unwrap();
        let root = temp_dir.path().join("ws");
        fs::create_dir_all(root.join("inside/.git")).unwrap();

        let outside = temp_dir.path().join("outside");
        fs::create_dir_all(outside.join("repo/.git")).unwrap();

        // Both a link to a directory holding repos and a link straight at one
        std::os::unix::fs::symlink(&outside, root.join("linked-dir")).unwrap();
        std::os::unix::fs::symlink(outside.join("repo"), root.join("linked-repo")).unwrap();

        let repos = find_git_repositories(&root);
        assert_eq!(repos, vec![root.join("inside")]);
    }

    /// Every caller of `scan_repos` pairs what came back with the repo list it
    /// passed in — `workset list` prints the two side by side — so a result
    /// that lands in the order its walk happened to finish names the wrong
    /// repo. Each repo also has to be scanned exactly once: a repo handed to
    /// two workers is counted twice in the summary, and one handed to none is
    /// missing from the listing.
    #[test]
    fn scan_repos_reports_each_repo_once_in_the_order_given() {
        // Many more repos than the machine has cores, and work that takes
        // longer the earlier the repo comes, so the walks can only finish out
        // of order
        let repos: Vec<PathBuf> = (0..200).map(|i| PathBuf::from(format!("r{i}"))).collect();
        let scans = std::sync::atomic::AtomicUsize::new(0);

        let scanned = scan_repos(&repos, |repo| {
            scans.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let index: u64 = repo.to_str().unwrap()[1..].parse().unwrap();
            std::thread::sleep(std::time::Duration::from_micros(200 - index.min(199)));
            repo.to_path_buf()
        });

        assert_eq!(scanned, repos);
        assert_eq!(
            scans.load(std::sync::atomic::Ordering::Relaxed),
            repos.len()
        );
    }

    #[cfg(unix)]
    #[test]
    fn drop_repo_refuses_a_repo_reached_through_a_symlink() {
        let temp_dir = TempDir::new().unwrap();
        let workspace = Workspace {
            path: temp_dir.path().join("ws").to_string_lossy().to_string(),
        };
        fs::create_dir_all(workspace.library_path()).unwrap();

        let outside = temp_dir.path().join("outside/repo");
        fs::create_dir_all(&outside).unwrap();
        gix::init(&outside).unwrap();
        fs::write(outside.join("file.txt"), "important").unwrap();

        std::os::unix::fs::symlink(
            temp_dir.path().join("outside"),
            Path::new(&workspace.path).join("linked"),
        )
        .unwrap();

        // The path spells out as workspace-relative, but the repo behind it
        // belongs to neither the workspace nor its library
        let through_the_link = Path::new(&workspace.path).join("linked/repo");
        let mut report = DropReport::default();
        for delete in [false, true] {
            assert!(
                workspace
                    .drop_repo(&through_the_link, delete, true, &mut report)
                    .is_err()
            );
            assert!(outside.join("file.txt").exists());
            assert!(outside.join(".git").exists());
        }
        assert!(report.is_empty());
    }

    #[test]
    fn drop_repo_refuses_the_workspace_root() {
        let temp_dir = TempDir::new().unwrap();
        let workspace = Workspace {
            path: temp_dir.path().join("ws").to_string_lossy().to_string(),
        };
        fs::create_dir_all(workspace.library_path()).unwrap();
        let root = PathBuf::from(&workspace.path);
        gix::init(&root).unwrap();

        // A workspace that is itself a git repo is still not a repo the
        // workspace holds; deleting it would take the library with it
        let mut report = DropReport::default();
        assert!(workspace.drop_repo(&root, true, true, &mut report).is_err());
        assert!(root.exists());
        assert!(Path::new(&workspace.library_path()).exists());
        assert!(report.is_empty());
    }

    #[test]
    fn remote_tree_path_prefers_origin() {
        let temp_dir = TempDir::new().unwrap();
        let repo_path = temp_dir.path();
        gix::init(repo_path).unwrap();

        // No remotes yet
        assert_eq!(remote_tree_path(repo_path), None);

        let config_path = repo_path.join(".git").join("config");
        let config = fs::read_to_string(&config_path).unwrap();
        fs::write(
            &config_path,
            format!(
                "{config}[remote \"backup\"]\n\turl = https://example.com/other/place.git\n\
                 [remote \"origin\"]\n\turl = git@github.com:fossable/workset.git\n"
            ),
        )
        .unwrap();
        assert_eq!(
            remote_tree_path(repo_path).as_deref(),
            Some("github.com/fossable/workset")
        );

        // Without origin, the first remote is used
        let config = fs::read_to_string(&config_path).unwrap();
        fs::write(&config_path, config.replace("\"origin\"", "\"upstream\"")).unwrap();
        assert_eq!(
            remote_tree_path(repo_path).as_deref(),
            Some("example.com/other/place")
        );
    }

    #[test]
    fn no_commits_clean_repo_reports_no_modification_time() {
        let temp_dir = TempDir::new().unwrap();
        let repo_path = temp_dir.path();
        gix::init(repo_path).unwrap();

        let (status, mod_time) = check_repo_status_and_modification_time(repo_path).unwrap();
        assert_eq!(status, RepoStatus::NoCommits);
        // A brand-new repo with no commits and a clean worktree has no
        // meaningful modification time.
        assert_eq!(mod_time, None);
    }

    #[test]
    fn no_commits_dirty_repo_reports_file_time() {
        let temp_dir = TempDir::new().unwrap();
        let repo_path = temp_dir.path();
        gix::init(repo_path).unwrap();
        fs::write(repo_path.join("untracked.txt"), "hello").unwrap();

        let (status, mod_time) = check_repo_status_and_modification_time(repo_path).unwrap();
        // The missing first commit is beside the point: the untracked file is
        // work that exists nowhere else, which is what Dirty means.
        assert_eq!(status, RepoStatus::Dirty);
        // With an untracked file present there is a real time to report,
        // and it must not be the UNIX_EPOCH sentinel.
        let time = mod_time.expect("expected a modification time from the untracked file");
        assert!(time > std::time::SystemTime::UNIX_EPOCH);
    }

    /// Run git in `dir`, asserting it succeeded, and return its stdout
    fn git(dir: &Path, args: &[&str]) -> String {
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
    }

    /// A repo with one commit on `main` and nothing uncommitted
    fn repo_with_a_commit(path: &Path) {
        fs::create_dir_all(path).unwrap();
        git(path, &["init", "--quiet", "--initial-branch=main"]);
        git(path, &["config", "user.name", "workset"]);
        git(path, &["config", "user.email", "workset@example.com"]);
        git(path, &["commit", "--quiet", "--allow-empty", "-m", "one"]);
    }

    #[test]
    fn detached_head_repo_is_clean_not_commit_less() {
        let temp_dir = TempDir::new().unwrap();
        let repo_path = temp_dir.path();
        repo_with_a_commit(repo_path);

        // A bisect, a rebase or a plain `git checkout --detach` all leave HEAD
        // pointing straight at a commit rather than at a branch
        git(repo_path, &["checkout", "--quiet", "--detach", "HEAD"]);
        git(
            repo_path,
            &["commit", "--quiet", "--allow-empty", "-m", "detached"],
        );

        // The history is right there, so calling it "no commits" is wrong
        assert_eq!(check_repo_status(repo_path).unwrap(), RepoStatus::Clean);

        let (status, mod_time) = check_repo_status_and_modification_time(repo_path).unwrap();
        assert_eq!(status, RepoStatus::Clean);
        // And the detached commit is a real time to report, not the absence
        // of one that a commit-less repo yields
        let time = mod_time.expect("expected the detached commit's time");
        assert!(time > std::time::SystemTime::UNIX_EPOCH);
        assert_eq!(get_repo_modification_time(repo_path).unwrap(), time);
    }

    #[test]
    fn detached_head_repo_with_work_is_still_dirty() {
        let temp_dir = TempDir::new().unwrap();
        let repo_path = temp_dir.path();
        repo_with_a_commit(repo_path);
        git(repo_path, &["checkout", "--quiet", "--detach", "HEAD"]);
        fs::write(repo_path.join("notes.txt"), "unfinished").unwrap();

        // Uncommitted work outranks where HEAD happens to point, so a drop
        // still refuses the repo
        assert_eq!(check_repo_status(repo_path).unwrap(), RepoStatus::Dirty);
    }

    /// A repository whose config gix refuses to load, which is what
    /// `git init --object-format=sha256` writes. Spelled out here rather than
    /// created with git so the test doesn't depend on the local git's version.
    fn repo_gix_cannot_open(path: &Path) {
        let git_dir = path.join(".git");
        fs::create_dir_all(git_dir.join("objects/info")).unwrap();
        fs::create_dir_all(git_dir.join("objects/pack")).unwrap();
        fs::create_dir_all(git_dir.join("refs/heads")).unwrap();
        fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        fs::write(
            git_dir.join("config"),
            "[core]\n\trepositoryformatversion = 1\n\tbare = false\n\
             [extensions]\n\tobjectformat = sha256\n",
        )
        .unwrap();
    }

    /// gix doesn't read every repository git can write, and a repo workset
    /// can't see into is not a repo with nothing in it. Reporting one as
    /// NoCommits said the exact opposite of the truth to everything that acts
    /// on status.
    #[test]
    fn unreadable_repo_is_unknown_not_empty() {
        let temp_dir = TempDir::new().unwrap();
        let repo = temp_dir.path().join("opaque");
        repo_gix_cannot_open(&repo);
        fs::write(repo.join("work.txt"), "exists nowhere else").unwrap();

        assert_eq!(check_repo_status(&repo).unwrap(), RepoStatus::Unknown);
        let (status, mod_time) = check_repo_status_and_modification_time(&repo).unwrap();
        assert_eq!(status, RepoStatus::Unknown);
        // Nothing was read, so there is no time to claim either
        assert_eq!(mod_time, None);
    }

    /// The drop gate exists to keep work that exists nowhere else. A repo
    /// whose status can't be determined may be full of it, so it is kept —
    /// before this, an unreadable repo read as NoCommits and the worktree was
    /// deleted (and with `--delete`, destroyed) without a word.
    #[test]
    fn drop_refuses_a_repo_it_cannot_read() {
        let temp_dir = TempDir::new().unwrap();
        let workspace = Workspace {
            path: temp_dir.path().join("ws").to_string_lossy().to_string(),
        };
        fs::create_dir_all(workspace.library_path()).unwrap();
        let repo = PathBuf::from(&workspace.path).join("opaque");
        repo_gix_cannot_open(&repo);
        fs::write(repo.join("work.txt"), "exists nowhere else").unwrap();

        let pattern = "opaque".parse::<RepoPattern>().unwrap();
        for delete in [false, true] {
            let report = workspace.drop(&pattern, delete, false).unwrap();
            assert!(report.dropped.is_empty());
            assert_eq!(
                report.skipped,
                vec![(
                    "opaque".to_string(),
                    DropBlocker::Outstanding(RepoStatus::Unknown)
                )]
            );
            assert_eq!(
                fs::read_to_string(repo.join("work.txt")).unwrap(),
                "exists nowhere else"
            );
            assert!(!Path::new(&workspace.library_path()).join("opaque").exists());
        }

        // --force is still the way to say "I know, drop it anyway"
        let report = workspace.drop(&pattern, true, true).unwrap();
        assert_eq!(report.dropped, vec!["opaque".to_string()]);
        assert!(!repo.exists());
    }

    /// Run git in `dir`, returning None when it failed instead of asserting,
    /// for probing whether the local git supports something
    fn try_git(dir: &Path, args: &[&str]) -> Option<()> {
        std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .ok()
            .filter(|out| out.status.success())
            .map(|_| ())
    }

    /// The other half of the same hazard, driven by real git: a reftable repo
    /// (git 2.45+) opens fine but yields nothing for HEAD or for the worktree
    /// status, so every signal workset reads says "empty" while the repo holds
    /// a commit and uncommitted work. The assertion is that the status is
    /// anything but NoCommits, which holds whether gix learns to read
    /// reftables or not.
    #[test]
    fn reftable_repo_is_not_reported_empty() {
        let temp_dir = TempDir::new().unwrap();
        let workspace = Workspace {
            path: temp_dir.path().join("ws").to_string_lossy().to_string(),
        };
        fs::create_dir_all(workspace.library_path()).unwrap();
        let repo = PathBuf::from(&workspace.path).join("reftable");
        fs::create_dir_all(&repo).unwrap();
        if try_git(&repo, &["init", "--quiet", "--ref-format=reftable"]).is_none() {
            // Too old a git to make one; nothing to check here
            return;
        }
        git(&repo, &["config", "user.name", "workset"]);
        git(&repo, &["config", "user.email", "workset@example.com"]);
        fs::write(repo.join("tracked.txt"), "committed").unwrap();
        git(&repo, &["add", "tracked.txt"]);
        git(&repo, &["commit", "--quiet", "-m", "one"]);
        fs::write(repo.join("tracked.txt"), "uncommitted").unwrap();

        assert_ne!(
            check_repo_status(&repo).unwrap(),
            RepoStatus::NoCommits,
            "a repo with a commit and uncommitted work is not empty"
        );

        let report = workspace
            .drop(&"reftable".parse::<RepoPattern>().unwrap(), true, false)
            .unwrap();
        assert!(report.dropped.is_empty());
        assert_eq!(
            fs::read_to_string(repo.join("tracked.txt")).unwrap(),
            "uncommitted"
        );
    }

    #[test]
    fn test_find_git_repositories() {
        let temp_dir = TempDir::new().unwrap();
        let base_path = temp_dir.path();

        // Create a git directory structure
        let repo1 = base_path.join("repo1");
        fs::create_dir_all(repo1.join(".git")).unwrap();

        let repo2 = base_path.join("nested/repo2");
        fs::create_dir_all(repo2.join(".git")).unwrap();

        let not_repo = base_path.join("not_a_repo");
        fs::create_dir_all(&not_repo).unwrap();

        let repos = find_git_repositories(base_path);

        assert_eq!(repos.len(), 2);
        assert!(repos.iter().any(|p| p.ends_with("repo1")));
        assert!(repos.iter().any(|p| p.ends_with("repo2")));
    }

    /// Repos come back in path order, not in whichever order the filesystem
    /// happened to store the directory entries, so everything that prints the
    /// list prints the same thing on every machine.
    #[test]
    fn find_git_repositories_reports_repos_in_path_order() {
        let temp_dir = TempDir::new().unwrap();
        let base_path = temp_dir.path();

        // Created in an order that is neither alphabetical nor reverse, so a
        // filesystem that keeps insertion order fails this too
        for name in [
            "gitlab.com/b/two",
            "github.com/b/one",
            "gitlab.com/a/one",
            "github.com/a/two",
            "github.com/a/one",
        ] {
            fs::create_dir_all(base_path.join(name).join(".git")).unwrap();
        }

        let repos: Vec<_> = find_git_repositories(base_path)
            .iter()
            .map(|repo| {
                repo.strip_prefix(base_path)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect();

        assert_eq!(
            repos,
            vec![
                "github.com/a/one",
                "github.com/a/two",
                "github.com/b/one",
                "gitlab.com/a/one",
                "gitlab.com/b/two",
            ]
        );
    }

    /// The library listing reports each dropped repo once, under the path it
    /// was dropped from, and stops at the entry — the object store inside it
    /// holds no library entries, and reading all of it is what the listing
    /// costs when it doesn't stop.
    #[test]
    fn list_library_reports_each_entry_once_and_stops_inside_it() {
        let temp_dir = TempDir::new().unwrap();
        let root = temp_dir.path();
        let workspace = Workspace {
            path: root.to_string_lossy().to_string(),
        };
        fs::create_dir_all(workspace.library_path()).unwrap();

        for name in ["gitlab.com/b/two", "github.com/a/one", "github.com/a/two"] {
            gix::init(root.join(name)).unwrap();
            workspace.store_in_library(name).unwrap();
        }

        // Only reachable by descending into an entry, and shaped exactly like
        // the repos the listing reports
        let entry = Path::new(&workspace.library_path()).join("github.com/a/one");
        fs::create_dir_all(entry.join("objects/decoy/.git")).unwrap();

        assert_eq!(
            workspace.list_library().unwrap(),
            vec!["github.com/a/one", "github.com/a/two", "gitlab.com/b/two"]
        );
    }

    /// An entry that still has its worktree — a restore that died halfway, or
    /// a repo moved into the library by hand — is a repo in the library and is
    /// listed as one, rather than being walked into as if it were a directory
    /// of entries.
    #[test]
    fn list_library_reports_an_entry_that_kept_its_worktree() {
        let temp_dir = TempDir::new().unwrap();
        let root = temp_dir.path();
        let workspace = Workspace {
            path: root.to_string_lossy().to_string(),
        };
        let entry = Path::new(&workspace.library_path()).join("github.com/user/repo");
        gix::init(&entry).unwrap();
        fs::write(entry.join("tracked.txt"), "hello").unwrap();

        assert_eq!(
            workspace.list_library().unwrap(),
            vec!["github.com/user/repo"]
        );
    }

    /// A dropped repo lives on in the library as a bare git directory, and the
    /// library lives inside the workspace, so the walk that enumerates the
    /// working set meets one. It must stop there: walking in costs a read_dir
    /// of every directory of every library entry's object store — the walk's
    /// whole runtime, once a library has grown — and anything found inside is
    /// not a repo the workspace contains.
    #[test]
    fn find_git_repositories_stops_at_a_library_entry() {
        let temp_dir = TempDir::new().unwrap();
        let root = temp_dir.path();
        let workspace = Workspace {
            path: root.to_string_lossy().to_string(),
        };
        fs::create_dir_all(workspace.library_path()).unwrap();

        // One repo stays in the workspace, one is dropped into the library
        let kept = root.join("github.com/user/kept");
        gix::init(&kept).unwrap();
        let dropped = root.join("github.com/user/dropped");
        gix::init(&dropped).unwrap();
        workspace
            .store_in_library("github.com/user/dropped")
            .unwrap();

        let library_entry = Path::new(&workspace.library_path()).join("github.com/user/dropped");
        assert!(
            library_entry.join("objects").is_dir(),
            "the library entry should be a bare git directory"
        );

        // A `.git` planted inside the library entry is the walk's own
        // evidence: it is only reachable by descending into the entry, and it
        // is exactly the shape the walk reports as a workspace repo.
        fs::create_dir_all(library_entry.join("objects/decoy/.git")).unwrap();

        let repos = find_git_repositories(root);
        assert_eq!(repos, vec![kept]);
    }

    #[test]
    fn test_find_submodules_parses_multiple_entries() {
        let temp_dir = TempDir::new().unwrap();
        let repo = temp_dir.path();
        fs::write(
            repo.join(".gitmodules"),
            "[submodule \"first\"]\n\
             \tpath = libs/first\n\
             \turl = https://example.com/first.git\n\
             [submodule \"second\"]\n\
             \tpath = libs/second\n\
             \turl = https://example.com/second.git\n",
        )
        .unwrap();
        // Only the first submodule is checked out
        fs::create_dir_all(repo.join("libs/first/.git")).unwrap();

        let submodules = find_submodules_in_repo(repo).unwrap();
        assert_eq!(submodules.len(), 2);

        let first = &submodules[0];
        assert_eq!(first.path, PathBuf::from("libs/first"));
        assert!(first.initialized);

        let second = &submodules[1];
        assert_eq!(second.path, PathBuf::from("libs/second"));
        assert!(!second.initialized);
    }

    #[test]
    fn test_find_submodules_missing_file_is_empty() {
        let temp_dir = TempDir::new().unwrap();
        assert!(find_submodules_in_repo(temp_dir.path()).unwrap().is_empty());
    }

    /// `.gitmodules` ships with the repo, so a cloned repo gets to say what is
    /// in it. A declared path that leaves the repo aims everything workset
    /// does to that row — the size walk, the status and diff git calls, the
    /// shell Enter opens — at a directory the repo doesn't own, and an
    /// absolute one replaces the parent's path entirely when joined.
    #[test]
    fn find_submodules_skips_paths_that_leave_the_repo() {
        let temp_dir = TempDir::new().unwrap();
        let repo = temp_dir.path().join("repo");
        fs::create_dir_all(temp_dir.path().join("outside")).unwrap();
        fs::create_dir_all(repo.join("libs/good/.git")).unwrap();

        fs::write(
            repo.join(".gitmodules"),
            "[submodule \"absolute\"]\n\tpath = /\n\
             [submodule \"parent\"]\n\tpath = ../outside\n\
             [submodule \"buried-parent\"]\n\tpath = libs/../../outside\n\
             [submodule \"empty\"]\n\tpath =\n\
             [submodule \"self\"]\n\tpath = .\n\
             [submodule \"good\"]\n\tpath = libs/good\n",
        )
        .unwrap();

        let submodules = find_submodules_in_repo(&repo).unwrap();
        let paths: Vec<_> = submodules.iter().map(|s| s.path.clone()).collect();
        assert_eq!(paths, vec![PathBuf::from("libs/good")]);
        assert!(submodules[0].initialized);
    }

    /// A symlink in the worktree spells out as a plain relative path while
    /// pointing anywhere at all, so a lexical check alone doesn't settle where
    /// a declared submodule path lands.
    #[cfg(unix)]
    #[test]
    fn find_submodules_skips_paths_that_leave_the_repo_through_a_symlink() {
        let temp_dir = TempDir::new().unwrap();
        let repo = temp_dir.path().join("repo");
        fs::create_dir_all(repo.join("libs")).unwrap();
        fs::create_dir_all(temp_dir.path().join("outside/repo/.git")).unwrap();
        std::os::unix::fs::symlink(temp_dir.path().join("outside"), repo.join("linked")).unwrap();

        fs::write(
            repo.join(".gitmodules"),
            "[submodule \"linked\"]\n\tpath = linked/repo\n\
             [submodule \"uninitialized\"]\n\tpath = libs/later\n",
        )
        .unwrap();

        // The link is refused, while a submodule that simply isn't checked out
        // yet has no link to follow and is kept
        let submodules = find_submodules_in_repo(&repo).unwrap();
        let paths: Vec<_> = submodules.iter().map(|s| s.path.clone()).collect();
        assert_eq!(paths, vec![PathBuf::from("libs/later")]);
        assert!(!submodules[0].initialized);
    }

    #[test]
    fn test_parse_with_provider() -> Result<(), Box<dyn Error>> {
        let pattern = str::parse::<RepoPattern>("github.com/user/repo")?;
        assert_eq!(pattern.provider, Some("github.com".to_string()));
        assert_eq!(pattern.path, "user/repo".to_string());
        Ok(())
    }

    #[test]
    fn test_parse_without_provider() -> Result<(), Box<dyn Error>> {
        let pattern = str::parse::<RepoPattern>("user/repo")?;
        assert_eq!(pattern.provider, None);
        assert_eq!(pattern.path, "user/repo".to_string());
        Ok(())
    }

    #[test]
    fn test_parse_simple_path() -> Result<(), Box<dyn Error>> {
        let pattern = str::parse::<RepoPattern>("repo")?;
        assert_eq!(pattern.provider, None);
        assert_eq!(pattern.path, "repo".to_string());
        Ok(())
    }

    #[test]
    fn test_parse_gitlab_path() -> Result<(), Box<dyn Error>> {
        let pattern = str::parse::<RepoPattern>("gitlab.com/company/project/repo")?;
        assert_eq!(pattern.provider, Some("gitlab.com".to_string()));
        assert_eq!(pattern.path, "company/project/repo".to_string());
        Ok(())
    }

    /// Every spelling of a clone URL a forge offers names the same repo, and so
    /// has to parse to the same pattern — otherwise the path it occupies in the
    /// workspace depends on which button the user copied.
    #[test]
    fn a_pasted_clone_url_names_the_same_repo_as_the_pattern() {
        let expected = RepoPattern {
            provider: Some("github.com".to_string()),
            path: "jqlang/jq".to_string(),
        };
        for url in [
            "github.com/jqlang/jq",
            "github.com/jqlang/jq.git",
            "github.com/jqlang/jq/",
            "https://github.com/jqlang/jq",
            "https://github.com/jqlang/jq.git",
            "http://github.com/jqlang/jq.git",
            "ssh://git@github.com/jqlang/jq.git",
            "git://github.com/jqlang/jq.git",
            "git@github.com:jqlang/jq.git",
            "git@github.com:jqlang/jq",
            "  https://github.com/jqlang/jq.git  ",
        ] {
            assert_eq!(
                str::parse::<RepoPattern>(url).unwrap(),
                expected,
                "parsing {url}"
            );
        }
    }

    /// A self-hosted forge publishes a port in its clone URLs, and the clone
    /// URL workset rebuilds from the pattern has to keep it or it reaches
    /// nothing. A port is also the one ':' in a URL that is not the SSH form's
    /// host/path separator.
    #[test]
    fn a_port_stays_with_the_provider() {
        let expected = RepoPattern {
            provider: Some("git.example.com:3000".to_string()),
            path: "cilki/workset".to_string(),
        };
        for url in [
            "https://git.example.com:3000/cilki/workset.git",
            "ssh://git@git.example.com:3000/cilki/workset.git",
        ] {
            assert_eq!(
                str::parse::<RepoPattern>(url).unwrap(),
                expected,
                "parsing {url}"
            );
        }
    }

    /// Patterns that name a path rather than a remote are what `drop` and
    /// `restore` are given, and URL syntax must not rewrite them: a bare name
    /// keeps its ".git" suffix so a directory that really has one can still be
    /// named, and a traversal attempt stays recognizable to `repo_path`.
    #[test]
    fn a_local_pattern_is_left_alone() {
        for pattern in ["repo", "repo.git", "user/repo", "./repo", "../outside/repo"] {
            assert_eq!(
                str::parse::<RepoPattern>(pattern).unwrap().full_path(),
                pattern,
            );
        }
    }

    #[test]
    fn test_provider_and_path() {
        let pattern = RepoPattern {
            provider: Some("github.com".to_string()),
            path: "user/repo".to_string(),
        };
        let (provider, path) = pattern.provider_and_path().unwrap();
        assert_eq!(provider, "github.com");
        assert_eq!(path, "user/repo");
    }

    #[test]
    fn test_provider_and_path_none() {
        let pattern = RepoPattern {
            provider: None,
            path: "user/repo".to_string(),
        };
        assert!(pattern.provider_and_path().is_none());
    }

    #[test]
    fn test_full_path_with_provider() {
        let pattern = RepoPattern {
            provider: Some("github.com".to_string()),
            path: "user/repo".to_string(),
        };
        assert_eq!(pattern.full_path(), "github.com/user/repo");
    }

    #[test]
    fn test_full_path_without_provider() {
        let pattern = RepoPattern {
            provider: None,
            path: "user/repo".to_string(),
        };
        assert_eq!(pattern.full_path(), "user/repo");
    }
}
