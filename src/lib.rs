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
    /// A pattern that names a path verbatim, for callers that already hold a
    /// workspace-relative path rather than something the user typed.
    ///
    /// Parsing is the wrong way to build one of those. [`FromStr`] reads its
    /// input as a clone URL — it strips a `.git` suffix, drops everything
    /// before `://`, drops the userinfo before an `@`, and turns the `:` of
    /// `host:owner` into a `/` — so a directory whose *name* happens to be
    /// URL-shaped comes back out as a different path. `github.com/user/repo.git`
    /// becomes `github.com/user/repo`, `evil@github.com/thing` becomes
    /// `github.com/thing`, and `host:owner` becomes `host/owner`. Those are all
    /// legal directory names, and a `drop` that rewrites one moves (or, with
    /// `--delete`, removes) a repo the user never named.
    ///
    /// The path still goes through [`Workspace::repo_path`] before anything is
    /// touched, so a verbatim pattern is no more able to escape the workspace
    /// than a parsed one.
    pub fn from_relative_path(path: &str) -> Self {
        Self {
            provider: None,
            path: path.to_string(),
        }
    }

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

    find_git_repositories_below(path)
}

/// The repos under `path`, without `path` itself ever being one of them.
///
/// Same walk as [`find_git_repositories`], minus the test that stops it on
/// the spot when the starting directory is a repo. Only the workspace root
/// is searched this way: see [`Workspace::repos_under`].
fn find_git_repositories_below(path: &Path) -> Vec<PathBuf> {
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

/// Return the config text with every `worktree` entry removed from its
/// `[core]` sections.
///
/// `core.worktree` names the directory a git directory keeps its worktree in,
/// and git honours it over the directory the `.git` is sitting in. workset's
/// model leaves no room for that: a repo in the workspace *is* the directory
/// holding its `.git`, which is the path a drop moves into the library and a
/// restore checks back out. A library entry that carried the key aimed the
/// restore's checkout wherever the key pointed — the repo's tracked files were
/// written to an absolute path outside the workspace, and the directory the
/// restore had just made in the workspace was left empty — and `.git` is part
/// of whatever a repo was when it arrived, so the key need not be one the user
/// ever set.
///
/// Removing it costs the repo nothing a drop hadn't already taken: a relative
/// path no longer resolves once the git directory lives in the library, and an
/// absolute one names the worktree the drop just deleted.
fn strip_core_worktree(config: &str) -> String {
    let mut kept = String::with_capacity(config.len());
    let mut in_core = false;
    // A value ending in '\' continues on the next line, so a dropped entry
    // has to take its continuation lines with it rather than leave them
    // behind as config of their own
    let mut continuing = false;

    for line in config.split_inclusive('\n') {
        let trimmed = line.trim();
        if continuing {
            continuing = trimmed.ends_with('\\');
            continue;
        }
        if trimmed.starts_with('[') {
            // Section names are case-insensitive, and `core` takes no
            // subsection, so a `[core "..."]` header is some other section
            in_core = trimmed.eq_ignore_ascii_case("[core]");
        } else if in_core {
            let key = trimmed.split('=').next().unwrap_or_default().trim();
            if key.eq_ignore_ascii_case("worktree") {
                continuing = trimmed.ends_with('\\');
                continue;
            }
        }
        kept.push_str(line);
    }

    kept
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

/// Rewrite `core.bare` in the given git directory's own config file, dropping
/// any `core.worktree` along with it. The library keeps its copies bare and a
/// workspace checkout is not, so a repo is flipped on its way in and back on
/// its way out; see [`strip_core_worktree`] for why a worktree the config
/// names is no longer the repo's to keep by then.
///
/// The new text is written to a file of its own and renamed over `config`,
/// rather than written where `config` currently points.
///
/// `.git` is part of whatever a repo was when it arrived — a tarball, a shared
/// directory, a copy someone handed over — so `config` need not be a file of
/// the repo's own. A symlink there is read and written straight through, and a
/// hard link shares its bytes with everything else linked to it, so an
/// in-place write landed on a file anywhere the user could write: `workset
/// drop` appended `[core] bare = true` to it, and the `workset restore` that
/// followed rewrote it again with `bare = false`. Pointed at the user's
/// `~/.gitconfig` that is `core.bare = true` set globally, which leaves every
/// non-bare repository on the machine answering "this operation must be run in
/// a work tree". A link is also the one write a drop makes that
/// [`Workspace::drop_repo`]'s containment check can't see, because the path it
/// resolves names the link and not its target.
///
/// A rename replaces the directory entry, so neither kind of link is followed
/// and nothing outside the git directory is touched. The repo keeps the config
/// it was reading — the text that comes back is what was there, plus the
/// `core.bare` change — as a file of its own; a link that was deliberate is
/// severed, which is what moving a git directory into the library would do to
/// a relative one anyway.
///
/// It also makes the rewrite atomic: a drop interrupted partway through can no
/// longer leave the repo it is about to move into the library holding a
/// truncated config, which is where its remotes are.
fn set_bare_in_config(git_dir: &Path, bare: bool) -> Result<()> {
    rewrite_config(git_dir, "config", |config| {
        set_core_bare(&strip_core_worktree(config), bare)
    })?;

    // `extensions.worktreeConfig` splits a repository's config in two, and the
    // second file carries `core.worktree` just as well as the first — so it
    // has to lose the key too, or the repo that comes back out of the library
    // still names a worktree somewhere else and every status check after the
    // restore reads that directory instead of the one in the workspace.
    if git_dir.join("config.worktree").is_file() {
        rewrite_config(git_dir, "config.worktree", strip_core_worktree)?;
    }
    Ok(())
}

/// Replace one of the git directory's config files with `rewrite` applied to
/// its text; see [`set_bare_in_config`] for why the write goes through a
/// rename rather than landing on the existing file.
fn rewrite_config(git_dir: &Path, name: &str, rewrite: impl FnOnce(&str) -> String) -> Result<()> {
    use std::io::Write;

    let config_path = git_dir.join(name);
    let config = std::fs::read_to_string(&config_path)?;
    let updated = rewrite(&config);

    // Created exclusively, so the write can't be aimed somewhere else by a
    // link planted under this name either. `remove_file` clears a stale one
    // left by an interrupted drop — and takes the link itself, never whatever
    // it points at.
    let temp_path = git_dir.join(format!("{}.workset-new", name));
    let _ = std::fs::remove_file(&temp_path);
    let write = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp_path)
        .and_then(|mut file| file.write_all(updated.as_bytes()))
        .and_then(|()| std::fs::rename(&temp_path, &config_path));
    if let Err(e) = write {
        // Nothing reads this file but the rename that was meant to consume it
        let _ = std::fs::remove_file(&temp_path);
        return Err(e.into());
    }
    Ok(())
}

/// Check out the worktree a restored git directory describes, the way the
/// final step of a clone does: build an index from what HEAD points at and
/// write those files into `worktree`.
///
/// `git_dir` is the repository that just moved out of the library, and
/// `worktree` the directory it now sits in.
fn check_out_worktree(worktree: &Path, git_dir: &Path) -> Result<()> {
    // The repo has to stop being bare before it has a worktree to check out
    // into at all
    set_bare_in_config(git_dir, false)?;

    // Opened in isolation, and written into `worktree` rather than into
    // whatever the repository answers that its worktree is. `core.worktree`
    // in the repo's own config, a `config.worktree` beside it, or a
    // `GIT_WORK_TREE` in the environment each name a directory of their own,
    // and this checkout has exactly one directory it may write to: the one
    // the restore just made for the repo inside the workspace.
    let repo = gix::open_opts(worktree, gix::open::Options::isolated())?;
    if repo.workdir().is_none() {
        bail!("{} has no worktree to check out into", worktree.display());
    }

    // An entry the library holds without any commits checks out nothing, and
    // an empty worktree matches no index at all — not even the one the repo
    // was carrying when it was dropped, which described files that aren't
    // there.
    let Some(head) = repo.head()?.try_peel_to_id()? else {
        let _ = std::fs::remove_file(git_dir.join("index"));
        return Ok(());
    };

    let tree = head.object()?.peel_to_tree()?.id;
    let mut index = repo.index_from_tree(&tree)?;
    let mut options =
        repo.checkout_options(gix::worktree::stack::state::attributes::Source::IdMapping)?;
    options.destination_is_initially_empty = true;

    gix::worktree::state::checkout(
        &mut index,
        worktree.to_owned(),
        repo.objects.clone().into_arc()?,
        &gix::progress::Discard,
        &gix::progress::Discard,
        &std::sync::atomic::AtomicBool::new(false),
        options,
    )?;

    // The index is what tells every later status check that the worktree
    // matches the commit it was checked out from; without it the whole
    // worktree reads as untracked.
    index.write(Default::default())?;
    Ok(())
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
    /// Repository holds commits no remote has (but is otherwise clean).
    ///
    /// Any ref counts, not just the checked-out branch: a branch `git
    /// checkout` left behind, and a stash, hold commits that exist nowhere
    /// else just as much as HEAD does.
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

    /// `.git` is a symlink, which git accepts as a repository: the git
    /// directory is whatever the link points at, which is not in this repo
    /// and need not even be in the workspace.
    ///
    /// This is the same statement `BorrowedGitDir` makes — my git directory
    /// lives elsewhere — written with a filesystem link instead of a
    /// `gitdir:` file, and a drop has no more to work with here. A rename
    /// moves the *link*, so the library would hold a pointer rather than the
    /// repo, while everything the git directory holds stays where it is,
    /// still shared with every other working tree pointing at it.
    LinkedGitDir,

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
            Self::LinkedGitDir => "replace the link with the git directory it points at",
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
            Self::LinkedGitDir => f.write_str("git directory is a symlink to somewhere else"),
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
    // What `.git` *is* decides whether there's a repo here the library can
    // hold, so it is stat'd without following links: a symlink is not the
    // directory it points at, and `is_file`/`is_dir` can't tell the two apart.
    let git_path = repo.join(".git");
    let Ok(git_kind) = std::fs::symlink_metadata(&git_path).map(|m| m.file_type()) else {
        // Nothing there to classify, which `find_git_repositories` doesn't
        // report, so the `.git` went away between the walk and now. Not a
        // shape of repo to refuse — the drop runs into the same missing
        // directory a moment later and says so — and refusing it here would
        // name a blocker that isn't one.
        return None;
    };

    // A repo that owns its history has a `.git` directory. A linked worktree
    // and a submodule have a `.git` *file* holding `gitdir: <path>` instead,
    // and that path is inside the repository the git directory belongs to.
    // Storing one in the library reads `<worktree>/.git/config` through that
    // file and fails with a bare ENOTDIR, which in a `workset drop` with no
    // pattern abandoned the whole request; `--delete` skips the library and
    // deletes the worktree while the owner keeps its registration.
    if git_kind.is_file() {
        return Some(DropBlocker::BorrowedGitDir);
    }

    // The third shape: `.git` is a symlink to a git directory, which git
    // reads as an ordinary repository. Everything a drop does to a `.git`
    // follows that link out of the repo — and, since the link may point
    // anywhere, out of the workspace that `drop_repo`'s containment check
    // just established:
    //
    //   * `store_in_library` writes `core.bare = true` into the *pointed-at*
    //     git directory, which leaves every working tree using it with
    //     "fatal: this operation must be run in a work tree"
    //   * the rename that follows moves the link and not the directory, so
    //     the library ends up holding a pointer to a git directory that is
    //     still in use somewhere else, and `restore` hands back a worktree
    //     wired to it rather than one of its own
    //   * the worktree is then deleted, including anything `--force` was used
    //     to discard, with nothing in the library that can bring it back
    //
    // A link is no more outstanding work than a `gitdir:` file is, so this is
    // checked ahead of `force` as well.
    if git_kind.is_symlink() {
        return Some(DropBlocker::LinkedGitDir);
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

/// How many repos [`scan_repos`] keeps in flight per core.
///
/// A repo scan is not a core grinding on numbers: it is a walk that asks the
/// kernel for a directory's entries, waits, stats what came back, waits again.
/// A worker is off the CPU for a good part of its repo, and with one worker
/// per core the core it was running on has nothing else to pick up and idles
/// until the walk comes back — which is why scanning a workspace on every core
/// came out well short of the speedup the core count promised. A second repo
/// in flight per core is what gives each core something to run while the first
/// one is waiting on the disk.
///
/// Two, and not more, because the point is covering the wait rather than
/// racing: past that the walks start competing for the same cores and the
/// cache they warmed, and the gain flattens out and then reverses.
const WORKERS_PER_CORE: usize = 2;

/// Scan every repo in `repos` with `scan` on a pool of worker threads, and
/// return what `scan` produced in the order the repos were given.
///
/// A repo is whatever the caller needs to identify one — a path, or a row
/// already carrying the name and tree position the UI gave it — so this is
/// the one worker pool every repo scan in workset runs on.
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
/// threads spawned for the work of one. A workspace holding fewer repos than
/// the pool has room for keeps that parallelism, because the share is the
/// cores divided by the workers actually started — and never less than the one
/// thread a worker needs to get its repo walked at all.
pub fn scan_repos<R, T, F>(repos: &[R], scan: F) -> Vec<T>
where
    R: Sync,
    F: Fn(&R) -> T + Send + Sync,
    T: Send,
{
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
    let workers = (cores * WORKERS_PER_CORE).min(repos.len());
    if workers <= 1 {
        return repos.iter().map(&scan).collect();
    }

    let next = std::sync::atomic::AtomicUsize::new(0);
    let scan = &scan;
    let mut scanned: Vec<(usize, T)> = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..workers)
            .map(|_| {
                let next = &next;
                scope.spawn(move || {
                    limit_status_walk_threads((cores / workers).max(1));
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
    match has_uncommitted_work(&repo, repo_path) {
        Some(true) => return Ok(RepoStatus::Dirty),
        // A status that couldn't be read is not a repo without changes
        None => return Ok(RepoStatus::Unknown),
        Some(false) => {}
    }
    // Which commits the repo holds that no remote does is a question about
    // its refs, not about the one HEAD happens to point at, so it is asked
    // before HEAD is looked at at all
    match unpublished_work(&repo) {
        Unpublished::Some => return Ok(RepoStatus::Unpushed),
        Unpublished::Unreadable => return Ok(RepoStatus::Unknown),
        Unpublished::None => {}
    }
    Ok(match head_state(&repo) {
        HeadState::Unborn => RepoStatus::NoCommits,
        HeadState::Unreadable => RepoStatus::Unknown,
        HeadState::Detached | HeadState::Branch => RepoStatus::Clean,
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

    let status = match unpublished_work(&repo) {
        Unpublished::Some => RepoStatus::Unpushed,
        Unpublished::Unreadable => return Ok((RepoStatus::Unknown, None)),
        Unpublished::None => match head_state(&repo) {
            HeadState::Unborn => {
                // No commits and a clean worktree: no timestamp to report at
                // all, so callers don't render a spurious "56y ago" from the
                // UNIX epoch.
                return Ok((RepoStatus::NoCommits, None));
            }
            HeadState::Unreadable => return Ok((RepoStatus::Unknown, None)),
            HeadState::Detached | HeadState::Branch => RepoStatus::Clean,
        },
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
enum HeadState {
    /// At a branch
    Branch,
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
fn head_state(repo: &gix::Repository) -> HeadState {
    let Ok(head) = repo.head() else {
        return HeadState::Unreadable;
    };
    if head.is_unborn() {
        return HeadState::Unborn;
    }
    match head.try_into_referent() {
        Some(_) => HeadState::Branch,
        None => HeadState::Detached,
    }
}

/// What a repository holds that no remote of its own does
enum Unpublished {
    /// Nothing: every commit the repo holds is on a remote, or the repo has
    /// no remote state to compare against
    None,
    /// At least one commit, so there is work here that exists nowhere else
    Some,
    /// The refs couldn't be read, so what the repo holds is unknown
    Unreadable,
}

/// Find out whether a repository holds commits that none of its remotes have:
/// the work a `drop --delete` destroys, and the work a plain drop takes out of
/// sight into the library.
///
/// Every local branch is asked about, not just the checked-out one. A branch
/// `git checkout` left behind holds its commits just as firmly as HEAD's does,
/// and nothing else in the workspace mentions them — which is how `drop
/// --delete` used to report a repo clean and delete the only copy of a feature
/// branch with it. The stash is included for the same reason: its commits hang
/// off `refs/stash` and no branch at all.
///
/// Remote-tracking refs are what "no remote has it" is measured against, so a
/// commit reachable from one of them is published, however far behind the ref
/// is — a merged branch left pointing into history is not unpushed work. A
/// repo with no remote-tracking refs at all (no remote, or a remote nothing
/// was ever fetched from) has nothing to measure against, and reporting every
/// local-only repo as unpushed would say nothing about any of them, so those
/// report [`Unpublished::None`] as they always have.
fn unpublished_work(repo: &gix::Repository) -> Unpublished {
    let Ok(references) = repo.references() else {
        // Refs that can't be read may name anything, and nothing may conclude
        // from that the repo holds no work
        return Unpublished::Unreadable;
    };
    let (Ok(remotes), Ok(locals)) = (references.remote_branches(), references.local_branches())
    else {
        return Unpublished::Unreadable;
    };

    // Everything the repo's remotes are known to hold. `refs/remotes/<remote>/HEAD`
    // is symbolic and names another of these, so skipping it loses nothing.
    let published: Vec<gix::ObjectId> = remotes
        .filter_map(|reference| Some(reference.ok()?.try_id()?.detach()))
        .collect();
    if published.is_empty() {
        debug!("No remote-tracking refs to compare against");
        return Unpublished::None;
    }

    let mut local = Vec::new();
    for reference in locals {
        let Ok(reference) = reference else {
            return Unpublished::Unreadable;
        };
        if let Some(id) = reference.try_id() {
            local.push(id.detach());
        }
    }
    // A stash is reachable from this ref and nothing else
    if let Ok(stash) = repo.find_reference("refs/stash")
        && let Some(id) = stash.try_id()
    {
        local.push(id.detach());
    }
    // A detached HEAD is a commit the repo holds like any other: mid-rebase,
    // mid-bisect, or just committed onto without a branch
    if let Ok(head) = repo.head_id() {
        local.push(head.detach());
    }

    // The tip of a branch that was pushed is published along with all of its
    // history, so the common case is settled without reading a commit
    if local.iter().all(|id| published.contains(id)) {
        return Unpublished::None;
    }

    // What's left needs the graph: a tip that isn't a remote's own tip may
    // still be inside the history one of them holds. Hiding the published tips
    // leaves the walk yielding exactly the commits that are reachable from a
    // local ref and from no remote-tracking ref, so one commit out of it is
    // enough of an answer.
    let walk = repo
        .rev_walk(local)
        .sorting(gix::revision::walk::Sorting::ByCommitTime(
            gix::traverse::commit::simple::CommitTimeOrder::NewestFirst,
        ))
        .with_hidden(published)
        .all();
    match walk.map(|mut walk| walk.next()) {
        Ok(None) => Unpublished::None,
        Ok(Some(Ok(_))) => Unpublished::Some,
        // History that can't be walked can't vouch for anything in it, and a
        // repo that may be holding the only copy of something is not clean
        Ok(Some(Err(e))) | Err(e) => {
            warn!(error = %e, "Failed to walk for unpublished commits");
            Unpublished::Unreadable
        }
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

/// The point in time a commit timestamp of `seconds` relative to the epoch
/// names, or None when this platform can't represent it.
///
/// A commit's time is *content*: a signed count of seconds written into the
/// commit object by whoever made it, which git neither bounds nor checks
/// against anything. A time before the epoch is what a wrong clock or an
/// importer with a bad date leaves behind — and a repo that arrived as files
/// (a tarball, a shared directory, a copy someone handed over) carries
/// whatever its objects say, chosen by whoever assembled it.
///
/// So the conversion has to be total. Casting the seconds to `u64` and adding
/// them to the epoch was not: every negative count turned into some 584
/// billion years after 1970, which `SystemTime` cannot represent and refuses
/// by panicking. One commit dated before 1970 anywhere in the workspace was
/// then enough to take down everything that reports when a repo last changed
/// — a shell completion exited 101 in the middle of a TAB, and the TUI's scan
/// thread died with the panic printed over the alternate screen, so no row
/// ever got a status.
fn commit_time(seconds: i64) -> Option<std::time::SystemTime> {
    let distance = std::time::Duration::from_secs(seconds.unsigned_abs());
    if seconds < 0 {
        std::time::SystemTime::UNIX_EPOCH.checked_sub(distance)
    } else {
        std::time::SystemTime::UNIX_EPOCH.checked_add(distance)
    }
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

    // A timestamp this machine has no point in time for is a time that can't
    // be reported, which every caller already handles: the repo is listed
    // without one rather than the whole listing going down with it
    match commit_time(timestamp) {
        Some(time) => Ok(time),
        None => bail!("Commit is dated {} seconds from the epoch", timestamp),
    }
}

/// Walk everything the repository holds that its last commit doesn't — staged
/// changes, unstaged changes and untracked files — in a single status pass,
/// calling `visit` with the worktree path of each, or None when git reports a
/// path this platform can't represent. Stops as soon as `visit` returns false,
/// so callers that only need to know whether there is any such work don't pay
/// for the rest of the walk.
///
/// Returns false when the walk couldn't be completed, in which case `visit`
/// never saw the changes the repo may well hold. Callers must not read that as
/// a repo with nothing uncommitted: that is what let `drop` delete work it
/// couldn't see.
#[must_use]
fn walk_uncommitted_changes(
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

    // Staged changes, unstaged changes and untracked files all come from the
    // same iterator. The HEAD-to-index half is what `into_index_worktree_iter`
    // leaves out, and leaving it out reported a repo whose only work was
    // staged — `git add` and nothing more — as holding nothing uncommitted,
    // which is exactly the work `drop --delete` destroys.
    //
    // Renames are not tracked across that half: a rename git would collapse
    // into one entry is a deletion plus an addition either way, so the
    // similarity detection would buy nothing but the time it costs.
    let iter = match platform
        .untracked_files(gix::status::UntrackedFiles::Files)
        .tree_index_track_renames(gix::status::tree_index::TrackRenames::Disabled)
        .into_iter(Vec::new())
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

        // Both halves of the status name a path relative to the worktree: the
        // index-worktree half the entry it walked, the HEAD-index half the
        // location of the staged change (the destination, for a rewrite).
        let rela_path: &gix::bstr::BStr = match &item {
            gix::status::Item::IndexWorktree(item) => item.rela_path(),
            gix::status::Item::TreeIndex(change) => change.location(),
        };

        // An entry whose path this platform can't represent is still a change,
        // so hand it to `visit` unnamed rather than dropping it and risking
        // uncommitted work reported as none
        let path = match gix::path::from_bstr(rela_path) {
            Ok(rela_path) => Some(repo_path.join(rela_path)),
            Err(e) => {
                warn!(
                    path = %repo_path.display(),
                    rela_path = %rela_path,
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

/// Whether the repo holds any staged change, unstaged change or untracked
/// file, or None when it couldn't be read and the answer is unknown
fn has_uncommitted_work(repo: &gix::Repository, repo_path: &Path) -> Option<bool> {
    let mut dirty = false;
    let readable = walk_uncommitted_changes(repo, repo_path, |_| {
        dirty = true;
        false
    });
    readable.then_some(dirty)
}

/// The most recent modification time among the staged, changed and untracked
/// files, or None when there is nothing uncommitted. Files that can't be
/// stat'd — a staged deletion's, for one — count as the UNIX epoch, so a repo
/// holding uncommitted work always yields some time.
///
/// The flag is false when the status couldn't be read, in which case the time
/// says nothing about what the repo holds.
fn dirty_files_time(
    repo: &gix::Repository,
    repo_path: &Path,
) -> (bool, Option<std::time::SystemTime>) {
    let mut latest = None;
    let readable = walk_uncommitted_changes(repo, repo_path, |file_path| {
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

    /// Every repo in the working set: the repos the workspace holds, found by
    /// walking it from the root.
    pub fn repos(&self) -> Vec<PathBuf> {
        self.repos_under(Path::new(&self.path))
    }

    /// The repos of the working set that live under `path`.
    ///
    /// The workspace root is never one of them, even when it holds a `.git`
    /// of its own. A repo, to workset, is something it can drop into the
    /// library and restore later, and the root is neither: dropping it would
    /// mean moving the library inside itself, which is why
    /// [`drop_repo`](Self::drop_repo) refuses it and why
    /// [`resolve_pattern`](Self::resolve_pattern) won't name it. Counting it
    /// as a repo anyway also hid every real one, because the walk stops at
    /// the first `.git` on each branch and the root's sits above them all:
    /// `workset list` in a workspace that was itself a git repo showed that
    /// one nameless row and nothing else, and `workset drop` with no pattern
    /// failed on it without dropping anything.
    pub fn repos_under(&self, path: &Path) -> Vec<PathBuf> {
        if self.is_root(path) {
            find_git_repositories_below(path)
        } else {
            find_git_repositories(path)
        }
    }

    /// Whether `path` is this workspace's root directory, compared on
    /// canonical paths so the two spellings of a root reached through a
    /// symlink are still the same directory.
    fn is_root(&self, path: &Path) -> bool {
        let canonical = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
        canonical(path) == canonical(Path::new(&self.path))
    }

    /// Search the workspace for local repos matching the given pattern.
    ///
    /// Patterns are always interpreted relative to the workspace root; see
    /// [`resolve_pattern`](Self::resolve_pattern) for turning a pattern the
    /// user typed relative to their current directory into one of these.
    pub fn search(&self, pattern: &RepoPattern) -> Result<Vec<PathBuf>> {
        Ok(self.repos_under(&self.repo_path(pattern)?))
    }

    /// Turn a pattern the user typed relative to `cwd` into one
    /// [`search`](Self::search) can use.
    ///
    /// The CLI documents patterns as relative to the current directory
    /// (`workset drop ./repo`), but [`search`](Self::search) only ever looks
    /// workspace-relative, so `./repo` — and a bare `repo` — matched nothing
    /// whenever the current directory wasn't the workspace root.
    ///
    /// A pattern that names an existing path inside the workspace is that
    /// path, and comes back as a verbatim one
    /// ([`RepoPattern::from_relative_path`]) — the directory is already
    /// located, so there is nothing left to interpret and re-reading it as a
    /// clone URL would only rename it. Anything else is a pattern in the
    /// `[provider]/<path>` sense and is parsed, so a pasted clone URL still
    /// names the repo it was cloned into.
    pub fn resolve_pattern(&self, cwd: &Path, pattern: &str) -> RepoPattern {
        let resolved = Path::new(&self.path)
            .canonicalize()
            .and_then(|root| Ok((root, cwd.join(pattern).canonicalize()?)))
            .ok()
            .and_then(|(root, target)| {
                let relative = target.strip_prefix(root).ok()?;
                // The workspace root itself isn't a pattern for any repo
                (!relative.as_os_str().is_empty()).then(|| relative.display().to_string())
            });

        match resolved {
            Some(relative) => RepoPattern::from_relative_path(&relative),
            // Infallible
            None => pattern.parse().unwrap(),
        }
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
        for repo in self.repos_under(&cwd) {
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
            .and_then(|resolved| Some(resolved.strip_prefix(&root).ok()?.to_path_buf()));
        let Some(relative) = relative else {
            bail!("{} is outside the workspace", repo.display());
        };
        // An empty relative path is the workspace root itself, which is not a
        // repo to drop even when it happens to contain a `.git`: the library
        // it would be moved into lives inside it. Saying so beats the
        // "outside the workspace" this used to report about the one directory
        // that is unmistakably in it.
        if relative.as_os_str().is_empty() {
            bail!(
                "{} is the workspace itself, not a repo in it",
                repo.display()
            );
        }
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
        set_bare_in_config(Path::new(&source), true)?;

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
    /// The worktree around it is checked out from the git directory that just
    /// arrived. It used to be cloned out of the library instead, and the
    /// clone's own git directory thrown away again immediately — which meant
    /// every restore wrote a second copy of the repo's whole object store to
    /// disk and then deleted it, for nothing but the worktree the checkout at
    /// the end of it produced. That copy is the bulk of what a restore costs,
    /// and it grows with the repo's history rather than with its worktree.
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

        // A restore materializes a worktree where the workspace has none, so a
        // path already holding something is not this repo's to write over —
        // and the rollback below would take whatever is there with it. The
        // clone this replaced refused a non-empty destination for the same
        // reason.
        if std::fs::read_dir(&dest).is_ok_and(|mut entries| entries.next().is_some()) {
            bail!("{} already exists in the workspace", relative_path);
        }

        let git_dir = dest.join(".git");
        let root = PathBuf::from(&self.path);
        std::fs::create_dir_all(&dest).map_err(|e| {
            anyhow::anyhow!("Failed to create {} in the workspace: {}", relative_path, e)
        })?;

        // The git directory moves, rather than being copied: that is what
        // keeps every branch, tag and reflog the repo had, and it is a rename
        // within the workspace, so it costs nothing per object.
        if let Err(e) = std::fs::rename(&source, &git_dir) {
            prune_empty_dirs(&root, &dest);
            bail!("Failed to move repository out of the library: {}", e);
        }

        // Everything from here on writes inside the destination, so a failure
        // puts the git directory back the way the library had it — bare, and
        // under the library path — instead of leaving the repo in a
        // half-restored workspace directory the library no longer holds a copy
        // of.
        if let Err(e) = check_out_worktree(&dest, &git_dir) {
            let _ = set_bare_in_config(&git_dir, true);
            let _ = std::fs::rename(&git_dir, &source);
            let _ = std::fs::remove_dir_all(&dest);
            prune_empty_dirs(&root, &dest);
            return Err(e);
        }

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

    /// `core.worktree` points git at a worktree of its own choosing, which a
    /// repo in the workspace does not get to have, so the key goes whenever
    /// the config is rewritten — whatever case it is spelled in, and wherever
    /// in the `[core]` section it sits. A `worktree` key in some *other*
    /// section is that section's business and stays.
    #[test]
    fn strip_core_worktree_removes_only_the_core_entry() {
        let config = concat!(
            "[core]\n",
            "\trepositoryformatversion = 0\n",
            "\tworktree = /outside\n",
            "\tfilemode = true\n",
            "[CORE]\n",
            "\tWorkTree=/outside/again\n",
            "[core \"sub\"]\n",
            "\tworktree = /kept/subsection\n",
            "[submodule \"x\"]\n",
            "\tworktree = /kept/other-section\n",
        );
        let stripped = strip_core_worktree(config);

        assert!(!stripped.contains("/outside"));
        assert!(stripped.contains("\trepositoryformatversion = 0\n"));
        assert!(stripped.contains("\tfilemode = true\n"));
        assert!(stripped.contains("[CORE]\n"));
        assert!(stripped.contains("/kept/subsection"));
        assert!(stripped.contains("/kept/other-section"));
    }

    /// A git config value ending in a backslash continues on the next line, so
    /// dropping the entry has to drop the rest of the value with it. Left
    /// behind, the continuation lines read as config of their own — and the
    /// first of them is a path, which git rejects as a bad config line and
    /// which would make the repo unopenable.
    #[test]
    fn strip_core_worktree_takes_a_continued_value_whole() {
        let config = "[core]\n\tworktree = /outside/one\\\n/two\\\n/three\n\tfilemode = true\n";
        let stripped = strip_core_worktree(config);

        assert_eq!(stripped, "[core]\n\tfilemode = true\n");
    }

    /// A config with nothing to strip comes back byte for byte, trailing
    /// newline included: the rewrite is only meant to remove the one key.
    #[test]
    fn strip_core_worktree_leaves_an_innocent_config_alone() {
        let config = "[core]\n\tbare = false\n[remote \"origin\"]\n\turl = x\n";
        assert_eq!(strip_core_worktree(config), config);
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
            workspace.resolve_pattern(&parent, "./project").full_path(),
            "github.com/user/project"
        );
        assert_eq!(
            workspace.resolve_pattern(&parent, "project").full_path(),
            "github.com/user/project"
        );

        // A full workspace-relative pattern names nothing under the parent, so
        // it is left alone and still matches from there
        assert_eq!(
            workspace
                .resolve_pattern(&parent, "github.com/user/project")
                .full_path(),
            "github.com/user/project"
        );

        // From the workspace root, a workspace-relative pattern is unchanged
        assert_eq!(
            workspace
                .resolve_pattern(temp_dir.path(), "github.com/user/project")
                .full_path(),
            "github.com/user/project"
        );
    }

    /// A directory name is not a URL, however much it looks like one. Parsing
    /// one as a clone URL renames it — `.git` comes off the end, the userinfo
    /// before an '@' goes, and the ':' of `host:owner` turns into a '/' — and
    /// `drop` then moves or deletes whatever the rewritten name happens to
    /// land on instead of the repo the user named.
    #[test]
    fn a_pattern_that_names_a_directory_is_not_read_as_a_url() {
        let temp_dir = TempDir::new().unwrap();
        let workspace = Workspace {
            path: temp_dir.path().to_string_lossy().to_string(),
        };
        let root = temp_dir.path();

        for name in [
            "github.com/user/repo.git",
            "evil@github.com/thing",
            "host:owner",
        ] {
            fs::create_dir_all(root.join(name)).unwrap();
            assert_eq!(
                workspace.resolve_pattern(root, name).full_path(),
                name,
                "resolving {name}"
            );
        }

        // What the parse would have turned each of them into: real
        // directories, so resolving them still has to reach them and not the
        // URL-shaped neighbours above
        for name in ["github.com/user/repo", "github.com/thing", "host/owner"] {
            fs::create_dir_all(root.join(name)).unwrap();
            assert_eq!(
                workspace.resolve_pattern(root, name).full_path(),
                name,
                "resolving {name}"
            );
        }
    }

    /// A clone URL is still a pattern: `clone` takes the one a forge hands
    /// out, so the repo it put in the workspace has to be reachable by the
    /// same spelling afterwards.
    #[test]
    fn a_pattern_that_names_nothing_on_disk_is_still_read_as_a_url() {
        let temp_dir = TempDir::new().unwrap();
        let workspace = Workspace {
            path: temp_dir.path().to_string_lossy().to_string(),
        };
        let root = temp_dir.path();

        for url in [
            "https://github.com/jqlang/jq.git",
            "git@github.com:jqlang/jq.git",
            "github.com/jqlang/jq.git",
        ] {
            assert_eq!(
                workspace.resolve_pattern(root, url).full_path(),
                "github.com/jqlang/jq",
                "resolving {url}"
            );
        }
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
        assert_eq!(
            workspace.resolve_pattern(&root, "missing").full_path(),
            "missing"
        );

        // Neither are paths that escape the workspace
        assert_eq!(
            workspace.resolve_pattern(&root, "../outside").full_path(),
            "../outside"
        );

        // Nor the workspace root itself, which names no repo
        assert_eq!(workspace.resolve_pattern(&root, ".").full_path(), ".");
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

    /// `workset init` in a directory that is already a git repo — a checkout
    /// of someone's `~/src`, a dotfiles repo, anything — used to leave the
    /// tool blind: the walk stops at the first `.git` it finds on each branch,
    /// and the root's sits above every repo in the workspace, so the working
    /// set came back as the root alone and not one of the repos actually in
    /// it.
    #[test]
    fn a_workspace_that_is_itself_a_repo_reports_the_repos_in_it() {
        let temp_dir = TempDir::new().unwrap();
        let root = temp_dir.path();
        let workspace = Workspace {
            path: root.to_string_lossy().to_string(),
        };
        fs::create_dir_all(workspace.library_path()).unwrap();
        gix::init(root).unwrap();

        let one = root.join("github.com/user/one");
        repo_with_a_commit(&one);
        let two = root.join("github.com/user/two");
        repo_with_a_commit(&two);

        // The repos in the workspace, and only those: the root is a directory
        // workset keeps repos in, not one of the repos it keeps
        assert_eq!(workspace.repos(), vec![one.clone(), two.clone()]);

        // The same walk backs every pattern that names the root, so
        // `workset drop` with no pattern and `workset drop .` see them too
        for pattern in ["", ".", "./"] {
            assert_eq!(
                workspace
                    .search(&pattern.parse::<RepoPattern>().unwrap())
                    .unwrap(),
                vec![one.clone(), two.clone()],
                "pattern {pattern:?}"
            );
        }
        assert_eq!(workspace.repos_under(root), vec![one.clone(), two.clone()]);

        // A repo below the root still ends the walk there, so a git directory
        // inside one of them is that repo's content and not a row of its own
        fs::create_dir_all(one.join("vendor/dep/.git")).unwrap();
        assert_eq!(workspace.repos(), vec![one.clone(), two]);
        assert_eq!(workspace.repos_under(&one), vec![one]);
    }

    /// Dropping "everything here" from the root of a workspace that is itself
    /// a repo takes the repos in it and leaves the workspace standing. It used
    /// to fail on the root with "outside the workspace" — the one directory
    /// that is unmistakably inside it — and drop nothing at all.
    #[test]
    fn dropping_everything_leaves_a_workspace_that_is_itself_a_repo_standing() {
        let temp_dir = TempDir::new().unwrap();
        let root = temp_dir.path();
        let workspace = Workspace {
            path: root.to_string_lossy().to_string(),
        };
        fs::create_dir_all(workspace.library_path()).unwrap();
        repo_with_a_commit(root);
        fs::write(root.join("notes.txt"), "the root's own content").unwrap();

        let repo = root.join("github.com/user/one");
        repo_with_a_commit(&repo);

        let report = workspace
            .drop(&".".parse::<RepoPattern>().unwrap(), false, false)
            .unwrap();
        assert_eq!(report.dropped, vec!["github.com/user/one".to_string()]);
        assert!(report.skipped.is_empty());
        assert!(!repo.exists());
        assert_eq!(workspace.list_library().unwrap(), ["github.com/user/one"]);

        // The workspace itself came through untouched: its git directory, the
        // library the drop just wrote to, and the files it was tracking
        assert!(root.join(".git").is_dir());
        assert!(Path::new(&workspace.library_path()).is_dir());
        assert!(root.join("notes.txt").exists());

        // And it can't be dropped by naming it either, however it is spelled
        for pattern in ["", ".", "./"] {
            let report = workspace
                .drop(&pattern.parse::<RepoPattern>().unwrap(), true, true)
                .unwrap();
            assert!(report.is_empty(), "pattern {pattern:?}: {report:?}");
            assert!(root.join(".git").is_dir(), "pattern {pattern:?}");
            assert!(root.join("notes.txt").exists(), "pattern {pattern:?}");
        }
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

    /// A repo whose `.git` is a symlink is an ordinary repository to git, and
    /// the git directory it names is not the repo's to give away: dropping it
    /// wrote `core.bare = true` into a directory that may sit anywhere on the
    /// machine — breaking every working tree that reads through it — moved the
    /// link rather than the git directory into the library, and then deleted
    /// the worktree, leaving nothing behind that a restore could rebuild.
    #[cfg(unix)]
    #[test]
    fn drop_refuses_a_symlinked_git_dir() {
        let temp_dir = TempDir::new().unwrap();
        let workspace = Workspace {
            path: temp_dir.path().join("ws").to_string_lossy().to_string(),
        };
        fs::create_dir_all(workspace.library_path()).unwrap();

        // The git directory lives outside the workspace, as it does for a
        // checkout that keeps its history on another disk — or for one that
        // arrived as files with the link already in it
        let git_dir = temp_dir.path().join("elsewhere/tool.git");
        fs::create_dir_all(&git_dir).unwrap();
        gix::init_bare(&git_dir).unwrap();
        let config = git_dir.join("config");
        let config_before = fs::read_to_string(&config).unwrap();

        let repo = PathBuf::from(&workspace.path).join("tool");
        fs::create_dir_all(&repo).unwrap();
        std::os::unix::fs::symlink(&git_dir, repo.join(".git")).unwrap();
        fs::write(repo.join("notes.txt"), "only here").unwrap();

        let pattern = "tool".parse::<RepoPattern>().unwrap();
        // The link is not outstanding work the user can decide to discard, so
        // `--force` doesn't get past it either
        for (delete, force) in [(false, false), (false, true), (true, true)] {
            let report = workspace.drop(&pattern, delete, force).unwrap();
            assert!(report.dropped.is_empty());
            assert_eq!(
                report.skipped,
                vec![("tool".to_string(), DropBlocker::LinkedGitDir)]
            );
            // The worktree, and the git directory the link points at, are
            // exactly as they were
            assert_eq!(
                fs::read_to_string(repo.join("notes.txt")).unwrap(),
                "only here"
            );
            assert!(
                fs::symlink_metadata(repo.join(".git"))
                    .unwrap()
                    .is_symlink()
            );
            assert_eq!(fs::read_to_string(&config).unwrap(), config_before);
            assert!(!workspace.library_contains("tool"));
        }
    }

    /// `.git` is part of whatever a repo was when it arrived, so `.git/config`
    /// need not be a file of the repo's own. Written in place, the `core.bare`
    /// flip a drop makes went straight through a symlink — and through the
    /// bytes a hard link shares — and rewrote a file anywhere the user can
    /// write: `[core] bare = true` appended on the way into the library, and
    /// `bare = false` on the way back out. Aimed at the user's `~/.gitconfig`
    /// that sets `core.bare` globally, which leaves every non-bare repository
    /// on the machine refusing to work.
    ///
    /// `drop_repo`'s containment check can't catch this one: the path it
    /// resolves is the link's, and the link is inside the workspace.
    #[cfg(unix)]
    #[test]
    fn a_drop_does_not_write_through_a_linked_config() {
        for hard in [false, true] {
            let temp_dir = TempDir::new().unwrap();
            let workspace = Workspace {
                path: temp_dir.path().join("ws").to_string_lossy().to_string(),
            };
            fs::create_dir_all(workspace.library_path()).unwrap();
            let repo = PathBuf::from(&workspace.path).join("tool");
            repo_with_a_commit(&repo);

            // A file of the user's own, which is what `.git/config` names
            let victim = temp_dir.path().join("gitconfig");
            fs::write(&victim, "[core]\n\tpager = less\n").unwrap();
            let victim_before = fs::read_to_string(&victim).unwrap();

            let config = repo.join(".git/config");
            fs::remove_file(&config).unwrap();
            if hard {
                fs::hard_link(&victim, &config).unwrap();
            } else {
                std::os::unix::fs::symlink(&victim, &config).unwrap();
            }

            let pattern = "tool".parse::<RepoPattern>().unwrap();
            let report = workspace.drop(&pattern, false, false).unwrap();
            assert_eq!(report.dropped, vec!["tool".to_string()], "hard: {hard}");
            assert_eq!(
                fs::read_to_string(&victim).unwrap(),
                victim_before,
                "the drop wrote through the link (hard: {hard})"
            );

            // The repo still reads the config it was reading, as a file of its
            // own: the entry the library holds is bare, and nothing but this
            // git directory was written to say so
            let entry = PathBuf::from(workspace.library_path()).join("tool");
            let stored = entry.join("config");
            assert!(fs::symlink_metadata(&stored).unwrap().file_type().is_file());
            assert!(
                fs::read_to_string(&stored)
                    .unwrap()
                    .contains("pager = less")
            );
            assert_eq!(git(&entry, &["config", "core.bare"]), "true");

            // And the way back out leaves it alone just the same
            workspace.restore_from_library("tool").unwrap();
            assert_eq!(
                fs::read_to_string(&victim).unwrap(),
                victim_before,
                "the restore wrote through the link (hard: {hard})"
            );
            assert_eq!(git(&repo, &["config", "core.bare"]), "false");
            assert_eq!(check_repo_status(&repo).unwrap(), RepoStatus::Clean);
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

    /// A restore has to put the worktree back as well as the git directory:
    /// the library keeps the repo bare, so the files it tracks exist nowhere
    /// until the restore writes them out. The index it writes has to describe
    /// exactly what it wrote, too, or every later status check reads the whole
    /// worktree as uncommitted work and `drop` refuses to take the repo back.
    #[test]
    fn restore_writes_the_worktree_and_an_index_that_matches_it() {
        let temp_dir = TempDir::new().unwrap();
        let workspace = Workspace {
            path: temp_dir.path().join("ws").to_string_lossy().to_string(),
        };
        fs::create_dir_all(workspace.library_path()).unwrap();
        let repo = PathBuf::from(&workspace.path).join("github.com/user/project");
        repo_with_a_commit(&repo);
        fs::create_dir_all(repo.join("src/nested")).unwrap();
        fs::write(repo.join("src/nested/one.txt"), "tracked\n").unwrap();
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "--quiet", "-m", "two"]);

        // A commit only a second branch can reach, so the restore is held to
        // bringing the whole git directory back rather than just HEAD's branch
        git(&repo, &["checkout", "--quiet", "-b", "side"]);
        git(
            &repo,
            &["commit", "--quiet", "--allow-empty", "-m", "on side"],
        );
        let side = git(&repo, &["rev-parse", "side"]);
        git(&repo, &["checkout", "--quiet", "main"]);

        let pattern = "github.com/user/project".parse::<RepoPattern>().unwrap();
        let report = workspace.drop(&pattern, false, false).unwrap();
        assert_eq!(report.dropped, vec!["github.com/user/project".to_string()]);
        assert!(!repo.exists());

        workspace
            .restore_from_library("github.com/user/project")
            .unwrap();

        assert_eq!(
            fs::read_to_string(repo.join("src/nested/one.txt")).unwrap(),
            "tracked\n"
        );
        assert_eq!(check_repo_status(&repo).unwrap(), RepoStatus::Clean);
        assert_eq!(git(&repo, &["rev-parse", "side"]), side);
        assert_eq!(git(&repo, &["config", "core.bare"]), "false");
        // The library hands the repo over rather than keeping a stale copy
        assert!(!workspace.library_contains("github.com/user/project"));
    }

    /// A restore writes files, and where it writes them must not be up to the
    /// repository being restored. `core.worktree` names a worktree directory
    /// of git's own choosing and git honours it over the directory the `.git`
    /// sits in, so a library entry carrying the key sent the checkout's files
    /// to an absolute path anywhere the user could write — a dotfile
    /// directory, a `bin` on PATH, an autostart directory — and left the
    /// workspace directory the restore had just made standing empty. `.git` is
    /// part of whatever a repo was when it arrived (a tarball, a shared
    /// directory, a copy someone handed over), so the key need not be one the
    /// user ever set.
    #[test]
    fn restore_ignores_a_worktree_the_library_entrys_config_names() {
        let temp_dir = TempDir::new().unwrap();
        let workspace = Workspace {
            path: temp_dir.path().join("ws").to_string_lossy().to_string(),
        };
        fs::create_dir_all(workspace.library_path()).unwrap();
        let repo = PathBuf::from(&workspace.path).join("github.com/user/project");
        repo_with_a_commit(&repo);
        fs::write(repo.join("payload.sh"), "echo pwned\n").unwrap();
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "--quiet", "-m", "two"]);

        workspace
            .drop(
                &"github.com/user/project".parse::<RepoPattern>().unwrap(),
                false,
                false,
            )
            .unwrap();

        // Written straight into the library entry's config, the way a repo
        // that arrived with it would carry it
        let outside = temp_dir.path().join("outside");
        fs::create_dir_all(&outside).unwrap();
        let entry = PathBuf::from(workspace.library_path()).join("github.com/user/project");
        let mut config = fs::read_to_string(entry.join("config")).unwrap();
        config.push_str(&format!("\n[core]\n\tworktree = {}\n", outside.display()));
        fs::write(entry.join("config"), config).unwrap();

        workspace
            .restore_from_library("github.com/user/project")
            .unwrap();

        // The repo is checked out where the workspace put it, and the
        // directory the config named was never touched
        assert_eq!(
            fs::read_to_string(repo.join("payload.sh")).unwrap(),
            "echo pwned\n"
        );
        assert!(!outside.join("payload.sh").exists());
        assert_eq!(fs::read_dir(&outside).unwrap().count(), 0);

        // And the restored repo no longer carries the key, so the git and gix
        // calls that come after the restore — the status scan, the info
        // panel's line count, a later drop — read the worktree in the
        // workspace rather than the one the config named
        assert_eq!(check_repo_status(&repo).unwrap(), RepoStatus::Clean);
        let query = ["config", "--default", "", "--get", "core.worktree"];
        assert_eq!(
            git(&repo, &query),
            "",
            "the restored repo kept core.worktree"
        );
    }

    /// Removing `core.worktree` from the config is not on its own enough to
    /// settle where a restore writes: a repo with `extensions.worktreeConfig`
    /// set keeps a second config file, `config.worktree`, beside the first,
    /// and the `core.worktree` in *that* one redirects the checkout just the
    /// same. Rather than chase every file and environment variable that can
    /// name a worktree, the checkout is given the one directory it is allowed
    /// to write to.
    #[test]
    fn restore_ignores_a_worktree_the_entrys_worktree_config_names() {
        let temp_dir = TempDir::new().unwrap();
        let workspace = Workspace {
            path: temp_dir.path().join("ws").to_string_lossy().to_string(),
        };
        fs::create_dir_all(workspace.library_path()).unwrap();
        let repo = PathBuf::from(&workspace.path).join("github.com/user/project");
        repo_with_a_commit(&repo);
        fs::write(repo.join("payload.sh"), "echo pwned\n").unwrap();
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "--quiet", "-m", "two"]);

        workspace
            .drop(
                &"github.com/user/project".parse::<RepoPattern>().unwrap(),
                false,
                false,
            )
            .unwrap();

        let outside = temp_dir.path().join("outside");
        fs::create_dir_all(&outside).unwrap();
        let entry = PathBuf::from(workspace.library_path()).join("github.com/user/project");
        let mut config = fs::read_to_string(entry.join("config")).unwrap();
        config.push_str("\n[extensions]\n\tworktreeConfig = true\n");
        fs::write(entry.join("config"), config).unwrap();
        fs::write(
            entry.join("config.worktree"),
            format!("[core]\n\tworktree = {}\n", outside.display()),
        )
        .unwrap();

        workspace
            .restore_from_library("github.com/user/project")
            .unwrap();

        assert_eq!(
            fs::read_to_string(repo.join("payload.sh")).unwrap(),
            "echo pwned\n"
        );
        assert_eq!(fs::read_dir(&outside).unwrap().count(), 0);

        // The per-worktree config loses the key as well, so the status checks
        // that follow the restore read the worktree in the workspace
        assert_eq!(check_repo_status(&repo).unwrap(), RepoStatus::Clean);
        let query = ["config", "--default", "", "--get", "core.worktree"];
        assert_eq!(
            git(&repo, &query),
            "",
            "the restored repo kept core.worktree"
        );
    }

    /// A restore that can't finish has to leave the repo where it found it.
    /// The library entry is the only copy there is until the worktree exists,
    /// so a half-built workspace directory with the library emptied out is the
    /// repo lost.
    #[test]
    fn a_restore_that_cannot_check_out_leaves_the_repo_in_the_library() {
        let temp_dir = TempDir::new().unwrap();
        let workspace = Workspace {
            path: temp_dir.path().join("ws").to_string_lossy().to_string(),
        };
        fs::create_dir_all(workspace.library_path()).unwrap();
        let repo = PathBuf::from(&workspace.path).join("github.com/user/project");
        repo_with_a_commit(&repo);
        workspace
            .drop(
                &"github.com/user/project".parse::<RepoPattern>().unwrap(),
                false,
                false,
            )
            .unwrap();

        // HEAD's branch names a commit the object store doesn't hold, so there
        // is no tree for the checkout to write
        let entry = PathBuf::from(workspace.library_path()).join("github.com/user/project");
        fs::write(
            entry.join("refs/heads/main"),
            "0000000000000000000000000000000000000001\n",
        )
        .unwrap();

        assert!(
            workspace
                .restore_from_library("github.com/user/project")
                .is_err()
        );

        // Nothing of the restore is left in the workspace, and the library is
        // holding the repo the way it was before: whole, and bare
        assert!(!PathBuf::from(&workspace.path).join("github.com").exists());
        assert!(workspace.library_contains("github.com/user/project"));
        assert!(entry.join("HEAD").is_file());
        assert_eq!(git(&entry, &["config", "core.bare"]), "true");
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

    /// The whole way through: a drop of a URL-shaped directory name takes that
    /// directory and leaves the repo whose name the parse would have produced
    /// where it is. Getting this wrong is destructive — the repo that was not
    /// named loses its worktree, and with `--delete` loses everything.
    #[test]
    fn drop_takes_the_url_shaped_directory_it_was_given() {
        let temp_dir = TempDir::new().unwrap();
        let workspace = Workspace {
            path: temp_dir.path().join("ws").to_string_lossy().to_string(),
        };
        let root = PathBuf::from(&workspace.path);
        fs::create_dir_all(workspace.library_path()).unwrap();

        // Each pair is (what the user names, what parsing it would reach)
        for (named, bystander) in [
            ("github.com/user/repo.git", "github.com/user/repo"),
            ("evil@github.com/thing", "github.com/thing"),
            ("host:owner", "host/owner"),
        ] {
            repo_with_a_commit(&root.join(named));
            repo_with_a_commit(&root.join(bystander));

            let pattern = workspace.resolve_pattern(&root, named);
            let report = workspace.drop(&pattern, false, false).unwrap();

            assert_eq!(report.dropped, vec![named.to_string()], "dropping {named}");
            assert!(!root.join(named).exists(), "{named} should be gone");
            assert!(
                root.join(bystander).join(".git").is_dir(),
                "{bystander} should have been left alone"
            );
            assert!(workspace.library_contains(named));
        }
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

    /// Every count of seconds a commit object can carry has to come out of the
    /// conversion as a time or as nothing, because a commit's date is content
    /// and the caller is a workspace listing that must not go down with it.
    #[test]
    fn a_commit_time_is_either_converted_or_declined() {
        use std::time::{Duration, SystemTime, UNIX_EPOCH};

        assert_eq!(commit_time(0), Some(UNIX_EPOCH));
        assert_eq!(commit_time(1), Some(UNIX_EPOCH + Duration::from_secs(1)));
        // The case that used to panic: seconds before the epoch, which are
        // the dates in 1969 and earlier, not 584 billion years hence
        assert_eq!(commit_time(-1), Some(UNIX_EPOCH - Duration::from_secs(1)));
        assert_eq!(
            commit_time(-2_208_988_800),
            Some(UNIX_EPOCH - Duration::from_secs(2_208_988_800))
        );

        // And the extremes, wherever this platform's limit falls: a count it
        // has no point in time for is declined, never mapped onto a date that
        // isn't the one the commit claims
        for seconds in [i64::MIN, i64::MIN + 1, i64::MAX - 1, i64::MAX] {
            let converted: Option<SystemTime> = commit_time(seconds);
            if let Some(time) = converted {
                assert_eq!(
                    (seconds < 0),
                    time < UNIX_EPOCH,
                    "{seconds} came out on the wrong side of the epoch"
                );
            }
        }
    }

    /// Nothing stops a commit from being dated before the epoch: a wrong
    /// clock, an importer with a bad date, or a repo that arrived as files
    /// with whoever assembled it choosing what its objects say. Reporting when
    /// such a repo last changed used to panic — a shell completion exited 101
    /// in the middle of a TAB, and the TUI's scan thread died without giving
    /// a single row a status — instead of reporting 1969.
    #[test]
    fn a_repo_whose_commit_predates_the_epoch_is_still_reported() {
        let temp_dir = TempDir::new().unwrap();
        let repo = temp_dir.path().join("repo");
        repo_with_a_commit(&repo);

        // `git commit` won't take a negative date, so the commit is written
        // as an object directly — outside the worktree, which must stay clean
        // for the status assertions below
        let object = temp_dir.path().join("commit-object");
        fs::write(
            &object,
            format!(
                "tree {}\n\
                 author workset <workset@example.com> -1 +0000\n\
                 committer workset <workset@example.com> -1 +0000\n\
                 \n\
                 before the epoch\n",
                git(&repo, &["rev-parse", "HEAD^{tree}"])
            ),
        )
        .unwrap();
        let commit = git(
            &repo,
            &[
                "hash-object",
                "-w",
                "-t",
                "commit",
                "--literally",
                &object.to_string_lossy(),
            ],
        );
        git(&repo, &["update-ref", "HEAD", &commit]);

        let dated = std::time::SystemTime::UNIX_EPOCH - std::time::Duration::from_secs(1);
        assert_eq!(get_repo_modification_time(&repo).unwrap(), dated);
        assert_eq!(
            check_repo_status_and_modification_time(&repo).unwrap(),
            (RepoStatus::Clean, Some(dated))
        );
        assert_eq!(check_repo_status(&repo).unwrap(), RepoStatus::Clean);

        // What the user sees of it is a date many years ago, which is what
        // the commit says (the exact number grows with every year that
        // passes, so only the unit is asserted)
        let ago = format_time_ago(dated);
        assert!(ago.ends_with('y'), "{ago}");
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

    /// A repo with `tracked` committed on `main`, then `write` (if any)
    /// created in the worktree and `stage` run to put the result in the index.
    /// The worktree is left matching the index, which is the case that got
    /// reported clean.
    fn repo_with_staged_work(path: &Path, write: Option<&str>, stage: &[&str]) {
        repo_with_a_commit(path);
        fs::write(path.join("tracked"), "committed").unwrap();
        git(path, &["add", "tracked"]);
        git(path, &["commit", "--quiet", "-m", "tracked"]);
        if let Some(write) = write {
            fs::write(path.join(write), "staged").unwrap();
        }
        git(path, stage);
    }

    #[test]
    fn staged_work_is_dirty_not_clean() {
        // Every shape of staged change git reports in its left-hand column.
        // Each one leaves the worktree matching the index, so the
        // index-to-worktree comparison on its own sees nothing at all and
        // called the repo clean — which told `drop --delete` it was free to
        // destroy a repo whose only work had been `git add`ed.
        //
        // (what git calls it, the file to write first, how to stage it)
        let staged: [(&str, Option<&str>, &[&str]); 3] = [
            // `A ` — a file that exists in no commit
            ("addition", Some("new.txt"), &["add", "new.txt"]),
            // `M ` — a committed file changed and staged
            ("modification", Some("tracked"), &["add", "tracked"]),
            // `D ` — a committed file staged for removal
            ("deletion", None, &["rm", "--quiet", "tracked"]),
        ];

        for (name, write, stage) in staged {
            let temp_dir = TempDir::new().unwrap();
            let repo_path = temp_dir.path();
            repo_with_staged_work(repo_path, write, stage);

            assert_eq!(
                check_repo_status(repo_path).unwrap(),
                RepoStatus::Dirty,
                "a staged {name} is uncommitted work"
            );

            // The TUI reads the status through this second entry point, which
            // walks the same changes to date the row, so it must agree
            let (status, mod_time) = check_repo_status_and_modification_time(repo_path).unwrap();
            assert_eq!(status, RepoStatus::Dirty, "staged {name}, with its time");
            assert!(
                mod_time.is_some(),
                "a repo holding a staged {name} has a time to report"
            );
        }
    }

    #[test]
    fn staged_rename_is_dirty_without_rename_tracking() {
        let temp_dir = TempDir::new().unwrap();
        let repo_path = temp_dir.path();
        // `git mv` stages a deletion and an addition that git collapses into
        // one `R ` entry. Rename tracking is switched off in the walk, so this
        // arrives as the two halves — either way there is work here.
        repo_with_staged_work(repo_path, None, &["mv", "tracked", "renamed"]);

        assert_eq!(check_repo_status(repo_path).unwrap(), RepoStatus::Dirty);
    }

    #[test]
    fn commit_less_repo_with_only_staged_work_is_dirty() {
        let temp_dir = TempDir::new().unwrap();
        let repo_path = temp_dir.path();
        gix::init(repo_path).unwrap();
        fs::write(repo_path.join("notes.txt"), "unfinished").unwrap();
        git(repo_path, &["add", "notes.txt"]);

        // Staging the file is what hid it: it stopped being untracked without
        // becoming a commit, and there is no commit for it to differ from
        // either. It exists in this one index and nowhere else.
        assert_eq!(check_repo_status(repo_path).unwrap(), RepoStatus::Dirty);
    }

    #[test]
    fn a_genuinely_clean_repo_is_still_clean() {
        let temp_dir = TempDir::new().unwrap();
        let repo_path = temp_dir.path();
        repo_with_a_commit(repo_path);
        fs::write(repo_path.join("tracked"), "committed").unwrap();
        git(repo_path, &["add", "tracked"]);
        git(repo_path, &["commit", "--quiet", "-m", "tracked"]);

        // Reading HEAD against the index as well must not invent a change out
        // of a committed file whose index entry matches the commit
        assert_eq!(check_repo_status(repo_path).unwrap(), RepoStatus::Clean);
        let (status, _) = check_repo_status_and_modification_time(repo_path).unwrap();
        assert_eq!(status, RepoStatus::Clean);
    }

    #[test]
    fn drop_refuses_a_repo_whose_only_work_is_staged() {
        let temp_dir = TempDir::new().unwrap();
        let workspace = Workspace {
            path: temp_dir.path().join("ws").to_string_lossy().to_string(),
        };
        fs::create_dir_all(workspace.library_path()).unwrap();
        let repo = PathBuf::from(&workspace.path).join("staged");
        repo_with_staged_work(&repo, Some("tracked"), &["add", "tracked"]);

        let pattern = "staged".parse::<RepoPattern>().unwrap();

        // `--delete` is the destructive one: it doesn't move the repo into the
        // library, it unlinks it. A staged change lives in the index alone, so
        // deleting the repo is the end of it.
        for delete in [false, true] {
            let report = workspace.drop(&pattern, delete, false).unwrap();
            assert!(report.dropped.is_empty());
            assert_eq!(
                report.skipped,
                vec![(
                    "staged".to_string(),
                    DropBlocker::Outstanding(RepoStatus::Dirty)
                )]
            );
            assert_eq!(fs::read_to_string(repo.join("tracked")).unwrap(), "staged");
        }

        // --force is still how you say you meant it
        let report = workspace.drop(&pattern, false, true).unwrap();
        assert_eq!(report.dropped, vec!["staged".to_string()]);
    }

    /// A repo with one commit on `main`, pushed to a bare remote beside it,
    /// so that `origin/main` is a real tracking ref
    fn repo_with_a_remote(temp_dir: &Path, repo: &Path) {
        git(temp_dir, &["init", "--quiet", "--bare", "remote.git"]);
        repo_with_a_commit(repo);
        fs::write(repo.join("file.txt"), "published\n").unwrap();
        git(repo, &["add", "file.txt"]);
        git(repo, &["commit", "--quiet", "-m", "published"]);
        git(
            repo,
            &[
                "remote",
                "add",
                "origin",
                &temp_dir.join("remote.git").to_string_lossy(),
            ],
        );
        git(
            repo,
            &["push", "--quiet", "--set-upstream", "origin", "main"],
        );
    }

    /// Whether a commit exists nowhere but this repo is a question about all
    /// of its refs. Asking it of the checked-out branch alone called a repo
    /// clean while a branch beside it — or a stash — held the only copy of
    /// somebody's work.
    #[test]
    fn commits_on_any_ref_can_be_unpushed() {
        let temp_dir = TempDir::new().unwrap();
        let repo = temp_dir.path().join("repo");
        repo_with_a_remote(temp_dir.path(), &repo);
        let published = git(&repo, &["rev-parse", "HEAD"]);
        assert_eq!(check_repo_status(&repo).unwrap(), RepoStatus::Clean);

        // A branch HEAD doesn't point at, holding a commit no remote has
        git(&repo, &["checkout", "--quiet", "-b", "side"]);
        git(
            &repo,
            &["commit", "--quiet", "--allow-empty", "-m", "only on side"],
        );
        git(&repo, &["checkout", "--quiet", "main"]);
        assert_eq!(check_repo_status(&repo).unwrap(), RepoStatus::Unpushed);
        // Both ways in report the same repo the same way
        let (status, _) = check_repo_status_and_modification_time(&repo).unwrap();
        assert_eq!(status, RepoStatus::Unpushed);

        // Pushing the branch is what makes the repo clean again
        git(&repo, &["push", "--quiet", "origin", "side"]);
        assert_eq!(check_repo_status(&repo).unwrap(), RepoStatus::Clean);

        // A branch left behind in published history is not unpushed work: its
        // commits are on the remote, however far back the branch points
        git(
            &repo,
            &["commit", "--quiet", "--allow-empty", "-m", "newer"],
        );
        git(&repo, &["push", "--quiet", "origin", "main"]);
        git(&repo, &["branch", "merged", &published]);
        assert_eq!(check_repo_status(&repo).unwrap(), RepoStatus::Clean);

        // A stash's commits hang off refs/stash and nothing else, so they are
        // the only copy too — and it leaves a clean worktree behind, which is
        // what hid it from the dirty check
        fs::write(repo.join("file.txt"), "work in progress\n").unwrap();
        git(&repo, &["stash", "push", "--quiet"]);
        assert_eq!(check_repo_status(&repo).unwrap(), RepoStatus::Unpushed);
        git(&repo, &["stash", "drop", "--quiet"]);
        assert_eq!(check_repo_status(&repo).unwrap(), RepoStatus::Clean);
    }

    /// `--delete` destroys the repo instead of storing it, so the only copy of
    /// a branch nobody pushed goes with it. The drop gate exists to stop
    /// exactly that, and the checked-out branch being in sync says nothing
    /// about the rest of them.
    #[test]
    fn drop_refuses_a_repo_whose_other_branch_is_unpushed() {
        let temp_dir = TempDir::new().unwrap();
        let workspace = Workspace {
            path: temp_dir.path().join("ws").to_string_lossy().to_string(),
        };
        fs::create_dir_all(workspace.library_path()).unwrap();
        let repo = PathBuf::from(&workspace.path).join("repo");
        repo_with_a_remote(temp_dir.path(), &repo);

        git(&repo, &["checkout", "--quiet", "-b", "side"]);
        fs::write(repo.join("side.txt"), "only here\n").unwrap();
        git(&repo, &["add", "side.txt"]);
        git(&repo, &["commit", "--quiet", "-m", "only on side"]);
        let side = git(&repo, &["rev-parse", "side"]);
        git(&repo, &["checkout", "--quiet", "main"]);

        let pattern = "repo".parse::<RepoPattern>().unwrap();
        for delete in [false, true] {
            let report = workspace.drop(&pattern, delete, false).unwrap();
            assert!(report.dropped.is_empty());
            assert_eq!(
                report.skipped,
                vec![(
                    "repo".to_string(),
                    DropBlocker::Outstanding(RepoStatus::Unpushed)
                )]
            );
            assert_eq!(git(&repo, &["rev-parse", "side"]), side);
        }

        // Having pushed it, there is nothing left here only, and the repo
        // drops like any other
        git(&repo, &["push", "--quiet", "origin", "side"]);
        let report = workspace.drop(&pattern, false, false).unwrap();
        assert_eq!(report.dropped, vec!["repo".to_string()]);
        assert!(!repo.exists());
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
