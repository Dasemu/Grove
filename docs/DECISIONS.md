# Decisions

1. Custom refs on Gitea/Forgejo: supported on Forgejo 11, no fallback needed (see `phase0-spike.md`). Also confirmed on git.zenvairo.es (Gitea 1.25.4); mirror exclusion still to verify there.
2. Snapshots are auto-applied only when the target worktree is clean, has no unpushed commits and HEAD can fast-forward to the snapshot's base; otherwise declined and reported (implemented in `receive::apply_snapshot`).
3. Config: TOML for user intent (implemented); runtime state storage still open.
4. License: MIT (decided).
5. Final name: Grove (decided; binary `grove`, crates `grove-core` and `grove`).
6. Open: `groved` under systemd gets `PATH=/usr/local/bin:/usr/bin:/bin`, so a `tea`/`gh` in `~/.local/bin` is not found and the token lookup fails. Options: ship `Environment=PATH=%h/.local/bin:...` in `contrib/groved.service`, or resolve the helpers by absolute path (`~/.local/bin`, then `PATH`). Workaround in use: a user drop-in (see `two-pc-run.md`).
7. Open: `grove add` refuses `extensions.worktreeConfig = true` even when no `config.worktree` exists. Claude Code worktrees leave that flag behind. Options: accept it when no per-worktree config file exists (and sparse checkout is off), or keep refusing and print the exact `git config --unset` hint.
