use anyhow::Result;
use std::io::IsTerminal;
use std::path::Path;
use std::process::ExitCode;
use tracing::level_filters::LevelFilter;
use workset::Workspace;

/// ANSI color codes
mod colors {
    pub const RESET: &str = "\x1b[0m";
    pub const BOLD: &str = "\x1b[1m";
    pub const CYAN: &str = "\x1b[36m";
    pub const GREEN: &str = "\x1b[32m";
    pub const YELLOW: &str = "\x1b[33m";
    pub const DIM: &str = "\x1b[2m";
}

/// Write `args` to stdout, ending the process quietly when the reader has gone
/// away. Used through [`outln!`] and [`out!`] for everything workset prints.
///
/// Rust ignores SIGPIPE, so a reader that stops before workset is done writing
/// — `workset list | head`, `workset status | grep -q`, a pager the user quits
/// — doesn't kill the process; the next write fails with `BrokenPipe` instead.
/// `println!` answers a failed write by panicking, so what the user got was a
/// panic message and a backtrace note on stderr and exit 101, where every
/// other command in the pipeline ended without a word. Restoring SIGPIPE's
/// default disposition would fix the symptom everywhere at once, but it also
/// turns every *other* failed write into a kill, including the ones gix makes
/// talking to a local `git upload-pack` over a pipe and handles itself.
///
/// The reader deciding it has seen enough isn't a failure of the command, so
/// the exit status stays successful and scripts under `set -o pipefail` keep
/// working.
fn write_out(args: std::fmt::Arguments) {
    use std::io::Write;

    // Flushed here rather than left to the buffer, so a failed write is seen
    // where it can still be acted on instead of on the way out of main, where
    // stdout's own flush discards the error
    let mut stdout = std::io::stdout().lock();
    match stdout.write_fmt(args).and_then(|()| stdout.flush()) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => std::process::exit(0),
        // A write that failed for any other reason (a full disk, a descriptor
        // that was closed) leaves the output incomplete, which is a failure —
        // reportable only on stderr, since stdout is where it just failed.
        Err(e) => {
            eprintln!("Failed to write output: {}", e);
            std::process::exit(1);
        }
    }
}

/// `println!` for workset's own output, without the panic on a closed pipe;
/// see [`write_out`].
macro_rules! outln {
    () => { crate::write_out(format_args!("\n")) };
    ($($arg:tt)*) => { crate::write_out(format_args!("{}\n", format_args!($($arg)*))) };
}

/// `print!` for workset's own output, without the panic on a closed pipe; see
/// [`write_out`].
macro_rules! out {
    ($($arg:tt)*) => { crate::write_out(format_args!($($arg)*)) };
}

/// Clone repositories matching the pattern. Returns false when nothing was
/// cloned, so the caller can exit non-zero.
fn clone_repos(workspace: &Workspace, pattern: &workset::RepoPattern) -> Result<bool> {
    use std::process::Command;

    // Check if pattern is for mass cloning from github.com or gitlab.com
    if let Some((provider, path)) = pattern.provider_and_path() {
        // Check if this is a partial path for mass cloning
        if (provider == "github.com" || provider == "gitlab.com") && !path.contains('/') {
            // This is a user/org pattern like "github.com/user" - use gh/glab to mass clone
            outln!("Fetching the repository list for {}/{}", provider, path);

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

            outln!("Found {} repository(ies)", repos.len());

            let mut cloned = 0;
            let mut skipped = 0;
            let mut failed = 0;

            for repo in repos {
                let Ok(repo_pattern) =
                    format!("{}/{}", provider, repo).parse::<workset::RepoPattern>();

                // Check if repo already exists in workspace
                if workspace
                    .repo_path(&repo_pattern)
                    .is_ok_and(|repo_path| repo_path.exists())
                {
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

            outln!(
                "Cloned {} repository(ies), skipped {}, failed {}",
                cloned,
                skipped,
                failed
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
    let repo_path = workspace.repo_path(pattern)?;

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

        outln!("Cloning {}", clone_url);

        // TODO show progress
        workset::gix_clone(&clone_url, &repo_path)?;

        outln!("Cloned {}", pattern.full_path());
        Ok(true)
    } else {
        anyhow::bail!("No provider specified. Use format like github.com/user/repo");
    }
}

/// The free arguments a subcommand was given, or the first flag-like argument
/// nobody recognized.
///
/// pico-args leaves whatever it didn't parse on the command line, so without
/// looking at the leftovers a mistyped `--delet` is silently taken as a repo
/// pattern and reported as a repo that doesn't exist, and every pattern after
/// the first is thrown away without a word.
fn free_args(args: pico_args::Arguments) -> std::result::Result<Vec<String>, String> {
    args.finish()
        .into_iter()
        .map(|arg| {
            let arg = arg.to_string_lossy().into_owned();
            if arg.starts_with('-') {
                Err(arg)
            } else {
                Ok(arg)
            }
        })
        .collect()
}

/// Run `action` over every pattern the user named, reporting a pattern that
/// couldn't be handled at all rather than abandoning the rest of the request.
/// Returns whether every pattern did what was asked of it, so the caller can
/// exit non-zero.
fn for_each_pattern(
    patterns: &[String],
    verb: &str,
    mut action: impl FnMut(&str) -> Result<bool>,
) -> bool {
    let mut succeeded = true;

    for requested in patterns {
        match action(requested) {
            Ok(done) => succeeded &= done,
            Err(e) => {
                eprintln!("Failed to {} '{}': {:#}", verb, requested, e);
                succeeded = false;
            }
        }
    }

    succeeded
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
                outln!("Restored {}", repo_path);
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
        outln!("workset {}", env!("CARGO_PKG_VERSION"));
        return Ok(ExitCode::SUCCESS);
    }

    // Print the usage text and exit. `-h` is handled alongside `--help` so the
    // conventional short form reaches the help instead of falling through to
    // the TUI, and before loading the workspace so it works anywhere.
    if args.contains(["-h", "--help"]) {
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
  {cmd}workset{reset} [-h|--help] [-V|--version]
  {cmd}workset{reset} init
  {cmd}workset{reset} clone <repo pattern>...
  {cmd}workset{reset} restore <repo pattern>...
  {cmd}workset{reset} drop [repo pattern]... [--delete] [--force]
  {cmd}workset{reset} list
  {cmd}workset{reset} status
{dim}
  Without a subcommand, the interactive TUI opens; '?' shows its keybindings.{reset}

{commands_header}
  {subcmd}init{reset}                                 Initialize a workspace in current directory
  {subcmd}clone{reset} {arg}<pattern>...{reset}                   Clone new repository(ies) to workspace
  {subcmd}restore{reset} {arg}<pattern>...{reset}                 Restore repository(ies) from library
  {subcmd}drop{reset} {arg}[pattern]{reset} {arg}[--delete]{reset} {arg}[--force]{reset}  Drop repository(ies) from workspace
{dim}                                       Several patterns can be given at once
                                       A directory pattern drops every repo in it
                                       Without pattern: drops every repo under the cwd
                                       With --delete: permanently delete (don't store)
                                       With --force: drop even with uncommitted changes{reset}
  {subcmd}list{reset}, {subcmd}ls{reset}                             List all repositories with their status
  {subcmd}status{reset}                               Show workspace summary and statistics

{examples_header}
  {cmd}workset init{reset}                              Initialize workspace here
  {cmd}workset clone github.com/user/repo{reset}        Clone a new repository
  {cmd}workset clone github.com/user{reset}             Clone all repos from github.com/user
  {cmd}workset restore repo{reset}                      Restore every library repo matching 'repo'
  {cmd}workset drop ./repo{reset}                       Drop repo (save to library)
  {cmd}workset drop github.com{reset}                   Drop every repo under github.com
  {cmd}workset drop repo1 repo2{reset}                  Drop both repos in one request
  {cmd}workset drop{reset}                              Drop every repo under the current dir
  {cmd}workset drop --delete ./old_repo{reset}          Permanently delete a repo
  {cmd}workset drop --force ./dirty_repo{reset}         Force drop repo and lose any changes
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
        out!("{}", help);

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

    // Take the repo patterns a subcommand was given, or report the first
    // argument nobody recognized and give up: an unrecognized flag taken as a
    // pattern would be reported as a repo that doesn't exist, and for `drop` it
    // could name a repo to consume that the user never asked for.
    macro_rules! patterns {
        ($args:expr) => {
            match free_args($args) {
                Ok(patterns) => patterns,
                Err(unexpected) => {
                    eprintln!("Unrecognized argument: {}", unexpected);
                    eprintln!("Run 'workset --help' for usage information");
                    return Ok(ExitCode::FAILURE);
                }
            }
        };
    }

    // Same, for the subcommands that take no arguments at all: a stray one is
    // a mistake, and silently ignoring it hides it.
    macro_rules! no_patterns {
        ($args:expr) => {
            if let Some(unexpected) = patterns!($args).first() {
                eprintln!("Unrecognized argument: {}", unexpected);
                eprintln!("Run 'workset --help' for usage information");
                return Ok(ExitCode::FAILURE);
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
                no_patterns!(args);
                let workspace_path = std::env::current_dir()?;
                let library_path = workspace_path.join(".workset");

                if library_path.exists() {
                    outln!(
                        "Workspace already initialized in {}",
                        workspace_path.display()
                    );
                } else {
                    std::fs::create_dir_all(&library_path)?;
                    outln!("Initialized workspace in {}", workspace_path.display());
                }
                true
            }
            "clone" => {
                let workspace = require_workspace!(maybe_workspace);
                let patterns = patterns!(args);
                if patterns.is_empty() {
                    eprintln!("Missing repository pattern");
                    eprintln!("Usage: workset clone <pattern>...");
                    false
                } else {
                    for_each_pattern(&patterns, "clone", |requested| {
                        let Ok(pattern) = requested.parse::<workset::RepoPattern>();
                        clone_repos(&workspace, &pattern)
                    })
                }
            }
            "restore" => {
                let workspace = require_workspace!(maybe_workspace);
                let patterns = patterns!(args);
                if patterns.is_empty() {
                    eprintln!("Missing repository pattern");
                    eprintln!("Usage: workset restore <pattern>...");
                    false
                } else {
                    for_each_pattern(&patterns, "restore", |requested| {
                        let Ok(pattern) = requested.parse::<workset::RepoPattern>();
                        restore_repos(&workspace, &pattern)
                    })
                }
            }
            "drop" => {
                let workspace = require_workspace!(maybe_workspace);
                let delete = args.contains("--delete");
                let force = args.contains("--force");

                drop_repos(&workspace, &patterns!(args), delete, force)?
            }
            "list" | "ls" => {
                let workspace = require_workspace!(maybe_workspace);
                no_patterns!(args);
                list_workspace_status(&workspace)?;
                true
            }
            "status" => {
                let workspace = require_workspace!(maybe_workspace);
                no_patterns!(args);
                show_workspace_summary(&workspace)?;
                true
            }
            _ => {
                eprintln!("Unknown command: {}", command);
                eprintln!("Run 'workset --help' for usage information");
                false
            }
        },
        // No subcommand: the TUI, unless what we were given was a flag nobody
        // recognized. Opening the interactive view in answer to `workset -x`
        // (or a typo like `--helpp`) hides the mistake, and with no terminal
        // to open it on the user gets an IO error naming nothing they typed.
        None => match args.finish().first() {
            Some(unexpected) => {
                eprintln!("Unrecognized argument: {}", unexpected.to_string_lossy());
                eprintln!("Run 'workset --help' for usage information");
                false
            }
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
        },
    };

    Ok(if succeeded {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

/// Drop every repo named by `patterns`, or every repo under the current
/// directory when no pattern was given. Returns whether the whole request went
/// through: a repo left in place because of outstanding changes, a pattern that
/// matched nothing, and a pattern that couldn't be dropped at all each count as
/// a failure, so the caller can exit non-zero.
fn drop_repos(
    workspace: &Workspace,
    patterns: &[String],
    delete: bool,
    force: bool,
) -> Result<bool> {
    if patterns.is_empty() {
        let report = workspace.drop_all(delete, force)?;
        report_drop(&report, delete);

        if report.is_empty() {
            eprintln!("No repositories found in the current directory");
            return Ok(false);
        }
        return Ok(report.skipped.is_empty());
    }

    // Every pattern is dropped and reported on its own, so one that names
    // nothing (or names something workset doesn't manage) is called out by name
    // instead of vanishing into a combined total, and the ones that do match
    // are still dropped. Patterns are relative to the current directory first,
    // so 'workset drop ./repo' works from the repo's parent.
    let cwd = std::env::current_dir()?;

    Ok(for_each_pattern(patterns, "drop", |requested| {
        let Ok(pattern) = workspace
            .resolve_pattern(&cwd, requested)
            .parse::<workset::RepoPattern>();

        let report = workspace.drop(&pattern, delete, force)?;
        report_drop(&report, delete);
        if report.is_empty() {
            eprintln!("No repository in the workspace matches '{}'", requested);
        }
        Ok(!report.is_empty() && report.skipped.is_empty())
    }))
}

/// Print what dropping one pattern did: the repos that moved, and the ones left
/// where they are with the status that blocked them.
fn report_drop(report: &workset::DropReport, delete: bool) {
    let verb = if delete { "deleted" } else { "dropped" };

    for repo in &report.dropped {
        outln!("  {} - ✓ {}", repo, verb);
    }

    for (repo, blocker) in &report.skipped {
        eprintln!("  {} - ⚠ kept ({}, {})", repo, blocker, blocker.remedy());
    }
}

/// List all repositories in the workspace with their status
fn list_workspace_status(workspace: &Workspace) -> Result<()> {
    let repos = workset::find_git_repositories(Path::new(&workspace.path))?;

    if repos.is_empty() {
        outln!("No repositories found in workspace");
        return Ok(());
    }

    outln!("Repositories in workspace ({}):", workspace.path);
    outln!();

    for repo in repos {
        let repo_name = workspace.relative_name(&repo);

        let status_str = match workset::check_repo_status(&repo) {
            Ok(workset::RepoStatus::Clean) => "✓ clean".to_string(),
            Ok(workset::RepoStatus::Dirty) => "⚠ modified".to_string(),
            Ok(workset::RepoStatus::Unpushed) => "⚠ unpushed".to_string(),
            Ok(workset::RepoStatus::NoCommits) => "⚠ no commits".to_string(),
            Err(_) => "✗ error".to_string(),
        };

        outln!("  {} - {}", repo_name, status_str);
    }

    Ok(())
}

/// Show a summary of the workspace
fn show_workspace_summary(workspace: &Workspace) -> Result<()> {
    outln!("Workspace: {}", workspace.path);
    outln!();

    // Show library information
    outln!("Library: {}", workspace.library_path());
    if let Ok(repos) = workspace.list_library() {
        outln!("  {} repository(ies) in library", repos.len());
    }

    // Count repositories in workspace
    outln!();
    if let Ok(repos) = workset::find_git_repositories(Path::new(&workspace.path)) {
        outln!("Active repositories: {}", repos.len());

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
            outln!("  ✓ {} clean", clean);
        }
        if modified > 0 {
            outln!("  ⚠ {} with uncommitted changes", modified);
        }
        if unpushed > 0 {
            outln!("  ⚠ {} with unpushed commits", unpushed);
        }
        if no_commits > 0 {
            outln!("  ⚠ {} with no commits", no_commits);
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
            &["clone", "restore", "drop", "list", "ls", "status"]
        } else {
            &["init"]
        };
        for subcommand in subcommands {
            if subcommand.starts_with(current_word) {
                outln!("{}", subcommand);
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
                        outln!("{}", repo);
                    }
                }
            }
        } else {
            // For drop and other commands, complete from workspace
            for repo in get_repo_completions(&workspace) {
                if repo.starts_with(current_word) {
                    outln!("{}", repo);
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
            outln!("clone\tClone new repository(ies) to workspace");
            outln!("restore\tRestore repository(ies) from library");
            outln!("drop\tDrop one or more repositories");
            outln!("list\tList all repositories with their status");
            outln!("ls\tList all repositories with their status");
            outln!("status\tShow workspace summary and statistics");
        } else {
            outln!("init\tInitialize a workspace in current directory");
        }
    } else if let Some(workspace) = maybe_workspace {
        // Complete repository paths based on the subcommand
        let subcommand = words.get(1).unwrap_or(&"");
        if *subcommand == "restore" {
            // For restore, complete from library
            if let Ok(library_repos) = workspace.list_library() {
                for repo in library_repos {
                    outln!("{}\tlibrary", repo);
                }
            }
        } else {
            // For drop and other commands, complete from workspace with metadata
            for (repo_name, description) in get_repo_completions_with_metadata(&workspace) {
                outln!("{}\t{}", repo_name, description);
            }
        }
    }

    Ok(())
}
