use anyhow::Result;
use std::io::IsTerminal;
use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;
use tracing::debug;
use tracing::level_filters::LevelFilter;
use workset::Workspace;

/// How often `mirror --watch` re-runs; matches the TUI's SYNC_INTERVAL
const WATCH_INTERVAL: Duration = Duration::from_secs(300);

/// ANSI color codes
mod colors {
    pub const RESET: &str = "\x1b[0m";
    pub const BOLD: &str = "\x1b[1m";
    pub const CYAN: &str = "\x1b[36m";
    pub const GREEN: &str = "\x1b[32m";
    pub const YELLOW: &str = "\x1b[33m";
    pub const DIM: &str = "\x1b[2m";
}

/// Clone repositories matching the pattern. Returns false when nothing was
/// cloned, so the caller can exit non-zero.
fn clone_repos(workspace: &Workspace, pattern: &workset::RepoPattern) -> Result<bool> {
    use std::path::PathBuf;
    use std::process::Command;

    // Check if pattern is for mass cloning from github.com or gitlab.com
    if let Some((provider, path)) = pattern.provider_and_path() {
        // Check if this is a partial path for mass cloning
        if (provider == "github.com" || provider == "gitlab.com") && !path.contains('/') {
            // This is a user/org pattern like "github.com/user" - use gh/glab to mass clone
            println!("Fetching the repository list for {}/{}", provider, path);

            // Get list of repos using gh/glab
            let output = if provider == "github.com" {
                Command::new("gh")
                    .args([
                        "repo",
                        "list",
                        path,
                        "--json",
                        "nameWithOwner",
                        "--limit",
                        "1000",
                    ])
                    .output()
                    .map_err(|e| {
                        anyhow::anyhow!("Failed to run 'gh'. Is it installed? Error: {}", e)
                    })?
            } else {
                Command::new("glab")
                    .args(["repo", "list", path, "--page", "1", "--per-page", "100"])
                    .output()
                    .map_err(|e| {
                        anyhow::anyhow!("Failed to run 'glab'. Is it installed? Error: {}", e)
                    })?
            };

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                anyhow::bail!("Failed to fetch repository list: {}", stderr);
            }

            // Parse the output
            let repos = if provider == "github.com" {
                // Parse JSON output from gh
                let json_str = String::from_utf8(output.stdout)?;
                let repos_json: Vec<serde_json::Value> = serde_json::from_str(&json_str)?;
                repos_json
                    .iter()
                    .filter_map(|r| r["nameWithOwner"].as_str())
                    .map(|s| s.to_string())
                    .collect::<Vec<_>>()
            } else {
                // Parse glab output (simple list format)
                String::from_utf8(output.stdout)?
                    .lines()
                    .map(|line| line.trim().to_string())
                    .filter(|line| !line.is_empty())
                    .collect::<Vec<_>>()
            };

            if repos.is_empty() {
                eprintln!("No repositories found for {}/{}", provider, path);
                return Ok(false);
            }

            println!("Found {} repository(ies)", repos.len());

            let mut cloned = 0;
            let mut skipped = 0;
            let mut failed = 0;

            for repo in repos {
                let Ok(repo_pattern) =
                    format!("{}/{}", provider, repo).parse::<workset::RepoPattern>();

                // Check if repo already exists in workspace
                let repo_path = PathBuf::from(&workspace.path).join(repo_pattern.full_path());
                if repo_path.exists() {
                    skipped += 1;
                    continue;
                }

                // Clone the individual repo
                match clone_single_repo(workspace, &repo_pattern) {
                    Ok(true) => cloned += 1,
                    Ok(false) => skipped += 1,
                    Err(e) => {
                        failed += 1;
                        eprintln!("Failed to clone {}: {}", repo_pattern.full_path(), e);
                    }
                }
            }

            println!(
                "Cloned {} repository(ies), skipped {}, failed {}",
                cloned, skipped, failed
            );
            return Ok(failed == 0);
        }
    }

    // Not a mass clone pattern, just clone the single repo
    clone_single_repo(workspace, pattern)
}

/// Clone a single repository. Returns false when the repo was not cloned
/// because it is already in the workspace or the library.
fn clone_single_repo(workspace: &Workspace, pattern: &workset::RepoPattern) -> Result<bool> {
    use std::path::PathBuf;

    let repo_path = PathBuf::from(&workspace.path).join(pattern.full_path());

    // Check if repo already exists in workspace
    if repo_path.exists() {
        eprintln!("{} is already in the workspace", pattern.full_path());
        return Ok(false);
    }

    // Check if it exists in library first
    if workspace.library_contains(&pattern.full_path()) {
        eprintln!(
            "{} is in the library; run 'workset restore {}' instead",
            pattern.full_path(),
            pattern.full_path()
        );
        return Ok(false);
    }

    // Clone from remote
    if let Some((provider, repo_path_str)) = pattern.provider_and_path() {
        let clone_url = format!("https://{}/{}", provider, repo_path_str);

        println!("Cloning {}", clone_url);

        // TODO show progress
        workset::gix_clone(&clone_url, &repo_path)?;

        println!("Cloned {}", pattern.full_path());
        Ok(true)
    } else {
        anyhow::bail!("No provider specified. Use format like github.com/user/repo");
    }
}

/// Workspace-relative paths of the checked-out repos whose path contains
/// `pattern`, matching how the library is searched.
fn workspace_matches(workspace: &Workspace, pattern: &str) -> Result<Vec<String>> {
    Ok(workset::find_git_repositories(Path::new(&workspace.path))?
        .iter()
        .map(|repo| workspace.relative_name(repo))
        .filter(|name| name.contains(pattern))
        .collect())
}

/// Restore repositories from library matching the pattern. Returns false when
/// nothing was restored, so the caller can exit non-zero.
fn restore_repos(workspace: &Workspace, pattern: &workset::RepoPattern) -> Result<bool> {
    use std::path::PathBuf;

    // Get all repos from library
    let library_repos = workspace.list_library()?;

    // Filter repos that match the pattern
    let pattern_str = pattern.full_path();
    let matching_repos: Vec<String> = library_repos
        .iter()
        .filter(|repo| repo.contains(&pattern_str))
        .cloned()
        .collect();

    if matching_repos.is_empty() {
        // Restoring moves a repo out of the library, so one that's already
        // checked out is missing from the library rather than present in it.
        // Saying it's in the workspace beats claiming the library has nothing
        // matching, which reads like the repo was lost.
        let in_workspace = workspace_matches(workspace, &pattern_str)?;
        if !in_workspace.is_empty() {
            for repo in in_workspace {
                eprintln!("{} is already in the workspace", repo);
            }
        } else if library_repos.is_empty() {
            eprintln!("The library is empty");
        } else {
            eprintln!("No repository in the library matches '{}'", pattern_str);
        }
        return Ok(false);
    }

    let mut restored = 0;
    let mut failed = 0;

    for repo_path in matching_repos {
        // Check if already exists in workspace
        let dest_path = PathBuf::from(&workspace.path).join(&repo_path);
        if dest_path.exists() {
            eprintln!("{} is already in the workspace", repo_path);
            continue;
        }

        // Restore from library
        match workspace.restore_from_library(&repo_path) {
            Ok(_) => {
                println!("Restored {}", repo_path);
                restored += 1;
            }
            Err(e) => {
                failed += 1;
                eprintln!("Failed to restore {}: {}", repo_path, e);
            }
        }
    }

    Ok(restored > 0 && failed == 0)
}

fn main() -> Result<ExitCode> {
    // Initialize logging
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::builder()
                .with_default_directive(LevelFilter::ERROR.into())
                .from_env_lossy(),
        )
        .init();

    // Dispatch shell completions before loading the workspace. Completions run
    // on every TAB press, so they must be fast, side-effect free, and
    // infallible: discover the workspace read-only rather than going through
    // `load`, which validates and creates the library directory and would abort
    // the whole process on error.
    if let Ok(shell_type) = std::env::var("_ARGCOMPLETE_") {
        let maybe_workspace = Workspace::discover();
        return match shell_type.as_str() {
            "bash" => complete_bash(maybe_workspace).map(|()| ExitCode::SUCCESS),
            "fish" => complete_fish(maybe_workspace).map(|()| ExitCode::SUCCESS),
            _ => anyhow::bail!("Unsupported shell type: {}", shell_type),
        };
    }

    let mut args = pico_args::Arguments::from_env();

    // Print the version and exit. Handled before loading the workspace so
    // `--version` works anywhere, not just inside a valid workspace.
    if args.contains(["-V", "--version"]) {
        println!("workset {}", env!("CARGO_PKG_VERSION"));
        return Ok(ExitCode::SUCCESS);
    }

    if args.contains("--help") {
        let is_tty = std::io::stdout().is_terminal();

        // Wrap text in a color code (and reset) only when writing to a
        // terminal; otherwise emit the text unadorned.
        let paint = |code: &str, text: &str| {
            if is_tty {
                format!("{}{}{}", code, text, colors::RESET)
            } else {
                text.to_string()
            }
        };
        // A bare color code, suppressed when not writing to a terminal, for the
        // inline `{cmd}...{reset}` spans in the template.
        let code = |c: &'static str| if is_tty { c } else { "" };

        let help = format!(
            r#"{workset} {version}

{desc_header}
  Manage git repos with working sets.

{usage_header}
  {cmd}workset{reset} [--help] [--version]
  {cmd}workset{reset} init
  {cmd}workset{reset} clone <repo pattern>
  {cmd}workset{reset} restore <repo pattern>
  {cmd}workset{reset} drop [repo pattern] [--delete] [--force]
  {cmd}workset{reset} list
  {cmd}workset{reset} status
  {cmd}workset{reset} mirror [repo pattern] [--dryrun] [--watch]
{dim}
  Without a subcommand, the interactive TUI opens; '?' shows its keybindings.{reset}

{commands_header}
  {subcmd}init{reset}                                 Initialize a workspace in current directory
  {subcmd}clone{reset} {arg}<pattern>{reset}                      Clone new repository(ies) to workspace
  {subcmd}restore{reset} {arg}<pattern>{reset}                    Restore repository(ies) from library
  {subcmd}drop{reset} {arg}[pattern]{reset} {arg}[--delete]{reset} {arg}[--force]{reset}  Drop repository(ies) from workspace
{dim}                                       Without pattern: drops all in current directory
                                       With --delete: permanently delete (don't store)
                                       With --force: drop even with uncommitted changes{reset}
  {subcmd}list{reset}, {subcmd}ls{reset}                             List all repositories with their status
  {subcmd}status{reset}                               Show workspace summary and statistics
  {subcmd}mirror{reset} {arg}[pattern]{reset} {arg}[--dryrun]{reset} {arg}[--watch]{reset}
{dim}                                       Mirror pushed commits between each repo's remotes
                                       On by default; opt a repo out with
                                       'git config workset.mirror false'
                                       With --dryrun: show what would be pushed without pushing
                                       With --watch: keep syncing every 5 minutes{reset}

{examples_header}
  {cmd}workset init{reset}                              Initialize workspace here
  {cmd}workset clone github.com/user/repo{reset}        Clone a new repository
  {cmd}workset clone github.com/user{reset}             Clone all repos from github.com/user
  {cmd}workset restore repo{reset}                      Restore 'repo' from library
  {cmd}workset drop ./repo{reset}                       Drop repo (save to library)
  {cmd}workset drop{reset}                              Drop all repos in current dir
  {cmd}workset drop --delete ./old_repo{reset}          Permanently delete a repo
  {cmd}workset drop --force ./dirty_repo{reset}         Force drop repo and lose any changes
  {cmd}workset mirror --watch{reset}                    Keep mirroring pushed commits
"#,
            workset = paint(colors::BOLD, "workset"),
            version = paint(colors::CYAN, env!("CARGO_PKG_VERSION")),
            desc_header = paint(colors::BOLD, "DESCRIPTION:"),
            usage_header = paint(colors::BOLD, "USAGE:"),
            commands_header = paint(colors::BOLD, "COMMANDS:"),
            examples_header = paint(colors::BOLD, "EXAMPLES:"),
            cmd = code(colors::GREEN),
            subcmd = code(colors::CYAN),
            arg = code(colors::YELLOW),
            dim = code(colors::DIM),
            reset = code(colors::RESET),
        );
        print!("{}", help);

        return Ok(ExitCode::SUCCESS);
    }

    // Load the workspace for a subcommand.
    let maybe_workspace = Workspace::load()?;

    // Resolve the current workspace or report "not in a workspace" and return
    // failure. Used by every subcommand that operates on an existing workspace.
    macro_rules! require_workspace {
        ($ws:expr) => {
            match $ws {
                Some(workspace) => workspace,
                None => {
                    eprintln!("Not in a workspace (run 'workset init' to create one)");
                    return Ok(ExitCode::FAILURE);
                }
            }
        };
    }

    // Whether the subcommand did what it was asked to do. Anything the user
    // requested but didn't get (a repo that couldn't be dropped, a pattern that
    // matched nothing, a missing argument) clears this so the process exits
    // non-zero and scripts can tell.
    let succeeded = match args.subcommand()? {
        Some(command) => match command.as_str() {
            "init" => {
                let workspace_path = std::env::current_dir()?;
                let library_path = workspace_path.join(".workset");

                if library_path.exists() {
                    println!(
                        "Workspace already initialized in {}",
                        workspace_path.display()
                    );
                } else {
                    std::fs::create_dir_all(&library_path)?;
                    println!("Initialized workspace in {}", workspace_path.display());
                }
                true
            }
            "clone" => {
                let workspace = require_workspace!(maybe_workspace);
                if let Some(pattern_str) = args.opt_free_from_str::<String>()? {
                    let Ok(pattern) = pattern_str.parse::<workset::RepoPattern>();
                    clone_repos(&workspace, &pattern)?
                } else {
                    eprintln!("Missing repository pattern");
                    eprintln!("Usage: workset clone <pattern>");
                    false
                }
            }
            "restore" => {
                let workspace = require_workspace!(maybe_workspace);
                if let Some(pattern_str) = args.opt_free_from_str::<String>()? {
                    let Ok(pattern) = pattern_str.parse::<workset::RepoPattern>();
                    restore_repos(&workspace, &pattern)?
                } else {
                    eprintln!("Missing repository pattern");
                    eprintln!("Usage: workset restore <pattern>");
                    false
                }
            }
            "drop" => {
                let workspace = require_workspace!(maybe_workspace);
                let delete = args.contains("--delete");
                let force = args.contains("--force");

                let requested = args.opt_free_from_str::<String>()?;
                let report = match &requested {
                    Some(path) => {
                        // Patterns are relative to the current directory first,
                        // so 'workset drop ./repo' works from the repo's parent
                        let cwd = std::env::current_dir()?;
                        let Ok(pattern) = workspace
                            .resolve_pattern(&cwd, path)
                            .parse::<workset::RepoPattern>();
                        workspace.drop(&pattern, delete, force)?
                    }
                    // Drop all repos in current directory
                    None => workspace.drop_all(delete, force)?,
                };
                report_drop(&report, delete, requested.as_deref())
            }
            "list" | "ls" => {
                let workspace = require_workspace!(maybe_workspace);
                list_workspace_status(&workspace)?;
                true
            }
            "status" => {
                let workspace = require_workspace!(maybe_workspace);
                show_workspace_summary(&workspace)?;
                true
            }
            "mirror" => {
                let workspace = require_workspace!(maybe_workspace);
                let dry_run = args.contains("--dryrun");
                let watch = args.contains("--watch");
                let pattern = args.opt_free_from_str::<String>()?;
                loop {
                    mirror_repos(&workspace, pattern.as_deref(), dry_run)?;
                    if !watch {
                        break;
                    }
                    println!(
                        "  next sync in {}s (Ctrl+C to stop)",
                        WATCH_INTERVAL.as_secs()
                    );
                    std::thread::sleep(WATCH_INTERVAL);
                }
                true
            }
            _ => {
                eprintln!("Unknown command: {}", command);
                eprintln!("Run 'workset --help' for usage information");
                false
            }
        },
        None => {
            #[cfg(feature = "tui")]
            {
                let workspace = require_workspace!(maybe_workspace);
                // Open TUI for interactive workspace management
                workset::tui::run_tui(&workspace)?;
                true
            }
            #[cfg(not(feature = "tui"))]
            {
                anyhow::bail!("No command provided. TUI feature is disabled.")
            }
        }
    };

    Ok(if succeeded {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

/// Report what a drop request did and whether it did everything asked. A repo
/// left in place because of outstanding changes, or a pattern that matched
/// nothing, counts as a failure, so the caller can exit non-zero.
fn report_drop(report: &workset::DropReport, delete: bool, pattern: Option<&str>) -> bool {
    let verb = if delete { "deleted" } else { "dropped" };

    for repo in &report.dropped {
        println!("  {} - ✓ {}", repo, verb);
    }

    for (repo, status) in &report.skipped {
        let reason = match status {
            workset::RepoStatus::Dirty => "uncommitted changes",
            workset::RepoStatus::Unpushed => "unpushed commits",
            // drop_repo only ever blocks on the two statuses above
            other => {
                debug!(?other, "Unexpected drop blocker");
                "outstanding changes"
            }
        };
        eprintln!(
            "  {} - ⚠ kept ({}, use --force to drop anyway)",
            repo, reason
        );
    }

    if report.is_empty() {
        match pattern {
            Some(pattern) => eprintln!("No repository in the workspace matches '{}'", pattern),
            None => eprintln!("No repositories found in the current directory"),
        }
        return false;
    }

    report.skipped.is_empty()
}

/// List all repositories in the workspace with their status
fn list_workspace_status(workspace: &Workspace) -> Result<()> {
    let repos = workset::find_git_repositories(Path::new(&workspace.path))?;

    if repos.is_empty() {
        println!("No repositories found in workspace");
        return Ok(());
    }

    println!("Repositories in workspace ({}):", workspace.path);
    println!();

    for repo in repos {
        let repo_name = workspace.relative_name(&repo);

        let status_str = match workset::check_repo_status(&repo) {
            Ok(workset::RepoStatus::Clean) => "✓ clean".to_string(),
            Ok(workset::RepoStatus::Dirty) => "⚠ modified".to_string(),
            Ok(workset::RepoStatus::Unpushed) => "⚠ unpushed".to_string(),
            Ok(workset::RepoStatus::NoCommits) => "⚠ no commits".to_string(),
            Err(_) => "✗ error".to_string(),
        };

        println!("  {} - {}", repo_name, status_str);
    }

    Ok(())
}

/// Mirror pushed commits to the remotes of mirror-enabled repos, printing
/// per-ref results
fn mirror_repos(workspace: &Workspace, pattern: Option<&str>, dry_run: bool) -> Result<()> {
    let repos = workset::find_git_repositories(Path::new(&workspace.path))?;
    let interrupt = std::sync::atomic::AtomicBool::new(false);
    let short = workset::sync::short_ref;

    let mut matched = false;
    for repo in repos {
        let repo_name = workspace.relative_name(&repo);
        if pattern.is_some_and(|p| !repo_name.contains(p)) {
            continue;
        }
        matched = true;

        match workset::sync::sync_repo(&repo, &interrupt, dry_run, &|| {}) {
            Ok(outcome) => {
                if outcome.skipped {
                    println!("  {} - skipped (mirroring disabled)", repo_name);
                    continue;
                }
                for (remote, refname) in &outcome.pushed {
                    println!(
                        "  {} - ✓ pushed {} to {}",
                        repo_name,
                        short(refname),
                        remote
                    );
                }
                for (remote, refname) in &outcome.would_push {
                    println!(
                        "  {} - would push {} to {}",
                        repo_name,
                        short(refname),
                        remote
                    );
                }
                for (remote, refname, reason) in &outcome.conflicts {
                    println!(
                        "  {} - ⚠ {} on {}: {}",
                        repo_name,
                        short(refname),
                        remote,
                        reason
                    );
                }
                for (remote, refname, error) in &outcome.push_errors {
                    println!(
                        "  {} - ✗ push {} to {} failed: {}",
                        repo_name,
                        short(refname),
                        remote,
                        error
                    );
                }
                for (remote, error) in &outcome.fetch_errors {
                    println!("  {} - ✗ fetch {} failed: {}", repo_name, remote, error);
                }
                if outcome.offline {
                    println!("  {} - ⚠ remotes unreachable", repo_name);
                } else if outcome.pushed.is_empty()
                    && outcome.would_push.is_empty()
                    && outcome.conflicts.is_empty()
                    && outcome.push_errors.is_empty()
                    && outcome.fetch_errors.is_empty()
                {
                    println!("  {} - ✓ in sync", repo_name);
                }
            }
            Err(e) => println!("  {} - ✗ mirror failed: {}", repo_name, e),
        }
    }

    if !matched {
        println!("No repositories matched");
    }
    Ok(())
}

/// Show a summary of the workspace
fn show_workspace_summary(workspace: &Workspace) -> Result<()> {
    println!("Workspace: {}", workspace.path);
    println!();

    // Show library information
    println!("Library: {}", workspace.library_path());
    if let Ok(repos) = workspace.list_library() {
        println!("  {} repository(ies) in library", repos.len());
    }

    // Count repositories in workspace
    println!();
    if let Ok(repos) = workset::find_git_repositories(Path::new(&workspace.path)) {
        println!("Active repositories: {}", repos.len());

        let mut clean = 0;
        let mut modified = 0;
        let mut unpushed = 0;
        let mut no_commits = 0;

        for repo in &repos {
            match workset::check_repo_status(repo) {
                Ok(workset::RepoStatus::Clean) => clean += 1,
                Ok(workset::RepoStatus::Dirty) => modified += 1,
                Ok(workset::RepoStatus::Unpushed) => unpushed += 1,
                Ok(workset::RepoStatus::NoCommits) => no_commits += 1,
                Err(_) => {}
            }
        }

        if clean > 0 {
            println!("  ✓ {} clean", clean);
        }
        if modified > 0 {
            println!("  ⚠ {} with uncommitted changes", modified);
        }
        if unpushed > 0 {
            println!("  ⚠ {} with unpushed commits", unpushed);
        }
        if no_commits > 0 {
            println!("  ⚠ {} with no commits", no_commits);
        }
    }

    Ok(())
}

/// Get repository completions from configured remotes
fn get_repo_completions(workspace: &Workspace) -> Vec<String> {
    let mut repos = Vec::new();

    // Only complete with local workspace repos
    if let Ok(local_repos) = workset::find_git_repositories(Path::new(&workspace.path)) {
        for repo in local_repos {
            if let Ok(relative) = repo.strip_prefix(&workspace.path) {
                repos.push(relative.display().to_string());
            }
        }
    }

    repos.sort();
    repos.dedup();
    repos
}

/// Get repository completions with metadata (status and modification time) for fish shell
fn get_repo_completions_with_metadata(workspace: &Workspace) -> Vec<(String, String)> {
    let mut repos = Vec::new();

    // Only complete with local workspace repos
    if let Ok(local_repos) = workset::find_git_repositories(Path::new(&workspace.path)) {
        for repo in local_repos {
            if let Ok(relative) = repo.strip_prefix(&workspace.path) {
                let repo_name = relative.display().to_string();

                // Get repo status and modification time in a single repo open
                // If this fails, we'll still provide a basic completion
                let (status, mod_time) =
                    match workset::check_repo_status_and_modification_time(&repo) {
                        Ok((status, mod_time)) => (Some(status), mod_time),
                        Err(_) => (None, None),
                    };

                // Build description with status and time
                let mut desc_parts = Vec::new();

                // Add status indicator
                match status {
                    Some(workset::RepoStatus::Clean) => desc_parts.push("clean".to_string()),
                    Some(workset::RepoStatus::Dirty) => desc_parts.push("dirty".to_string()),
                    Some(workset::RepoStatus::Unpushed) => desc_parts.push("unpushed".to_string()),
                    Some(workset::RepoStatus::NoCommits) => {
                        desc_parts.push("no commits".to_string())
                    }
                    None => {} // Don't add "unknown" if status check failed
                }

                // Add modification time
                if let Some(time) = mod_time {
                    desc_parts.push(workset::format_time_ago(time));
                }

                // If we couldn't get any metadata, use a default description
                let description = if desc_parts.is_empty() {
                    "repository".to_string()
                } else {
                    desc_parts.join(", ")
                };

                repos.push((repo_name, description));
            }
        }
    }

    // Sort by repo name
    repos.sort_by(|a, b| a.0.cmp(&b.0));
    repos.dedup();
    repos
}

/// Output dynamic completions for bash
fn complete_bash(maybe_workspace: Option<Workspace>) -> Result<()> {
    let comp_line = std::env::var("COMP_LINE").unwrap_or_default();

    // COMP_POINT is the cursor's byte offset into COMP_LINE, but bash has
    // multibyte quirks where it can exceed the line length or land inside a
    // UTF-8 character; clamp it to the nearest valid boundary.
    let mut comp_point = std::env::var("COMP_POINT")
        .ok()
        .and_then(|p| p.parse::<usize>().ok())
        .unwrap_or(comp_line.len())
        .min(comp_line.len());
    while !comp_line.is_char_boundary(comp_point) {
        comp_point -= 1;
    }

    let current_line = &comp_line[..comp_point];
    let words: Vec<&str> = current_line.split_whitespace().collect();

    // The word being completed: empty if the cursor follows whitespace,
    // otherwise the last word. Bash inserts our output into COMPREPLY
    // verbatim, so we must filter candidates by this prefix ourselves.
    let (current_word, word_index) =
        if current_line.is_empty() || current_line.ends_with(char::is_whitespace) {
            ("", words.len())
        } else {
            (*words.last().unwrap(), words.len() - 1)
        };

    if word_index <= 1 {
        // Complete subcommands
        let subcommands: &[&str] = if maybe_workspace.is_some() {
            &["clone", "restore", "drop", "list", "ls", "status", "mirror"]
        } else {
            &["init"]
        };
        for subcommand in subcommands {
            if subcommand.starts_with(current_word) {
                println!("{}", subcommand);
            }
        }
    } else if let Some(workspace) = maybe_workspace {
        // Complete repository paths based on the subcommand
        let subcommand = words.get(1).unwrap_or(&"");
        if *subcommand == "restore" {
            // For restore, complete from library
            if let Ok(library_repos) = workspace.list_library() {
                for repo in library_repos {
                    if repo.starts_with(current_word) {
                        println!("{}", repo);
                    }
                }
            }
        } else {
            // For drop and other commands, complete from workspace
            for repo in get_repo_completions(&workspace) {
                if repo.starts_with(current_word) {
                    println!("{}", repo);
                }
            }
        }
    }

    Ok(())
}

/// Output dynamic completions for fish
fn complete_fish(maybe_workspace: Option<Workspace>) -> Result<()> {
    let comp_line = std::env::var("COMP_LINE").unwrap_or_default();
    let words: Vec<&str> = comp_line.split_whitespace().collect();

    // Determine what to complete based on context
    if words.len() <= 1 || (words.len() == 2 && !comp_line.ends_with(' ')) {
        // Complete subcommands
        if maybe_workspace.is_some() {
            println!("clone\tClone new repository(ies) to workspace");
            println!("restore\tRestore repository(ies) from library");
            println!("drop\tDrop one or more repositories");
            println!("list\tList all repositories with their status");
            println!("ls\tList all repositories with their status");
            println!("status\tShow workspace summary and statistics");
            println!("mirror\tMirror pushed commits to a repo's other remotes");
        } else {
            println!("init\tInitialize a workspace in current directory");
        }
    } else if let Some(workspace) = maybe_workspace {
        // Complete repository paths based on the subcommand
        let subcommand = words.get(1).unwrap_or(&"");
        if *subcommand == "restore" {
            // For restore, complete from library
            if let Ok(library_repos) = workspace.list_library() {
                for repo in library_repos {
                    println!("{}\tlibrary", repo);
                }
            }
        } else {
            // For drop and other commands, complete from workspace with metadata
            for (repo_name, description) in get_repo_completions_with_metadata(&workspace) {
                println!("{}\t{}", repo_name, description);
            }
        }
    }

    Ok(())
}
