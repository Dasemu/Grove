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
- The daemon still syncs every repo under `root.path`, whether or not its directory is a group.
