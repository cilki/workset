use anyhow::Result;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};
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

/// Clone a repository. Returns false when the repo was not cloned because it
/// is already in the workspace or the library.
fn clone_repo(workspace: &Workspace, pattern: &workset::RepoPattern) -> Result<bool> {
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

/// Why an argument a subcommand was given can't be taken for a repo pattern.
#[derive(Debug, PartialEq, Eq)]
enum BadArg {
    /// A flag-like argument nobody recognized
    Unrecognized(String),
    /// The empty string, which names no repo at all
    Empty,
}

impl std::fmt::Display for BadArg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            // The argument is echoed back, and an argument can be a repo's
            // name: the shell completions offer them, and scripts pass them
            // along; see [`workset::quote_path`]
            Self::Unrecognized(arg) => {
                write!(f, "Unrecognized argument: {}", quote_path(arg))
            }
            Self::Empty => write!(f, "Empty repository pattern"),
        }
    }
}

/// The free arguments a subcommand was given, or the first one that can't be a
/// repo pattern.
///
/// pico-args leaves whatever it didn't parse on the command line, so without
/// looking at the leftovers a mistyped `--delet` is silently taken as a repo
/// pattern and reported as a repo that doesn't exist, and every pattern after
/// the first is thrown away without a word.
///
/// An empty argument is rejected for the same reason, and it is the more
/// dangerous of the two: it names no repo, but `drop` resolves a pattern
/// against the current directory before anything else, and the current
/// directory is what the empty string resolves to — so `workset drop ""` took
/// the entire working set, and `workset drop --delete ""` destroyed it. That is
/// what `workset drop "$repo"` does when the variable is unset, which is
/// exactly when the user meant to name one repo and named none. Restoring is no
/// better off: every library path contains the empty string, so `workset
/// restore ""` checked the whole library back out. A pattern that was given has
/// to name something; the bare `workset drop` is how you ask for everything
/// under the current directory.
fn free_args(args: pico_args::Arguments) -> std::result::Result<Vec<String>, BadArg> {
    args.finish()
        .into_iter()
        .map(|arg| {
            let arg = arg.to_string_lossy().into_owned();
            if arg.starts_with('-') {
                Err(BadArg::Unrecognized(arg))
            } else if arg.is_empty() {
                Err(BadArg::Empty)
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
    let mut repos: Vec<(String, PathBuf)> = workspace
        .repos()
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
fn restore_repos(workspace: &Workspace, requested: &str) -> Result<bool> {
    // Get all repos from library
    let library_repos = workspace.list_library();

    // A pattern that is exactly one of the library's entries names that entry
    // and nothing else. Parsing it would read it as a clone URL instead, and a
    // repo whose path happens to be URL-shaped — one ending in ".git", or
    // holding an '@' or a ':' — comes back out spelled differently, so the
    // repo the user just dropped could no longer be restored by the name the
    // drop reported.
    let pattern_str = if library_repos.iter().any(|repo| repo == requested) {
        requested.to_string()
    } else {
        let Ok(pattern) = requested.parse::<workset::RepoPattern>();
        pattern.full_path()
    };

    // Substring matching is for the short patterns a user types by hand; a
    // pattern that spells out one repo's whole path names that repo alone.
    // Matching it as a substring too restored every sibling it is a prefix
    // of — `github.com/user/repo` also pulled back `github.com/user/repo-old`
    // — even though that full path is exactly what drop reports, what
    // completion offers, and what clone tells the user to run. And when the
    // named repo was already checked out, its siblings were restored in its
    // place instead of the request being refused.
    let matching_repos: Vec<String> = if library_repos.contains(&pattern_str) {
        vec![pattern_str.clone()]
    } else if workspace_repos(workspace)
        .iter()
        .any(|(name, _)| *name == pattern_str)
    {
        eprintln!("{} is already in the workspace", quote_path(&pattern_str));
        return Ok(false);
    } else {
        library_repos
            .iter()
            .filter(|repo| repo.contains(&pattern_str))
            .cloned()
            .collect()
    };

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
{dim}
  Without a subcommand, the interactive TUI opens; '?' shows its keybindings.{reset}

{commands_header}
  {subcmd}init{reset}                                 Initialize a workspace in current directory
  {subcmd}clone{reset} {arg}<pattern>...{reset}                   Clone new repository(ies) to workspace
{dim}                                       A clone URL, with or without '.git', works too{reset}
  {subcmd}restore{reset} {arg}<pattern>...{reset}                 Restore repository(ies) from library
  {subcmd}drop{reset} {arg}[pattern]{reset} {arg}[--delete]{reset} {arg}[--force]{reset}  Drop repository(ies) from workspace
{dim}                                       Several patterns can be given at once
                                       A directory pattern drops every repo in it
                                       Without pattern: drops every repo under the cwd
                                       With --delete: permanently delete (don't store)
                                       With --force: drop despite uncommitted changes or
                                       unpushed commits{reset}
  {subcmd}list{reset}, {subcmd}ls{reset}                             List all repositories with their status
{dim}                                       Ends with how many repos the library holds{reset}

{examples_header}
  {cmd}workset init{reset}                              Initialize workspace here
  {cmd}workset clone github.com/user/repo{reset}        Clone a new repository
  {cmd}workset clone git@github.com:user/repo{reset}    Same repo, named by its clone URL
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
        // The emblem is drawn with the terminal's own graphics protocol where
        // there is one, and in block characters where there isn't. Everywhere
        // else -- a pipe, a file, `NO_COLOR`, a build without the TUI -- the
        // help is the text below and nothing but, byte for byte as it has
        // always been.
        #[cfg(feature = "tui")]
        if let Some(emblem) = cilki::terminal::banner(&cilki::emblem::WORKSET, 56, 7) {
            out!("{}", emblem);
            outln!();
        }

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
    // argument that isn't one and give up: an unrecognized flag taken as a
    // pattern would be reported as a repo that doesn't exist, and for `drop`
    // both it and an empty argument could name a repo to consume that the user
    // never asked for. Nothing is attempted when any argument is refused, so a
    // request the user didn't mean takes no repo with it.
    macro_rules! patterns {
        ($args:expr) => {
            match free_args($args) {
                Ok(patterns) => patterns,
                Err(bad) => {
                    eprintln!("{}", bad);
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
                        clone_repo(&workspace, &pattern)
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
                        restore_repos(&workspace, requested)
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
        let pattern = workspace.resolve_pattern(&cwd, requested);

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

/// List all repositories in the workspace with their status, followed by how
/// many repos the library is holding
fn list_workspace_status(workspace: &Workspace) -> Result<()> {
    let repos = workspace.repos();

    if repos.is_empty() {
        outln!(
            "No repositories found in workspace ({})",
            quote_path(&workspace.path)
        );
    } else {
        outln!(
            "Repositories in workspace ({}):",
            quote_path(&workspace.path)
        );
        outln!();

        let statuses = workset::scan_repos(&repos, |repo| workset::check_repo_status(repo));

        for (repo, status) in repos.iter().zip(statuses) {
            let repo_name = workspace.relative_name(repo);

            let status_str = match status {
                workset::RepoStatus::Clean => "✓ clean",
                workset::RepoStatus::Dirty => "⚠ modified",
                workset::RepoStatus::Unpushed => "⚠ unpushed",
                workset::RepoStatus::NoCommits => "⚠ no commits",
                workset::RepoStatus::Unknown => "✗ unreadable",
            };

            // A repo's directory name is the repo's to choose, and this line
            // is what the user reads before deciding what to drop; see
            // [`workset::quote_path`]
            outln!("  {} - {}", quote_path(&repo_name), status_str);
        }
    }

    // The repos that aren't here are in the library, so what it holds belongs
    // in the command that reports on the working set.
    outln!();
    outln!(
        "Library ({}): {} repository(ies)",
        quote_path(&workspace.library_path()),
        workspace.list_library().len()
    );

    Ok(())
}

const SUBCOMMANDS: &[(&str, &str)] = &[
    ("clone", "Clone new repository(ies) to workspace"),
    ("restore", "Restore repository(ies) from library"),
    ("drop", "Drop one or more repositories"),
    ("list", "List all repositories with their status"),
    ("ls", "List all repositories with their status"),
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
    /// The same candidates with the ones no shell can be handed left out.
    ///
    /// Both completion protocols are line-oriented: bash reads our output
    /// with `mapfile -t`, fish splits the command substitution on newlines,
    /// and fish's own lines are `candidate<TAB>description`. So a candidate
    /// holding a newline is not offered as one awkward word, it is offered as
    /// *two* — and the second one is a candidate the workspace never had.
    ///
    /// A repo's directory name is not workset's to choose: on Unix it may
    /// hold any byte but `/` and NUL, and a repo can arrive as files — a
    /// tarball, a shared directory, a copy someone handed over. A repo named
    /// `evil<LF>github.com` therefore put a bare `github.com` in the list,
    /// and `github.com` is not a repo at all but is still accepted as a
    /// pattern: taking the suggestion ran `workset drop github.com`, which
    /// matches every repo under it. That is the same escalation the unquoted
    /// `COMPREPLY=($(...))` in `completions/workset.bash` used to allow by
    /// globbing, reached through the candidate's own bytes instead.
    ///
    /// Printed output answers this by quoting the name
    /// ([`workset::quote_path`]), but a candidate can't be quoted: bash
    /// inserts it into the command line verbatim, so the C-style escapes
    /// would become part of the pattern and name nothing. The candidate is
    /// dropped instead, along with the rest of what
    /// [`workset::path_rewrites_output`] covers — a tab truncates a fish
    /// candidate at the description, and an ESC sequence or a bidi override
    /// makes the menu read as a name other than the one that gets inserted.
    /// The repo keeps everything but the suggestion: it is still listed (with
    /// quoting), and still named by a pattern typed out by hand.
    fn displayable(self) -> Self {
        match self {
            // workset's own words, not a repo's
            Self::Subcommands(subcommands) => Self::Subcommands(subcommands),
            Self::Library(repos) => Self::Library(
                repos
                    .into_iter()
                    .filter(|repo| !workset::path_rewrites_output(repo))
                    .collect(),
            ),
            Self::Workspace(repos) => Self::Workspace(
                repos
                    .into_iter()
                    .filter(|(name, _)| !workset::path_rewrites_output(name))
                    .collect(),
            ),
        }
    }

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
                Completions::Library(workspace.list_library())
            }
            Some(workspace) => Completions::Workspace(workspace_repos(workspace)),
            None => Completions::Workspace(Vec::new()),
        }
    };

    // One gate for both shells, since both protocols are line-oriented and
    // neither can carry the candidates this leaves out
    (current_word, candidates.displayable())
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
    let (status, modification_time) = workset::check_repo_status_and_modification_time(repo);

    let status = match status {
        workset::RepoStatus::Clean => "clean",
        workset::RepoStatus::Dirty => "dirty",
        workset::RepoStatus::Unpushed => "unpushed",
        workset::RepoStatus::NoCommits => "no commits",
        workset::RepoStatus::Unknown => "unreadable",
    };
    match modification_time {
        Some(time) => format!("{status}, {}", workset::format_time_ago(time)),
        None => status.to_string(),
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

/// Whether `candidate` is one the shell can still display for what has been
/// typed of the word.
///
/// fish narrows the candidate list itself, and the weakest match it will
/// accept is a subsequence one: the typed characters appearing in order,
/// anywhere in the candidate. So a candidate that doesn't even manage that is
/// one fish will never put on screen, whatever we say about it.
///
/// Deliberately more generous than fish is — matching here only means the
/// candidate *may* be displayed, and the cost of saying yes too often is a
/// description nobody reads, while saying no too often would cost a candidate
/// its description. Quoting characters are ignored for the same reason: the
/// word arrives as the shell wrote it, and fish matches against the word it
/// has already unquoted.
fn shell_can_display(candidate: &str, typed: &str) -> bool {
    // One pass over the candidate for the whole word: each typed character is
    // looked for in what the previous one left, which is what makes the match
    // a subsequence rather than a set of characters
    let mut candidate = candidate.chars().flat_map(char::to_lowercase);
    typed
        .chars()
        .filter(|c| !matches!(c, '\'' | '"' | '\\'))
        .flat_map(char::to_lowercase)
        .all(|wanted| candidate.any(|c| c == wanted))
}

/// How long a completion may spend describing repos.
///
/// Describing one costs a walk of its whole worktree, and however well those
/// walks are spread over the cores there are only so many cores: the time a
/// TAB press takes grows with the workspace, with nothing bounding it, and
/// the workspace is the part of this tool you are meant to be able to grow.
/// A press that stops feeling instant is worse than a candidate without a
/// description — which is what every candidate was before descriptions
/// existed, and what the ones fish can't display already get.
///
/// Seventy-five milliseconds, because a key press stops feeling instant
/// somewhere around a tenth of a second and the rest of that tenth goes on
/// starting the process and enumerating the workspace. A workspace of a few
/// dozen repos is described in full well inside it; the budget is what keeps
/// the one holding hundreds from stalling the shell.
const DESCRIBE_BUDGET: Duration = Duration::from_millis(75);

/// Describe as many of `paths` as `budget` allows, keyed by path.
///
/// The walks run on every core, and each one is started only while there is
/// budget left, so the cost of a completion is bounded by the budget however
/// many repos the workspace holds. Which repos miss out is therefore decided
/// by how long the ones before them took — a workspace small enough to
/// describe inside the budget always describes all of it, and a workspace
/// that isn't describes a prefix of it.
fn describe_within(
    paths: &[PathBuf],
    budget: Duration,
) -> std::collections::HashMap<PathBuf, String> {
    let deadline = Instant::now() + budget;
    paths
        .iter()
        .cloned()
        .zip(workset::scan_repos(paths, |repo| {
            (Instant::now() < deadline).then(|| describe_workspace_repo(repo))
        }))
        .filter_map(|(path, description)| Some((path, description?)))
        .collect()
}

/// The lines fish is handed: the candidate word, and after a tab the
/// description, where there is one.
///
/// A workspace repo's description is its status, which costs a walk of the
/// repo's whole worktree — the most expensive thing workset does per repo, and
/// completions run on every TAB press. So only the candidates fish can still
/// display for what has been typed are described, and only for as long as a
/// keystroke may take; the rest are offered unadorned, which is all fish would
/// have shown of them anyway. The walks that are left go to the whole machine
/// at once.
fn fish_lines(current_word: &str, candidates: Completions) -> Vec<String> {
    match candidates {
        Completions::Subcommands(subcommands) => subcommands
            .iter()
            .map(|(name, description)| format!("{}\t{}", name, description))
            .collect(),
        Completions::Library(repos) => repos
            .iter()
            .map(|repo| format!("{}\tlibrary", repo))
            .collect(),
        Completions::Workspace(repos) => {
            let paths: Vec<PathBuf> = repos
                .iter()
                .filter(|(name, _)| shell_can_display(name, current_word))
                .map(|(_, path)| path.clone())
                .collect();
            let described = describe_within(&paths, DESCRIBE_BUDGET);

            repos
                .iter()
                .map(|(name, path)| match described.get(path) {
                    Some(description) => format!("{}\t{}", name, description),
                    None => name.clone(),
                })
                .collect()
        }
    }
}

/// Output dynamic completions for fish, which shows a description after a tab
/// and narrows the list by the current token itself
fn complete_fish(maybe_workspace: Option<Workspace>) {
    let (comp_line, comp_point) = completion_request();
    let (current_word, candidates) = completions(maybe_workspace.as_ref(), &comp_line, comp_point);
    for line in fish_lines(current_word, candidates) {
        outln!("{}", line);
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

    /// The free arguments of a command line, as the subcommands get them
    fn args_of(args: &[&str]) -> std::result::Result<Vec<String>, BadArg> {
        free_args(pico_args::Arguments::from_vec(
            args.iter().map(std::ffi::OsString::from).collect(),
        ))
    }

    /// The empty string names no repo, so it is refused rather than resolved.
    /// `drop` resolves a pattern against the current directory, which is what
    /// the empty string resolves to, so taking it meant taking every repo
    /// under the cwd — and with `--delete`, destroying them. Scripts hand it
    /// over by accident all the time: `workset drop "$repo"` with `repo`
    /// unset.
    #[test]
    fn an_empty_argument_is_not_a_pattern() {
        assert_eq!(args_of(&[""]), Err(BadArg::Empty));
        // Refused wherever it appears, and the patterns around it are not
        // acted on: half a request the user didn't make is still not theirs
        assert_eq!(args_of(&["repo", ""]), Err(BadArg::Empty));
        assert_eq!(args_of(&["", "repo"]), Err(BadArg::Empty));
        assert_eq!(
            BadArg::Empty.to_string(),
            "Empty repository pattern".to_string()
        );
    }

    /// A flag nobody parsed is still reported as the argument it is, by name
    #[test]
    fn an_unparsed_flag_is_reported_by_name() {
        assert_eq!(
            args_of(&["--delet", "repo"]),
            Err(BadArg::Unrecognized("--delet".to_string()))
        );
        assert_eq!(
            BadArg::Unrecognized("--delet".to_string()).to_string(),
            "Unrecognized argument: --delet".to_string()
        );
    }

    /// Everything that does name a repo comes through untouched, in order, and
    /// no pattern at all stays no pattern at all — which is how the bare
    /// `workset drop` asks for every repo under the current directory
    #[test]
    fn repo_patterns_come_through_in_order() {
        assert_eq!(args_of(&[]), Ok(Vec::new()));
        assert_eq!(
            args_of(&["github.com/user/repo", "./other"]),
            Ok(vec![
                "github.com/user/repo".to_string(),
                "./other".to_string()
            ])
        );
    }

    const ALL_SUBCOMMANDS: [&str; 5] = ["clone", "restore", "drop", "list", "ls"];

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

    /// Every candidate fish could display has to survive the pass that skips
    /// the ones it can't: the test is what fish accepts, not what bash does,
    /// so a word matching in the middle of a repo's path still counts.
    #[test]
    fn a_candidate_is_matched_the_way_fish_matches_it() {
        for typed in [
            "",                       // nothing typed yet: everything is offered
            "github.com/user/alpha",  // the whole thing
            "github",                 // a prefix
            "user/alpha",             // a substring, which bash would not take
            "ghalpha",                // a subsequence, fish's weakest match
            "GitHub.com/User/Alpha",  // fish's matching is case-insensitive
            "'github.com/user/alpha", // a word the shell is still quoting
        ] {
            assert!(
                shell_can_display("github.com/user/alpha", typed),
                "typed: {typed}"
            );
        }

        // Characters fish can't find, or can't find in that order
        for typed in ["gitlab", "alpha/user", "github.com/user/alpha2"] {
            assert!(
                !shell_can_display("github.com/user/alpha", typed),
                "typed: {typed}"
            );
        }
    }

    /// The expensive half of a completion — a repo's status, which walks its
    /// whole worktree — is only computed for the candidates fish can still
    /// display. The candidates it can't must still be offered, just without a
    /// description, because dropping one would stop the completion working.
    #[test]
    fn fish_offers_every_candidate_and_describes_the_ones_it_can_display() {
        let temp = tempfile::TempDir::new().unwrap();
        let workspace = workspace(temp.path());

        let lines = |line: &str| {
            let (current_word, candidates) = completions(Some(&workspace), line, None);
            fish_lines(current_word, candidates)
        };

        // The repo is freshly initialized, so its description is its status
        assert_eq!(
            lines("workset drop github"),
            ["github.com/user/alpha\tno commits"]
        );
        // Nothing typed describes everything, as before
        assert_eq!(
            lines("workset drop "),
            ["github.com/user/alpha\tno commits"]
        );
        // A word fish could never display this candidate for: still offered,
        // but no worktree was walked for it
        assert_eq!(lines("workset drop gitlab"), ["github.com/user/alpha"]);

        // The other candidate sources describe themselves for free
        assert_eq!(
            lines("workset restore gitlab"),
            ["github.com/user/stored\tlibrary"]
        );
        assert_eq!(
            lines("workset l").first().unwrap(),
            "clone\tClone new repository(ies) to workspace"
        );
    }

    /// Describing a repo walks its worktree, so a workspace big enough turns
    /// a TAB press into a stall that grows with it. The budget stops the
    /// walks instead of the candidates: a repo left undescribed is still
    /// offered, exactly as one fish couldn't display for the typed word is.
    #[test]
    fn describing_stops_at_the_budget_but_offering_does_not() {
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path();
        let workspace = Workspace {
            path: root.to_string_lossy().to_string(),
        };
        let paths: Vec<PathBuf> = ["alpha", "beta", "gamma"]
            .iter()
            .map(|name| {
                let path = root.join(name);
                std::fs::create_dir_all(&path).unwrap();
                gix::init(&path).unwrap();
                path
            })
            .collect();

        // No time to describe anything: every repo keeps its candidate and
        // none of them is walked
        assert!(describe_within(&paths, Duration::ZERO).is_empty());

        // Given the time, every one of them is described
        let described = describe_within(&paths, Duration::from_secs(60));
        assert_eq!(described.len(), paths.len());
        for path in &paths {
            assert_eq!(described.get(path).map(String::as_str), Some("no commits"));
        }

        // A completion run against the real budget still offers all three,
        // described or not
        let (current_word, candidates) = completions(Some(&workspace), "workset drop ", None);
        let offered: Vec<String> = fish_lines(current_word, candidates)
            .into_iter()
            .map(|line| line.split('\t').next().unwrap().to_string())
            .collect();
        assert_eq!(offered, ["alpha", "beta", "gamma"]);
    }

    /// A completion candidate is one line of a line-oriented protocol, so a
    /// repo whose directory name holds a newline used to be offered as two
    /// candidates — and the extra one is a word the workspace never had. A
    /// repo named `evil<LF>github.com` offered a bare `github.com`, which is
    /// not a repo but is still a pattern, and `workset drop github.com`
    /// matches every repo under it. The name is the repo's own (it can arrive
    /// as files: a tarball, a shared directory, a copy someone handed over),
    /// so the bytes are not workset's to trust.
    #[test]
    fn a_repo_name_cannot_forge_a_second_completion_candidate() {
        let temp = tempfile::TempDir::new().unwrap();
        let workspace = workspace(temp.path());

        // Both candidate sources: a repo checked out in the workspace, which
        // `drop` and `clone` name, and a library entry, which `restore` does
        for name in ["evil\ngithub.com", "evil\tgithub.com", "evil\u{1b}[2Kx"] {
            let checked_out = temp.path().join(name);
            std::fs::create_dir_all(&checked_out).unwrap();
            gix::init(&checked_out).unwrap();
            let stored = Path::new(&workspace.library_path()).join(name);
            std::fs::create_dir_all(&stored).unwrap();
            gix::init_bare(&stored).unwrap();
        }

        // Nothing but the repos that can be spelled on one line, so no line
        // of the output splits into a candidate of its own
        assert_eq!(
            offered(Some(&workspace), "workset drop "),
            ["github.com/user/alpha"]
        );
        assert_eq!(
            offered(Some(&workspace), "workset restore "),
            ["github.com/user/stored"]
        );
        assert_eq!(
            offered_to_bash(Some(&workspace), "workset drop ", None),
            ["github.com/user/alpha"]
        );

        // Including through the lines fish is handed, where the candidate and
        // its description are themselves separated by a tab
        let (current_word, candidates) = completions(Some(&workspace), "workset drop ", None);
        assert_eq!(
            fish_lines(current_word, candidates),
            ["github.com/user/alpha\tno commits"]
        );
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
