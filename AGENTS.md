This tool manages git repositories in two directories: a "workspace" for active
repos and a "library" of inactive repos. Moving single or groups of repos
between the two should be quick and easy.

|                 |                                                                                                                                                                                     |
| --------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **Workspace**   | Local directory where you clone Git repositories. Initialized with `workset init`.                                                                                                  |
| **Library**     | The workspace's own `.workset/` directory, created by `workset init`, where **workset** keeps your repos when they're not in your workspace.                                        |
| **Working Set** | Set of repos in your workspace at any given time.                                                                                                                                   |
| **Drop**        | Move a repo from your workspace to the library. The repo disappears from your workspace, but remains in the library. Only "clean" repos without uncommitted changes can be dropped. |
| **Restore**     | Bringing a repos from the library back into your workspace.                                                                                                                         |

## Remote status

The TUI keeps remote state fresh in the background: a sync (fetch every
remote, recompute the repo's status from the refreshed tracking refs) runs
on TUI startup, after the interactive shell exits, when a push from another
terminal updates `.git/refs/remotes`, and periodically while the TUI is
open. Repo rows show "fetching" while a sync runs, and fetch failures
surface on the row; when every remote is unreachable the repo is reported
as offline instead. The info panel lists each remote with how many commits
it is behind the newest id published to any remote for each branch
("↓ N behind"). workset never pushes or modifies refs — local or remote.

The core logic lives in `src/sync.rs`; the TUI scheduling in `SyncManager`
(`src/tui/mod.rs`).

## Testing

Unit tests live beside the code (`cargo test`). End-to-end tests are
[attest](https://github.com/fossable/attest) shell tests in `tests/`, which
drive the `workset` binary from `PATH`:

```sh
cargo build
attest --bin-dir target/debug tests/
```

## TODO list
