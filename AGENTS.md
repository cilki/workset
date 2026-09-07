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

Mirroring is on by default: all of a repo's remotes are mirrors and receive
commits the user has pushed to at least one remote, for every branch and tag.
Propagation is remote→remote: for each ref the newest published id is
mirrored to the remotes that are behind or missing it, even when the local
checkout is behind or doesn't have the ref at all (e.g. a push made from
another machine). Local refs are never modified. Repos with mirroring
disabled are skipped entirely — no fetch, no network activity. Commits that
exist only locally are never pushed.
The TUI never pushes on its own: a check-only sync (fetch remotes, report
what would be pushed) runs in the background on TUI startup, after the
interactive shell exits, when a push from another terminal updates
`.git/refs/remotes`, and periodically while the TUI is open. Pending pushes
show up on the repo row ("↑ N to push") and per remote in the info panel;
pressing `s` on a repo (or a directory node, covering every repo under it)
performs the actual push. Repo rows show "fetching" during the fetch phase
and "syncing" only while commits are being pushed. Diverged mirror refs are
reported as errors, never force-pushed.

A repo opts out by setting the local git config key `workset.mirror` to
false; any other value (or no key at all) means enabled. The TUI toggles the
key with `m` on a workspace repo. The old multi-valued pattern keys
`workset.mirrorBranches` and `workset.mirrorTags` are ignored. The key
survives drop/restore because drop moves `.git` wholesale and restore copies
the original config back over the fresh clone.

The core logic lives in `src/sync.rs`; the TUI scheduling in `SyncManager`
(`src/tui/mod.rs`). `workset mirror` runs the same logic from the CLI
(pushing by default, unlike the TUI's background checks);
`workset mirror --dryrun` reports what would be pushed without pushing,
and `workset mirror --watch` re-runs the sync every few minutes.

## Testing

Unit tests live beside the code (`cargo test`). End-to-end tests are
[attest](https://github.com/fossable/attest) shell tests in `tests/`, which
drive the `workset` binary from `PATH`:

```sh
cargo build
attest --bin-dir target/debug tests/
```

## TODO list
