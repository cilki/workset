This tool manages git repositories in two directories: a "workspace" for active
repos and a "library" of inactive repos. Moving single or groups of repos
between the two should be quick and easy.

|                 |                                                                                                                                                                                     |
| --------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **Workspace**   | Local directory where you clone Git repositories. Initialized with `workset init`.                                                                                                  |
| **Library**     | The workspace's own `.workset/` directory, created by `workset init`, where **workset** keeps your repos when they're not in your workspace.                                        |
| **Working Set** | Set of repos in your workspace at any given time.                                                                                                                                   |
| **Drop**        | Move a repo from your workspace to the library. The repo disappears from your workspace, but remains in the library. A repo holding uncommitted changes or unpushed commits is left alone unless you pass `--force`. |
| **Restore**     | Bringing a repos from the library back into your workspace.                                                                                                                         |

## Remote status

The TUI keeps remote state fresh in the background: a sync (fetch every
remote, recompute the repo's status from the refreshed tracking refs) runs
for the workspace repo the selection rests on — debounced, once per
selection, with a per-repo cooldown so returning to a row doesn't
immediately refetch — and for any repo whose `.git/refs/remotes` a push
from another terminal updates. There is no whole-workspace sync: startup
and returning from the interactive shell are covered by the fetch of the
then-selected row. Repo rows show "fetching" while a sync runs, and a fetch
that fails reads `sync failed: fetch <remote>: ...`; when *every* remote
is unreachable the row says nothing at all — the outcome is marked offline
instead, which clears the row's sync status and surfaces per remote in the
info panel as "unreachable". The info panel lists each remote with the
short id its tracking ref holds for the checked-out branch (omitted when
HEAD is detached or the remote doesn't track the branch) and how many
commits it is behind the newest id published to any remote, summed over the
repo's branches ("↓ N behind").

workset never pushes. It does write refs, though, so don't describe a sync
as leaving the repo untouched: the fetch refreshes
`refs/remotes/<remote>/*`, `--prune` deletes the entries whose branches are
gone from the remote, and git's default tag following creates any
`refs/tags/*` reachable from what was fetched. What a sync leaves alone is
the working tree and — with the default fetch refspec — `refs/heads/*`.

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
