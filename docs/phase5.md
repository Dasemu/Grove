# Phase 5: resolve, safety, packaging

## Done

- **Resolve** (`grove resolve <worktree> --take-remote | --keep-local | --save-as BRANCH | --compare`,
  also in the GUI). Every choice writes a backup ref first (`refs/sync-backup/<machine>/<stamp>/resolve-local/<branch>`
  or `resolve-remote/<branch>`). `--keep-local` uses force-with-lease on the observed upstream tip.
  `--take-remote` and `--save-as` refuse on a dirty worktree or when ignored files would be overwritten.
  `--save-as` needs the bare + worktrees layout. `status --plan` explains what the next pass will do.
  Covered by `crates/grove-core/tests/resolve.rs` (4 tests).
- **Size limit**: `sync.max_file_mb` (default 50, 0 = unlimited). Oversized changed files are left out of
  the snapshot (reset to their HEAD version, or absent), reported in the pass result and the daemon log.
  Covered by `tests/size_limit.rs`.
- **Encrypted files** (`grove secrets init|push|pull|rekey`, age x25519): see `phase4.md`.
- **Packaging**: `cargo build --release --workspace && cargo deb -p grove --no-build` produces
  `target/debian/grove_*.deb` (CLI, daemon, systemd *user* unit `/usr/lib/systemd/user/groved.service`).
  The package was built; it was **not installed or run**. `dpkg-shlibdeps` is unavailable here, so `$auto`
  dependencies were not computed.

## Not done

- AppImage (no `appimagetool`) and Flatpak: not attempted. The Tauri bundle config requests `deb` and
  `appimage`, untested.
- Daemon-side mirroring fallback for GitHub (GitHub has no push mirrors; `configure_mirror` returns `Unsupported`).
- Syncing agent sessions (optional item in the plan): not implemented.
- A real two-PC run, a real cross-forge mirror, Secret Service on a desktop, github.com: unverified.
