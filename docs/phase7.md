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
removes `~/.config/autostart/grove.desktop`; its `Exec` is the running binary's absolute path,
because desktop sessions often lack `~/.local/bin` in `PATH` (a source install from the README). The deb recommends `libnotify-bin` (`notify-send`). Dev builds need no system Qt: Slint is built with the `winit` backend (Wayland and
X11 loaded at runtime) and the software renderer.

## Dependencies

- `slint` / `slint-build` 1.18.1 (`std`, `backend-winit`, `renderer-software`, `compat-1-2`; no Qt,
  no femtovg, no system-tray feature). Style `fluent` (pure Slint). Slint is used under its
  Royalty-free Desktop, Mobile, and Web Applications License 2.0, so Grove stays MIT. Its attribution
  requirement is met by the `AboutSlint` widget on the About view (reachable from the sidebar); keep
  that view if the UI is reorganised.
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

## Import speed, git protocol, import robustness

- `Provider::list_owner_repos(owner, login)` lists only the owner's repos: Gitea `/orgs/{org}/repos`
  or `/user/repos` (filtered by owner) for the login; GitHub `/orgs/{org}/repos?type=all` or
  `/user/repos?affiliation=owner`. A 404 on the org endpoint falls back to `/users/{owner}/repos`.
  The two sides of a split group are scanned in parallel threads. `grove import Darío` on the
  owner's real forges: 3.5 s -> 1.3 s.
- The GUI scan runs on its own thread (not the status worker), is cached per group for the session,
  and has a Rescan button. Switching groups shows the cached list; an import drops the cache.
- "Already on this machine" is case-insensitive by directory name, or when an existing bare repo's
  `origin` points to the same host/owner/name (https, `ssh://` and scp-like forms).
- A failing clone no longer aborts a multi-repo import: the others continue, failures are printed
  as `FAILED <name>: <reason>`, the exit status is non-zero and the GUI lists the failed names.
- Per-provider `protocol = "https" | "ssh"` (default `https`; also `grove init --protocol` and the
  onboarding form). With `ssh`, `new`/`import`/`bootstrap` clone from the forge's `ssh_url`, registry
  entries store that URL, and git gets no token header (only `GIT_TERMINAL_PROMPT=0`, plus
  `GIT_SSH_COMMAND="ssh -o BatchMode=yes"` unless the user has their own ssh command). Mirrors stay
  HTTPS with tokens (forge to forge). The Settings summary shows the protocol of each provider.

```toml
[providers.gitea]
kind = "gitea"
url = "https://git.example.com"
protocol = "ssh"   # optional, default "https"
```

## Migrating clones from the GUI

The "Not migrated clones" card has a Migrate button per row and a Migrate all button. Each asks
for confirmation first (the dialog names the backup directory, `<state dir>/backups`), then runs
`grove add <path>` one clone at a time off the UI thread, showing "Migrating…", then the migrated
path or the refusal message (linked worktrees, submodules, detached HEAD...). The bridge only runs
`add` for an existing, non-symlink directory directly under `<root>/<group>/` and passes no flags
(so no custom backup directory). Afterwards the status is refreshed; the daemon computes its
not-migrated list at runtime, so after a migration the row keeps its result on screen until the
next status refresh drops it. A second `grove add` is refused while a `.grove-busy` marker is held;
Migrate all runs strictly one clone at a time and shows that refusal text on the row. Repos the
daemon reports as `stalled` (interrupted operation, marker left behind) get their own card with
the hint to check the backups; `repo_added`, `repo_removed` and `repo_stalled` (warning) appear in
Activity.

## Diagnostics log

`<state dir>/grove-gui.log` (rotated at 256 KiB) records start-up, daemon subscribe/disconnect,
tray errors and restarts, panics with a backtrace (from any thread) and why the event loop ended.

## Hardening of import, clone and bootstrap

- Any non-empty directory in the group counts as "already here". A plain clone is listed
  separately ("use `grove add`") and `import` refuses it; `clone_bare_layout` refuses a non-empty
  target itself.
- A clone that fails after `git clone` removes what it created (`.bare`, worktree directories,
  and the directory when it created it). If that fails, `.grove-stage` is left so the daemon
  reports the repo as stalled.
- The registry stores canonical https URLs (`https://host/owner/name.git`). `bootstrap` picks the
  provider by the entry's host, the transport from that provider's `protocol`, and uses only that
  provider's credentials; an entry on a host no provider serves is refused. Old entries with ssh
  or https URLs are normalised when read.
- Repo names must be plain directory names (no `/`, `\`, leading `.` or `-`, control chars);
  clone URLs must be `https://`, `ssh://` or `user@host:path` on the provider's host; `git clone`
  gets `--`.
- Pagination stops on `X-Total-Count`, on a `Link` without `rel="next"` (GitHub), or an empty or
  repeated page, not on a short page. For a user that is not the login, GitHub/Gitea listing uses
  `/user/repos` (collaborator repos included) and says when `/orgs/{owner}` is a 404.
- The GUI hands `grove add` the canonical path it validated.

## Autostart at login (Plasma Wayland)

Seen on Plasma 6 / Wayland: after an autostart (`grove-gui --hidden`) the window had a task bar
entry, but clicking it showed nothing; opening it from the tray worked (that path hides and
re-shows the window). A window mapped while the compositor is still starting ends up in that
state. Two ways it got shown at login, both handled now:

- No tray yet: Plasma's StatusNotifier watcher/host can come up after autostart, and a failed tray
  start used to fall back to opening the window. A `--hidden` start now registers the tray item
  with ksni's `assume_sni_available`: it waits for the watcher and registers when it appears
  (`tray: StatusNotifier watcher …` lines in the log). Without a D-Bus session at all the window
  still opens, as before. The tray respawn after a lost D-Bus connection also waits.
- Session restore: the desktop relaunches `grove-gui` (no `--hidden`) at login, which asks the
  running instance to show its window. During the first 20 s after a `--hidden` start
  (`model::LOGIN_GRACE`) such requests are ignored and logged. The tray's "Open Grove" is never
  ignored.

Every time the window is shown, `grove-gui.log` records why (start without `--hidden`, another
launch, tray, no tray) and whether it was visible before; the session type is logged at start.
Checked under Xvfb with a private D-Bus session and no tray host (starts hidden, an early second
launch is ignored, a later one opens the window). The watcher appearing later was not simulated,
and the original symptom was not reproduced here (no Plasma).
