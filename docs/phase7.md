# Phase 7: native GUI

`grove-gui` (crate `crates/grove-gui`) replaces the web UI of Phase 3. It is a native desktop app
(Slint UI, StatusNotifierItem tray) that lives in the system tray. It contains no sync logic: state
comes from the daemon over its Unix socket and every action runs an allow-listed `grove` subcommand,
exactly as before.

## Architecture

| Part | Where |
|---|---|
| Daemon IPC + allow-listed CLI calls + config (formerly `grove-bridge`) | `src/bridge.rs` |
| Pure logic: status -> aggregate/tray state, formatting, `grove import` listing parser, settings summary | `src/model.rs` |
| Tray (ksni), icons drawn in code per state | `src/tray.rs` |
| Single instance (Unix socket) | `src/instance.rs` |
| Window, callbacks, worker threads | `src/main.rs`, `ui/app.slint` |

Threads: the Slint event loop; a worker that fetches `status` (and `log`/config only while the window
is visible); a subscriber that holds a `subscribe` connection to the daemon and wakes the worker on
every event (reconnects every 5 s while the daemon is down); the tray service; the single-instance
listener; short-lived threads for CLI calls so the UI never blocks. Fallback poll: 5 s with the
window open, 30 s hidden. Daemon events are coalesced (at most one status refresh per 1 s visible,
10 s hidden). Hidden, the worker only asks for `status`: it drives the tray (touched only when the
aggregate changes; pixmaps are rendered once per state) and notifications, and nothing is sent to
the UI thread. Log, config and models are fetched when the window opens. No Slint timers run while idle.

Tray states: ok, syncing, attention (diverged, error, `ahead`, `behinddirty`; also sets the item to `NeedsAttention`), paused, daemon offline.
Menu: Open, Sync now, Pause/Resume, Quit; left click opens the window. Closing the window hides it;
Quit exits. If no StatusNotifier host exists, closing the window quits. A transition into
`Diverged`/`Error` shows a toast when the window is open and calls `notify-send` (best effort) when hidden.

Single instance: `$XDG_RUNTIME_DIR/grove/gui.sock`. A second `grove-gui` asks the first to show
its window and exits; `grove-gui --hidden` (autostart) only checks and never raises the window.

Daemon scope and snapshot policy (Phase 6) in the GUI: each repo shows `snapshots withheld: <reason>`
(list and detail), the repositories view lists the `not_migrated` clones with the hint to run
`grove add`, settings show the default snapshot policy and per-group overrides, and Activity colours
`snapshots_withheld`/`snapshots_purged` as warnings and `snapshots_enabled` as ok.

Screens (parity with Phase 3): repositories with per-worktree state, repository detail with the
resolve panel (confirmation first), activity with filter, new repository, import (scan, pick,
import), settings (machine, providers, groups incl. public route and `owners`, config repo, token
login, secrets), onboarding when there is no config, offline state with a hint.

## Run

```bash
cargo run --release -p grove-gui            # window + tray
grove-gui --hidden                          # tray only
grove-gui --config PATH --socket PATH --grove-bin PATH   # test against another daemon/config
grove-gui --view settings                   # start on a view (activity, new, import, repo:<path>)
```

`grove` is looked up next to the `grove-gui` binary, then in `PATH`. Packaging: `.deb` installs
`grove-gui`, `contrib/grove.desktop`, `assets/grove.png` and a copy of the autostart file under
`/usr/share/grove/`. Autostart is opt-in: Settings has a "Start Grove at login" toggle that writes or
removes `~/.config/autostart/grove.desktop`. The deb recommends `libnotify-bin` (`notify-send`). Dev builds need no system Qt: Slint is built with the `winit` backend (Wayland and
X11 loaded at runtime) and the software renderer.

## Dependencies

- `slint` / `slint-build` 1.18.1 (`std`, `backend-winit`, `renderer-software`, `compat-1-2`; no Qt,
  no femtovg, no system-tray feature). Style `fluent` (pure Slint). Slint is licensed GPL-3.0 or its
  royalty-free license (attribution required) or a commercial one; pick one before distributing.
- `ksni` 0.3.7 (`async-io` + `blocking`, no tokio).

## Memory (release build, KDE Plasma 6 / Wayland, `ps -o rss`)

| State | Software renderer (used) | femtovg (GL) |
|---|---|---|
| Started with `--hidden` | 24 MB | 94 MB |
| Window open | 31 MB | 105 MB |
| After closing the window (hidden again) | 25 MB | not measured |

Hidden CPU against a busy daemon (8 repos, 5 s fetch interval, ~4 events/s): about 4 ticks in 60 s
(0.07%), RSS flat. 9 threads.

## Limits

- Slint does not expose xdg-activation, so launching again (or clicking the tray) on a visible window re-maps it (hide + show); the compositor focuses the fresh window (checked on KWin: keyboard focus arrives). The compositor picks the window position again.
- Activity timestamps use the local UTC offset read via `date +%z` (no timezone crate).
- Onboarding stays on screen until login and bootstrap succeed. If a config already exists it is kept and `grove init` is skipped (the form fields are then ignored, the output says so).
- Resolve output under a worktree disappears once that worktree's state changes after the run; a toast reports the result.
- The activity list shows the first line of each message, repository errors are cut at 160 characters
  in the list (full text in the detail view).
- Slint's wrapped text under-reports its height in layouts, so long text is elided, not wrapped.
- Side-by-side compare for `Diverged` stays in the CLI (`grove resolve --compare`).
- Only the layout and read-only views were checked visually; forms and CLI-driven flows were not
  clicked through (no input injection available), only their parsing and logic are unit-tested.
