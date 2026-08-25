This tool manages git repositories in two directories: a "workspace" for active
repos and a "library" of inactive repos. Moving single or groups of repos
between the two should be quick and easy.

|                 |                                                                                                                                                                                     |
| --------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **Workspace**   | Local directory where you clone Git repositories. Initialized with `workset init`.                                                                                                  |
| **Library**     | Local directory (default: `~/.workset`) where **workset** keeps your repos when they're not in your workspace.                                                                      |
| **Working Set** | Set of repos in your workspace at any given time.                                                                                                                                   |
| **Drop**        | Move a repo from your workspace to the library. The repo disappears from your workspace, but remains in the library. Only "clean" repos without uncommitted changes can be dropped. |
| **Restore**     | Bringing a repos from the library back into your workspace.                                                                                                                         |

## Mirroring

Mirroring is opt-in per repo: when enabled, all of the repo's remotes become
mirrors and receive commits the user has pushed to at least one remote (all
shared branches plus tags). Repos with mirroring disabled are skipped entirely
— no fetch, no network activity. Commits that exist only locally are never
pushed. Sync runs in the background on TUI startup, after the interactive
shell exits, when a push from another terminal updates `.git/refs/remotes`,
and periodically while the TUI is open. Diverged mirror refs are reported as
errors, never force-pushed.

Mirroring is enabled with `workset mirror init [pattern]` (without a pattern:
all repos under the current directory), or toggled in the TUI with `Ctrl+R` on
a workspace repo. The flag is stored in the boolean local git config key
`workset.mirror` and survives drop/restore because drop moves `.git` wholesale
and restore copies the original config back over the fresh clone.

The core logic lives in `src/sync.rs`; the TUI scheduling in `SyncManager`
(`src/tui/mod.rs`). `workset mirror sync` runs the same logic from the CLI;
`workset mirror sync --dryrun` reports what would be pushed without pushing,
and `workset mirror sync --watch` re-runs the sync every few minutes.

## Testing

Unit tests live beside the code (`cargo test`). End-to-end tests are
[attest](https://github.com/fossable/attest) shell tests in `tests/`, which
drive the `workset` binary from `PATH`:

```sh
cargo build
attest --bin-dir target/debug tests/
```

## TODO list

- Add an "info" panel above "Library"?
  - If a repo is selected, show stats
    - Total size
    - Clean or number of outstanding changes
    - Show mirror(s) status
