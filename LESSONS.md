# Lessons

- Forgejo push mirrors to localhost are blocked by default; set `ALLOW_LOCALNETWORKS` in throwaway test instances.
- `git clone --bare` sets no `remote.origin.fetch`; without it `fetch` never creates `refs/remotes/*`. Use `receive::ensure_fetch_refspec`.
- `Git::run` trims stdout, which also eats the leading space of the first `status --porcelain` line (` M x` becomes `M x`). Don't assert on that format via `run`.
- Fetch other machines' snapshots into `refs/sync-remote/*`, never into `refs/sync/*`, or a fetch can clobber this machine's own refs.
- Run tests that create many temp repos with unique dir names (pid + nanos); the shared `tests/common` helper does this.
- Bare-layout clones must set upstream tracking per branch, or `status` silently reports Synced for Behind/Diverged. Assert tracking in tests of any clone path.
- Unix socket paths are limited to ~100 bytes: use a short `XDG_RUNTIME_DIR` (e.g. `/tmp/gd`) when running a daemon from deep scratch dirs.
- Mock-server tests must parse the JSON request body; pretty-printed JSON breaks string asserts.
