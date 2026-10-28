<p align="center">
	<img src="https://raw.githubusercontent.com/fossable/fossable/master/emblems/workset.svg" style="width:90%; height:auto;"/>
</p>

![License](https://img.shields.io/github/license/fossable/workset)
![Build](https://github.com/fossable/workset/actions/workflows/ci.yml/badge.svg)
![GitHub repo size](https://img.shields.io/github/repo-size/fossable/workset)
![Stars](https://img.shields.io/github/stars/fossable/workset?style=social)

<hr>

**workset** is yet another tool for managing your local git repos.

|                 |                                                                                                                                                                                     |
| --------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **Workspace**   | Local directory where you clone Git repositories. Initialized with `workset init`.                                                                                                  |
| **Library**     | The workspace's own `.workset/` directory, created by `workset init`, where **workset** keeps your repos when they're not in your workspace.                                        |
| **Working Set** | Set of repos in your workspace at any given time.                                                                                                                                   |
| **Drop**        | Move a repo from your workspace to the library. The repo disappears from your workspace, but remains in the library. A repo holding uncommitted changes or unpushed commits is left alone unless you pass `--force`. |
| **Restore**     | Bringing a repos from the library back into your workspace.                                                                                                                         |

![](./.github/assets/main.gif)

## Quickstart

`workset init` and a bare `workset drop` act on the current directory. `clone`
and `restore` place a repo at its full path under the workspace root, and `list`
(aliased `ls`) always covers the whole workspace, wherever you run it from.

```sh
# Initialize a new workspace in the current directory
❯ workset init

# Add a repository to your workspace
❯ workset clone github.com/jqlang/jq

# A clone URL is a pattern too, so the URL a forge gives you can be pasted
# straight in — scheme, credentials and the '.git' suffix are all dropped, and
# every spelling below names that same github.com/jqlang/jq
❯ workset clone https://github.com/jqlang/jq.git
❯ workset clone git@github.com:jqlang/jq.git

# The repository's local path always reflects the remote path
❯ cd ./github.com/jqlang/jq

# Drop the repo from the working set (it remains in the library, which is the
# .workset directory in the workspace root)
❯ cd ..
❯ workset drop ./jq

# Or, you can drop every repository at or below the current directory (any that
# have uncommitted or unpushed changes will not be touched). Run from the
# workspace root, this empties the whole working set.
❯ workset drop

# If you don't want a repo to remain in the library, use --delete
❯ workset drop --delete ./delete_this_repo

# When you need to work on a repository again, it's restored from the local library
❯ workset restore jq
```

A pattern isn't limited to a single repo. `drop` reads its pattern as a path in
the workspace, so naming a directory takes everything under it, while `restore`
matches library paths by substring — which is why the bare `jq` above finds
`github.com/jqlang/jq`, and why a short pattern can pull back more than you
meant:

```sh
# Drops github.com/jqlang/jq, github.com/fossable/workset, and so on
❯ workset drop github.com

# Restores every repo in the library whose path contains 'jq'
❯ workset restore jq
```

A pattern that is a repo's full path — the name `drop` reports and completion
offers — restores just that repo, even when it is also a prefix of others such
as `github.com/user/repo-old`.

`clone`, `restore` and `drop` take as many patterns as you want to name, and
each one is reported on its own:

```sh
❯ workset drop github.com/jqlang/jq github.com/fossable/workset no-such-repo
  github.com/jqlang/jq - ✓ dropped
  github.com/fossable/workset - ✓ dropped
No repository in the workspace matches 'no-such-repo'
❯ echo $?
1
```

### What a drop keeps

A drop stores the repository, not the directory: the repo's `.git` directory is
moved into the library and the working tree is deleted, and `restore` checks the
working tree back out of the library. So a round trip keeps everything git has —
every branch, tag and stash — but nothing it doesn't.

A repo's path only exists to hold the repo, so the directories above it go with
it as far up as they are left empty: dropping `github.com/jqlang/jq` takes
`github.com/jqlang` too, and `github.com` as well once it holds nothing else.
Anything that isn't workset's keeps its directory — another repo, a file of your
own — and so does the directory you happen to be standing in, so `workset drop`
from the workspace root is what empties the tree completely.

Untracked files are safe, because they make the repo "modified" and a drop
refuses to touch it. **Ignored** files are not: git doesn't report them, so the
repo counts as clean and they are deleted without warning and don't come back.

```sh
❯ cat github.com/jqlang/jq/.gitignore
target/
.env

# Ignored files don't make a repo modified
❯ workset list
Repositories in workspace (/home/user/workspace):

  github.com/jqlang/jq - ✓ clean

❯ workset drop ./github.com/jqlang/jq
  github.com/jqlang/jq - ✓ dropped
❯ workset restore jq
Restored github.com/jqlang/jq

# target/ and .env are gone for good
❯ ls -A github.com/jqlang/jq
.git
.gitignore
Makefile.am
src
```

Losing `target/` is usually the point — it's what makes a dropped repo cheap to
keep. Losing an untracked-but-ignored `.env` is not, so keep anything you can't
regenerate either committed or outside the repo.

A commit no remote has makes the repo `⚠ unpushed`, and a drop keeps it where
it is until you pass `--force`. Every ref counts, not just the branch you have
checked out: a branch you left behind and a `git stash` hold commits that exist
nowhere else just as much as `HEAD` does. A repo that was never pushed anywhere
— no remote, or nothing fetched from one yet — has no published history to
compare against, so it isn't measured this way at all.

A submodule is half-kept. Its git directory lives under the parent's `.git`, so
it travels into the library with everything else, but its checkout is part of
the parent's working tree, which the drop deletes. A round trip therefore brings
the submodule's history back and leaves its directory empty, and neither `list`
nor `git status` in the parent points that out — both call the repo clean.
`git submodule update` fills the directory in again from the git directory that
came back, without fetching anything:

```sh
❯ git -C github.com/jqlang/jq submodule update --init
Submodule path 'vendor/oniguruma': checked out '4ef89209a239c1aea328cf13c05a2807e5c146d1'

❯ workset drop ./github.com/jqlang/jq
  github.com/jqlang/jq - ✓ dropped
❯ workset restore jq
Restored github.com/jqlang/jq

# The submodule's directory came back empty, and git calls it uninitialized
❯ ls -A github.com/jqlang/jq/vendor/oniguruma
❯ git -C github.com/jqlang/jq submodule status
-4ef89209a239c1aea328cf13c05a2807e5c146d1 vendor/oniguruma

❯ git -C github.com/jqlang/jq submodule update
Submodule path 'vendor/oniguruma': checked out '4ef89209a239c1aea328cf13c05a2807e5c146d1'
```

A repository **workset** can't read at all is kept too, and reported as
`✗ unreadable` by `list`. Not every repository git can write can be read by the
library **workset** uses, so rather than guess at what such a repo holds, it is
left where it is until you drop it with `--force`.

A repo whose `core.worktree` names some other directory reads as `✗ unreadable`
for the same reason. A status answers what the directory in your workspace holds
that exists nowhere else, and a drop deletes that directory on the strength of
the answer — so if git would look somewhere else to work it out, **workset** has
nothing to say about this repo rather than something about another directory.
Remove the key (`git config --unset core.worktree`) and the repo reads normally;
a drop that takes it anyway, with `--force`, leaves the key behind rather than
storing it, so what comes back out of the library reports on its own directory.

### What a drop won't touch at all

A drop moves a git directory into the library and deletes the working tree
around it, which only makes sense for a repo that owns its git directory and is
the only working tree using it. `git worktree add` breaks both halves of that,
so a linked worktree inside the workspace leaves you with two repos that stay
put — and `--force` is no help here, because neither is outstanding work you
could decide to discard:

```sh
❯ git -C github.com/jqlang/jq worktree add ../jq-wt some-branch

❯ workset drop --force
  github.com/jqlang/jq - ⚠ kept (git directory shared with 1 linked worktree, remove it with 'git worktree remove' first)
  github.com/jqlang/jq-wt - ⚠ kept (git directory belongs to another repository, drop that repository instead)
❯ echo $?
1
```

The main worktree is kept because its git directory is the one the linked
worktree reads its objects and refs from; the linked worktree is kept because
its `.git` is a file pointing back at that git directory, so there's nothing
here for the library to store. `git worktree remove` leaves an ordinary repo
behind, which drops as usual:

```sh
❯ git -C github.com/jqlang/jq worktree remove ../jq-wt

❯ workset drop
  github.com/jqlang/jq - ✓ dropped
```

A repo whose `.git` is a *symlink* stays put for the same reason. git reads one
as an ordinary repository, but the git directory is wherever the link points —
another disk, your home directory, anywhere — so there is nothing here the
library can take: moving the link would store a pointer rather than the repo
and leave its history where it is, still shared with whatever else reads
through the same link.

```sh
❯ workset drop --force github.com/jqlang/jq
  github.com/jqlang/jq - ⚠ kept (git directory is a symlink to somewhere else, replace the link with the git directory it points at)
```

Every command reports what it did and exits non-zero when it couldn't do all of
it, so `workset` composes with other commands:

```sh
# Repos with uncommitted or unpushed changes are named, and the drop fails
❯ workset drop
  github.com/jqlang/jq - ✓ dropped
  github.com/fossable/workset - ⚠ kept (uncommitted changes, use --force to drop anyway)
❯ echo $?
1
```

`list` reports on the whole workspace, no matter which directory you run it
from: every repo with its status, and how many repos the library is holding
for you.

```sh
❯ workset list
Repositories in workspace (/home/user/workspace):

  github.com/jqlang/jq - ✓ clean
  github.com/fossable/workset - ⚠ modified

Library (/home/user/workspace/.workset): 12 repository(ies)
```

Running `workset` with no subcommand opens the [TUI](#the-tui).

Shell completion fills in the repo paths for you: `restore` suggests everything
in the library, while `drop` suggests the repos currently in your working set.
Candidates are always workspace-relative paths, so they work from any directory
inside the workspace. Under fish, each workspace repo is annotated with its
status and how long ago it changed.

## The TUI

`workset` with no subcommand opens the interactive view: your working set on the
left, the library on the right, both as a collapsible tree of the repos' remote
paths. `?` toggles an overlay listing the keybindings for the panel you're in.

| Key     | Action                                                                  |
| ------- | ----------------------------------------------------------------------- |
| `Tab`   | switch section (swap panels when the terminal is too narrow for both)   |
| `↑`/`↓` | navigate                                                                |
| `←`/`→` | expand or collapse the selected directory                               |
| `Enter` | workspace: open a shell in the repo — library: restore it                |
| `d`     | drop the selected repo, or every repo under the selected directory      |
| `/`     | fuzzy-filter both trees                                                 |
| `?`     | show this list                                                          |
| `Esc`   | quit (`Ctrl+C` too)                                                     |

`d` is offered in the workspace panel only, since there is nothing in the
library to drop; the overlay leaves the row out while the library has focus.

`Enter` on a workspace repo hands the terminal to `$SHELL` in that repo's
directory; leaving the shell comes back to the TUI. `d` is held to the same rule
as `workset drop`, so a repo with uncommitted changes or unpushed commits stays
where it is and its row says which. Cloning is `workset clone` and has no key of
its own.

### Remote status

While the TUI is open, workset fetches remotes in the background: for the
workspace repo your selection comes to rest on — so moving down a long list
doesn't fetch everything you pass over, and coming back to a row you just
visited doesn't fetch it again — and for any repo whose remote-tracking refs a
push from another terminal updates. A repo's row reads `fetching` while that
runs, and `sync failed: ...` if a fetch comes back with an error. Selecting a
repo expands it with what the fetch learned:

```
  ✓ demo                                                           1m ago
      Size: 11B
      Changes: clean
      Remotes:
        backup 852616d ↓ 1 behind
        origin f6d98dc ✓ in sync
```

Each remote is followed by the id its tracking ref holds for the checked-out
branch — omitted when `HEAD` is detached, or when that remote doesn't have the
branch — and then by its state: `✓ in sync`, `not synced yet` before the first
fetch lands, `fetching…` while one is running, or `⚠ unreachable` when it
couldn't be reached at all.

The count is how many commits that remote is behind the newest commit published
to any of the repo's remotes, added up over its branches; a remote missing a
branch altogether, or one that has diverged, isn't counted. So a push you made
from another machine shows up here as the remotes that didn't get it.

workset only ever fetches — bringing a lagging remote up to date is left to you.
Nothing is pushed, and your branches and working tree are not touched; what a
fetch does rewrite is the repo's remote-tracking refs, dropping the ones whose
branches are gone from the remote and picking up any tags it reaches.

## Keep your working set small

The point of dropping repos out of your workspace is to avoid the inevitable
accumulation of stagnant repos.

By keeping your _working set_ small, you reduce the cognitive (and CPU) load
required to search through your repos. It also makes it easier to see which
repos have outstanding changes that need to be finished and pushed.

Adhering to this principle manually involves frequently cloning and deleting
repositories from your workspace which is probably more effort wasted than
saved.

`workset` makes these mechanics _fast_ and _easy_. When repositories are dropped
from your workspace, they are just saved locally in a library so restoring them
later can be done in an instant.

## Installation

<details>
<summary>Crates.io</summary>

![Crates.io Total Downloads](https://img.shields.io/crates/d/workset)

#### Install from crates.io

```sh
cargo install workset
```

</details>

<details>
<summary>Nixpkgs</summary>

#### Install from nixpkgs

[Nixpkgs](https://search.nixos.org/packages?channel=unstable&query=workset):
`nix-env -i workset`

</details>

### Shell completions

Completion scripts for bash and fish live in [`completions/`](./completions).

```sh
# Bash: source it from ~/.bashrc (or copy into a bash-completion directory)
source /path/to/workset/completions/workset.bash

# Fish: copy it where fish autoloads completions
cp /path/to/workset/completions/workset.fish ~/.config/fish/completions/
```

Completions are generated dynamically by the `workset` binary itself, which the
scripts above invoke on every TAB press.
