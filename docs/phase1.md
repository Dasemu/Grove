# Phase 1: core + CLI

Crates: `grove-core` (library) and `grove` (CLI). Everything shells out to the system `git`.

## CLI

| Command | What it does |
|---|---|
| `grove status [path]` | State of every worktree (`Synced`, `Dirty`, `Ahead`, `Behind`, `BehindDirty`, `Diverged`, `Error`) |
| `grove snapshot [path]` | Snapshot the worktree into `refs/sync/<machine>/<branch>` (local only) |
| `grove add <path>` | Migrate a plain clone to `.bare/` + one worktree per branch, in place |
| `grove sync [path] [--offline]` | One full pass (below) |
| `grove doctor` | Checks git, config, root, repo remotes and refspecs, inotify limit |

Config: `~/.config/grove/config.toml` (or `--config`). Machine names are embedded in ref names, so
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
  refuses to overwrite untracked files. Both it and the fast-forward in `receive` also decline when
  the incoming change would overwrite a local *ignored* file (`ignored_conflicts`).
- `archive_gone` keeps any worktree whose `status --porcelain --ignored` is non-empty (so `.env`
  and similar are never deleted), never archives when the remote lists no branches, and writes a
  backup ref before deleting a branch.
- Backup refs live in per-kind namespaces (`refs/sync-backup/<machine>/<stamp>/<kind>`, kinds
  `add`, `receive/<branch>`, `apply/<branch>`, `archive/<branch>`) so several in one pass cannot
  collide as ref directory/file.
- `add` also refuses sparse checkouts and `extensions.worktreeConfig`.

## Known limitations

- Nested untracked git repos are snapshotted only as gitlinks (their content is not synced).
- No crash-resume: a hard kill in the middle of `add` leaves `.grove-stage`; the error says where
  the files are, but recovery is manual (the backup tar and ref exist).
- Machine-name uniqueness across machines is not enforced.
- Split-index repos are untested.

## Not done yet (later phases)

- Real two-PC validation by the author (acceptance criterion of this phase).
- Daemon, file watching, scheduling, IPC (Phase 2). Locking against concurrent user git commands.
- Migration of repos with submodules or existing linked worktrees; `new` and provider code (Phase 4).
- Resolve actions for `Diverged` (Phase 5): today it is only detected and left intact.

## Validation on the real Gitea (2026-11-16)

Two simulated machines (`pca` as a plain clone, `pcb` migrated with `grove add`) against a
throwaway private repo on git.zenvairo.es (Gitea 1.25.4, HTTPS), deleted afterwards. All via the
`grove` binary:

| Scenario | Result |
|---|---|
| `add` on a plain clone | Migrated, backup tar + ref created |
| Uncommitted + untracked work on A, `sync` on both | Snapshot pushed to `refs/sync/pca/main`, applied on B |
| B has local edits when A's work arrives | `BehindDirty`, local edits untouched, snapshot declined |
| A commits and pushes, B clean | B fast-forwarded |
| Branch created then deleted upstream | Worktree materialized, then archived with a backup ref |
| B has an unpushed commit, A pushes | `Diverged`, nothing modified |

Not covered: real two-PC run (same code paths, but different machines/filesystems), push-mirror
exclusion of `refs/sync/*` on this Gitea (needs a mirror target and credentials).

Observed UX noise: a stale snapshot from another machine is reported as "not applied" on every
pass until that machine syncs again.
