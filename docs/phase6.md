# Phase 6: routing by visibility, owners, CLI tokens

## Config

Two optional keys per group, both backward compatible. Nothing is hard-coded: each user decides per
group which forge is primary, where it is mirrored and whether public repos route differently.
Without these keys a group behaves as before (one route for every repo, owned by the token's user).
Example (one person's setup, not a default):

```toml
[[groups]]
name = "Zenvairo"
primary = "gitea"                       # private repos (and all repos without [groups.public])
mirror_to = []
owners = { gitea = "Zenvairo" }         # account/org per provider; unset = authenticated user
[groups.public]                         # public repos take this route instead
primary = "github"
mirror_to = ["gitea"]
```

- `Group::route(private)` picks the route; `Group::owner(provider)` the owner.
- Validation: the public route follows the same rules as the base one (known providers, primary
  not in its own `mirror_to`); `owners` keys must be known providers, values non-empty.

## Flows

- `grove new <group> <name> [--public]` creates the repo on the route's primary, under the
  configured owner (`POST /orgs/{owner}/repos`), then sets up each mirror of that route:
  - primary can push-mirror (Gitea/Forgejo): target repo created under the mirror's owner, then a
    push mirror on the primary (as before);
  - otherwise (GitHub primary): a **pull mirror** on the target via Gitea's `POST /repos/migrate`
    (`mirror: true`, 8 h interval). Credentials of the source are sent only for private sources.
- `grove import <group>` lists, per route, the primary's repos owned by the configured owner (or
  the authenticated user), and with a public route only those of the route's visibility. Repos of
  other owners (orgs you belong to) no longer show up in every group: give an org group its
  `owners` entry. If one route's provider is unreachable, the whole group's import fails.
- An owner equal to the authenticated login means "my account" (`/user/repos`), not an org.
- Read-only pull mirrors (Gitea `mirror: true` + `original_url`, GitHub `mirror_url`) are never
  used as `origin`: `grove import` clones the mirror's source instead, and `grove add` (also the
  GUI's Migrate) repoints an existing `origin` to it and fetches. The source must be on a
  configured provider and, for import, have the mirror's visibility; otherwise import fails for
  that repo (it keeps the mirror's name locally) and `add` only warns.
- Wizard git over HTTPS authenticates with the resolved token on both forge kinds
  (`http.extraHeader`, process env only; GitHub as Basic `x-access-token`).

## Tokens

`token::resolve`: `GROVE_TOKEN_<ID>`, then the Secret Service, then the forge CLI's login:
`tea login helper get` for Gitea/Forgejo (matched by the provider URL's host) and
`gh auth token --hostname <host>` for GitHub (`github.com` without a URL). `GROVE_NO_CLI_TOKENS=1`
disables the fallback. Each CLI call is killed after 10 s (e.g. a pending keyring unlock).
`grove doctor` reports the source per provider without contacting the forge.

Mirrors store a token on the forge: the mirror provider's token in the primary's push-mirror
config, and the source's token in a pull mirror of a **private** source. With the CLI fallback that
is the broad-scope `tea`/`gh` token; use `grove login` with a narrower token to avoid that.

## Validation

- `grove-core` config tests: routing, owners, round trip, invalid public routes/owners.
- `grove-providers/tests/api.rs`: migrate request shape (owner, credentials only when private),
  GitHub `Unsupported`, tea/gh fallback with fake binaries on `PATH`, opt-out variable.
- `grove` unit test: import filtering by owner (case-insensitive) and visibility.
- Real machine: `grove doctor` found the tea (git.zenvairo.es) and gh (github.com) tokens.

## Not verified

- Pull mirror creation on the real Gitea and push to a real GitHub org (mock-tested only).
- Daemon scope and snapshot privacy (below) are verified with local bare remotes and mocks only, not against a real forge.

## Daemon scope and snapshot privacy

### Scope

`groved` manages only repos in the bare + worktrees layout (a `.bare` directory) at
`<root>/<group>/<repo>` for groups present in the config. Everything else under the root is
ignored: plain clones (never migrated with `grove add`), directories that are not configured
groups, repos directly under the root. Ignored plain clones inside a configured group are listed as
"not migrated (run grove add)" by `grove daemon status` and `grove doctor`. Discovery reads one
directory per configured group, so it stays cheap.

### Rediscovery at runtime

Discovery is not startup-only. The daemon watches (non-recursively) the root, each configured group
directory and every child directory that is not managed yet; managed repos keep their recursive
watch. A child directory created, deleted or renamed, or `.bare` / `.git` appearing or disappearing
inside one (`grove add` migrating a clone, `grove import` cloning into `<group>/<name>/.bare`,
a clone moved to the trash) triggers a debounced (500 ms) rediscovery and diff:

- New bare-layout repos are managed at once: watches set up, a pass scheduled, activity event
  `repo_added`.
- Repos that are gone (or no longer bare layout) are dropped with their watches and state, one
  `repo_removed` event, no error; their files are never touched. A repo whose directory vanishes
  while a pass runs is dropped the same way.
- The "not migrated" list is refreshed.

A repo is only picked up when it is **ready**: `.bare/HEAD` exists, there is no `.grove-busy` and no
`.grove-stage`. `grove add`, clone/import and init hold `<repo>/.grove-busy` (pid + start time) from
before their first change until their very last step, also on rollback, so the daemon never
fetches, materializes or snapshots a half-built layout (e.g. a `--no-checkout` worktree whose files
are still in `.grove-stage`). Creating or removing `.grove-busy` / `.grove-stage` triggers a
rediscovery, so the repo is picked up as soon as the marker goes. A marker whose process is gone
(or a leftover `.grove-stage`) means an interrupted operation: the repo stays unmanaged, the daemon
logs one `repo_stalled` warning to check it and restore from the backups, and never deletes the
marker. `pass` re-checks readiness and skips unready repos. Clones are not built in a temp dir and
renamed, because worktree link files hold absolute paths.

Managed repos are watched recursively, including `node_modules` / `target`; notify cannot exclude
subtrees, so raise `fs.inotify.max_user_watches` on machines with many large repos.

As a safety net every full pass (every `fetch_interval_s`) re-runs discovery too, so missed
inotify events delay detection by at most that interval. `grove daemon status` reflects the new set
right after rediscovery, also while paused (discovery runs while paused, syncing does not).

Config reload is still out of scope: restart the daemon after editing `config.toml` (new or removed
groups are only picked up then).

**Breaking change:** group names must now be plain directory names (non-empty, no `/`, no leading
`.`); a config with other names no longer loads.

### Repos deleted on the forge

A managed repo whose remote was deleted is moved out of the synced tree, never deleted locally:

```toml
[sync]
purged_dir = "Purged"           # default; "" disables the move
```

- Trigger: a fetch fails with a "not found"-style error (also Gitea's SSH "Cannot find repository"). That is only a hint: the forge serving
  `origin` must then answer 404 while the same token works (`test_connection`), and still do so
  `purge::CONFIRM_AFTER_SECS` (10 min) after the first confirmation (a gap of more than twice
  that starts over). A working fetch, or the forge seeing the repo again, clears it. Network or
  token errors change nothing.
- Only repos owned by the token's account, or by an organisation named in some group's `owners`
  for that provider, are ever moved: someone else's repo turning private or revoking access looks
  the same as a deletion. Being removed from one of your own `owners` organisations is still
  indistinguishable and moves its repos.
- Never while busy: the move holds `.grove-busy` and is skipped (`purge_failed`, retried next
  pass) when `.grove-stage` exists or a worktree has a git operation in progress (`index.lock`,
  rebase, merge, cherry-pick, revert, bisect).
- Action: `root/<group>/<repo>` moves to `root/<purged_dir>/<group>/<repo>` (a `-<stamp>` suffix
  if taken) by `rename` (same file system only; otherwise it stays and `purge_failed` is logged),
  then `git worktree repair` fixes the in-repo and linked worktrees (paths compared after
  resolving symlinks). Worktrees whose directory is missing at that moment are listed in the
  `purged` entry for a later `git worktree repair`. The directory is not a configured group, so
  the daemon stops syncing it.
- Activity log: `purge_pending` (first confirmation), `purged` (moved, with the destination),
  `purge_failed` (not moved, with the reason).
- `purged_dir` must be a plain directory name and must not also be a group.
- The confirmation state is in memory: a daemon restart restarts the 10-minute wait.

### Snapshot policy

WIP snapshots (`refs/sync/<machine>/<branch>`) contain uncommitted work, so on a public repo anyone
could read them. New key, global default under `[sync]` and optional override per group:

```toml
[sync]
snapshots = "private-only"      # default: "private-only" | "all" | "none"

[[groups]]
name = "Zenvairo"
primary = "gitea"
snapshots = "all"               # optional per-group override
```

- `private-only`: push snapshots only when the forge says the repo is private.
- `all`: always push. `none`: never push.
- When snapshots are withheld the daemon neither takes nor pushes them for that repo. Fetching, fast
  forwarding and applying other machines' snapshots are unaffected. The reason shows in
  `grove daemon status` ("snapshots withheld: ...") and once per change in the daemon log
  (`snapshots_withheld` / `snapshots_enabled`).
- `grove sync` (manual) still pushes snapshots regardless of this setting.

### How visibility is resolved

Under `private-only` snapshots are pushed only if **every place the push reaches** is known to be
private:

1. Every push URL of `origin` (`git remote get-url --push --all origin`, so `pushurl`, several
   urls and `pushInsteadOf` count). Each is matched to exactly one configured provider by host,
   port (http/https; an ssh port is ignored) and the provider URL's path prefix; zero or several
   matching providers mean unknown. The token comes from `token::resolve` (tea/gh fallback) and the
   lookup is `GET /repos/{owner}/{repo}` (`Provider::get_repo`). On GitHub only
   `visibility = "private"` counts (internal repos do not; a reply without the field is unknown); on Gitea only `private: true`.
2. The group's mirror targets for the same repo name: every provider in `mirror_to` of the group's
   private and public routes, under the group's `owners` entry or else the token's login. Gitea
   push/pull mirrors copy `refs/sync/*`. A mirror that exists and is not private, or a failed
   lookup, withholds; a 404 (no mirror) is fine.

Answers are cached in `<state dir>/visibility.json` (mode 0600), keyed by provider id, provider URL, owner
and name (lowercase, no `.git`). A "private" answer is trusted for 1 hour (a repo can be made public at
any time) and so is an "absent" mirror answer; only "not private" (which merely withholds) is kept 6 hours, a cache timestamp in the future counts as expired, and an
expired entry is never reused when the refresh fails. A failed lookup is retried at most every 5
minutes. Unknown counts as **public**: no or several matching providers, no token, network error,
404 on the repo itself, a reply without the `private` flag, no push URL. Offline (backoff) passes do
no lookup: they use the last verdict, and take no snapshot when there is none.

When snapshots are withheld, the daemon deletes this machine's snapshot refs
(`refs/sync/<machine>/*` only, never other machines' refs or branches). It lists them on every
push URL with `ls-remote` (so refs without a local copy are found too), pushes the deletions per
URL, and counts a URL as clean only when a fresh `ls-remote` shows none left; only then are the
local copies removed. It logs `snapshots_purged` as a warning; any failure (also a server saying
the repository does not exist) is logged as `error` and retried on the next pass. Git runs with
`LC_ALL=C`, and snapshot pushes use `push.recurseSubmodules=no`.

Residual risks: when matching a provider, an ssh URL's port is ignored (it says nothing about the
web port), so two forges on one host that differ only by port cannot be told apart by an ssh remote;
and mirrors made by hand outside grove's config (a push mirror set up in the forge UI,
a second remote) are unknown to the daemon. Use `snapshots = "none"` for repos where that matters.

### auto_materialize

`auto_materialize` (default `true`) makes the daemon create a worktree directory for every remote
branch of a repo, including long-merged ones, and archive worktrees whose branch is gone. It is
decided **per group** (it used to apply only when every group enabled it): set
`auto_materialize = false` in a group to stop that for its repos. The default is unchanged.
