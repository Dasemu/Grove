# Phase 3: GUI (superseded)

> **Superseded by [Phase 7](phase7.md).** The HTML/JS frontend, the localhost bridge and the Tauri
> shell described below were removed; the native `grove-gui` replaces them. Kept for history.


## Layout

| Part | Where | Status |
|---|---|---|
| Frontend (plain HTML/CSS/JS, no build step) | `gui/ui/` | Implemented, exercised in a browser against a live daemon |
| Dev bridge (`grove-bridge`, std-only HTTP server on 127.0.0.1) | `crates/grove-bridge/` | Implemented, unit-tested, exercised end to end |
| Tauri v2 shell with tray | `gui/src-tauri/` | **Written but never compiled** (see below) |

The frontend talks to a backend through one adapter in `gui/ui/app.js`: `window.__TAURI__` commands
(`ipc`, `cli`, `config`) when running inside Tauri, otherwise `fetch('/api/...')` against the bridge.
The GUI contains no sync logic: state comes from the daemon over its Unix socket, and every action
(new, import, resolve, login, secrets, bootstrap) shells out to the allow-listed `grove` subcommands.

## Screens

Repositories (aggregate pill: all synced / syncing / paused / needs attention / daemon offline),
repository detail (per-worktree state and what the next pass will do, resolve panel for `Diverged`),
Activity (live log, filter), New repository, Import, Settings (config, token login, config repo,
secrets), and onboarding when no config exists. Toasts (and browser notifications when permitted)
fire on a transition into `Diverged` or `Error`.

Resolve actions (save as branch / take remote / keep mine) always ask for confirmation and write a
backup ref first. Comparing both sides stays in the CLI (`grove resolve --compare`).

## Dev bridge safety

Binds 127.0.0.1 only, rejects non-local `Host` headers (DNS rebinding) and POSTs without
`X-Grove: 1` (cross-site forms cannot set it). Only these subcommands are forwarded: `init login new
import config-repo-init bootstrap resolve secrets sync status`. Verified: a POST without the header gets 403.

```bash
cargo run -p grove-bridge -- --ui gui/ui      # then open http://127.0.0.1:4780
```

## Tauri shell: not verified

Needs system packages that were missing on the dev machine and no root was available:

```bash
sudo dnf install webkit2gtk4.1-devel libsoup3-devel librsvg2-devel   # Fedora
cd gui/src-tauri && cargo install tauri-cli --locked && cargo tauri dev
```

The shell is ~80 lines: three commands reusing `grove-bridge`, a tray (open / pause-resume / quit),
and close-to-tray. The CSP allows only `self`; the capability file grants `core:default` only.
Expect small compile fixes on the first build. It is intentionally outside the workspace so that
`cargo test` works without webkit installed.

## Not done

- Screenshots/visual QA inside the real Tauri window.
- Side-by-side compare view for `Diverged` (CLI only).
- Native notifications from the Tauri side (the web Notification API is used).
