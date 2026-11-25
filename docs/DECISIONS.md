# Decisions

1. Custom refs on Gitea/Forgejo: supported on Forgejo 11, no fallback needed (see `phase0-spike.md`). Also confirmed on git.zenvairo.es (Gitea 1.25.4); mirror exclusion still to verify there.
2. Snapshots are auto-applied only when the target worktree is clean, has no unpushed commits and HEAD can fast-forward to the snapshot's base; otherwise declined and reported (implemented in `receive::apply_snapshot`).
3. Config: TOML for user intent (implemented); runtime state storage still open.
4. License: MIT (decided).
5. Final name: Grove (decided; binary `grove`, crates `grove-core` and `grove`).
6. Open: `groved` under systemd gets `PATH=/usr/local/bin:/usr/bin:/bin`, so a `tea`/`gh` in `~/.local/bin` is not found and the token lookup fails. Options: ship `Environment=PATH=%h/.local/bin:...` in `contrib/groved.service`, or resolve the helpers by absolute path (`~/.local/bin`, then `PATH`). Workaround in use: a user drop-in (see `two-pc-run.md`).
7. Open: `grove add` refuses `extensions.worktreeConfig = true` even when no `config.worktree` exists. Claude Code worktrees leave that flag behind. Options: accept it when no per-worktree config file exists (and sparse checkout is off), or keep refusing and print the exact `git config --unset` hint.
8. Merged worktrees are pruned (`prune-merged.md`), but the PR's head branch is left on the forge and this machine's `refs/sync/<machine>/<branch>` snapshot ref stays there too. Open: should grove delete either once the PR is merged (the forge's "delete branch after merge" covers the first)?
9. Open: a branch recreated under the name of a fast-forward-merged PR before the default branch moves has the PR head as tip and is pruned at once if clean (nothing is lost; a backup ref is kept). Options: accept it, or require the worktree to be older than the merge.
10. Open: `groved` reads `config.toml` only at start, so namespaces/providers added with `grove group|provider add` (or the GUI) need `systemctl --user restart groved`. Options: an IPC `reload` command the CLI/GUI calls after saving, or watching the file. Also open: config edits rewrite the file without comments (backup in `config.toml.bak`); `toml_edit` would keep them but is not an approved dependency.
11. Open: the `claude-desktop` built-in launcher runs `claude-desktop <worktree>`. Whether the Linux Claude desktop app accepts a folder (and opens Claude Code there) is unverified; override it with `[[open.launchers]] name = "claude-desktop"` once the real command is known.
