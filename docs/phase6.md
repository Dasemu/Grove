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
directory per configured group, so it stays cheap. Group names must be plain directory names.

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

For `private-only` the daemon matches the repo's `origin` URL (https, `ssh://` or scp-like) against
the configured providers by host (port ignored; GitHub without a URL is `github.com`), gets the
token with `token::resolve` (so the `tea`/`gh` fallback applies) and calls
`GET /repos/{owner}/{repo}` (`Provider::get_repo`, Gitea/Forgejo and GitHub). The answer is cached in
`<state dir>/visibility.json` for 6 hours; a failed lookup is retried at most every 5 minutes. A
cached answer older than 6 hours is never used when the refresh fails.

Unknown visibility counts as **public**: no matching provider, no token, network error, 404 (e.g. a
token without access to a private GitHub repo), a reply without the `private` flag, or no `origin`.
No lookup happens during an offline (backoff) pass, since nothing is pushed then.

### auto_materialize

`auto_materialize` (default `true`) makes the daemon create a worktree directory for every remote
branch of a repo, including long-merged ones, and archive worktrees whose branch is gone. It is
decided **per group** (it used to apply only when every group enabled it): set
`auto_materialize = false` in a group to stop that for its repos. The default is unchanged.
