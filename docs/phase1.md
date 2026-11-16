# Phase 1: core + CLI

Crates: `gitsync-core` (library) and `gitsync` (CLI). Everything shells out to the system `git`.

## CLI

| Command | What it does |
|---|---|
| `gitsync status [path]` | State of every worktree (`Synced`, `Dirty`, `Ahead`, `Behind`, `BehindDirty`, `Diverged`, `Error`) |
| `gitsync snapshot [path]` | Snapshot the worktree into `refs/sync/<machine>/<branch>` (local only) |
| `gitsync add <path>` | Migrate a plain clone to `.bare/` + one worktree per branch, in place |
| `gitsync sync [path] [--offline]` | One full pass (below) |
| `gitsync doctor` | Checks git, config, root, repo remotes and refspecs, inotify limit |

Config: `~/.config/gitsync/config.toml` (or `--config`). Machine names are embedded in ref names, so
they may only contain letters, digits, `-` and `_`.

## Sync pass (`sync_repo`)

1. `fetch --prune` plus `+refs/sync/*:refs/sync-remote/*` (other machines' snapshots land in a
   separate namespace so a machine's own `refs/sync/*` is never overwritten).
2. Bare layout only: archive worktrees whose upstream branch is gone, then materialize worktrees for
   new remote branches.
3. Per worktree: snapshot (if dirty, or if an older snapshot ref exists so a stale WIP never lingers)
   and force-push the machine's own snapshot ref; fast-forward if clean and strictly behind;
   apply another machine's snapshot as working changes if clean, not ahead, and HEAD can fast-forward
   to the snapshot's base commit.
4. Step failures are reported per worktree and never abort the pass. Dirty or diverged worktrees
   are never modified.

## Safety mechanisms

- `add`: full tar backup + `refs/sync-backup/<machine>/<ts>` first; refuses detached HEAD, no commits,
  merge/rebase/cherry-pick/bisect in progress, submodules, existing linked worktrees, backup dir
  inside the repo. Staged state, stashes, untracked and ignored files are preserved (the index is
  carried over). Status, stashes and refs are compared before/after; any failure triggers rollback
  to the original layout (tested by injecting a failure after every step).
- Every operation that rewrites a worktree writes a backup ref first.
- `apply_snapshot` declines (changing nothing) on any local work; `read-tree -m -u` additionally
  refuses to overwrite untracked files.
- `archive_gone` keeps dirty worktrees and writes a backup ref before deleting a branch.

## Not done yet (later phases)

- Real two-PC validation by the author (acceptance criterion of this phase).
- Daemon, file watching, scheduling, IPC (Phase 2). Locking against concurrent user git commands.
- Migration of repos with submodules or existing linked worktrees; `new` and provider code (Phase 4).
- Resolve actions for `Diverged` (Phase 5): today it is only detected and left intact.
