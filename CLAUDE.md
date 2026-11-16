# Grove (working name: gitsync)

## Overview
Linux tool that keeps chosen git repos continuously in sync across machines, including
uncommitted work, via snapshot refs (`refs/sync/<machine>/<branch>`) on a primary remote.
Planned as a Rust workspace (core, providers, daemon, CLI, Tauri GUI). Currently in
**Phase 0 done, Phase 1 in progress**: `gitsync-core` has the git runner, snapshots and state classification; config loading, worktree inspection and a minimal CLI (`status`, `snapshot`) exist; migration (`add`), receive logic, `doctor` and two-machine tests are pending. Do not start a
new phase before its predecessor's results are written to `docs/`.

## Commands
- Check shell scripts: `for f in spikes/*.sh; do bash -n "$f"; done`
- Throwaway Forgejo (dev only, Podman): `spikes/forgejo-up.sh`, then `spikes/phase0.sh`
- Rust: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`

## Architecture
See `docs/` (`phase0-spike.md`, `DECISIONS.md`). Git is the storage layer; shell out to
system `git`; snapshots use a temp `GIT_INDEX_FILE` + `commit-tree`, never touching the
user's branch or index.

## Conventions
- Dependencies: serde+toml (config), clap+anyhow (CLI), approved. Ask before adding more.
- Everything in the repo is English. Open questions go to `docs/DECISIONS.md`.
- Never destroy local work; divergence is surfaced, never auto-merged.

## Workflow quirks
- Remote: Gitea `git.zenvairo.es`, owner `dasemu` (section `Darío`), CLI `tea`. SSH (port 22)
  is unreachable from the dev machine; use HTTPS with `git -c credential.helper='!tea login helper'`.
- In zsh, aliases like `g`/`gg` shadow shell functions; use unusual function names in scripts.
- Throwaway repos created on the real Gitea for tests must be deleted afterwards.

## Deployment
Native Linux app (systemd user service). Never Docker for deployment.

## Gotchas
- Forgejo push mirrors to localhost need `migrations.ALLOW_LOCALNETWORKS` (test instances only).
- Push-mirror exclusion of `refs/sync/*` is verified on Forgejo 11 only, not yet on the real Gitea.
