# Phase 8: personal workflow (namespaces, providers, open in editor)

Goal: run the daily steps from grove itself, without hand-editing `config.toml`. You set up
namespaces and forges, create or clone repos, and open a worktree in Zed, VS Code, Claude Code
or Claude Desktop. Creating repos (`grove new`), cloning existing ones (`grove import`) and
bootstrapping a fresh machine (`grove bootstrap`) existed already (Phase 4). This phase adds the
pieces around them.

## Namespaces and providers

A namespace is a `[[groups]]` entry: a directory `<root>/<name>` whose repos are created on one
provider (`primary`), optionally mirrored to others (`mirror_to`), owned by an account or
organisation (`owners`), with a snapshot policy and an optional public route.

```bash
grove provider add gitea --kind gitea --url https://git.example.com --protocol ssh
grove provider add gh --kind github
grove login gh                       # token on stdin (or picked up from `gh`/`tea`)
grove group add work --primary gitea --mirror-to gh --owner Zenvairo --owner gh=zenvairo-gh
grove group add oss --primary gh --snapshots none
grove group add work --primary gh --replace      # change an existing one
grove group list ; grove provider list
grove group remove oss               # config only: <root>/oss stays on disk, unsynced
grove provider remove gh             # refused while a namespace or the config repo uses it
```

- `--owner OWNER` without `PROVIDER=` applies to the primary provider.
- `--public-primary` / `--public-mirror-to` set the public route (CLI only, not in the GUI form).
- `group add` creates `<root>/<name>`. `--replace` keeps the namespace's `layout`.
- Every edit is checked by the config parser before anything is written. The previous file is kept
  as `config.toml.bak`. The TOML writer drops comments and writes every `[sync]` default out
  explicitly (as `grove init` already did).
- The daemon reads the config only at start. When its socket exists the CLI reminds you to run
  `systemctl --user restart groved`. Hot reload is open (DECISIONS 10).

GUI (Settings): "Add or change a provider" (id, kind, URL, git over https/ssh, replace),
"Add or change a namespace" (name, snapshots, primary and owner, mirror and owner on the mirror,
replace), and "Remove" (namespace, provider). Each runs the CLI command above. The output appears
at the bottom of Settings, and a toast reports the result.

## Opening a worktree: `grove open`

```bash
grove open                    # the current directory, default launcher
grove open app                # repo "app" in any namespace (exact name, else case-insensitive)
grove open work/app@feat -w claude
grove open ~/Workspace/work/app --with vscode
grove open --list             # launchers, * = default, and whether they are installed
grove open app --print        # show the command, do not run it
grove new work tool --open    # create, then open with the default launcher (--open zed: that one)
```

A bare-layout repo resolves to the worktree of the default branch (`origin/HEAD`, then `main`,
`master`, then the first worktree). `@branch` picks another worktree. A plain directory opens as is.
Ambiguous names (the same repo name in two namespaces) are refused with the candidates listed.

Built-in launchers (the first program found wins; lookup is `PATH` plus `~/.local/bin`,
`~/.claude/local` and `~/.cargo/bin`, which desktop sessions often lack):

| name | programs | runs |
|---|---|---|
| `zed` | `zed`, `zeditor`, `zedit` | `zed <worktree>` |
| `vscode` | `code`, `codium`, `code-oss` | `code <worktree>` |
| `claude` | `claude` | in a terminal, started in the worktree |
| `claude-desktop` | `claude-desktop` | `claude-desktop <worktree>` (unverified, see DECISIONS 11) |

Terminal for `claude` (and any `terminal = true` launcher): `open.terminal` if set, else `$TERMINAL`,
else the first of kitty, ghostty, alacritty, foot, wezterm, konsole, ptyxis, gnome-terminal,
xfce4-terminal, xterm. Each known terminal gets the arguments that start the command in the
worktree. The process is started detached (own process group, no stdio) with the worktree as its
working directory. A program that exits with an error within 0.5 s is reported.

Own launchers and defaults (machine-local, in `config.toml`):

```toml
[open]
default = "zed"                                   # else the first installed one
terminal = ["kitty", "--directory", "{path}"]     # the command is appended

[[open.launchers]]
name = "nvim"
label = "Neovim"
command = ["nvim", "."]
terminal = true

[[open.launchers]]                                # same name as a built-in: replaces it
name = "claude-desktop"
command = ["/opt/Claude/claude-desktop", "{path}"]
```

GUI: each repo row on Repositories has an "Open" button (the default launcher, default-branch
worktree). Each worktree in the repository view has "Open in <launcher>" buttons, for installed
launchers only. New repository has an "Afterwards: Open in …" choice. The GUI runs `grove open`
only for a directory inside the workspace root, passed in canonical form, and only with `--with`.

## Checks

- Unit tests: config `[open]` parsing and validation, launcher resolution and terminal choice
  (fake finder), target resolution on a real bare layout (`launch.rs`), detached spawn error
  reporting, namespace/provider editing and save with backup (`namespace.rs`), GUI form → args
  and bridge allow-list (`model.rs`, `bridge.rs`).
- CLI run by hand on a throwaway config: provider/group add, list, remove (in-use refusal),
  `open --list`, `--print`, launch, missing launcher.
- GUI screenshots under Xvfb (Settings forms, repository view buttons, repo list Open, New
  repository). As in Phase 7, the forms were not clicked through (no input injection).
- Not tried on a real desktop: actual Zed/VS Code/Claude launches and each terminal's argument
  form (kitty, foot, alacritty and konsole forms are from their documentation).

## Not done (next steps)

- Repo templates for `grove new` (README, .gitignore, CLAUDE.md…); the initial commit is still
  empty.
- Daemon hot reload of the config (DECISIONS 10).
- Editing a namespace's public route from the GUI.
