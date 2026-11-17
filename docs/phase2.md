# Phase 2: daemon

`groved` (crate `grove-daemon`) is a per-user daemon. It adds no sync logic of its own: every pass
is `grove_core::sync::sync_repo`, the same code the CLI runs. The daemon only decides *when*.

## Behaviour

- **Repos**: bare-layout repos (`.bare`) at `root/<group>/<repo>` for configured groups only; see
  `docs/phase6.md` (Daemon scope and snapshot privacy). Rediscovered at every full pass, so new repos
  are picked up.
- **Watching**: inotify (crate `notify`), recursive per repo. Changes under `.git`, `.bare`,
  `.grove-stage` and anything matching `sync.ignore_extra` (exact component name or `*suffix`) are
  ignored. If a watch cannot be set (typically `ENOSPC`, raise `fs.inotify.max_user_watches`) it is
  logged and the periodic pass still covers that repo.
- **Debounce**: each event pushes the repo's next pass to `now + snapshot_debounce_ms`, but never
  later than `max(10 x debounce, 5 s)` after the first unsynced event, so a constantly written file
  cannot starve snapshots.
- **Periodic pass**: every `fetch_interval_s` all repos get a full pass (fetch, receive, apply
  snapshots, materialize/archive). Also what catches missed events.
- **Worker pool**: the loop thread only schedules; `sync.workers` threads (default 4, 1 to 32) run
  the passes, so a slow or hanging fetch of one repo no longer holds up the others. A repo never
  has two passes at once: one that becomes due while its pass runs is dispatched when it ends.
  Events that arrive during a pass queue another pass; it finds nothing to do and ends the chain.
  Passes are handed out only to idle workers, so the rest stay due: pause stops them, and whether
  a pass runs offline (backoff) is decided when it starts. A `sync` request for a repo whose pass
  is running is not undone by that pass failing. A panicking pass is logged and counts as a
  failure. Rediscovery does not drop or flag (stalled) a repo while its pass runs.
  The visibility and merged-PR caches are shared and never locked across a forge request.
  Shutdown and pause let running passes finish. `workers = 1` is the old sequential behaviour.
- **Failures** (network, auth, git errors) back off exponentially per repo: 10 s, 20 s, ... capped at
  5 min, and are logged as `error`. `sync` over IPC clears the backoff. While backing off, changes
  are still snapshotted locally (an offline pass); only the network is retried after the backoff.
- **Locks**: grove never takes the user's index (snapshots use a temp index) so git's own locks are
  not contended; a git command that fails because the user holds a lock is just a failed pass that
  is retried.
- **Pause**: `pause` stops all passes (events keep accumulating, the loop idles at 1 s ticks);
  `resume` runs a full pass.
- **Crash safety**: nothing durable lives in the daemon. Killing it at any point leaves the working
  tree and index untouched; the next start rediscovers and resyncs (covered by a kill -9 test).
  There is no signal handler: SIGTERM ends the process, which is safe for the same reason.

## IPC (protocol version 1)

Unix socket `$XDG_RUNTIME_DIR/grove/grove.sock` (directory mode 0700; falls back to the state dir).
Line-delimited JSON, one request per line:

```json
{"v":1,"cmd":"status"}
{"v":1,"ok":true,"data":{"machine":"pca","paused":false,"repos":[{"path":"...","phase":"idle",
  "last_sync":1790000000,"last_error":null,"failures":0,
  "worktrees":[{"branch":"main","state":"synced"}]}]}}
```

| `cmd` | Extra fields | Result |
|---|---|---|
| `ping` | | `"pong"` |
| `status` | | machine, paused flag, repos with phase (`idle`/`pending`/`syncing`/`error`) and worktree states |
| `pause`, `resume` | | `{"paused": bool}` |
| `sync` | `repo` (optional path) | `"queued"` |
| `log` | `limit` (default 50) | recent activity events |
| `subscribe` | | `"subscribed"`, then a stream of `{"v":1,"event":{...}}` until the client closes |

Errors: `{"v":1,"ok":false,"error":"..."}`. A different `v` is rejected. A second daemon on the same
socket is refused; a stale socket file left by a crash is replaced.

## Activity log

JSON lines in `<state dir>/activity.jsonl` (rotated at 5 MiB to `.jsonl.1`), the last 500 events in
memory, and live via `subscribe`. Kinds: `start`, `stop`, `snapshot`, `fast_forward`, `applied`,
`declined`, `snapshots_withheld`, `snapshots_enabled`, `snapshots_purged`, `state` (worktree state changed), `materialized`, `archived`, `repo_added`, `repo_removed`, `repo_stalled`, `error`, `watch_error`,
`pause`, `resume`.

## CLI

`grove daemon status | pause | resume | sync [REPO] | log [--limit N]`.

## Install (systemd user service)

```bash
install -Dm755 target/release/groved ~/.local/bin/groved
install -Dm644 contrib/groved.service ~/.config/systemd/user/groved.service
systemctl --user daemon-reload
systemctl --user enable --now groved
journalctl --user -u groved -f
```

Socket activation was considered and not done: the daemon must run anyway to watch files.

## Latency

Measured on the real Gitea with default settings (2 s debounce, 60 s fetch): an edit on A showed up on
B after 58 s, and a commit pushed on A left B clean at the new commit after 60 s. Worst case is
`debounce + fetch_interval_s + pass time`; lower `fetch_interval_s` for faster convergence.

## Found by the daemon tests

A worktree that had another machine's snapshot applied stayed `BehindDirty` forever once the author
committed and pushed that work. `receive` now adopts the upstream commit with a mixed reset when the
worktree's content is byte-identical to the upstream tree (`receive::adopt_if_identical`); any
other dirty worktree is still left alone, including one with staged changes (the reset would
discard the staged version).

## Not done

- `Daemon::shutdown` finishes the queued passes first, and idle client connections are not joined.
  `groved` never calls it (no signal handler), so this only matters for tests and embedding.
- Peer-credential check on the socket: access control is the 0700 directory only.
- GUI/tray (Phase 3), providers and wizards (Phase 4).
- Remote-change notifications (webhooks); only polling at `fetch_interval_s`.
- Config reload: restart the daemon after editing `config.toml`. (Repos added, migrated or removed under the configured groups are picked up at runtime; see `docs/phase6.md`.)
- Watching skips no heavy ignored directories (`node_modules`, `target`) at watch time; add them to
  `sync.ignore_extra` to avoid triggering passes, and raise the inotify limit if needed.
