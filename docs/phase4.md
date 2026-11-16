# Phase 4: providers and wizards

Crate `grove-providers`: `Provider` trait with Gitea/Forgejo (`/api/v1`) and GitHub clients over
`ureq`, plus token storage. The CLI (`crates/grove/src/workflow.rs`) composes them.

## Tokens

`GROVE_TOKEN_<ID>` (id upper-cased, non-alphanumerics to `_`) wins; otherwise the desktop Secret
Service via `keyring` (service `grove`, user = provider id). Tokens are never written to the config
file. `grove login <id>` reads the token from stdin (never argv), checks it against the forge and
stores it. HTTPS git operations of the wizards authenticate through `http.extraHeader` set in the
process environment only (Gitea/Forgejo); for GitHub they use the user's credential helper.
Since phase 6 the `tea`/`gh` logins are used when grove has no token of its own (`phase6.md`).

## Commands

| Command | What it does |
|---|---|
| `grove init` | Writes a first `config.toml` (machine, root, one provider, one group, optional config repo) |
| `grove login <provider>` | Stores the API token after verifying it |
| `grove new <group> <name> [--public]` | Creates the repo on the group's primary forge, the local bare+worktree layout, an initial commit, pushes, configures push mirrors, registers it in the config repo |
| `grove import <group> [names...]` | Lists remote repos not on this machine; with names, clones them into the layout |
| `grove config-repo-init` | Creates the private config repo and registers every repo already on disk |
| `grove bootstrap` | Fresh machine: clones every repo in the config repo under the root |

## Config repo

A private repo (name from `[config_repo]`) holding `repos.toml` (`group`, `name`, `url` per repo) and
`machines/<name>.json` (written only when the registry changes, so no commit noise). The local
clone lives in `<state dir>/config-repo`. It is created on the first registration if missing.

## Mirrors

Gitea/Forgejo: the primary creates a push mirror via `POST /repos/{o}/{r}/push_mirrors`
(`sync_on_commit`, 8 h interval) to a repo created on the mirror provider. GitHub has no push
mirrors: `configure_mirror` reports `Unsupported` and `new` warns. The daemon-side fallback (push
branches and tags only, never `refs/sync/*`) is **not implemented**. Phase 6 adds a pull mirror on
the Gitea side for GitHub primaries (`phase6.md`).

## Validation

- Unit/mock tests: `crates/grove-providers/tests/api.rs` (requests, auth headers, error mapping,
  org endpoint, no-push-mirror on GitHub, env token override), `grove-core/tests/registry.rs`
  (bare layout creation, push, clone into layout, registry round trip).
- End to end against a local Forgejo 11: `spikes/phase4-e2e.sh` (new x2, config repo, a second
  machine rebuilds the workspace with `login` env token + `bootstrap`). Passed; repos cleaned up.
- Also exercised there: `import` (list, clone, unknown name rejected), invalid token rejected (401).

## Not done / not verified

- Mirror creation against a real forge pair (only the API call shape is mock-tested).
- Secret Service storage on a real desktop session (the sandbox has none; env override used).
- GitHub provider against github.com (mock only). GitHub git auth relies on the credential helper.
- Deleting remote repos is deliberately not implemented.
