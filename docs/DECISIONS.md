# Decisions

1. Custom refs on Gitea/Forgejo: supported on Forgejo 11, no fallback needed (see `phase0-spike.md`). Also confirmed on git.zenvairo.es (Gitea 1.25.4); mirror exclusion still to verify there.
2. Snapshots are auto-applied only when the target worktree is clean, has no unpushed commits and HEAD can fast-forward to the snapshot's base; otherwise declined and reported (implemented in `receive::apply_snapshot`).
3. Config: TOML for user intent (implemented); runtime state storage still open.
4. License: MIT (decided).
5. Final name: Grove (decided; binary `grove`, crates `grove-core` and `grove`).
