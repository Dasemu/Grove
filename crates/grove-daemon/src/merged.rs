//! Which branches of a repo were merged through a pull request on its forge (`origin`), so
//! that their worktrees can be pruned (`grove_core::prune`).
//!
//! The daemon asks the forge at most every [`TTL`] per repo, and only for repos that have a
//! worktree on a branch other than the default one. A failed lookup keeps the last answer, so a
//! forge outage never makes `materialize` bring back worktrees that were pruned.

use crate::visibility::{RealForge, parse_remote, provider_for};
use grove_core::config::{Config, Provider as ProviderCfg};
use grove_core::git::Git;
use grove_core::inspect::worktrees;
use grove_core::prune::MergedHead;
use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// How long a forge answer is reused.
pub const TTL: Duration = Duration::from_secs(300);

/// Lists the merged pull requests of a repo on a forge.
pub trait PullSource {
    fn merged(
        &self,
        id: &str,
        pc: &ProviderCfg,
        owner: &str,
        name: &str,
    ) -> Result<Vec<MergedHead>, String>;
}

impl PullSource for RealForge {
    fn merged(
        &self,
        id: &str,
        pc: &ProviderCfg,
        owner: &str,
        name: &str,
    ) -> Result<Vec<MergedHead>, String> {
        let (token, _) = grove_providers::token::resolve(id, pc).map_err(|e| e.to_string())?;
        let forge = grove_providers::build(pc, token);
        let pulls = grove_providers::merged_pulls_following_moves(forge.as_ref(), owner, name)
            .map_err(|e| e.to_string())?;
        Ok(pulls
            .into_iter()
            .map(|p| MergedHead {
                number: p.number,
                branch: p.head_ref,
                sha: p.head_sha,
            })
            .collect())
    }
}

/// The merged pull requests of the repo at `repo` (bare layout), looked up on the forge that
/// serves its `remote` fetch URL.
pub fn lookup(
    cfg: &Config,
    repo: &Path,
    remote: &str,
    src: &dyn PullSource,
) -> Result<Vec<MergedHead>, String> {
    let url = Git::new(repo.join(".bare"))
        .run(&["remote", "get-url", remote])
        .map_err(|e| e.to_string())?;
    let id = parse_remote(&url).ok_or_else(|| format!("cannot parse remote URL {url:?}"))?;
    let (pid, pc) = provider_for(cfg, &id)?;
    src.merged(pid, pc, &id.owner, &id.name)
}

/// True when the repo has a worktree on a branch other than `.bare`'s HEAD: only then can
/// anything be pruned, so only then is the forge asked.
pub fn has_candidates(repo: &Path) -> bool {
    let bare = repo.join(".bare");
    let default = Git::new(&bare)
        .probe(&["symbolic-ref", "--short", "-q", "HEAD"])
        .ok()
        .flatten();
    worktrees(&bare).is_ok_and(|wts| {
        wts.iter()
            .filter_map(|w| w.branch.as_deref())
            .any(|b| Some(b) != default.as_deref())
    })
}

#[derive(Default)]
struct Entry {
    checked: Option<Instant>,
    heads: Vec<MergedHead>,
    error: Option<String>,
    /// "Kept" notes already logged, so a merged worktree with local work is reported once.
    noted: BTreeSet<String>,
}

/// Per-repo forge answers and notes, held by the daemon loop (in memory only).
#[derive(Default)]
pub struct MergedCache {
    entries: HashMap<PathBuf, Entry>,
}

/// Result of [`MergedCache::heads`].
pub struct Heads {
    pub heads: Vec<MergedHead>,
    /// A lookup error that differs from the previous one (to be logged once).
    pub new_error: Option<String>,
}

impl MergedCache {
    /// The merged heads of `repo`, asking `src` when the cached answer is older than [`TTL`].
    pub fn heads(
        &mut self,
        cfg: &Config,
        repo: &Path,
        remote: &str,
        src: &dyn PullSource,
        now: Instant,
    ) -> Heads {
        let e = self.entries.entry(repo.to_path_buf()).or_default();
        if e.checked
            .is_some_and(|t| now.saturating_duration_since(t) < TTL)
        {
            return Heads {
                heads: e.heads.clone(),
                new_error: None,
            };
        }
        e.checked = Some(now);
        let mut new_error = None;
        match lookup(cfg, repo, remote, src) {
            Ok(h) => {
                e.heads = h;
                e.error = None;
            }
            Err(err) => {
                if e.error.as_deref() != Some(&err) {
                    new_error = Some(err.clone());
                }
                e.error = Some(err);
            }
        }
        Heads {
            heads: e.heads.clone(),
            new_error,
        }
    }

    /// Of the "kept" notes of this pass, those not logged yet for `repo`. Notes that no longer
    /// apply are forgotten, so they are logged again if they come back.
    pub fn new_notes(&mut self, repo: &Path, notes: Vec<String>) -> Vec<String> {
        let e = self.entries.entry(repo.to_path_buf()).or_default();
        let now: BTreeSet<String> = notes.into_iter().collect();
        let fresh = now.difference(&e.noted).cloned().collect();
        e.noted = now;
        fresh
    }

    /// The last answer for `repo`, without asking the forge (offline passes).
    pub fn cached(&self, repo: &Path) -> Vec<MergedHead> {
        self.entries
            .get(repo)
            .map(|e| e.heads.clone())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_tmp as tmp;
    use std::cell::{Cell, RefCell};

    struct Fake {
        calls: Cell<u32>,
        asked: RefCell<Vec<String>>,
        answer: RefCell<Result<Vec<MergedHead>, String>>,
    }
    impl PullSource for Fake {
        fn merged(
            &self,
            id: &str,
            _: &ProviderCfg,
            owner: &str,
            name: &str,
        ) -> Result<Vec<MergedHead>, String> {
            self.calls.set(self.calls.get() + 1);
            self.asked.borrow_mut().push(format!("{id}:{owner}/{name}"));
            self.answer.borrow().clone()
        }
    }

    fn head(n: u64) -> MergedHead {
        MergedHead {
            number: n,
            branch: format!("b{n}"),
            sha: "0".repeat(40),
        }
    }

    #[test]
    fn answers_are_cached_and_kept_on_failure() {
        let d = tmp::TempDir::new("merged-cache");
        let repo = d.path().join("G/r");
        std::fs::create_dir_all(&repo).unwrap();
        let bare = repo.join(".bare");
        Git::new(d.path())
            .run(&["init", "-q", "--bare", bare.to_str().unwrap()])
            .unwrap();
        Git::new(&bare)
            .run(&["remote", "add", "origin", "git@git.example.es:Org/r.git"])
            .unwrap();
        let cfg = Config::parse(
            "[machine]\nname=\"m\"\n[root]\npath=\"/x\"\n\
             [providers.gitea]\nkind=\"gitea\"\nurl=\"https://git.example.es\"\n",
        )
        .unwrap();
        let f = Fake {
            calls: Cell::new(0),
            asked: RefCell::new(vec![]),
            answer: RefCell::new(Ok(vec![head(1)])),
        };
        let mut c = MergedCache::default();
        let t0 = Instant::now();
        assert_eq!(c.heads(&cfg, &repo, "origin", &f, t0).heads, vec![head(1)]);
        assert_eq!(f.asked.borrow().as_slice(), ["gitea:Org/r"]);
        // Within the TTL the forge is not asked again.
        c.heads(&cfg, &repo, "origin", &f, t0 + Duration::from_secs(10));
        assert_eq!(f.calls.get(), 1);
        // A failure keeps the last answer and is reported once.
        *f.answer.borrow_mut() = Err("down".into());
        let h = c.heads(&cfg, &repo, "origin", &f, t0 + TTL);
        assert_eq!((h.heads, h.new_error), (vec![head(1)], Some("down".into())));
        let h = c.heads(&cfg, &repo, "origin", &f, t0 + TTL * 2);
        assert_eq!((h.heads, h.new_error), (vec![head(1)], None));
        assert_eq!(f.calls.get(), 3);
    }

    #[test]
    fn notes_are_reported_once_while_they_hold() {
        let mut c = MergedCache::default();
        let r = Path::new("/r");
        assert_eq!(c.new_notes(r, vec!["a".into()]), vec!["a".to_string()]);
        assert!(c.new_notes(r, vec!["a".into()]).is_empty());
        assert!(c.new_notes(r, vec![]).is_empty());
        assert_eq!(c.new_notes(r, vec!["a".into()]), vec!["a".to_string()]);
    }
}
