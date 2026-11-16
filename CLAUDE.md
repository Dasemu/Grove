# Grove

## Overview
Linux tool that keeps chosen git repos continuously in sync across machines, including
uncommitted work, via snapshot refs (`refs/sync/<machine>/<branch>`) on a primary remote.
Rust workspace: `grove-core`, `grove` (CLI), `grove-daemon` (`groved`), `grove-providers` (Gitea/Forgejo/GitHub),
`grove-bridge` (localhost HTTP bridge for the GUI), plus `gui/ui` (plain HTML/JS) and `gui/src-tauri` (Tauri shell,
outside the workspace, never compiled yet). Phases 0-5 are implemented; the real two-PC run is still pending.
Do not start new work before the previous results are written to `docs/`.

## Commands
- Check shell scripts: `for f in spikes/*.sh; do bash -n "$f"; done`
- Throwaway Forgejo (dev only, Podman): `spikes/forgejo-up.sh`, then `spikes/phase0.sh`
- Rust: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`
- Daemon: `cargo run -p grove-daemon --bin groved -- --config PATH`; client `grove daemon status`
- GUI in a browser: `cargo run -p grove-bridge -- --ui gui/ui`; Tauri shell: see `docs/phase3.md`
- Package: `cargo build --release --workspace && cargo deb -p grove --no-build`
- Throwaway e2e on a local Forgejo: `spikes/phase4-e2e.sh`

## Architecture
See `docs/` (`phase0-spike.md` ... `phase5.md`, `DECISIONS.md`). Git is the storage layer; shell out to
system `git`; snapshots use a temp `GIT_INDEX_FILE` + `commit-tree`, never touching the
user's branch or index.

## Conventions
- Dependencies: serde+toml, clap+anyhow, notify+serde_json, ureq, keyring, age, tauri v2 (GUI shell), approved. Ask before adding more.
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
