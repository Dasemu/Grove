//! Opening a worktree in an editor or tool (`grove open`): the launcher table, terminal
//! detection, target resolution and a detached spawn. No sync logic lives here.

use crate::config::{Config, Open, expand_home};
use crate::git::Git;
use crate::inspect::worktrees;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// A launcher grove knows without configuration.
pub struct Builtin {
    pub name: &'static str,
    pub label: &'static str,
    /// Candidate programs; the first one found wins (distributions rename some of them).
    pub programs: &'static [&'static str],
    pub args: &'static [&'static str],
    pub terminal: bool,
}

pub const BUILTIN: &[Builtin] = &[
    Builtin {
        name: "zed",
        label: "Zed",
        programs: &["zed", "zeditor", "zedit"],
        args: &["{path}"],
        terminal: false,
    },
    Builtin {
        name: "vscode",
        label: "VS Code",
        programs: &["code", "codium", "code-oss"],
        args: &["{path}"],
        terminal: false,
    },
    Builtin {
        name: "claude",
        label: "Claude Code",
        programs: &["claude"],
        args: &[],
        terminal: true,
    },
    Builtin {
        name: "claude-desktop",
        label: "Claude Desktop",
        programs: &["claude-desktop"],
        args: &["{path}"],
        terminal: false,
    },
];

/// Known terminals and the arguments that run a command in `{path}` (the command follows).
const TERMINALS: &[(&str, &[&str])] = &[
    ("kitty", &["--directory", "{path}"]),
    ("ghostty", &["--working-directory={path}", "-e"]),
    ("alacritty", &["--working-directory", "{path}", "-e"]),
    ("foot", &["--working-directory={path}"]),
    ("wezterm", &["start", "--cwd", "{path}", "--"]),
    ("konsole", &["--workdir", "{path}", "-e"]),
    (
        "ptyxis",
        &["--new-window", "--working-directory={path}", "--"],
    ),
    ("gnome-terminal", &["--working-directory={path}", "--"]),
    ("xfce4-terminal", &["--working-directory={path}", "-x"]),
    ("xterm", &["-e"]),
];

/// A launcher as resolved on this machine.
#[derive(Debug, Clone, PartialEq)]
pub struct Resolved {
    pub name: String,
    pub label: String,
    /// Program (absolute when found) and arguments, `{path}` not yet substituted.
    pub command: Vec<String>,
    pub terminal: bool,
    /// The program was found.
    pub available: bool,
}

/// Program lookup and environment, injectable for tests.
pub trait Finder {
    fn find(&self, program: &str) -> Option<PathBuf>;
    fn env(&self, key: &str) -> Option<String>;
}

/// The real machine: `PATH` plus the user-level install directories a desktop session's `PATH`
/// often lacks (`~/.local/bin`, `~/.claude/local`, `~/.cargo/bin`).
pub struct System;

impl Finder for System {
    fn find(&self, program: &str) -> Option<PathBuf> {
        find_program(program)
    }
    fn env(&self, key: &str) -> Option<String> {
        std::env::var(key).ok().filter(|v| !v.trim().is_empty())
    }
}

pub fn find_program(program: &str) -> Option<PathBuf> {
    if program.contains('/') {
        let p = expand_home(program);
        return is_executable(&p).then_some(p);
    }
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    if let Some(h) = std::env::var_os("HOME") {
        let h = PathBuf::from(h);
        dirs.extend([
            h.join(".local/bin"),
            h.join(".claude/local"),
            h.join(".cargo/bin"),
        ]);
    }
    dirs.into_iter()
        .map(|d| d.join(program))
        .find(|c| is_executable(c))
}

fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    p.metadata()
        .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// Every launcher: the configured ones first (in config order), then the built-ins they do not
/// replace.
pub fn launchers(open: &Open, f: &dyn Finder) -> Vec<Resolved> {
    let mut out: Vec<Resolved> = open
        .launchers
        .iter()
        .map(|l| {
            let found = f.find(&l.command[0]);
            let mut command = l.command.clone();
            if let Some(p) = &found {
                command[0] = p.to_string_lossy().into_owned();
            }
            Resolved {
                name: l.name.clone(),
                label: l.label.clone().unwrap_or_else(|| l.name.clone()),
                command,
                terminal: l.terminal,
                available: found.is_some(),
            }
        })
        .collect();
    for b in BUILTIN {
        if out.iter().any(|r| r.name == b.name) {
            continue;
        }
        let found = b.programs.iter().find_map(|p| f.find(p));
        let program = found.as_ref().map_or(b.programs[0].to_string(), |p| {
            p.to_string_lossy().into_owned()
        });
        out.push(Resolved {
            name: b.name.into(),
            label: b.label.into(),
            command: std::iter::once(program)
                .chain(b.args.iter().map(|a| a.to_string()))
                .collect(),
            terminal: b.terminal,
            available: found.is_some(),
        });
    }
    out
}

/// The launcher named `name`, or the default one: `open.default`, else the first available.
pub fn pick<'a>(
    all: &'a [Resolved],
    open: &Open,
    name: Option<&str>,
) -> Result<&'a Resolved, String> {
    let wanted = name.or(open.default.as_deref());
    let l = match wanted {
        Some(n) => all.iter().find(|l| l.name == n).ok_or_else(|| {
            let names: Vec<&str> = all.iter().map(|l| l.name.as_str()).collect();
            format!("unknown launcher {n:?} (known: {})", names.join(", "))
        })?,
        None => all.iter().find(|l| l.available).ok_or(
            "no editor or tool found (install zed, code or claude, or add [[open.launchers]])",
        )?,
    };
    if !l.available {
        let tried = BUILTIN
            .iter()
            .find(|b| b.name == l.name && l.command[0] == b.programs[0])
            .map_or_else(|| l.command[0].clone(), |b| b.programs.join(", "));
        return Err(format!(
            "{} is not installed (looked for {tried} in PATH, ~/.local/bin, ~/.claude/local, ~/.cargo/bin)",
            l.label
        ));
    }
    Ok(l)
}

/// The terminal prefix for terminal launchers (`{path}` not yet substituted).
pub fn terminal(open: &Open, f: &dyn Finder) -> Result<Vec<String>, String> {
    if let Some(t) = &open.terminal {
        return Ok(t.clone());
    }
    let known = |prog: &str, base: &str| {
        let args = TERMINALS
            .iter()
            .find(|(n, _)| *n == base)
            .map_or(&["-e"][..], |(_, a)| a);
        std::iter::once(prog.to_string())
            .chain(args.iter().map(|a| a.to_string()))
            .collect::<Vec<_>>()
    };
    if let Some(t) = f.env("TERMINAL") {
        let prog = t.split_whitespace().next().unwrap_or_default().to_string();
        if let Some(p) = f.find(&prog) {
            let base = Path::new(&prog)
                .file_name()
                .map(|b| b.to_string_lossy().into_owned())
                .unwrap_or_default();
            return Ok(known(&p.to_string_lossy(), &base));
        }
    }
    TERMINALS
        .iter()
        .find_map(|(n, _)| f.find(n).map(|p| known(&p.to_string_lossy(), n)))
        .ok_or_else(|| "no terminal found: set $TERMINAL or open.terminal in config.toml".into())
}

/// The full command line that opens `path` with `l`.
pub fn argv(l: &Resolved, path: &Path, open: &Open, f: &dyn Finder) -> Result<Vec<String>, String> {
    let p = path.to_string_lossy();
    let sub = |a: &String| a.replace("{path}", &p);
    let cmd: Vec<String> = l.command.iter().map(sub).collect();
    if !l.terminal {
        return Ok(cmd);
    }
    let mut t: Vec<String> = terminal(open, f)?.iter().map(sub).collect();
    t.extend(cmd);
    Ok(t)
}

/// Starts `argv` in `cwd`, detached (own process group, no stdio), so it outlives grove. A
/// program that fails within half a second (bad arguments, missing display) is reported.
pub fn spawn(argv: &[String], cwd: &Path) -> Result<(), String> {
    use std::os::unix::process::CommandExt;
    let (prog, args) = argv.split_first().ok_or("empty command")?;
    let mut child = Command::new(prog)
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .map_err(|e| format!("cannot start {prog}: {e}"))?;
    let until = Instant::now() + Duration::from_millis(500);
    while Instant::now() < until {
        match child.try_wait() {
            Ok(Some(st)) if !st.success() => return Err(format!("{prog} exited with {st}")),
            Ok(Some(_)) => return Ok(()),
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(e) => return Err(format!("{prog}: {e}")),
        }
    }
    Ok(())
}

/// The directory `target` names: a path (a worktree, or a bare-layout repo whose default-branch
/// worktree is picked), or `[group/]repo[@branch]` under the root.
pub fn resolve_target(cfg: &Config, target: &str) -> Result<PathBuf, String> {
    let as_path = expand_home(target);
    if as_path.exists() {
        let dir = as_path
            .canonicalize()
            .map_err(|e| format!("{}: {e}", as_path.display()))?;
        if !dir.is_dir() {
            return Err(format!("{} is not a directory", dir.display()));
        }
        return if dir.join(".bare").is_dir() {
            pick_worktree(&dir, None)
        } else {
            Ok(dir)
        };
    }
    let (name, branch) = match target.rsplit_once('@') {
        Some((n, b)) if !b.is_empty() => (n, Some(b)),
        _ => (target, None),
    };
    let root = cfg.root_path();
    let dir = match name.split_once('/') {
        Some((g, r)) => {
            let d = root.join(g).join(r);
            if !is_plain(g) || !is_plain(r) || !d.is_dir() {
                return Err(format!("no repository {name:?} under {}", root.display()));
            }
            d
        }
        None => find_repo(cfg, &root, name)?,
    };
    if dir.join(".bare").is_dir() {
        pick_worktree(&dir, branch)
    } else if let Some(b) = branch {
        Err(format!(
            "{} is not in the bare layout, so it has no worktree for {b:?}",
            dir.display()
        ))
    } else {
        Ok(dir)
    }
}

fn is_plain(s: &str) -> bool {
    !s.is_empty() && !s.starts_with('.') && !s.contains(['/', '\\'])
}

/// `root/<group>/<name>` across all groups: an exact match, else a case-insensitive one.
fn find_repo(cfg: &Config, root: &Path, name: &str) -> Result<PathBuf, String> {
    if !is_plain(name) {
        return Err(format!("{name:?} is not a repository name or path"));
    }
    let mut exact = Vec::new();
    let mut loose = Vec::new();
    for g in &cfg.groups {
        let Ok(rd) = std::fs::read_dir(root.join(&g.name)) else {
            continue;
        };
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().into_owned();
            if !e.path().is_dir() || n.starts_with('.') {
                continue;
            }
            if n == name {
                exact.push((g.name.clone(), e.path()));
            } else if n.eq_ignore_ascii_case(name) {
                loose.push((g.name.clone(), e.path()));
            }
        }
    }
    let hits = if exact.is_empty() { loose } else { exact };
    match hits.len() {
        0 => Err(format!(
            "no repository {name:?} in any group under {}",
            root.display()
        )),
        1 => Ok(hits.into_iter().next().map(|(_, p)| p).unwrap_or_default()),
        _ => {
            let names: Vec<String> = hits
                .iter()
                .map(|(g, p)| {
                    format!(
                        "{g}/{}",
                        p.file_name().unwrap_or_default().to_string_lossy()
                    )
                })
                .collect();
            Err(format!(
                "{name:?} is ambiguous: {} (use group/repo)",
                names.join(", ")
            ))
        }
    }
}

/// The worktree of `branch` in the bare-layout repo `dir`; without a branch the default
/// branch's (`origin/HEAD`, then `main`, `master`), else the first one, else `dir` itself.
pub fn pick_worktree(dir: &Path, branch: Option<&str>) -> Result<PathBuf, String> {
    let bare = dir.join(".bare");
    let wts: Vec<_> = worktrees(&bare)
        .map_err(|e| e.to_string())?
        .into_iter()
        .filter(|w| w.path.is_dir())
        .collect();
    if let Some(b) = branch {
        return wts
            .iter()
            .find(|w| w.branch.as_deref() == Some(b))
            .map(|w| w.path.clone())
            .ok_or_else(|| {
                let have: Vec<&str> = wts.iter().filter_map(|w| w.branch.as_deref()).collect();
                format!(
                    "{} has no worktree for branch {b:?} (have: {})",
                    dir.display(),
                    have.join(", ")
                )
            });
    }
    let head = Git::new(&bare)
        .probe(&["symbolic-ref", "--short", "refs/remotes/origin/HEAD"])
        .ok()
        .flatten()
        .and_then(|h| h.strip_prefix("origin/").map(str::to_string));
    let by = |b: &str| wts.iter().find(|w| w.branch.as_deref() == Some(b));
    Ok(head
        .as_deref()
        .and_then(by)
        .or_else(|| by("main"))
        .or_else(|| by("master"))
        .or(wts.first())
        .map_or_else(|| dir.to_path_buf(), |w| w.path.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Launcher;
    use std::collections::HashMap;

    struct Fake {
        progs: Vec<&'static str>,
        env: HashMap<&'static str, &'static str>,
    }

    impl Finder for Fake {
        fn find(&self, p: &str) -> Option<PathBuf> {
            self.progs
                .contains(&p)
                .then(|| PathBuf::from("/bin").join(p))
        }
        fn env(&self, k: &str) -> Option<String> {
            self.env.get(k).map(|v| v.to_string())
        }
    }

    fn fake(progs: &[&'static str]) -> Fake {
        Fake {
            progs: progs.to_vec(),
            env: HashMap::new(),
        }
    }

    #[test]
    fn builtins_resolve_to_the_first_program_found() {
        let all = launchers(&Open::default(), &fake(&["zeditor", "claude", "kitty"]));
        let zed = all.iter().find(|l| l.name == "zed").unwrap();
        assert!(zed.available);
        assert_eq!(zed.command, ["/bin/zeditor", "{path}"]);
        let code = all.iter().find(|l| l.name == "vscode").unwrap();
        assert!(!code.available);
        let p = Path::new("/w/repo/main");
        let open = Open::default();
        let f = fake(&["zeditor", "claude", "kitty"]);
        assert_eq!(
            argv(zed, p, &open, &f).unwrap(),
            ["/bin/zeditor", "/w/repo/main"]
        );
        let claude = pick(&all, &open, Some("claude")).unwrap();
        assert_eq!(
            argv(claude, p, &open, &f).unwrap(),
            ["/bin/kitty", "--directory", "/w/repo/main", "/bin/claude"]
        );
        let e = pick(&all, &open, Some("vscode")).unwrap_err();
        assert!(e.contains("code, codium"), "{e}");
        assert!(
            pick(&all, &open, Some("emacs"))
                .unwrap_err()
                .contains("unknown")
        );
        // No name, no default: the first available.
        assert_eq!(pick(&all, &open, None).unwrap().name, "zed");
        assert!(pick(&launchers(&open, &fake(&[])), &open, None).is_err());
    }

    #[test]
    fn configured_launchers_replace_builtins_and_set_the_default() {
        let open = Open {
            default: Some("claude".into()),
            terminal: Some(vec![
                "myterm".into(),
                "-d".into(),
                "{path}".into(),
                "--".into(),
            ]),
            launchers: vec![
                Launcher {
                    name: "nvim".into(),
                    label: Some("Neovim".into()),
                    command: vec!["nvim".into(), ".".into()],
                    terminal: true,
                },
                Launcher {
                    name: "zed".into(),
                    label: None,
                    command: vec!["/opt/zed/bin/zed".into(), "-n".into(), "{path}".into()],
                    terminal: false,
                },
            ],
        };
        let f = fake(&["nvim", "claude"]);
        let all = launchers(&open, &f);
        assert_eq!(all[0].label, "Neovim");
        assert_eq!(all.iter().filter(|l| l.name == "zed").count(), 1);
        assert!(!all[1].available);
        assert_eq!(pick(&all, &open, None).unwrap().name, "claude");
        let nvim = pick(&all, &open, Some("nvim")).unwrap();
        assert_eq!(
            argv(nvim, Path::new("/r"), &open, &f).unwrap(),
            ["myterm", "-d", "/r", "--", "/bin/nvim", "."]
        );
    }

    #[test]
    fn terminal_prefers_env_then_known_list() {
        let mut f = fake(&["foot", "alacritty"]);
        let open = Open::default();
        assert_eq!(terminal(&open, &f).unwrap()[0], "/bin/alacritty");
        f.env.insert("TERMINAL", "foot");
        assert_eq!(
            terminal(&open, &f).unwrap(),
            ["/bin/foot", "--working-directory={path}"]
        );
        f.progs.push("myterm");
        f.env.insert("TERMINAL", "myterm");
        assert_eq!(terminal(&open, &f).unwrap(), ["/bin/myterm", "-e"]);
        assert!(terminal(&open, &fake(&[])).is_err());
    }

    fn git(dir: &Path, args: &[&str]) {
        Git::new(dir).run(args).unwrap();
    }

    #[test]
    fn targets_resolve_to_worktrees() {
        let tmp = crate::test_tmp::TempDir::new("launch");
        let root = tmp.path().canonicalize().unwrap();
        // work/app: bare layout with main and feat worktrees, origin/HEAD -> main.
        let app = root.join("work/app");
        std::fs::create_dir_all(&app).unwrap();
        let wt = crate::registry::init_bare_layout(&app, "https://example.invalid/x.git").unwrap();
        let bare = app.join(".bare");
        git(&bare, &["update-ref", "refs/remotes/origin/main", "main"]);
        git(
            &bare,
            &[
                "symbolic-ref",
                "refs/remotes/origin/HEAD",
                "refs/remotes/origin/main",
            ],
        );
        git(&bare, &["branch", "feat", "main"]);
        let feat = app.join("feat");
        git(
            &bare,
            &["worktree", "add", "-q", &feat.to_string_lossy(), "feat"],
        );
        // home/notes: a plain directory; other/App: a case-insensitive clash with work/app.
        std::fs::create_dir_all(root.join("home/notes")).unwrap();
        let cfg = Config::parse(&format!(
            "[machine]\nname=\"m\"\n[root]\npath=\"{}\"\n[providers.p]\nkind=\"gitea\"\n\
             [[groups]]\nname=\"work\"\nprimary=\"p\"\n[[groups]]\nname=\"home\"\nprimary=\"p\"\n\
             [[groups]]\nname=\"other\"\nprimary=\"p\"\n",
            root.display()
        ))
        .unwrap();
        let wt = wt.canonicalize().unwrap();
        let feat = feat.canonicalize().unwrap();
        let r = |t: &str| resolve_target(&cfg, t);
        assert_eq!(r("app").unwrap(), wt);
        assert_eq!(r("work/app").unwrap(), wt);
        assert_eq!(r("APP").unwrap(), wt);
        assert_eq!(r("app@feat").unwrap(), feat);
        assert_eq!(r(&app.to_string_lossy()).unwrap(), wt);
        assert_eq!(r(&feat.to_string_lossy()).unwrap(), feat);
        assert_eq!(r("notes").unwrap(), root.join("home/notes"));
        assert!(r("app@nope").unwrap_err().contains("have: "));
        assert!(r("notes@main").is_err());
        assert!(r("missing").is_err());
        assert!(r("work/../home").is_err());
        std::fs::create_dir_all(root.join("other/App")).unwrap();
        assert_eq!(r("app").unwrap(), wt, "an exact match wins");
        std::fs::create_dir_all(root.join("other/app")).unwrap();
        assert!(r("app").unwrap_err().contains("ambiguous"));
    }

    #[test]
    fn spawn_reports_early_failures() {
        let tmp = crate::test_tmp::TempDir::new("spawn");
        assert!(spawn(&["true".into()], tmp.path()).is_ok());
        assert!(
            spawn(&["false".into()], tmp.path())
                .unwrap_err()
                .contains("exited")
        );
        assert!(spawn(&["/nonexistent/x".into()], tmp.path()).is_err());
        assert!(spawn(&[], tmp.path()).is_err());
    }
}
