# Grove

Keeps your git repos in sync across machines without manual commit/push/pull, including
work in progress. Early stage (Phase 1: core library and CLI). See `docs/` for details.

## Install from source

```bash
cargo install --locked --root ~/.local --path crates/grove
cargo install --locked --root ~/.local --path crates/grove-daemon
cargo install --locked --root ~/.local --path crates/grove-gui
install -Dm644 contrib/groved.service ~/.config/systemd/user/groved.service
systemctl --user enable --now groved
# Menu entry: desktop sessions often lack ~/.local/bin in PATH, so use the absolute path.
sed "s|^Exec=grove-gui|Exec=$HOME/.local/bin/grove-gui|" contrib/grove.desktop \
  > ~/.local/share/applications/grove.desktop
install -Dm644 assets/grove.png ~/.local/share/icons/hicolor/64x64/apps/grove.png
```

Start-at-login is a toggle in the app's Settings. A `.deb` is also available
(`cargo deb -p grove`, see `docs/phase7.md`).

## License

MIT, see `LICENSE`.

The desktop app (`grove-gui`) is built with [Slint](https://slint.dev), used under the
Slint Royalty-free Desktop, Mobile, and Web Applications License 2.0. As that licence requires,
the app shows the `AboutSlint` attribution on its About screen (sidebar, "About").
