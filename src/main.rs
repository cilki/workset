use anyhow::Result;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use tracing::level_filters::LevelFilter;
use workset::{Workspace, quote_path};

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
            outln!(
                "Fetching the repository list for {}/{}",
                quote_path(provider),
                quote_path(path)
            );

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
                eprintln!(
                    "No repositories found for {}/{}",
                    quote_path(provider),
                    quote_path(path)
                );
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
                        // The name came from the provider's listing, not from
                        // anything the user typed
                        eprintln!(
                            "Failed to clone {}: {}",
                            quote_path(&repo_pattern.full_path()),
                            e
                        );
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
        eprintln!(
            "{} is already in the workspace",
            quote_path(&pattern.full_path())
        );
        return Ok(false);
    }

    // Check if it exists in library first
    if workspace.library_contains(&pattern.full_path()) {
        eprintln!(
            "{} is in the library; run 'workset restore {}' instead",
            quote_path(&pattern.full_path()),
            quote_path(&pattern.full_path())
        );
        return Ok(false);
    }

    // Clone from remote
    if let Some((provider, repo_path_str)) = pattern.provider_and_path() {
        let clone_url = format!("https://{}/{}", provider, repo_path_str);

        outln!("Cloning {}", quote_path(&clone_url));

        // TODO show progress
        workspace.clone_into(&clone_url, &repo_path)?;

        outln!("Cloned {}", quote_path(&pattern.full_path()));
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
                eprintln!("Failed to {} '{}': {:#}", verb, quote_path(requested), e);
                succeeded = false;
            }
        }
    }

    succeeded
}

/// Every repo checked out in the workspace, as its workspace-relative path
/// paired with where it lives on disk, sorted by path.
fn workspace_repos(workspace: &Workspace) -> Vec<(String, PathBuf)> {
    let mut repos: Vec<(String, PathBuf)> =
        workset::find_git_repositories(Path::new(&workspace.path))
            .into_iter()
            .map(|repo| (workspace.relative_name(&repo), repo))
            .collect();
    repos.sort();
    repos.dedup();
    repos
}

/// Workspace-relative paths of the checked-out repos whose path contains
/// `pattern`, matching how the library is searched.
fn workspace_matches(workspace: &Workspace, pattern: &str) -> Vec<String> {
    workspace_repos(workspace)
        .into_iter()
        .map(|(name, _)| name)
        .filter(|name| name.contains(pattern))
        .collect()
}

/// Restore repositories from library matching the pattern. Returns false when
/// nothing was restored, so the caller can exit non-zero.
fn restore_repos(workspace: &Workspace, pattern: &workset::RepoPattern) -> Result<bool> {
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
        let in_workspace = workspace_matches(workspace, &pattern_str);
        if !in_workspace.is_empty() {
            for repo in in_workspace {
                eprintln!("{} is already in the workspace", quote_path(&repo));
            }
        } else if library_repos.is_empty() {
            eprintln!("The library is empty");
        } else {
            eprintln!(
                "No repository in the library matches '{}'",
                quote_path(&pattern_str)
            );
        }
        return Ok(false);
    }

    let mut restored = 0;
    let mut failed = 0;

    for repo_path in matching_repos {
        // Check if already exists in workspace
        let dest_path = PathBuf::from(&workspace.path).join(&repo_path);
        if dest_path.exists() {
            eprintln!("{} is already in the workspace", quote_path(&repo_path));
            continue;
        }

        // Restore from library
        match workspace.restore_from_library(&repo_path) {
            Ok(_) => {
                outln!("Restored {}", quote_path(&repo_path));
                restored += 1;
            }
            Err(e) => {
                failed += 1;
                eprintln!("Failed to restore {}: {}", quote_path(&repo_path), e);
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
        match shell_type.as_str() {
            "bash" => complete_bash(maybe_workspace),
            "fish" => complete_fish(maybe_workspace),
            _ => anyhow::bail!("Unsupported shell type: {}", shell_type),
        }
        return Ok(ExitCode::SUCCESS);
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

                let workspace_name = workspace_path.display().to_string();
                if library_path.exists() {
                    outln!(
                        "Workspace already initialized in {}",
                        quote_path(&workspace_name)
                    );
                } else {
                    std::fs::create_dir_all(&library_path)?;
                    outln!("Initialized workspace in {}", quote_path(&workspace_name));
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
            eprintln!(
                "No repository in the workspace matches '{}'",
                quote_path(requested)
            );
        }
        Ok(!report.is_empty() && report.skipped.is_empty())
    }))
}

/// Print what dropping one pattern did: the repos that moved, and the ones left
/// where they are with the status that blocked them.
fn report_drop(report: &workset::DropReport, delete: bool) {
    let verb = if delete { "deleted" } else { "dropped" };

    for repo in &report.dropped {
        outln!("  {} - ✓ {}", quote_path(repo), verb);
    }

    for (repo, blocker) in &report.skipped {
        eprintln!(
            "  {} - ⚠ kept ({}, {})",
            quote_path(repo),
            blocker,
            blocker.remedy()
        );
    }
}

/// List all repositories in the workspace with their status
fn list_workspace_status(workspace: &Workspace) -> Result<()> {
    let repos = workset::find_git_repositories(Path::new(&workspace.path));

    if repos.is_empty() {
        outln!("No repositories found in workspace");
        return Ok(());
    }

    outln!(
        "Repositories in workspace ({}):",
        quote_path(&workspace.path)
    );
    outln!();

    let statuses = workset::scan_repos(&repos, workset::check_repo_status);

    for (repo, status) in repos.iter().zip(statuses) {
        let repo_name = workspace.relative_name(repo);

        let status_str = match status {
            Ok(workset::RepoStatus::Clean) => "✓ clean",
            Ok(workset::RepoStatus::Dirty) => "⚠ modified",
            Ok(workset::RepoStatus::Unpushed) => "⚠ unpushed",
            Ok(workset::RepoStatus::NoCommits) => "⚠ no commits",
            Ok(workset::RepoStatus::Unknown) => "✗ unreadable",
            Err(_) => "✗ error",
        };

        // A repo's directory name is the repo's to choose, and this line is
        // what the user reads before deciding what to drop; see
        // [`workset::quote_path`]
        outln!("  {} - {}", quote_path(&repo_name), status_str);
    }

    Ok(())
}

/// Show a summary of the workspace
fn show_workspace_summary(workspace: &Workspace) -> Result<()> {
    outln!("Workspace: {}", quote_path(&workspace.path));
    outln!();

    // Show library information
    outln!("Library: {}", quote_path(&workspace.library_path()));
    if let Ok(repos) = workspace.list_library() {
        outln!("  {} repository(ies) in library", repos.len());
    }

    // Count repositories in workspace
    outln!();
    let repos = workset::find_git_repositories(Path::new(&workspace.path));
    outln!("Active repositories: {}", repos.len());

    let mut clean = 0;
    let mut modified = 0;
    let mut unpushed = 0;
    let mut no_commits = 0;
    let mut unreadable = 0;

    for status in workset::scan_repos(&repos, workset::check_repo_status) {
        match status {
            Ok(workset::RepoStatus::Clean) => clean += 1,
            Ok(workset::RepoStatus::Dirty) => modified += 1,
            Ok(workset::RepoStatus::Unpushed) => unpushed += 1,
            Ok(workset::RepoStatus::NoCommits) => no_commits += 1,
            Ok(workset::RepoStatus::Unknown) => unreadable += 1,
            Err(_) => unreadable += 1,
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
    if unreadable > 0 {
        outln!("  ✗ {} workset can't read", unreadable);
    }

    Ok(())
}

/// Subcommands offered for completion, each with the one-line description
/// that shells able to show one (fish) put next to it. Which table applies
/// depends on whether there is a workspace: `init` is what makes one, and
/// everything else needs one to already exist.
const SUBCOMMANDS: &[(&str, &str)] = &[
    ("clone", "Clone new repository(ies) to workspace"),
    ("restore", "Restore repository(ies) from library"),
    ("drop", "Drop one or more repositories"),
    ("list", "List all repositories with their status"),
    ("ls", "List all repositories with their status"),
    ("status", "Show workspace summary and statistics"),
];
const INIT_SUBCOMMAND: &[(&str, &str)] = &[("init", "Initialize a workspace in current directory")];

/// What a completion request should offer.
///
/// Candidates carry only what is free to produce. A workspace repo's
/// description costs a repo open, so the path on disk is kept here and only
/// the shells that render descriptions go and get one.
enum Completions {
    /// (subcommand, description)
    Subcommands(&'static [(&'static str, &'static str)]),
    /// Repos the library holds, by workspace-relative path
    Library(Vec<String>),
    /// Repos checked out in the workspace: workspace-relative path and the
    /// path on disk to describe it from
    Workspace(Vec<(String, PathBuf)>),
}

impl Completions {
    /// Just the candidate words, for shells that take no descriptions
    fn values(&self) -> Vec<&str> {
        match self {
            Self::Subcommands(subcommands) => subcommands.iter().map(|(name, _)| *name).collect(),
            Self::Library(repos) => repos.iter().map(String::as_str).collect(),
            Self::Workspace(repos) => repos.iter().map(|(name, _)| name.as_str()).collect(),
        }
    }
}

/// The command line up to the cursor.
///
/// `COMP_POINT` is the cursor's byte offset into `COMP_LINE`, but bash has
/// multibyte quirks where it can exceed the line length or land inside a UTF-8
/// character, so it is clamped to the nearest valid boundary. Shells that hand
/// over a line already cut at the cursor (fish) pass no `COMP_POINT` at all.
fn line_before_cursor(comp_line: &str, comp_point: Option<usize>) -> &str {
    let mut point = comp_point.unwrap_or(comp_line.len()).min(comp_line.len());
    while !comp_line.is_char_boundary(point) {
        point -= 1;
    }
    &comp_line[..point]
}

/// Everything the word at the cursor could become, together with what has
/// been typed of that word already.
///
/// Candidates are not filtered by the prefix here because the two shells want
/// opposite things: bash takes our output into COMPREPLY verbatim, so it has
/// to be filtered, while fish filters by the current token itself and wants
/// the whole context.
fn completions<'line>(
    maybe_workspace: Option<&Workspace>,
    comp_line: &'line str,
    comp_point: Option<usize>,
) -> (&'line str, Completions) {
    let line = line_before_cursor(comp_line, comp_point);
    let words: Vec<&str> = line.split_whitespace().collect();

    // The word being completed is empty when the cursor follows whitespace,
    // otherwise it is the last word
    let (current_word, word_index) = if line.is_empty() || line.ends_with(char::is_whitespace) {
        ("", words.len())
    } else {
        (*words.last().unwrap(), words.len() - 1)
    };

    // Word 0 is the binary's own name, so the subcommand is word 1
    let candidates = if word_index <= 1 {
        Completions::Subcommands(match maybe_workspace {
            Some(_) => SUBCOMMANDS,
            None => INIT_SUBCOMMAND,
        })
    } else {
        // Past the subcommand every argument names a repo, which only means
        // something inside a workspace
        match maybe_workspace {
            // `restore` names repos the library holds; every other subcommand
            // names repos that are checked out
            Some(workspace) if words.get(1) == Some(&"restore") => {
                Completions::Library(workspace.list_library().unwrap_or_default())
            }
            Some(workspace) => Completions::Workspace(workspace_repos(workspace)),
            None => Completions::Workspace(Vec::new()),
        }
    };

    (current_word, candidates)
}

/// The completion request as the shell describes it in the environment
fn completion_request() -> (String, Option<usize>) {
    let comp_line = std::env::var("COMP_LINE").unwrap_or_default();
    let comp_point = std::env::var("COMP_POINT")
        .ok()
        .and_then(|point| point.parse().ok());
    (comp_line, comp_point)
}

/// How a workspace repo is described next to its completion: its status and
/// how long ago it changed, both from a single repo open
fn describe_workspace_repo(repo: &Path) -> String {
    // A repo we can't read still gets a completion, just an unadorned one
    let (status, modification_time) = workset::check_repo_status_and_modification_time(repo)
        .map(|(status, time)| (Some(status), time))
        .unwrap_or((None, None));

    let mut parts: Vec<String> = Vec::new();
    if let Some(status) = status {
        parts.push(
            match status {
                workset::RepoStatus::Clean => "clean",
                workset::RepoStatus::Dirty => "dirty",
                workset::RepoStatus::Unpushed => "unpushed",
                workset::RepoStatus::NoCommits => "no commits",
                workset::RepoStatus::Unknown => "unreadable",
            }
            .to_string(),
        );
    }
    if let Some(time) = modification_time {
        parts.push(workset::format_time_ago(time));
    }

    if parts.is_empty() {
        "repository".to_string()
    } else {
        parts.join(", ")
    }
}

/// Output dynamic completions for bash, which takes bare words and inserts
/// them verbatim, so the prefix has to be honoured here
fn complete_bash(maybe_workspace: Option<Workspace>) {
    let (comp_line, comp_point) = completion_request();
    let (current_word, candidates) = completions(maybe_workspace.as_ref(), &comp_line, comp_point);
    for value in candidates.values() {
        if value.starts_with(current_word) {
            outln!("{}", value);
        }
    }
}

/// Output dynamic completions for fish, which shows a description after a tab
/// and narrows the list by the current token itself
fn complete_fish(maybe_workspace: Option<Workspace>) {
    let (comp_line, comp_point) = completion_request();
    match completions(maybe_workspace.as_ref(), &comp_line, comp_point).1 {
        Completions::Subcommands(subcommands) => {
            for (name, description) in subcommands {
                outln!("{}\t{}", name, description);
            }
        }
        Completions::Library(repos) => {
            for repo in repos {
                outln!("{}\tlibrary", repo);
            }
        }
        Completions::Workspace(repos) => {
            // Completions run on every TAB press, so the per-repo worktree
            // walks behind these descriptions go to the whole machine at once
            let paths: Vec<PathBuf> = repos.iter().map(|(_, path)| path.clone()).collect();
            let descriptions = workset::scan_repos(&paths, describe_workspace_repo);
            for ((name, _), description) in repos.iter().zip(descriptions) {
                outln!("{}\t{}", name, description);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A workspace holding one checked-out repo and one library entry
    fn workspace(root: &Path) -> Workspace {
        let workspace = Workspace {
            path: root.to_string_lossy().to_string(),
        };
        let checked_out = root.join("github.com/user/alpha");
        std::fs::create_dir_all(&checked_out).unwrap();
        gix::init(&checked_out).unwrap();
        std::fs::create_dir_all(
            Path::new(&workspace.library_path()).join("github.com/user/stored"),
        )
        .unwrap();
        gix::init_bare(Path::new(&workspace.library_path()).join("github.com/user/stored"))
            .unwrap();
        workspace
    }

    /// The candidate words for the given line, as fish gets them: everything
    /// the completion context offers, since fish narrows the list itself
    fn offered(maybe_workspace: Option<&Workspace>, line: &str) -> Vec<String> {
        completions(maybe_workspace, line, None)
            .1
            .values()
            .iter()
            .map(|value| value.to_string())
            .collect()
    }

    /// The candidate words for the given line, as bash gets them: narrowed by
    /// what has been typed of the word at `comp_point`, because bash inserts
    /// them verbatim
    fn offered_to_bash(
        maybe_workspace: Option<&Workspace>,
        line: &str,
        comp_point: Option<usize>,
    ) -> Vec<String> {
        let (current_word, candidates) = completions(maybe_workspace, line, comp_point);
        candidates
            .values()
            .iter()
            .filter(|value| value.starts_with(current_word))
            .map(|value| value.to_string())
            .collect()
    }

    const ALL_SUBCOMMANDS: [&str; 6] = ["clone", "restore", "drop", "list", "ls", "status"];

    #[test]
    fn the_first_word_completes_to_a_subcommand() {
        let temp = tempfile::TempDir::new().unwrap();
        let workspace = workspace(temp.path());

        assert_eq!(offered(Some(&workspace), "workset "), ALL_SUBCOMMANDS);
        // Bash is handed only what it may insert; fish gets the whole context
        // and narrows it itself
        assert_eq!(
            offered_to_bash(Some(&workspace), "workset l", None),
            ["list", "ls"]
        );
        assert_eq!(offered(Some(&workspace), "workset l"), ALL_SUBCOMMANDS);
        assert!(offered_to_bash(Some(&workspace), "workset nope", None).is_empty());
    }

    #[test]
    fn outside_a_workspace_only_init_is_offered() {
        // Every other subcommand needs a workspace, and no repo can be named
        // before there is one
        assert_eq!(offered(None, "workset "), ["init"]);
        assert!(offered(None, "workset init ").is_empty());
    }

    #[test]
    fn restore_names_the_library_and_everything_else_the_workspace() {
        let temp = tempfile::TempDir::new().unwrap();
        let workspace = workspace(temp.path());

        // A restore takes a repo out of the library, so only its entries can
        // be named
        assert_eq!(
            offered(Some(&workspace), "workset restore "),
            ["github.com/user/stored"]
        );
        // Everything else acts on repos that are checked out, and keeps
        // offering them after the first one
        for line in [
            "workset drop ",
            "workset clone ",
            "workset drop github.com/user/alpha ",
        ] {
            assert_eq!(
                offered(Some(&workspace), line),
                ["github.com/user/alpha"],
                "line: {line}"
            );
        }
        // For bash, a repo name is narrowed by its prefix like anything else
        assert!(offered_to_bash(Some(&workspace), "workset drop gitlab", None).is_empty());
    }

    #[test]
    fn the_cursor_decides_which_word_is_being_completed() {
        let temp = tempfile::TempDir::new().unwrap();
        let workspace = workspace(temp.path());

        // With the cursor still in the subcommand, the repo after it is not
        // what's being completed
        let line = "workset dr github.com/user/alpha";
        assert_eq!(
            offered_to_bash(Some(&workspace), line, Some("workset dr".len())),
            ["drop"]
        );

        // The cursor inside the command's own name leaves nothing to offer;
        // bash completes command names itself
        assert!(offered_to_bash(Some(&workspace), "workset", Some(7)).is_empty());

        // A COMP_POINT past the end, or inside a multi-byte character, must
        // not panic on a non-boundary slice
        assert_eq!(
            offered_to_bash(Some(&workspace), "workset ", Some(999)),
            ALL_SUBCOMMANDS
        );
        // 'ü' spans bytes 13..15, so a cursor at 14 is inside it and the line
        // is cut back to the boundary before it
        let multibyte = "workset drop ünicode";
        assert_eq!(
            offered_to_bash(Some(&workspace), multibyte, Some(14)),
            ["github.com/user/alpha"]
        );
        assert!(offered_to_bash(Some(&workspace), multibyte, Some(15)).is_empty());
    }
}
