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

Mirroring is opt-in per repo: remotes listed in the multi-valued local git
config key `workset.mirror` receive commits the user has pushed to at least one
remote (all shared branches plus tags). Repos without any `workset.mirror`
entries are skipped entirely — no fetch, no network activity. When mirrors are
configured, all remotes are still fetched (the planner needs to see where a ref
is published, and the status display needs fresh tracking refs), but only mirror
remotes are pushed to; divergence on a non-mirror remote is not an error.
Commits that exist only locally are never pushed. Sync runs in the background on
TUI startup, after the interactive shell exits, when a push from another
terminal updates `.git/refs/remotes`, and periodically while the TUI is open.
Diverged mirror refs are reported as errors, never force-pushed.

Mirror remotes are toggled in the TUI with `Ctrl+R` on a workspace repo, or
manually with `git config --add workset.mirror <remote>`. The config survives
drop/restore because drop moves `.git` wholesale and restore copies the original
config back over the fresh clone.

The core logic lives in `src/sync.rs`; the TUI scheduling in `SyncManager`
(`src/tui/mod.rs`). `workset mirror` runs the same logic from the CLI;
`workset mirror --dryrun` reports what would be pushed without pushing.

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
