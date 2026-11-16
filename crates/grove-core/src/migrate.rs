//! Migrate a plain clone to the bare + worktree layout in place (section 5).
//!
//! Safety: a full tar backup and a backup ref are taken first; the observable state
//! (`status --ignored`, stashes, refs) is compared before and after; any failure after
//! the first modification triggers a rollback to the original layout.
//!
//! Linked worktrees (`git worktree add`) are supported: the ones nested inside the repo move
//! together with the main checkout, the ones outside stay where they are, and the two link files
//! of each (`<wt>/.git` and `.bare/worktrees/<id>/gitdir`) are rewritten to the new locations.
//! Locked, missing/prunable and mid-operation linked worktrees are refused.

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
    /// Linked worktrees that existed before the migration (they were kept usable).
    pub linked: Vec<LinkedReport>,
    /// Extra tar backups, one per linked worktree outside the repo directory.
    pub linked_backups: Vec<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkedReport {
    /// Location before the migration.
    pub old_path: PathBuf,
    /// Location now (differs from `old_path` only for worktrees nested inside the repo).
    pub path: PathBuf,
    /// `None` for a detached HEAD.
    pub branch: Option<String>,
}

/// A linked worktree found by the pre-flight, with what is needed to relink and roll back.
#[derive(Debug, Clone)]
struct Linked {
    old_path: PathBuf,
    branch: Option<String>,
    /// Directory name under `.git/worktrees/`.
    id: String,
    /// Original contents of `<wt>/.git` and `.git/worktrees/<id>/gitdir`.
    orig_gitfile: String,
    orig_gitdir: String,
}

impl Linked {
    fn new_path(&self, root: &Path, dir: &str) -> PathBuf {
        match self.old_path.strip_prefix(root) {
            Ok(rel) => root.join(dir).join(rel),
            Err(_) => self.old_path.clone(),
        }
    }
}

/// Steps after which a failure can be injected (tests only).
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Staged,
    GitMoved,
    WorktreeAdded,
    Filled,
    Relinked,
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
    /// Link files of linked worktrees may have been rewritten.
    relinking: bool,
    /// Ids under `.git/worktrees/` before the migration; any other one is ours.
    orig_ids: Vec<String>,
    /// Original `core.worktree`, restored on rollback.
    orig_core_worktree: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
struct Observed {
    status: String,
    stash: String,
    refs: String,
    /// Per linked worktree (same order as the pre-flight list): HEAD, branch, status.
    linked: Vec<String>,
}

fn observe(git: &Git, linked: &[PathBuf]) -> Result<Observed, GitError> {
    let refs = git.run(&["for-each-ref", "--format=%(refname) %(objectname)"])?;
    let refs = refs
        .lines()
        .filter(|l| !l.starts_with("refs/sync-backup/"))
        .collect::<Vec<_>>()
        .join("\n");
    let mut lk = Vec::new();
    for path in linked {
        let g = Git::new(path);
        lk.push(format!(
            "{}\n{}\n{}",
            g.run(&["rev-parse", "HEAD"])?,
            g.probe(&["symbolic-ref", "-q", "HEAD"])?
                .unwrap_or_default(),
            g.run(&["status", "--porcelain", "--ignored"])?
        ));
    }
    Ok(Observed {
        status: git.run(&["status", "--porcelain", "--ignored"])?,
        stash: git.run(&["stash", "list", "--format=%H"])?,
        refs,
        linked: lk,
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

fn preflight(root: &Path) -> Result<(String, Vec<Linked>), MigrateError> {
    let dotgit = root.join(".git");
    if root.join(BARE).exists() {
        return refuse("already migrated (.bare exists)");
    }
    if root.join(STAGE).exists() {
        return refuse(format!(
            "{STAGE} exists: a previous migration was interrupted. Your files are in {STAGE} and the git dir in .bare; restore from the backup tar or move them back by hand. Linked worktrees may also need their .git link files restored from the backup tars or `git worktree repair`"
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
    if let Some(f) = operation_in_progress(&dotgit) {
        return refuse(format!(
            "operation in progress ({f}): finish or abort it first"
        ));
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
    if root.join(".gitmodules").exists()
        || dotgit.join("modules").exists()
        || list_ids(&dotgit)
            .iter()
            .any(|id| dotgit.join("worktrees").join(id).join("modules").exists())
    {
        return refuse("submodules are not supported yet");
    }
    for key in ["worktree.useRelativePaths", "extensions.relativeWorktrees"] {
        if matches!(
            git.probe(&["config", "--type=bool", "--get", key])?
                .as_deref(),
            Some("true")
        ) {
            return refuse(format!(
                "{key} (relative worktree links) is not supported yet"
            ));
        }
    }
    let linked = linked_worktrees(root, &git)?;
    Ok((branch, linked))
}

const OPERATION_FILES: [&str; 6] = [
    "rebase-merge",
    "rebase-apply",
    "MERGE_HEAD",
    "CHERRY_PICK_HEAD",
    "REVERT_HEAD",
    "BISECT_LOG",
];

/// First sign of an unfinished operation in a git dir (`.git` or `.git/worktrees/<id>`).
fn operation_in_progress(gitdir: &Path) -> Option<&'static str> {
    OPERATION_FILES
        .into_iter()
        .find(|f| gitdir.join(f).exists())
}

/// Linked worktrees of the plain repo at `root`, or a refusal if one cannot be moved safely.
fn linked_worktrees(root: &Path, git: &Git) -> Result<Vec<Linked>, MigrateError> {
    let out = git.run(&["worktree", "list", "--porcelain"])?;
    let mut linked = Vec::new();
    // The first block is the main worktree.
    for block in out.split("\n\n").skip(1).filter(|b| !b.trim().is_empty()) {
        let mut path = None;
        let mut branch = None;
        let (mut locked, mut prunable) = (false, false);
        for line in block.lines() {
            if let Some(p) = line.strip_prefix("worktree ") {
                path = Some(PathBuf::from(p));
            } else if let Some(b) = line.strip_prefix("branch refs/heads/") {
                branch = Some(b.to_string());
            } else if line == "locked" || line.starts_with("locked ") {
                locked = true;
            } else if line == "prunable" || line.starts_with("prunable ") {
                prunable = true;
            }
        }
        let Some(path) = path else { continue };
        let shown = path.display();
        if locked {
            return refuse(format!(
                "linked worktree {shown} is locked: unlock it first (git worktree unlock)"
            ));
        }
        let Ok(old_path) = path.canonicalize() else {
            return refuse(format!(
                "linked worktree {shown} is missing on disk: restore it or run `git worktree prune`"
            ));
        };
        if prunable {
            return refuse(format!(
                "linked worktree {shown} is prunable (broken link): fix it or run `git worktree prune`"
            ));
        }
        if root.starts_with(&old_path) {
            return refuse(format!(
                "linked worktree {shown} contains the repository; not supported"
            ));
        }
        let gitfile = old_path.join(".git");
        let orig_gitfile = fs::read_to_string(&gitfile).map_err(|e| {
            MigrateError::Refused(format!("cannot read {}: {e}", gitfile.display()))
        })?;
        let meta = orig_gitfile
            .trim()
            .strip_prefix("gitdir: ")
            .map(PathBuf::from)
            .filter(|m| m.is_absolute())
            .ok_or_else(|| {
                MigrateError::Refused(format!(
                    "linked worktree {shown}: unsupported .git file (relative links?)"
                ))
            })?;
        let meta = meta.canonicalize()?;
        let id = match (meta.parent(), meta.file_name()) {
            (Some(p), Some(id)) if p == root.join(".git").join("worktrees") => {
                id.to_string_lossy().into_owned()
            }
            _ => {
                return refuse(format!(
                    "linked worktree {shown} does not belong to this repository's .git"
                ));
            }
        };
        if let Some(f) = operation_in_progress(&meta) {
            return refuse(format!(
                "operation in progress ({f}) in linked worktree {shown}: finish or abort it first"
            ));
        }
        if old_path.join(".gitmodules").exists() {
            return refuse(format!(
                "linked worktree {shown} has submodules; not supported yet"
            ));
        }
        let orig_gitdir = fs::read_to_string(meta.join("gitdir"))?;
        linked.push(Linked {
            old_path,
            branch,
            id,
            orig_gitfile,
            orig_gitdir,
        });
    }
    Ok(linked)
}

fn backup(
    root: &Path,
    git: &Git,
    machine: &str,
    backup_dir: &Path,
    stamp: &str,
    linked: &[Linked],
) -> Result<(PathBuf, String, Vec<PathBuf>), MigrateError> {
    // Check before creating anything so a refusal never leaves stray directories behind.
    if std::path::absolute(backup_dir)?.starts_with(root) {
        return refuse("backup directory must be outside the repository");
    }
    fs::create_dir_all(backup_dir)?;
    let backup_dir = backup_dir.canonicalize()?;
    if backup_dir.starts_with(root) {
        return refuse("backup directory must be outside the repository");
    }
    for l in linked.iter().filter(|l| !l.old_path.starts_with(root)) {
        if backup_dir.starts_with(&l.old_path) {
            return refuse("backup directory must be outside the linked worktrees");
        }
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
    // Linked worktrees outside the repo dir are not in the tar above: back each one up too.
    let mut extra = Vec::new();
    for (i, l) in linked.iter().enumerate() {
        if l.old_path.starts_with(root) {
            continue;
        }
        let (Some(wp), Some(wn)) = (l.old_path.parent(), l.old_path.file_name()) else {
            continue;
        };
        let t = backup_dir.join(format!(
            "{name}-{stamp}-linked{i}-{}.tar",
            wn.to_string_lossy()
        ));
        let out = Command::new("tar")
            .arg("-cf")
            .arg(&t)
            .arg("-C")
            .arg(wp)
            .arg(wn)
            .output()?;
        if !out.status.success() {
            let _ = fs::remove_file(&t);
            let _ = fs::remove_file(&tar);
            for e in &extra {
                let _ = fs::remove_file(e);
            }
            return refuse(format!(
                "tar backup of linked worktree {} failed: {}",
                l.old_path.display(),
                String::from_utf8_lossy(&out.stderr)
            ));
        }
        extra.push(t);
    }
    let backup_ref = format!("refs/sync-backup/{machine}/{stamp}/add");
    git.run(&["update-ref", &backup_ref, "HEAD"])?;
    Ok((tar, backup_ref, extra))
}

fn migrate_inner(
    repo: &Path,
    machine: &str,
    backup_dir: &Path,
    stamp: &str,
    inject: Option<Step>,
) -> Result<Report, MigrateError> {
    let root = repo.canonicalize()?;
    let (branch, linked) = preflight(&root)?;
    let git = Git::new(&root);
    let old_paths: Vec<PathBuf> = linked.iter().map(|l| l.old_path.clone()).collect();
    let before = observe(&git, &old_paths)?;
    let (backup_tar, backup_ref, linked_backups) =
        backup(&root, &git, machine, backup_dir, stamp, &linked)?;

    let dir = dir_name(&branch, &[BARE, STAGE]);
    let mut p = Progress {
        orig_ids: list_ids(&root.join(".git")),
        ..Progress::default()
    };
    match run_steps(&root, &branch, &dir, &linked, &before, &mut p, inject) {
        Ok(worktree) => Ok(Report {
            bare_dir: root.join(BARE),
            worktree,
            branch,
            backup_tar,
            backup_ref,
            linked: linked
                .iter()
                .map(|l| LinkedReport {
                    old_path: l.old_path.clone(),
                    path: l.new_path(&root, &dir),
                    branch: l.branch.clone(),
                })
                .collect(),
            linked_backups,
        }),
        Err(cause) => {
            let rb = rollback(&root, &p, &linked);
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

fn list_ids(gitdir: &Path) -> Vec<String> {
    fs::read_dir(gitdir.join("worktrees"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect()
}

/// Point every linked worktree's link files at the new locations.
fn relink(root: &Path, dir: &str, linked: &[Linked]) -> io::Result<()> {
    let meta_root = root.join(BARE).join("worktrees");
    for l in linked {
        let new = l.new_path(root, dir);
        let meta = meta_root.join(&l.id);
        fs::write(new.join(".git"), format!("gitdir: {}\n", meta.display()))?;
        fs::write(
            meta.join("gitdir"),
            format!("{}\n", new.join(".git").display()),
        )?;
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
    linked: &[Linked],
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
        .map(|m| wt.join(m))
        .ok_or("unexpected .git file in new worktree")?;
    p.wt_meta = Some(meta.clone());
    hook(inject, Step::WorktreeAdded)?;

    // 4. Put the user's files into the worktree.
    p.filled = true;
    move_entries(&stage, &wt, &[]).map_err(s)?;
    hook(inject, Step::Filled)?;

    // 4b. Repair the links of the linked worktrees (nested ones moved with the files).
    p.relinking = true;
    relink(root, dir, linked).map_err(s)?;
    hook(inject, Step::Relinked)?;

    // 5. Carry over the index so staged changes survive.
    let old_index = root.join(BARE).join("index");
    if old_index.exists() {
        fs::rename(&old_index, meta.join("index")).map_err(s)?;
        p.index_moved = true;
    }
    hook(inject, Step::IndexMoved)?;

    // 6. Verify nothing observable changed.
    let new_paths: Vec<PathBuf> = linked.iter().map(|l| l.new_path(root, dir)).collect();
    let listing = Git::new(&wt)
        .run(&["worktree", "list", "--porcelain"])
        .map_err(s)?;
    for np in &new_paths {
        if !listing
            .lines()
            .any(|l| l == format!("worktree {}", np.display()))
        {
            return Err(format!(
                "verification failed: linked worktree {} is not registered",
                np.display()
            ));
        }
    }
    if listing
        .lines()
        .any(|l| l == "prunable" || l.starts_with("prunable "))
    {
        return Err("verification failed: a linked worktree link is broken".into());
    }
    let after = observe(&Git::new(&wt), &new_paths).map_err(s)?;
    if &after != before {
        let what = if after.status != before.status {
            "status"
        } else if after.stash != before.stash {
            "stashes"
        } else if after.linked != before.linked {
            "linked worktrees"
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

fn rollback(root: &Path, p: &Progress, linked: &[Linked]) -> Result<(), String> {
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
        // Drop metadata of a half-created worktree (e.g. `worktree add` failed midway). Never
        // `worktree prune` here: linked worktrees are temporarily at stale paths.
        for id in list_ids(&bare) {
            if !p.orig_ids.contains(&id) {
                fs::remove_dir_all(bare.join("worktrees").join(id)).map_err(s)?;
            }
        }
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
    if p.relinking {
        // Everything is back in place: restore the link files of the linked worktrees.
        for l in linked {
            fs::write(l.old_path.join(".git"), &l.orig_gitfile).map_err(s)?;
            fs::write(
                root.join(".git")
                    .join("worktrees")
                    .join(&l.id)
                    .join("gitdir"),
                &l.orig_gitdir,
            )
            .map_err(s)?;
        }
    }
    Ok(())
}
