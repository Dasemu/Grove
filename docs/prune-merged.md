# Pruning merged worktrees

Worktrees of branches that were pushed, reviewed in a pull request and merged used to stay
forever: `archive_gone` only removes a worktree when its remote branch is deleted, the forge
often keeps the head branch, squash and rebase merges leave no ancestry for git to see, and a
Rust/Node worktree always holds ignored build output that kept it "dirty". The daemon now asks
the forge which pull requests were merged and removes those worktrees itself. Branches merged
into the default branch without a pull request (merged locally and pushed) are removed too.

## Config

```toml
[sync]
prune_merged = true                       # default
prune_disposable = ["target", "node_modules", ".venv", "venv", "__pycache__", ".pytest_cache",
  ".mypy_cache", ".ruff_cache", ".tox", ".gradle", ".next", ".nuxt", ".svelte-kit", ".turbo",
  ".parcel-cache", "dist", "build", "out", "coverage", ".cache"]   # default
prune_protected = []                      # default; extra branch names never pruned
```

- `prune_merged = false` turns the feature off.
- `prune_protected`: extra branch names (exact, no globs) never pruned nor kept from being
  materialized, added to the built-in `main`, `master`, `development`, `develop`, `dev`,
  `staging`, `production` (a `development` -> `main` PR has `development` as its head). The
  default branch (`.bare`'s HEAD) never is either. Other long-lived branches (`release/1.2`,
  `hotfix/x`) merged back with no new commits are pruned like any other: list them here.
- `prune_disposable`: names (not paths, not globs) of ignored files/directories that are
  regenerable and may go with a pruned worktree. Entries must be plain names.

## What counts as merged

`Provider::merged_pulls(owner, repo)` lists one page of the most recently updated closed pull
requests (GitHub: 100, `merged_at` set; Gitea/Forgejo: 50, `merged: true`), keeping only those
whose head repo is the base repo: fork PRs reuse branch names freely. The repo is found from
`origin`'s fetch URL, matched to a configured provider like the visibility check (`phase6.md`),
with the token from `token::resolve`. When that fails, the repo is looked up and, if the forge
reports another owner/name (transferred or renamed: git and the repo endpoint follow the old
`origin` by redirect, the pulls endpoint does not), its pulls are listed under the current one.

Without a pull request, a branch counts as merged when its tip was pushed
(`origin/<branch>` points at it), is a strict ancestor of `origin/<default>` and is not on its
first-parent history, i.e. a merge commit brought it in. This needs no forge. A branch created
from the default branch with no commits of its own is on that history and is never pruned this
way; neither is a fast-forward merge (only the PR path catches those).

## When a worktree is removed

All of these must hold (`grove_core::prune::prune_merged`):

1. The repo is bare layout, the fetch of this pass worked, and the pass is not offline.
2. The worktree is on a branch that is not `.bare`'s HEAD (the default branch) nor in
   `prune_protected`, and it lives
   inside the repo directory (`root/<group>/<repo>/...`, so nested ones such as
   `main/.claude/worktrees/x` count). Linked worktrees elsewhere are the user's and are never
   touched.
3. A merged PR has that branch as head and its head commit equals the local tip, or contains it
   (the PR got more commits from another machine before the merge); or, without a PR, the
   ancestry rule above holds.
4. No local work: no staged, modified or untracked file, no git operation in progress
   (`index.lock`, rebase, merge, cherry-pick, revert, bisect), and no ignored entry whose name
   is not in `prune_disposable` (`.env` keeps it, `target/` does not).

Then the tip is saved under `refs/sync-backup/<machine>/<stamp>/merged/<branch>`, the worktree
is removed (`git worktree remove --force`, needed for the disposable ignored files only) and the
local branch is deleted. Nothing on the forge changes: the remote branch, if any, stays.

A merged worktree with local work is kept and reported, once per change, as `prune_kept`
(`feat/x: merged in PR #12 but kept: ignored files: .env`).

## Not brought back by `auto_materialize`

`materialize` creates a worktree for every remote branch without one, so a pruned branch whose
head was not deleted on the forge would come back on the next pass. It now skips a remote
branch whose tip is the head of a merged PR or the tip this machine pruned (its `merged/` backup
ref), so this holds also offline or when the forge cannot be asked. New commits pushed to the
branch after the merge are new work: the worktree is materialized again.

## Daemon

- The forge is asked at most every 5 minutes per repo, and only when the repo has a worktree
  on a non-default branch. The answer is in memory only.
- A failed lookup keeps the last answer and is logged once as `prune_lookup_failed` (until the
  error changes).
- Offline passes (backoff) prune nothing, but still use the last answer to keep pruned branches
  from being materialized.
- The ancestry rule runs on every online pass, also when the forge lookup fails.
- Activity log: `pruned_merged` (removed, with the PR or "merged into the default branch", path
  and backup ref) and `prune_kept`.

`grove sync <path>` does the same in one shot (no cache) and prints `pruned ...` / `kept ...`
lines; a failed lookup is a warning, and only the ancestry rule applies.

## Validation

- `grove-core/tests/prune.rs`: squash merge with the head branch kept on the remote (pruned with
  `target/` present, backup ref, not re-materialized with or without a forge answer, comes back
  after a new commit), local work (untracked file, non-disposable ignored file, local commit)
  keeps the worktree, an older tip contained in the PR head is pruned, the default branch and a
  linked worktree outside the repo are never touched.
- `grove-providers/tests/api.rs`: request shape on both forges, only merged same-repo PRs kept
  (fork and deleted-fork heads left out), API errors surface.
- `grove-daemon` `merged` unit tests: TTL cache, last answer kept on failure, error and kept
  notes logged once.
- Config: defaults and validation of `prune_disposable`.

## Not verified

- Against a real GitHub or Gitea (mock-tested only).
- Only the first page of closed PRs is read: a branch merged long ago in a busy repo (more than
  100 / 50 PRs closed since) is not found. `grove sync` on it does not help either; remove it by
  hand.
