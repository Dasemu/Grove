# Real two-PC run

First real run on two machines, 16/11/2026: `Dasemu-LT` (laptop, already running Grove) and
`Dasemu-PC` (desktop, Fedora 44, KDE Plasma / Wayland, installed fresh from `development` at
`e6030f7`). Real forges: Gitea `git.zenvairo.es` (SSH on port 2222) and github.com, both with
`protocol = "ssh"`.

## Result

Passed. An uncommitted file in a private test repo on `Dasemu-PC` was snapshotted by the daemon
and pushed as `refs/sync/Dasemu-PC/main` (snapshot commit on top of `main`, containing only the
untracked file). On `Dasemu-LT`, after `grove import Darío <test repo>` and one daemon pass, the
snapshot showed up as `refs/sync-remote/Dasemu-PC/main`. The other direction was already visible
on real repos: `Academia-LMS` carries both `refs/sync/Dasemu-LT/feat/student-portal` and
`refs/sync/Dasemu-PC/feat/student-portal`. The throwaway repo was deleted from Gitea afterwards.

Notes:

- Right after `grove import`, the clone has no `refs/sync-remote/*` yet; they arrive with the
  daemon's first pass over the new repo (up to `fetch_interval_s`, or `grove daemon sync PATH`).
- A repo created while the daemon runs is picked up (`repo_added`), but its worktree states only
  appear after its first pass.

## Install on the second machine

`cargo test` passed; `grove`, `groved` and `grove-gui` installed with `cargo install --root ~/.local`.
`grove doctor` found both tokens (tea, gh). Migration of the existing `~/Hacking`:

- 14 plain clones migrated with `grove add`; 9 of them had a read-only Gitea pull mirror as
  `origin` and were switched to their GitHub source (the `e6030f7` fix), Grove itself included.
- 19 missing repos cloned with `grove import`; 4 unversioned folders turned into repos with
  `grove new` (2 public on GitHub with a Gitea mirror, 2 private on Gitea).
- First daemon pass: no `diverged`, 8 `dirty`, 2 `ahead`; 36 public repos report
  `snapshots withheld` as intended.

## Problems found

1. **systemd PATH.** The user unit runs with `PATH=/usr/local/bin:/usr/bin:/bin`. `tea` lived in
   `~/.local/bin`, so under systemd `groved` found no Gitea token (`grove doctor` run with that
   PATH: `no token for provider "gitea"`). Worked around with a drop-in
   `~/.config/systemd/user/groved.service.d/path.conf` setting
   `Environment=PATH=%h/.local/bin:/usr/local/bin:/usr/bin:/bin`. See `DECISIONS.md` (6).
2. **Leftover `extensions.worktreeConfig`.** Claude Code worktrees set
   `extensions.worktreeConfig = true`; it stays after the worktrees are removed. `grove add`
   refused 12 such clones ("sparse checkout / per-worktree config is not supported yet") although
   none had a `config.worktree` file. Unsetting the extension (only when no `config.worktree`
   exists and sparse checkout is off) let them migrate. See `DECISIONS.md` (7).
3. **Clones with a dirty linked worktree** still cannot be migrated (2 repos left as plain clones).
4. **Repos deleted on the forge after import** (2 on Gitea) make the daemon log a fetch error on
   every pass. `development` now moves such repos to `Purged/` (PR #4); not part of this install.
