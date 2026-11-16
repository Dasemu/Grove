# Lessons

- Forgejo push mirrors to localhost are blocked by default; set `ALLOW_LOCALNETWORKS` in throwaway test instances.
- `git clone --bare` sets no `remote.origin.fetch`; without it `fetch` never creates `refs/remotes/*`. Use `receive::ensure_fetch_refspec`.
- `Git::run` trims stdout, which also eats the leading space of the first `status --porcelain` line (` M x` becomes `M x`). Don't assert on that format via `run`.
- Fetch other machines' snapshots into `refs/sync-remote/*`, never into `refs/sync/*`, or a fetch can clobber this machine's own refs.
- Run tests that create many temp repos with unique dir names (pid + nanos); the shared `tests/common` helper does this.
