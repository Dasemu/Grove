# Lessons

- Forgejo push mirrors to localhost are blocked by default; set `ALLOW_LOCALNETWORKS` in throwaway test instances.
- `git clone --bare` sets no `remote.origin.fetch`; without it `fetch` never creates `refs/remotes/*`. Use `receive::ensure_fetch_refspec`.
- `Git::run` trims stdout, which also eats the leading space of the first `status --porcelain` line (` M x` becomes `M x`). Don't assert on that format via `run`.
- Fetch other machines' snapshots into `refs/sync-remote/*`, never into `refs/sync/*`, or a fetch can clobber this machine's own refs.
- Run tests that create many temp repos with unique dir names (pid + nanos + counter) via the shared RAII `test-support/tmp.rs` (`TempDir`, included with `#[path]`); never call `std::env::temp_dir()` directly in a test.
- Tests must clean their temp dirs, also on failure: `/tmp` is tmpfs and ran out of inodes after a day of `cargo test` runs. Use `TempDir` (removed on drop) and kill spawned daemons by their own `Child` handle before the dir goes.
- Bare-layout clones must set upstream tracking per branch, or `status` silently reports Synced for Behind/Diverged. Assert tracking in tests of any clone path.
- Unix socket paths are limited to ~100 bytes: use a short `XDG_RUNTIME_DIR` (e.g. `/tmp/gd`) when running a daemon from deep scratch dirs.
- Mock-server tests must parse the JSON request body; pretty-printed JSON breaks string asserts.
- Slint (winit/Wayland): `show()` from an idle event loop leaves the window unmapped until another event arrives; follow it with a one-shot `request_redraw` timer.
- Slint has no top-level `function`; use a `global`. A wrapped `Text` under-reports its preferred height inside layouts, so elide long log/error text instead of wrapping it.
