//! Repos deleted on their forge are moved out of the synced tree, never deleted locally.
//!
//! A fetch error alone never moves anything: the forge must answer 404 for a repo the token's
//! account owns (its login, or an organisation named in a group's `owners`) while that token
//! works, and must still do so [`CONFIRM_AFTER_SECS`] after the first confirmation. The repo then
//! moves to `root/<sync.purged_dir>/<group>/<repo>`, which is not a configured group, so it is no
//! longer synced; its worktree links are repaired.

use crate::visibility::{Forge, Seen, parse_remote, provider_for};
use grove_core::busy::BusyGuard;
use grove_core::config::Config;
use grove_core::git::Git;
use std::path::{Path, PathBuf};

/// How long a remote deletion must keep being confirmed before the repo is moved.
pub const CONFIRM_AFTER_SECS: u64 = 600;
/// A confirmation older than this (no check in between, e.g. the forge was unreachable) no
/// longer counts and the wait starts over. Much longer than the time between two passes of
/// one repo, which can exceed 20 minutes with many repos.
pub const STALE_AFTER_SECS: u64 = 6 * 3600;

/// Fetch failures that may mean the remote repo is gone (only a hint; see [`confirmed_gone`]).
pub fn looks_gone(fetch_error: &str) -> bool {
    let e = fetch_error.to_ascii_lowercase();
    [
        "not found",
        "does not appear to be a git repository",
        "does not exist",
        "cannot find repository", // Gitea/Forgejo over SSH
        "error: 404",
    ]
    .iter()
    .any(|p| e.contains(p))
}

/// True only when the forge serving `remote` answers 404 for a repo owned by the token's own
/// account or by an organisation the config names in some group's `owners`, while that token
/// works. A repo of someone else (made private, or access revoked) is never reported gone.
pub fn confirmed_gone(
    cfg: &Config,
    repo: &Path,
    remote: &str,
    forge: &dyn Forge,
) -> Result<bool, String> {
    let url = Git::new(repo.join(".bare"))
        .run(&["remote", "get-url", remote])
        .map_err(|e| e.to_string())?;
    let id = parse_remote(&url).ok_or_else(|| format!("cannot parse remote URL {url:?}"))?;
    let (pid, pc) = provider_for(cfg, &id)?;
    // A 404 from a broken or foreign token proves nothing.
    let login = forge.login(pid, pc)?;
    let owned = id.owner.eq_ignore_ascii_case(&login)
        || cfg.groups.iter().any(|g| {
            g.owners
                .get(pid)
                .is_some_and(|o| o.eq_ignore_ascii_case(&id.owner))
        });
    if !owned {
        return Ok(false);
    }
    Ok(forge.seen(pid, pc, &id.owner, &id.name)? == Seen::Absent)
}

/// Where `repo` (`root/<group>/<name>`) goes: `root/<purged_dir>/<group>/<name>`, with a
/// `-<stamp>` suffix when that is taken.
pub fn destination(root: &Path, purged_dir: &str, repo: &Path, stamp: &str) -> Option<PathBuf> {
    let name = repo.file_name()?;
    let group = repo.parent()?.file_name()?;
    let dir = root.join(purged_dir).join(group);
    let first = dir.join(name);
    if !first.exists() {
        return Some(first);
    }
    Some(dir.join(format!("{}-{stamp}", name.to_string_lossy())))
}

#[derive(Debug)]
pub enum MoveError {
    /// Nothing was moved (busy, an operation in progress, cross-device...).
    NotMoved(String),
    /// The repo is at its destination, but some worktree links could not be repaired.
    Repair(String),
}

/// A finished move: worktrees whose directory was missing (unmounted disk, prunable) could not
/// be repaired; `git worktree repair <path>` from the new `.bare` fixes them later.
#[derive(Debug, Default)]
pub struct Moved {
    pub unrepaired: Vec<PathBuf>,
}

/// An operation in progress in some worktree (or the repo) that a move would break.
fn in_progress(repo: &Path, wts: &[PathBuf]) -> Option<String> {
    if repo.join(".grove-stage").exists() {
        return Some("an interrupted grove operation left .grove-stage".into());
    }
    for w in wts.iter().filter(|w| w.exists()) {
        let Ok(gd) = Git::new(w).run(&["rev-parse", "--absolute-git-dir"]) else {
            continue;
        };
        let gd = PathBuf::from(gd);
        for f in [
            "index.lock",
            "rebase-merge",
            "rebase-apply",
            "MERGE_HEAD",
            "CHERRY_PICK_HEAD",
            "REVERT_HEAD",
            "BISECT_LOG",
        ] {
            if gd.join(f).exists() {
                return Some(format!("{}: {f} (git operation in progress)", w.display()));
            }
        }
    }
    None
}

/// Moves the bare-layout repo to `dest` (same file system) and repairs every worktree link:
/// worktrees inside the repo move with it, linked ones elsewhere are pointed at the new `.bare`.
/// Holds `.grove-busy` while moving and refuses while a git operation is in progress.
pub fn move_repo(repo: &Path, dest: &Path) -> Result<Moved, MoveError> {
    let not = |m: String| MoveError::NotMoved(m);
    // git reports worktree paths resolved (symlinks followed): compare against resolved paths.
    let real_repo = repo
        .canonicalize()
        .map_err(|e| not(format!("{}: {e}", repo.display())))?;
    let bare = real_repo.join(".bare");
    let list = Git::new(&bare)
        .run(&["worktree", "list", "--porcelain"])
        .map_err(|e| not(e.to_string()))?;
    let wts: Vec<PathBuf> = list
        .lines()
        .filter_map(|l| l.strip_prefix("worktree "))
        .map(PathBuf::from)
        .filter(|p| *p != bare)
        .collect();
    let guard =
        BusyGuard::acquire(&real_repo).map_err(|e| not(format!("repo busy (.grove-busy): {e}")))?;
    if let Some(why) = in_progress(&real_repo, &wts) {
        return Err(not(why));
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| not(format!("{}: {e}", parent.display())))?;
    }
    // A rename never copies: across file systems it fails and the repo stays where it was.
    std::fs::rename(&real_repo, dest)
        .map_err(|e| not(format!("cannot move to {}: {e}", dest.display())))?;
    // The marker moved with the repo: the guard's old path is gone, remove the moved one.
    drop(guard);
    let _ = std::fs::remove_file(dest.join(".grove-busy"));
    let real_dest = dest.canonicalize().unwrap_or_else(|_| dest.to_path_buf());
    let mut moved = Moved::default();
    let mut repair: Vec<String> = Vec::new();
    for w in &wts {
        let now = match w.strip_prefix(&real_repo) {
            Ok(rel) => real_dest.join(rel),
            Err(_) => w.clone(),
        };
        if now.exists() {
            repair.push(now.display().to_string());
        } else {
            moved.unrepaired.push(now);
        }
    }
    if !repair.is_empty() {
        let mut args = vec!["worktree", "repair"];
        args.extend(repair.iter().map(String::as_str));
        Git::new(real_dest.join(".bare"))
            .run(&args)
            .map_err(|e| MoveError::Repair(e.to_string()))?;
    }
    Ok(moved)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_tmp as tmp;
    use grove_core::config::Provider as ProviderCfg;
    use std::cell::Cell;

    #[test]
    fn only_not_found_style_fetch_errors_are_hints() {
        assert!(looks_gone(
            "git fetch failed (Some(128)): ERROR: Repository not found."
        ));
        assert!(looks_gone(
            "fatal: 'x.git' does not appear to be a git repository"
        ));
        assert!(looks_gone(
            "git fetch -q --prune origin failed (Some(128)): error:\nerror: Cannot find \
             repository: dasemu/x\nerror:\nfatal: Could not read from remote repository."
        ));
        assert!(!looks_gone(
            "ssh: connect to host h port 22: Connection timed out"
        ));
        assert!(!looks_gone("fatal: Authentication failed"));
    }

    struct Fake {
        login_ok: bool,
        seen: Seen,
        lookups: Cell<u32>,
    }
    impl Forge for Fake {
        fn seen(&self, _: &str, _: &ProviderCfg, _: &str, _: &str) -> Result<Seen, String> {
            self.lookups.set(self.lookups.get() + 1);
            Ok(self.seen)
        }
        fn login(&self, _: &str, _: &ProviderCfg) -> Result<String, String> {
            if self.login_ok {
                Ok("me".into())
            } else {
                Err("401".into())
            }
        }
    }

    fn git(dir: &Path, args: &[&str]) {
        Git::new(dir).run(args).unwrap();
    }

    /// `base/root/G/r` in bare layout with a `main` worktree and a linked one outside the repo.
    fn bare_repo(base: &Path) -> (PathBuf, PathBuf) {
        let repo = base.join("root/G/r");
        std::fs::create_dir_all(&repo).unwrap();
        let bare = repo.join(".bare");
        git(
            base,
            &["init", "-q", "--bare", "-b", "main", bare.to_str().unwrap()],
        );
        git(
            &bare,
            &["remote", "add", "origin", "https://git.example.es/me/r.git"],
        );
        let main = repo.join("main");
        git(
            &bare,
            &[
                "worktree",
                "add",
                "-q",
                "--orphan",
                "-b",
                "main",
                main.to_str().unwrap(),
            ],
        );
        std::fs::write(main.join("f"), "x").unwrap();
        git(&main, &["add", "f"]);
        git(
            &main,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-qm",
                "c",
            ],
        );
        let ext = base.join("elsewhere");
        git(
            &bare,
            &["worktree", "add", "-q", "-b", "side", ext.to_str().unwrap()],
        );
        (repo, ext)
    }

    fn cfg() -> Config {
        Config::parse(
            "[machine]\nname=\"m\"\n[root]\npath=\"/x\"\n\
             [providers.gitea]\nkind=\"gitea\"\nurl=\"https://git.example.es\"\n",
        )
        .unwrap()
    }

    #[test]
    fn a_404_counts_only_with_a_working_token() {
        let d = tmp::TempDir::new("purge-confirm");
        let (repo, _) = bare_repo(d.path());
        let cfg = cfg();
        let gone = |login_ok, seen| {
            let f = Fake {
                login_ok,
                seen,
                lookups: Cell::new(0),
            };
            confirmed_gone(&cfg, &repo, "origin", &f)
        };
        assert_eq!(gone(true, Seen::Absent), Ok(true));
        assert_eq!(gone(true, Seen::Private), Ok(false));
        assert!(gone(false, Seen::Absent).is_err());
        // Someone else's repo (made private, access revoked) is never "gone".
        git(
            &repo.join(".bare"),
            &[
                "remote",
                "set-url",
                "origin",
                "https://git.example.es/other/r.git",
            ],
        );
        let f = Fake {
            login_ok: true,
            seen: Seen::Absent,
            lookups: Cell::new(0),
        };
        assert_eq!(confirmed_gone(&cfg, &repo, "origin", &f), Ok(false));
        assert_eq!(f.lookups.get(), 0);
        // ...unless the config names that organisation as an owner.
        let org = Config::parse(
            "[machine]\nname=\"m\"\n[root]\npath=\"/x\"\n\
             [providers.gitea]\nkind=\"gitea\"\nurl=\"https://git.example.es\"\n\
             [[groups]]\nname=\"G\"\nprimary=\"gitea\"\nowners={gitea=\"Other\"}\n",
        )
        .unwrap();
        assert_eq!(confirmed_gone(&org, &repo, "origin", &f), Ok(true));
    }

    #[test]
    fn moving_keeps_every_worktree_working() {
        let d = tmp::TempDir::new("purge-move");
        let (repo, ext) = bare_repo(d.path());
        let root = d.path().join("root");
        let dest = destination(&root, "Purged", &repo, "1").unwrap();
        assert_eq!(dest, root.join("Purged/G/r"));
        move_repo(&repo, &dest).unwrap();
        assert!(!repo.exists());
        for wt in [dest.join("main"), ext.clone()] {
            let st = Git::new(&wt).run(&["status", "--porcelain"]);
            assert!(st.is_ok(), "{}: {st:?}", wt.display());
        }
        let list = Git::new(dest.join(".bare"))
            .run(&["worktree", "list", "--porcelain"])
            .unwrap();
        assert!(list.contains(&dest.join("main").display().to_string()));
        assert!(!list.contains("prunable"), "{list}");
        // A second repo of the same name does not overwrite the first.
        std::fs::create_dir_all(&repo).unwrap();
        assert_eq!(
            destination(&root, "Purged", &repo, "7").unwrap(),
            root.join("Purged/G/r-7")
        );
    }

    #[test]
    fn moving_through_a_symlinked_root_repairs_the_worktrees() {
        let d = tmp::TempDir::new("purge-symlink");
        std::fs::create_dir_all(d.path().join("real")).unwrap();
        std::os::unix::fs::symlink(d.path().join("real"), d.path().join("link")).unwrap();
        let base = d.path().join("link");
        let (repo, ext) = bare_repo(&base);
        let dest = destination(&base.join("root"), "Purged", &repo, "1").unwrap();
        let moved = move_repo(&repo, &dest).unwrap();
        assert!(moved.unrepaired.is_empty());
        for wt in [dest.join("main"), ext] {
            assert!(Git::new(&wt).run(&["status"]).is_ok(), "{}", wt.display());
        }
        assert!(!dest.join(".grove-busy").exists());
    }

    #[test]
    fn a_busy_repo_or_a_git_operation_in_progress_is_left_alone() {
        let d = tmp::TempDir::new("purge-busy");
        let (repo, _) = bare_repo(d.path());
        let dest = d.path().join("root/Purged/G/r");
        std::fs::write(repo.join(".grove-busy"), "1 1\n").unwrap();
        assert!(matches!(
            move_repo(&repo, &dest),
            Err(MoveError::NotMoved(_))
        ));
        std::fs::remove_file(repo.join(".grove-busy")).unwrap();
        let gd = Git::new(repo.join("main"))
            .run(&["rev-parse", "--absolute-git-dir"])
            .unwrap();
        std::fs::write(Path::new(&gd).join("MERGE_HEAD"), "x").unwrap();
        assert!(matches!(
            move_repo(&repo, &dest),
            Err(MoveError::NotMoved(_))
        ));
        assert!(repo.join(".bare").is_dir() && !dest.exists());
        assert!(!repo.join(".grove-busy").exists());
    }
}
