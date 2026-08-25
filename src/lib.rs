use anyhow::{Result, bail};
use std::path::Path;
use std::path::PathBuf;
use std::str::FromStr;
use tracing::{debug, info, warn};

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

impl FromStr for RepoPattern {
    type Err = std::convert::Infallible;

    fn from_str(path: &str) -> std::result::Result<Self, Self::Err> {
        // If the first component looks like a domain (contains '.'), it's a provider
        Ok(match path.split_once('/') {
            Some((first, rest)) if first.contains('.') => Self {
                provider: Some(first.to_string()),
                path: rest.to_string(),
            },
            _ => Self {
                provider: None,
                path: path.to_string(),
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
    /// The submodule name from .gitmodules
    pub name: String,
    /// Relative path within parent repo
    pub path: PathBuf,
    /// Clone URL
    pub url: String,
    /// Whether submodule is checked out
    pub initialized: bool,
}

/// Recursively find "top-level" git repositories.
/// This function will not traverse into .git directories or nested git repositories.
pub fn find_git_repositories(path: &Path) -> Result<Vec<PathBuf>> {
    debug!(path = %path.display(), "Recursively searching for git repositories");
    let mut found: Vec<PathBuf> = Vec::new();

    // Check if this path itself is a git repository
    if path.join(".git").exists() {
        found.push(path.to_path_buf());
        return Ok(found); // Don't traverse into git repositories
    }

    // Otherwise, recursively search subdirectories
    if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.filter_map(|e| e.ok()) {
            let entry_path = entry.path();

            // Only traverse directories
            if entry_path.is_dir() {
                match find_git_repositories(&entry_path) {
                    Ok(mut repos) => found.append(&mut repos),
                    Err(e) => {
                        // Log but don't fail on permission errors
                        debug!(path = %entry_path.display(), error = %e, "Skipping directory");
                    }
                }
            }
        }
    }

    Ok(found)
}

/// Find all submodules in a git repository by parsing the .gitmodules file
pub fn find_submodules_in_repo(repo_path: &Path) -> Result<Vec<SubmoduleInfo>> {
    let gitmodules_path = repo_path.join(".gitmodules");

    // If .gitmodules doesn't exist, return empty vec
    if !gitmodules_path.exists() {
        return Ok(Vec::new());
    }

    let content = std::fs::read_to_string(&gitmodules_path)?;
    let mut submodules = Vec::new();

    // Simple parser for .gitmodules INI format
    let mut current_name: Option<String> = None;
    let mut current_path: Option<PathBuf> = None;
    let mut current_url: Option<String> = None;

    for line in content.lines() {
        let line = line.trim();

        // Skip empty lines and comments
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }

        // Parse [submodule "name"] section headers
        if line.starts_with('[') && line.ends_with(']') {
            // Save previous submodule if we have all required fields
            if let (Some(name), Some(path), Some(url)) =
                (current_name.take(), current_path.take(), current_url.take())
            {
                // Check if submodule is initialized
                let initialized = repo_path.join(&path).join(".git").exists();

                submodules.push(SubmoduleInfo {
                    name: name.clone(),
                    path,
                    url,
                    initialized,
                });
            }

            // Extract submodule name from [submodule "name"]
            if let Some(start) = line.find('"')
                && let Some(end) = line.rfind('"')
                && start < end
            {
                current_name = Some(line[start + 1..end].to_string());
            }
            continue;
        }

        // Parse key = value lines
        if let Some(eq_pos) = line.find('=') {
            let key = line[..eq_pos].trim();
            let value = line[eq_pos + 1..].trim();

            match key {
                "path" => current_path = Some(PathBuf::from(value)),
                "url" => current_url = Some(value.to_string()),
                _ => {} // Ignore other fields
            }
        }
    }

    // Don't forget the last submodule
    if let (Some(name), Some(path), Some(url)) = (current_name, current_path, current_url) {
        let initialized = repo_path.join(&path).join(".git").exists();

        submodules.push(SubmoduleInfo {
            name,
            path,
            url,
            initialized,
        });
    }

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

/// Repository status information
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepoStatus {
    /// Repository is clean (has commits, no changes, no unpushed)
    Clean,
    /// Repository has uncommitted changes or untracked files
    Dirty,
    /// Repository has no commits yet
    NoCommits,
    /// Repository has unpushed commits (but is otherwise clean)
    Unpushed,
}

/// Check repository status (commits, changes, unpushed) in a single pass
pub fn check_repo_status(repo_path: &Path) -> Result<RepoStatus> {
    let repo = match gix::open(repo_path) {
        Ok(r) => r,
        Err(e) => {
            warn!(
                path = %repo_path.display(),
                error = %e,
                "Failed to open repository"
            );
            return Ok(RepoStatus::NoCommits);
        }
    };
    check_repo_status_with_handle(&repo, repo_path)
}

/// Check repository status and get modification time in a single repo open
/// and a single worktree scan
pub fn check_repo_status_and_modification_time(
    repo_path: &Path,
) -> Result<(RepoStatus, Option<std::time::SystemTime>)> {
    let repo = match gix::open(repo_path) {
        Ok(r) => r,
        Err(e) => {
            warn!(
                path = %repo_path.display(),
                error = %e,
                "Failed to open repository"
            );
            return Ok((RepoStatus::NoCommits, None));
        }
    };

    let (change_count, dirty_files_time) = scan_worktree_changes(&repo, repo_path);

    let Some(head_ref) = head_referent(&repo) else {
        // With no commits, the only timestamp available is from dirty files.
        // If the worktree is also clean, `dirty_files_time` is still at
        // UNIX_EPOCH, which is not a real modification time — report None so
        // callers don't render it as a spurious "56y ago".
        let mod_time = (change_count > 0).then_some(dirty_files_time);
        return Ok((RepoStatus::NoCommits, mod_time));
    };

    if change_count > 0 {
        // For dirty repos, use the max of last commit time and dirty file times
        let commit_time = get_last_commit_time(&repo).unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        return Ok((RepoStatus::Dirty, Some(commit_time.max(dirty_files_time))));
    }

    let status = check_unpushed_status(&repo, head_ref);
    Ok((status, get_last_commit_time(&repo).ok()))
}

/// Get the HEAD reference, or None if the repository has no commits
fn head_referent(repo: &gix::Repository) -> Option<gix::Reference<'_>> {
    repo.head().ok()?.try_into_referent()
}

/// Check repository status using an already-opened repository handle.
/// Stops scanning the worktree at the first change found.
fn check_repo_status_with_handle(repo: &gix::Repository, repo_path: &Path) -> Result<RepoStatus> {
    let Some(head_ref) = head_referent(repo) else {
        return Ok(RepoStatus::NoCommits);
    };

    // Check for uncommitted changes using a single status call
    let platform = match repo.status(gix::progress::Discard) {
        Ok(p) => p,
        Err(e) => {
            warn!(
                path = %repo_path.display(),
                error = %e,
                "Failed to create status platform"
            );
            return Ok(RepoStatus::Clean);
        }
    };

    // Check both tracked changes and untracked files in one pass
    let has_changes = match platform
        .untracked_files(gix::status::UntrackedFiles::Files)
        .into_index_worktree_iter(Vec::new())
    {
        Ok(mut iter) => iter.by_ref().flatten().next().is_some(),
        Err(e) => {
            warn!(
                path = %repo_path.display(),
                error = %e,
                "Failed to check for changes"
            );
            false
        }
    };

    if has_changes {
        return Ok(RepoStatus::Dirty);
    }

    Ok(check_unpushed_status(repo, head_ref))
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
    let Some(head_ref) = head_referent(repo) else {
        bail!("Repository has no commits");
    };

    let commit = head_ref.id().object()?.try_into_commit()?;
    let timestamp = commit.time()?.seconds;

    Ok(std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(timestamp as u64))
}

/// Count uncommitted changes and untracked files in the worktree. Walks the
/// full status iterator; use the check_repo_status* functions when only a
/// boolean is needed, since they stop at the first change.
pub fn count_worktree_changes(repo_path: &Path) -> Result<usize> {
    let repo = gix::open(repo_path)?;
    Ok(scan_worktree_changes(&repo, repo_path).0)
}

/// Scan the worktree once, returning the number of changed or untracked files
/// and the most recent modification time among them
fn scan_worktree_changes(
    repo: &gix::Repository,
    repo_path: &Path,
) -> (usize, std::time::SystemTime) {
    let mut change_count = 0;
    let mut latest_time = std::time::SystemTime::UNIX_EPOCH;

    let platform = match repo.status(gix::progress::Discard) {
        Ok(p) => p,
        Err(e) => {
            warn!(
                path = %repo_path.display(),
                error = %e,
                "Failed to create status platform"
            );
            return (change_count, latest_time);
        }
    };

    // Iterate both tracked changes and untracked files in one pass
    match platform
        .untracked_files(gix::status::UntrackedFiles::Files)
        .into_index_worktree_iter(Vec::new())
    {
        Ok(iter) => {
            for item in iter.flatten() {
                change_count += 1;
                let file_path = repo_path.join(gix::path::from_bstr(item.rela_path()));
                if let Ok(metadata) = std::fs::metadata(&file_path)
                    && let Ok(modified) = metadata.modified()
                    && modified > latest_time
                {
                    latest_time = modified;
                }
            }
        }
        Err(e) => {
            warn!(
                path = %repo_path.display(),
                error = %e,
                "Failed to check for changes"
            );
        }
    }

    (change_count, latest_time)
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

    /// Load workspace from current directory.
    pub fn load() -> Result<Option<Self>> {
        let mut workspace_root = std::env::current_dir()?;

        // Search up for a .workset/ directory
        loop {
            let workset_dir = workspace_root.join(".workset");
            if workset_dir.exists() && workset_dir.is_dir() {
                let workspace = Workspace {
                    path: workspace_root.display().to_string(),
                };

                debug!(workspace_path = %workspace.path, "Found workspace");

                // Validate the workspace configuration
                workspace.validate()?;

                // Make sure library directory exists
                std::fs::create_dir_all(workspace.library_path_buf())
                    .map_err(|e| anyhow::anyhow!("Failed to create library directory: {}", e))?;

                return Ok(Some(workspace));
            }

            // Try parent directory
            match workspace_root.parent() {
                Some(parent) => workspace_root = parent.to_path_buf(),
                None => return Ok(None),
            }
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

    /// Search the workspace for local repos matching the given pattern.
    pub fn search(&self, pattern: &RepoPattern) -> Result<Vec<PathBuf>> {
        find_git_repositories(&Path::new(&self.path).join(pattern.full_path()))
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

    /// Drop a repository from this workspace
    pub fn drop(&self, pattern: &RepoPattern, delete: bool, force: bool) -> Result<()> {
        debug!("Drop requested for pattern: {:?}", pattern);

        let repos = self.search(pattern)?;

        if repos.is_empty() {
            warn!(pattern = %pattern.full_path(), "No repositories found matching pattern");
            return Ok(());
        }

        for repo in repos {
            self.drop_repo(&repo, delete, force)?;
        }
        Ok(())
    }

    /// Drop all repositories in the current directory
    pub fn drop_all(&self, delete: bool, force: bool) -> Result<()> {
        debug!("Drop all requested in current directory");

        let cwd = std::env::current_dir()?;
        let mut dropped = 0;
        let mut skipped = 0;

        for repo in find_git_repositories(&cwd)? {
            if self.drop_repo(&repo, delete, force)? {
                dropped += 1;
            } else {
                skipped += 1;
            }
        }

        if dropped > 0 {
            info!(count = dropped, "Dropped repositories");
        }
        if skipped > 0 {
            warn!(
                count = skipped,
                "Skipped repositories - use --force to drop anyway"
            );
        }

        Ok(())
    }

    /// Drop a single repository: store it in the library (unless deleting) and
    /// remove it from the workspace. Returns false if the repo was skipped
    /// because it has uncommitted or unpushed changes.
    fn drop_repo(&self, repo: &Path, delete: bool, force: bool) -> Result<bool> {
        // Check for uncommitted changes unless --force is given
        if !force {
            match check_repo_status(repo)? {
                RepoStatus::Dirty => {
                    warn!(repo = %repo.display(), "Refusing to drop repository with uncommitted changes");
                    warn!("Use --force to drop anyway");
                    return Ok(false);
                }
                RepoStatus::Unpushed => {
                    warn!(repo = %repo.display(), "Refusing to drop repository with unpushed commits");
                    warn!("Use --force to drop anyway");
                    return Ok(false);
                }
                _ => {}
            }
        }

        if !delete {
            // Store the repository in the library using workspace-relative path
            let relative_path = repo
                .strip_prefix(&self.path)
                .unwrap_or(repo)
                .to_string_lossy()
                .trim_start_matches('/')
                .to_string();
            self.store_in_library(&relative_path)?;
        }

        // Remove the directory
        debug!(path = ?repo, "Removing directory");
        std::fs::remove_dir_all(repo)?;
        Ok(true)
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
            let dest_path = Path::new(&self.path).join(pattern.full_path());

            gix_clone(&clone_url, &dest_path)?;
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

    /// Restore a repository from the library to the workspace.
    /// relative_path: the relative path of the repo within the workspace (e.g. "github.com/user/repo")
    pub fn restore_from_library(&self, relative_path: &str) -> Result<()> {
        let library_path = self.library_path();
        let source = format!("{}/{}", library_path, relative_path);
        let dest = format!("{}/{}", self.path, relative_path);

        // Verify the library entry exists
        if std::fs::metadata(&source).is_err() {
            bail!(
                "Repository not found in library for path: {}",
                relative_path
            );
        }

        // The library config is the original workspace config, moved wholesale
        // by store_in_library. Read it before cloning so it can be restored
        // verbatim afterwards.
        let source_config = std::fs::read_to_string(Path::new(&source).join("config"))?;

        // Clone from the library using gix
        gix_clone(&source, Path::new(&dest))?;

        // Replace the fresh clone's config (which points its origin at the
        // library) with the original config, preserving remotes, workset.*
        // keys, and any other settings the repo had when it was dropped
        let dest_config_path = std::path::Path::new(&dest).join(".git/config");
        std::fs::write(&dest_config_path, set_core_bare(&source_config, false))?;

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
                // Check if this is a bare git repository
                if gix::open(current_path).is_ok() {
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
        let config = "[workset]\n\tmirror = true\n";
        let updated = set_core_bare(config, false);
        assert!(updated.contains("[core]\n\tbare = false"));
        assert!(updated.contains("mirror = true"));
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
        assert_eq!(status, RepoStatus::NoCommits);
        // With an untracked file present there is a real time to report,
        // and it must not be the UNIX_EPOCH sentinel.
        let time = mod_time.expect("expected a modification time from the untracked file");
        assert!(time > std::time::SystemTime::UNIX_EPOCH);
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

        let repos = find_git_repositories(base_path).unwrap();

        assert_eq!(repos.len(), 2);
        assert!(repos.iter().any(|p| p.ends_with("repo1")));
        assert!(repos.iter().any(|p| p.ends_with("repo2")));
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
