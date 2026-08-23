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
| **Library**     | Local directory (default: `~/.workset`) where **workset** keeps your repos when they're not in your workspace.                                                                      |
| **Working Set** | Set of repos in your workspace at any given time.                                                                                                                                   |
| **Drop**        | Move a repo from your workspace to the library. The repo disappears from your workspace, but remains in the library. Only "clean" repos without uncommitted changes can be dropped. |
| **Restore**     | Bringing a repos from the library back into your workspace.                                                                                                                         |

![](./.github/assets/main.gif)

## Quickstart

All `workset` commands run in reference to the current directory.

```sh
# Initialize a new workspace in the current directory
❯ workset init

# Add a repository to your workspace
❯ workset clone github.com/jqlang/jq

# The repository's local path always reflects the remote path
❯ cd ./github.com/jqlang/jq

# Drop the repo from the working set (it remains in the library: ~/.workset)
❯ cd ..
❯ workset drop ./jq

# Or, you can drop all repositories in the current directory (any that have
# unpushed changes will not be touched).
❯ workset drop

# If you don't want a repo to remain in the library, use --delete
❯ workset drop --delete ./delete_this_repo

# When you need to work on a repository again, it's restored from the local library
❯ workset restore jq
```

The shell autocomplete is smart enough to look at your CWD and suggest repos
that you might want to restore into your working set. Repos that were dropped
most recently are prioritized.

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

## Control your repos

Don't let Github be the only place you store your repos!

**Workset** makes it easy to keep local copies of all of your repos without
having to sift through them to find the ones you're currently working on.

**Workset** can mirror your pushed commits to other remotes of a repo. Opt in
per repo by marking remotes as mirrors — press `Ctrl+R` on a repo in the TUI,
or from the command line:

```sh
git config --add workset.mirror <remote>
```

Push to any remote (from anywhere — inside or outside workset) and the mirror
remotes catch up in the background: all shared branches and tags are
propagated, commits you haven't pushed anywhere are never touched, and
diverged refs show an error instead of being force-pushed. Repos without any
mirror remotes are left alone entirely. `workset sync` does the same from the
command line.

Background pushes never prompt for credentials (SSH runs in batch mode), so
use an SSH agent or credential helper for remotes that need authentication.

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

Completions are generated dynamically by the `workset` binary itself: `drop`
suggests repos in your workspace, and `restore` suggests repos in your library.
