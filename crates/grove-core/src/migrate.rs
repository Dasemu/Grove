//! Migrate a plain clone to the bare + worktree layout in place (section 5).
//!
//! Safety: a full tar backup and a backup ref are taken first; the observable state
//! (`status --ignored`, stashes, refs) is compared before and after; any failure after
//! the first modification triggers a rollback to the original layout.

use crate::git::{Git, GitError};
use crate::layout::dir_name;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

const BARE: &str = ".bare";
const STAGE: &str = ".grove-stage";

#[derive(Debug)]
pub enum MigrateError {
    /// Pre-flight check failed; nothing was touched.
    Refused(String),
    Git(GitError),
    Io(io::Error),
    /// A step failed after the backup was taken.
    Failed {
        cause: String,
        rolled_back: bool,
        backup: PathBuf,
    },
}

impl fmt::Display for MigrateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MigrateError::Refused(m) => write!(f, "refused: {m}"),
            MigrateError::Git(e) => write!(f, "{e}"),
            MigrateError::Io(e) => write!(f, "io error: {e}"),
            MigrateError::Failed {
                cause,
                rolled_back,
                backup,
            } => write!(
                f,
                "migration failed: {cause}; {}; backup at {}",
                if *rolled_back {
                    "original layout restored"
                } else {
                    "ROLLBACK INCOMPLETE, restore from the backup"
                },
                backup.display()
            ),
        }
    }
}

impl std::error::Error for MigrateError {}

impl From<GitError> for MigrateError {
    fn from(e: GitError) -> Self {
        MigrateError::Git(e)
    }
}
impl From<io::Error> for MigrateError {
    fn from(e: io::Error) -> Self {
        MigrateError::Io(e)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub bare_dir: PathBuf,
    pub worktree: PathBuf,
    pub branch: String,
    pub backup_tar: PathBuf,
    pub backup_ref: String,
}

/// Steps after which a failure can be injected (tests only).
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Staged,
    GitMoved,
    WorktreeAdded,
    Filled,
    IndexMoved,
    Verified,
}

#[derive(Default)]
struct Progress {
    staged: bool,
    git_moved: bool,
    wt_dir: Option<PathBuf>,
    wt_meta: Option<PathBuf>,
    filled: bool,
    index_moved: bool,
    /// Original `core.worktree`, restored on rollback.
    orig_core_worktree: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
struct Observed {
    status: String,
    stash: String,
    refs: String,
}

fn observe(git: &Git) -> Result<Observed, GitError> {
    let refs = git.run(&["for-each-ref", "--format=%(refname) %(objectname)"])?;
    let refs = refs
        .lines()
        .filter(|l| !l.starts_with("refs/sync-backup/"))
        .collect::<Vec<_>>()
        .join("\n");
    Ok(Observed {
        status: git.run(&["status", "--porcelain", "--ignored"])?,
        stash: git.run(&["stash", "list", "--format=%H"])?,
        refs,
    })
}

fn s<E: fmt::Display>(e: E) -> String {
    e.to_string()
}

/// Migrate the plain repo whose root is `repo`. `stamp` must be filesystem-safe.
pub fn migrate(
    repo: &Path,
    machine: &str,
    backup_dir: &Path,
    stamp: &str,
) -> Result<Report, MigrateError> {
    migrate_inner(repo, machine, backup_dir, stamp, None)
}

#[doc(hidden)]
pub fn migrate_failing_after(
    repo: &Path,
    machine: &str,
    backup_dir: &Path,
    stamp: &str,
    step: Step,
) -> Result<Report, MigrateError> {
    migrate_inner(repo, machine, backup_dir, stamp, Some(step))
}

fn refuse<T>(m: impl Into<String>) -> Result<T, MigrateError> {
    Err(MigrateError::Refused(m.into()))
}

fn preflight(root: &Path) -> Result<String, MigrateError> {
    let dotgit = root.join(".git");
    if root.join(BARE).exists() {
        return refuse("already migrated (.bare exists)");
    }
    if root.join(STAGE).exists() {
        return refuse(format!(
            "{STAGE} exists: a previous migration was interrupted. Your files are in {STAGE} and the git dir in .bare; restore from the backup tar or move them back by hand"
        ));
    }
    if !dotgit.is_dir() {
        return refuse("not a plain clone (.git is not a directory)");
    }
    let git = Git::new(root);
    let top = PathBuf::from(git.run(&["rev-parse", "--show-toplevel"])?).canonicalize()?;
    if top != root {
        return refuse("path is not the repository root");
    }
    let Some(branch) = git.probe(&["symbolic-ref", "--short", "-q", "HEAD"])? else {
        return refuse("detached HEAD: check out a branch first");
    };
    if git
        .probe(&["rev-parse", "-q", "--verify", "HEAD"])?
        .is_none()
    {
        return refuse("repository has no commits yet");
    }
    for f in [
        "rebase-merge",
        "rebase-apply",
        "MERGE_HEAD",
        "CHERRY_PICK_HEAD",
        "REVERT_HEAD",
        "BISECT_LOG",
    ] {
        if dotgit.join(f).exists() {
            return refuse(format!(
                "operation in progress ({f}): finish or abort it first"
            ));
        }
    }
    if matches!(
        git.probe(&["config", "--get", "core.sparseCheckout"])?
            .as_deref(),
        Some("true")
    ) || matches!(
        git.probe(&["config", "--get", "extensions.worktreeConfig"])?
            .as_deref(),
        Some("true")
    ) {
        return refuse("sparse checkout / per-worktree config is not supported yet");
    }
    if root.join(".gitmodules").exists() || dotgit.join("modules").exists() {
        return refuse("submodules are not supported yet");
    }
    if git
        .run(&["worktree", "list", "--porcelain"])?
        .matches("worktree ")
        .count()
        > 1
    {
        return refuse("repository already has linked worktrees; not supported yet");
    }
    Ok(branch)
}

fn backup(
    root: &Path,
    git: &Git,
    machine: &str,
    backup_dir: &Path,
    stamp: &str,
) -> Result<(PathBuf, String), MigrateError> {
    // Check before creating anything so a refusal never leaves stray directories behind.
    if std::path::absolute(backup_dir)?.starts_with(root) {
        return refuse("backup directory must be outside the repository");
    }
    fs::create_dir_all(backup_dir)?;
    let backup_dir = backup_dir.canonicalize()?;
    if backup_dir.starts_with(root) {
        return refuse("backup directory must be outside the repository");
    }
    let name = root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "repo".into());
    let tar = backup_dir.join(format!("{name}-{stamp}.tar"));
    let parent = root
        .parent()
        .ok_or_else(|| MigrateError::Refused("repo has no parent".into()))?;
    let out = Command::new("tar")
        .arg("-cf")
        .arg(&tar)
        .arg("-C")
        .arg(parent)
        .arg(&name)
        .output()?;
    if !out.status.success() {
        let _ = fs::remove_file(&tar);
        return refuse(format!(
            "tar backup failed: {}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    let backup_ref = format!("refs/sync-backup/{machine}/{stamp}/add");
    git.run(&["update-ref", &backup_ref, "HEAD"])?;
    Ok((tar, backup_ref))
}

fn migrate_inner(
    repo: &Path,
    machine: &str,
    backup_dir: &Path,
    stamp: &str,
    inject: Option<Step>,
) -> Result<Report, MigrateError> {
    let root = repo.canonicalize()?;
    let branch = preflight(&root)?;
    let git = Git::new(&root);
    let before = observe(&git)?;
    let (backup_tar, backup_ref) = backup(&root, &git, machine, backup_dir, stamp)?;

    let dir = dir_name(&branch, &[BARE, STAGE]);
    let mut p = Progress::default();
    match run_steps(&root, &branch, &dir, &before, &mut p, inject) {
        Ok(worktree) => Ok(Report {
            bare_dir: root.join(BARE),
            worktree,
            branch,
            backup_tar,
            backup_ref,
        }),
        Err(cause) => {
            let rb = rollback(&root, &p);
            let (cause, rolled_back) = match rb {
                Ok(()) => (cause, true),
                Err(e) => (format!("{cause}; rollback error: {e}"), false),
            };
            Err(MigrateError::Failed {
                cause,
                rolled_back,
                backup: backup_tar,
            })
        }
    }
}

fn move_entries(from: &Path, to: &Path, skip: &[&str]) -> io::Result<()> {
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let name = entry.file_name();
        if skip.iter().any(|k| name == *k) {
            continue;
        }
        fs::rename(entry.path(), to.join(&name))?;
    }
    Ok(())
}

fn hook(inject: Option<Step>, at: Step) -> Result<(), String> {
    if inject == Some(at) {
        Err(format!("injected failure after {at:?}"))
    } else {
        Ok(())
    }
}

fn run_steps(
    root: &Path,
    branch: &str,
    dir: &str,
    before: &Observed,
    p: &mut Progress,
    inject: Option<Step>,
) -> Result<PathBuf, String> {
    let stage = root.join(STAGE);
    let root_git = Git::new(root);

    // 1. Move everything except .git into a staging dir.
    fs::create_dir(&stage).map_err(s)?;
    p.staged = true;
    move_entries(root, &stage, &[".git", STAGE]).map_err(s)?;
    hook(inject, Step::Staged)?;

    // 2. .git -> .bare, make it bare.
    fs::rename(root.join(".git"), root.join(BARE)).map_err(s)?;
    p.git_moved = true;
    root_git
        .run(&["--git-dir", BARE, "config", "core.bare", "true"])
        .map_err(s)?;
    p.orig_core_worktree = root_git
        .probe(&["--git-dir", BARE, "config", "--get", "core.worktree"])
        .map_err(s)?;
    root_git
        .probe(&["--git-dir", BARE, "config", "--unset", "core.worktree"])
        .map_err(s)?;
    hook(inject, Step::GitMoved)?;

    // 3. Register the worktree without checking anything out.
    let wt = root.join(dir);
    p.wt_dir = Some(wt.clone());
    let wt_str = wt.to_string_lossy().into_owned();
    root_git
        .run(&[
            "--git-dir",
            BARE,
            "worktree",
            "add",
            "--no-checkout",
            &wt_str,
            branch,
        ])
        .map_err(s)?;
    let pointer = fs::read_to_string(wt.join(".git")).map_err(s)?;
    let meta = pointer
        .trim()
        .strip_prefix("gitdir: ")
        .map(PathBuf::from)
        .ok_or("unexpected .git file in new worktree")?;
    p.wt_meta = Some(meta.clone());
    hook(inject, Step::WorktreeAdded)?;

    // 4. Put the user's files into the worktree.
    p.filled = true;
    move_entries(&stage, &wt, &[]).map_err(s)?;
    hook(inject, Step::Filled)?;

    // 5. Carry over the index so staged changes survive.
    let old_index = root.join(BARE).join("index");
    if old_index.exists() {
        fs::rename(&old_index, meta.join("index")).map_err(s)?;
        p.index_moved = true;
    }
    hook(inject, Step::IndexMoved)?;

    // 6. Verify nothing observable changed.
    let after = observe(&Git::new(&wt)).map_err(s)?;
    if &after != before {
        let what = if after.status != before.status {
            "status"
        } else if after.stash != before.stash {
            "stashes"
        } else {
            "refs"
        };
        return Err(format!(
            "verification failed: {what} differ after migration"
        ));
    }
    hook(inject, Step::Verified)?;

    fs::remove_dir(&stage).map_err(s)?;
    Ok(wt)
}

fn rollback(root: &Path, p: &Progress) -> Result<(), String> {
    let stage = root.join(STAGE);
    let bare = root.join(BARE);
    if p.index_moved
        && let Some(meta) = &p.wt_meta
    {
        fs::rename(meta.join("index"), bare.join("index")).map_err(s)?;
    }
    if let Some(wt) = &p.wt_dir
        && wt.exists()
    {
        if p.filled {
            fs::create_dir_all(&stage).map_err(s)?;
            move_entries(wt, &stage, &[".git"]).map_err(s)?;
        }
        // Only delete the worktree dir if user files are no longer in it.
        let leftovers = fs::read_dir(wt)
            .map_err(s)?
            .filter_map(Result::ok)
            .any(|e| e.file_name() != ".git");
        if leftovers {
            return Err(format!(
                "{} still holds user files; not deleting",
                wt.display()
            ));
        }
        fs::remove_dir_all(wt).map_err(s)?;
    }
    if let Some(meta) = &p.wt_meta
        && meta.exists()
    {
        fs::remove_dir_all(meta).map_err(s)?;
    }
    if p.git_moved {
        let g = Git::new(root);
        // Drop metadata of a half-created worktree (e.g. `worktree add` failed midway).
        let _ = g.probe(&["--git-dir", BARE, "worktree", "prune"]);
        let _ = fs::remove_dir(bare.join("worktrees"));
        g.run(&["--git-dir", BARE, "config", "core.bare", "false"])
            .map_err(s)?;
        if let Some(w) = &p.orig_core_worktree {
            g.run(&["--git-dir", BARE, "config", "core.worktree", w])
                .map_err(s)?;
        }
        fs::rename(&bare, root.join(".git")).map_err(s)?;
    }
    if p.staged && stage.exists() {
        move_entries(&stage, root, &[]).map_err(s)?;
        fs::remove_dir(&stage).map_err(s)?;
    }
    Ok(())
}
