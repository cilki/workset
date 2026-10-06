<p align="center">
	<img src="https://raw.githubusercontent.com/fossable/fossable/master/emblems/workset.svg" style="width:90%; height:auto;"/>
</p>

![License](https://img.shields.io/github/license/fossable/workset)
![Build](https://github.com/fossable/workset/actions/workflows/test.yml/badge.svg)
![GitHub repo size](https://img.shields.io/github/repo-size/fossable/workset)
![Stars](https://img.shields.io/github/stars/fossable/workset?style=social)

<hr>

**workset** is yet another tool for managing your local git repos.

|                 |                                                                                                                                                                                     |
| --------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **Workspace**   | Local directory where you clone Git repositories. Initialized with `workset init`.                                                                                                  |
| **Library**     | The workspace's own `.workset/` directory, created by `workset init`, where **workset** keeps your repos when they're not in your workspace.                                        |
| **Working Set** | Set of repos in your workspace at any given time.                                                                                                                                   |
| **Drop**        | Move a repo from your workspace to the library. The repo disappears from your workspace, but remains in the library. Only "clean" repos without uncommitted changes can be dropped. |
| **Restore**     | Bringing a repos from the library back into your workspace.                                                                                                                         |

![](./.github/assets/main.gif)

## Quickstart

`workset init` and a bare `workset drop` act on the current directory. `clone`
and `restore` place a repo at its full path under the workspace root, and `list`
and `status` always cover the whole workspace, wherever you run them from.

```sh
# Initialize a new workspace in the current directory
❯ workset init

# Add a repository to your workspace
❯ workset clone github.com/jqlang/jq

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

### What a drop keeps

A drop stores the repository, not the directory: the repo's `.git` directory is
moved into the library and the working tree is deleted, and `restore` checks the
working tree back out of the library. So a round trip keeps everything git has —
every branch, tag and stash — but nothing it doesn't.

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

Two read-only commands report on the whole workspace, no matter which directory
you run them from:

```sh
# Every repo in the workspace, with its status
❯ workset list
Repositories in workspace (/home/user/workspace):

  github.com/jqlang/jq - ✓ clean
  github.com/fossable/workset - ⚠ modified

# Workspace and library totals
❯ workset status
Workspace: /home/user/workspace

Library: /home/user/workspace/.workset
  12 repository(ies) in library

Active repositories: 2
  ✓ 1 clean
  ⚠ 1 with uncommitted changes
```

Running `workset` with no subcommand opens the TUI, where `?` shows the
keybindings.

Shell completion fills in the repo paths for you: `restore` suggests everything
in the library, while `drop` suggests the repos currently in your working set.
Candidates are always workspace-relative paths, so they work from any directory
inside the workspace. Under fish, each workspace repo is annotated with its
status and how long ago it changed.

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
