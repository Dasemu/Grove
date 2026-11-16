//! `groved`: the grove user daemon (run it as a systemd user service, see `contrib/`).

use anyhow::{Context, Result};
use grove_core::config::Config;
use grove_daemon::{Daemon, Settings};
use std::path::PathBuf;

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let mut config: Option<PathBuf> = None;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--config" => config = Some(args.next().context("--config needs a path")?.into()),
            "-h" | "--help" => {
                println!("groved [--config PATH]\nKeeps the repos under root.path in sync.");
                return Ok(());
            }
            other => anyhow::bail!("unknown argument {other:?}"),
        }
    }
    let path = config
        .or_else(Config::default_path)
        .context("cannot determine config path")?;
    let cfg = Config::load(&path)?;
    grove_core::snapshot::set_max_file_bytes(cfg.sync.max_file_mb * 1024 * 1024);
    let st = Settings::standard();
    eprintln!(
        "groved: machine {:?}, root {}, socket {}",
        cfg.machine.name,
        cfg.root_path().display(),
        st.socket.display()
    );
    let _daemon = Daemon::start(cfg, st).context("cannot start daemon")?;
    loop {
        std::thread::park();
    }
}
