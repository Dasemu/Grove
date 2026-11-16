//! Encrypted channel for files git must never hold in clear (`.env` and similar).
//!
//! Each machine has an age identity in its state dir; the public keys live in the config repo
//! (`machines/<name>.age.pub`). A pushed file is encrypted to every machine's key and stored as
//! `secrets/<group>/<repo>/<file>.age` in the config repo. Decrypted copies are only ever written
//! to worktrees, mode 0600, and never overwrite a file whose content differs.

use crate::workflow::{commit_and_push, config_repo_open};
use age::secrecy::ExposeSecret;
use age::x25519::{Identity, Recipient};
use anyhow::{Context, Result, bail};
use grove_core::config::Config;
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

fn identity_path() -> Result<PathBuf> {
    Ok(Config::state_dir()
        .context("cannot determine state dir")?
        .join("age-identity.txt"))
}

fn load_identity() -> Result<Identity> {
    let p = identity_path()?;
    let text = fs::read_to_string(&p)
        .with_context(|| format!("no identity at {} (run `grove secrets init`)", p.display()))?;
    text.trim()
        .parse::<Identity>()
        .map_err(|e| anyhow::anyhow!("bad identity file: {e}"))
}

pub fn encrypt(recipients: &[Recipient], data: &[u8]) -> Result<Vec<u8>> {
    if recipients.is_empty() {
        bail!("no recipients: run `grove secrets init` on each machine first");
    }
    let enc = age::Encryptor::with_recipients(recipients.iter().map(|r| r as &dyn age::Recipient))
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let mut out = Vec::new();
    let mut w = enc.wrap_output(&mut out)?;
    w.write_all(data)?;
    w.finish()?;
    Ok(out)
}

pub fn decrypt(id: &Identity, blob: &[u8]) -> Result<Vec<u8>> {
    age::decrypt(id, blob)
        .map_err(|e| anyhow::anyhow!("cannot decrypt (not encrypted to this machine?): {e}"))
}

fn recipients(dir: &Path) -> Result<Vec<Recipient>> {
    let mut out = Vec::new();
    let Ok(rd) = fs::read_dir(dir.join("machines")) else {
        return Ok(out);
    };
    for e in rd.filter_map(Result::ok) {
        if e.file_name().to_string_lossy().ends_with(".age.pub") {
            let t = fs::read_to_string(e.path())?;
            out.push(
                t.trim()
                    .parse::<Recipient>()
                    .map_err(|e| anyhow::anyhow!("bad key: {e}"))?,
            );
        }
    }
    Ok(out)
}

pub fn init(cfg: &Config) -> Result<()> {
    let p = identity_path()?;
    if !p.exists() {
        fs::create_dir_all(p.parent().unwrap())?;
        let id = Identity::generate();
        fs::write(&p, format!("{}\n", id.to_string().expose_secret()))?;
        fs::set_permissions(&p, fs::Permissions::from_mode(0o600))?;
        println!("generated identity {}", p.display());
    }
    let id = load_identity()?;
    let (dir, forge) = config_repo_open(cfg, true)?;
    fs::create_dir_all(dir.join("machines"))?;
    fs::write(
        dir.join("machines")
            .join(format!("{}.age.pub", cfg.machine.name)),
        format!("{}\n", id.to_public()),
    )?;
    commit_and_push(cfg, &dir, &forge, "publish age key")?;
    println!("public key published for machine {}", cfg.machine.name);
    println!(
        "note: files pushed earlier are not readable here until another machine runs `grove secrets rekey`"
    );
    Ok(())
}

/// `<root>/<group>/<repo>/<worktree>/<rel...>` -> (group, repo, worktree dir name, rel path).
fn locate(cfg: &Config, file: &Path) -> Result<(String, String, PathBuf, String)> {
    let file = fs::canonicalize(file).with_context(|| format!("{} not found", file.display()))?;
    let root = fs::canonicalize(cfg.root_path())?;
    let rel = file
        .strip_prefix(&root)
        .context("file is not under the workspace root")?;
    let parts: Vec<_> = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    if parts.len() < 4 {
        bail!("expected <root>/<group>/<repo>/<worktree>/<file>");
    }
    Ok((
        parts[0].clone(),
        parts[1].clone(),
        root.join(&parts[0]).join(&parts[1]),
        parts[3..].join("/"),
    ))
}

fn blob_path(dir: &Path, group: &str, repo: &str, rel: &str) -> PathBuf {
    dir.join("secrets")
        .join(group)
        .join(repo)
        .join(format!("{}.age", rel.replace('/', "%2F")))
}

pub fn push(cfg: &Config, file: &Path) -> Result<()> {
    let (group, repo, _, rel) = locate(cfg, file)?;
    let (dir, forge) = config_repo_open(cfg, false)?;
    let blob = encrypt(&recipients(&dir)?, &fs::read(file)?)?;
    let dest = blob_path(&dir, &group, &repo, &rel);
    fs::create_dir_all(dest.parent().unwrap())?;
    fs::write(&dest, blob)?;
    commit_and_push(cfg, &dir, &forge, &format!("secret {group}/{repo}/{rel}"))?;
    println!("encrypted and stored {group}/{repo}/{rel}");
    Ok(())
}

pub fn pull(cfg: &Config) -> Result<()> {
    let id = load_identity()?;
    let (dir, _) = config_repo_open(cfg, false)?;
    let sroot = dir.join("secrets");
    let (mut wrote, mut same, mut conflict) = (0, 0, 0);
    for g in fs::read_dir(&sroot)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
    {
        for r in fs::read_dir(g.path())?.filter_map(Result::ok) {
            let repo_dir = cfg.root_path().join(g.file_name()).join(r.file_name());
            for b in fs::read_dir(r.path())?.filter_map(Result::ok) {
                let name = b.file_name().to_string_lossy().into_owned();
                let Some(rel) = name.strip_suffix(".age").map(|n| n.replace("%2F", "/")) else {
                    continue;
                };
                let plain = match decrypt(&id, &fs::read(b.path())?) {
                    Ok(p) => p,
                    Err(e) => {
                        println!("skip {}/{}: {e}", g.file_name().to_string_lossy(), rel);
                        continue;
                    }
                };
                for wt in fs::read_dir(&repo_dir)
                    .into_iter()
                    .flatten()
                    .filter_map(Result::ok)
                {
                    if !wt.path().join(".git").exists() {
                        continue;
                    }
                    let target = wt.path().join(&rel);
                    match fs::read(&target) {
                        Ok(cur) if cur == plain => same += 1,
                        Ok(_) => {
                            conflict += 1;
                            println!(
                                "kept local {} (differs from the stored copy)",
                                target.display()
                            );
                        }
                        Err(_) => {
                            fs::create_dir_all(target.parent().unwrap())?;
                            fs::write(&target, &plain)?;
                            fs::set_permissions(&target, fs::Permissions::from_mode(0o600))?;
                            wrote += 1;
                            println!("wrote {}", target.display());
                        }
                    }
                }
            }
        }
    }
    println!("{wrote} written, {same} already identical, {conflict} kept (differ)");
    Ok(())
}

/// Re-encrypts every stored secret to the current set of machine keys.
pub fn rekey(cfg: &Config) -> Result<()> {
    let id = load_identity()?;
    let (dir, forge) = config_repo_open(cfg, false)?;
    let rec = recipients(&dir)?;
    let mut n = 0;
    let mut stack = vec![dir.join("secrets")];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d)
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
        {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|x| x == "age") {
                let plain = decrypt(&id, &fs::read(&p)?)?;
                fs::write(&p, encrypt(&rec, &plain)?)?;
                n += 1;
            }
        }
    }
    commit_and_push(cfg, &dir, &forge, "rekey secrets")?;
    println!("re-encrypted {n} file(s) to {} machine key(s)", rec.len());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_recipient_can_decrypt_and_strangers_cannot() {
        let (a, b, c) = (
            Identity::generate(),
            Identity::generate(),
            Identity::generate(),
        );
        let blob = encrypt(&[a.to_public(), b.to_public()], b"TOKEN=1\n").unwrap();
        assert!(!String::from_utf8_lossy(&blob).contains("TOKEN"));
        assert_eq!(decrypt(&a, &blob).unwrap(), b"TOKEN=1\n");
        assert_eq!(decrypt(&b, &blob).unwrap(), b"TOKEN=1\n");
        assert!(decrypt(&c, &blob).is_err());
        assert!(encrypt(&[], b"x").is_err());
    }
}
