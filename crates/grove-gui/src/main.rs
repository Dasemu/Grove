//! grove-gui: native desktop GUI (Slint) with a StatusNotifierItem tray.
//!
//! Threads: the Slint event loop (main), a worker that polls the daemon, a subscriber that wakes
//! the worker on daemon events, the tray service, the single-instance listener, and short-lived
//! threads for CLI calls. Nothing runs on a timer while the daemon is quiet.

mod bridge;
mod instance;
mod model;
mod tray;

use anyhow::{Context, Result};
use bridge::Bridge;
use model::{Aggregate, TrayState};
use serde_json::{Value, json};
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel, Weak};
use std::cell::RefCell;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tray::Action;

slint::include_modules!();

/// Fallback poll interval; daemon events wake the worker earlier.
const POLL_VISIBLE: Duration = Duration::from_secs(5);
const POLL_HIDDEN: Duration = Duration::from_secs(30);
const MIN_GAP: Duration = Duration::from_millis(500);
const RECONNECT: Duration = Duration::from_secs(5);

/// Everything the UI shows, taken from one refresh.
struct Snapshot {
    status: Option<Value>,
    log: Vec<Value>,
    config: Option<Value>,
    alerts: Vec<(String, String)>,
}

/// State shared between threads.
struct Core {
    bridge: Bridge,
    tx: Mutex<Sender<()>>,
    paused: AtomicBool,
    visible: AtomicBool,
    tray_ok: AtomicBool,
}

impl Core {
    fn wake(&self) {
        let _ = self.tx.lock().unwrap().send(());
    }

    fn set_visible(&self, v: bool) {
        self.visible.store(v, Ordering::SeqCst);
        let _ = self.tx.lock().unwrap().send(());
    }

    /// Fire-and-forget daemon command; `then` runs on the UI thread with the answer.
    fn ipc_bg(self: &Arc<Self>, req: Value, then: impl FnOnce(&Value) + Send + 'static) {
        let core = self.clone();
        std::thread::spawn(move || {
            let r = core.bridge.ipc(&req);
            let _ = slint::invoke_from_event_loop(move || then(&r));
            core.wake();
        });
    }

    fn toggle_pause(self: &Arc<Self>) {
        let cmd = if self.paused.load(Ordering::SeqCst) {
            "resume"
        } else {
            "pause"
        };
        self.ipc_bg(json!({"v": 1, "cmd": cmd}), |_| {});
    }

    fn sync(self: &Arc<Self>, repo: Option<&str>, weak: Weak<MainWindow>) {
        let mut req = json!({"v": 1, "cmd": "sync"});
        let msg = if let Some(r) = repo {
            req["repo"] = json!(r);
            "Sync queued"
        } else {
            "Sync queued for all repositories"
        };
        self.ipc_bg(req, move |r| {
            if let Some(ui) = weak.upgrade() {
                let ok = r["ok"] == true;
                toast(
                    &ui,
                    if ok {
                        msg
                    } else {
                        r["error"].as_str().unwrap_or("failed")
                    },
                    !ok,
                );
            }
        });
    }
}

#[derive(Clone)]
struct Ctx {
    core: Arc<Core>,
    weak: Weak<MainWindow>,
}

/// UI-thread state.
struct UiState {
    snap: Option<Snapshot>,
    outputs: HashMap<String, (bool, String)>,
    filter: String,
    pending: Option<Vec<String>>,
    picks: Rc<VecModel<Pick>>,
    toast_gen: u32,
    last_groups: Vec<String>,
    last_providers: Vec<String>,
}

thread_local! {
    static ST: RefCell<UiState> = RefCell::new(UiState {
        snap: None,
        outputs: HashMap::new(),
        filter: String::new(),
        pending: None,
        picks: Rc::new(VecModel::default()),
        toast_gen: 0,
        last_groups: Vec::new(),
        last_providers: Vec::new(),
    });
}

fn s(v: &Value, k: &str) -> String {
    v.get(k).and_then(Value::as_str).unwrap_or("").to_string()
}

fn utc_offset_s() -> i64 {
    let out = std::process::Command::new("date").arg("+%z").output().ok();
    let z = out
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    if z.len() == 5 {
        let sign = if z.starts_with('-') { -1 } else { 1 };
        if let (Ok(h), Ok(m)) = (z[1..3].parse::<i64>(), z[3..5].parse::<i64>()) {
            return sign * (h * 3600 + m * 60);
        }
    }
    0
}

fn pill_key(t: TrayState) -> &'static str {
    match t {
        TrayState::Ok => "ok",
        TrayState::Syncing => "warn",
        TrayState::Attention => "bad",
        TrayState::Paused => "paused",
        TrayState::Offline => "offline",
    }
}

fn str_model(v: &[String]) -> ModelRc<SharedString> {
    ModelRc::from(Rc::new(VecModel::from(
        v.iter().map(SharedString::from).collect::<Vec<_>>(),
    )))
}

fn render(ui: &MainWindow) {
    ST.with(|st| {
        let mut st = st.borrow_mut();
        let st = &mut *st;
        let Some(snap) = &st.snap else {
            return;
        };
        let agg = model::aggregate(snap.status.as_ref());
        let now = snap
            .status
            .as_ref()
            .and_then(|s| s["now"].as_u64())
            .unwrap_or(0);
        ui.set_loading(false);
        ui.set_offline(snap.status.is_none());
        ui.set_paused(agg.state == TrayState::Paused);
        ui.set_pill_text(agg.text.clone().into());
        ui.set_pill_key(pill_key(agg.state).into());
        ui.set_machine(
            snap.status
                .as_ref()
                .map(|s| s["machine"].as_str().unwrap_or("").to_string())
                .unwrap_or_default()
                .into(),
        );
        ui.set_repo_count(agg.repos as i32);
        ui.set_wt_count(agg.worktrees as i32);
        ui.set_attention_count(agg.attention as i32);
        ui.set_has_config(snap.config.is_some());
        let nm: Vec<SharedString> = snap
            .status
            .as_ref()
            .and_then(|s| s["not_migrated"].as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|p| p.as_str().map(SharedString::from))
                    .collect()
            })
            .unwrap_or_default();
        ui.set_not_migrated(ModelRc::from(Rc::new(VecModel::from(nm))));

        let empty = Vec::new();
        let repos: Vec<Repo> = snap
            .status
            .as_ref()
            .and_then(|s| s["repos"].as_array())
            .unwrap_or(&empty)
            .iter()
            .map(|r| {
                let wts: Vec<Wt> = r["worktrees"]
                    .as_array()
                    .unwrap_or(&empty)
                    .iter()
                    .map(|w| {
                        let state = s(w, "state");
                        let key = model::state_name(&state).to_string();
                        let path = s(w, "path");
                        let (out, out_ok) = st
                            .outputs
                            .get(&path)
                            .map(|(ok, t)| (t.clone(), *ok))
                            .unwrap_or_default();
                        let branch = s(w, "branch");
                        Wt {
                            branch: if branch.is_empty() {
                                "(detached)".into()
                            } else {
                                branch.into()
                            },
                            path: path.into(),
                            label: model::state_label(&state).into(),
                            plan: model::state_plan(&state).into(),
                            state: key.into(),
                            out: out.into(),
                            out_ok,
                        }
                    })
                    .collect();
                let path = s(r, "path");
                Repo {
                    name: model::short(&path).into(),
                    phase: s(r, "phase").into(),
                    last_sync: model::ago(r["last_sync"].as_u64().unwrap_or(0), now).into(),
                    error: r["last_error"].as_str().unwrap_or("").into(),
                    withheld: r["snapshots_withheld"].as_str().unwrap_or("").into(),
                    path: path.into(),
                    wts: ModelRc::from(Rc::new(VecModel::from(wts))),
                }
            })
            .collect();

        let selected = ui.get_selected_path().to_string();
        if let Some(r) = repos.iter().find(|r| r.path == selected) {
            ui.set_detail(r.clone());
            ui.set_detail_found(true);
        } else {
            ui.set_detail_found(false);
        }
        // Long errors are cut in the list; the detail view shows them in full.
        let repos: Vec<Repo> = repos
            .into_iter()
            .map(|mut r| {
                if r.error.chars().count() > 160 {
                    r.error = format!("{}…", r.error.chars().take(160).collect::<String>()).into();
                }
                r
            })
            .collect();
        ui.set_repos(ModelRc::from(Rc::new(VecModel::from(repos))));

        // The activity log is only built while its view is open.
        if ui.get_view() == "activity" {
            let f = st.filter.to_lowercase();
            let off = utc_offset_s();
            let rows: Vec<LogRow> = snap
                .log
                .iter()
                .rev()
                .filter(|e| {
                    f.is_empty()
                        || format!("{} {} {}", s(e, "kind"), s(e, "repo"), s(e, "msg"))
                            .to_lowercase()
                            .contains(&f)
                })
                .map(|e| LogRow {
                    time: model::clock(e["ts"].as_u64().unwrap_or(0), off).into(),
                    kind: s(e, "kind").into(),
                    repo: e["repo"].as_str().unwrap_or("–").into(),
                    tone: model::log_tone(&s(e, "kind")).into(),
                    msg: s(e, "msg").lines().next().unwrap_or("").into(),
                })
                .collect();
            ui.set_log(ModelRc::from(Rc::new(VecModel::from(rows))));
        }

        if let Some(cfg) = &snap.config {
            let groups: Vec<String> = cfg["groups"]
                .as_array()
                .unwrap_or(&empty)
                .iter()
                .map(|g| format!("{} → {}", s(g, "name"), s(g, "primary")))
                .collect();
            let providers: Vec<String> = cfg["providers"]
                .as_object()
                .map(|m| m.keys().cloned().collect())
                .unwrap_or_default();
            // Re-setting a combo model would reset its selection: only on change.
            if groups != st.last_groups {
                ui.set_groups(str_model(&groups));
                st.last_groups = groups;
            }
            if providers != st.last_providers {
                ui.set_providers(str_model(&providers));
                st.last_providers = providers;
            }
            let secs: Vec<Section> = model::settings_sections(cfg)
                .into_iter()
                .map(|(title, rows)| Section {
                    title: title.into(),
                    rows: ModelRc::from(Rc::new(VecModel::from(
                        rows.into_iter()
                            .map(|(k, v)| Kv {
                                key: k.into(),
                                value: v.into(),
                            })
                            .collect::<Vec<_>>(),
                    ))),
                })
                .collect();
            ui.set_sections(ModelRc::from(Rc::new(VecModel::from(secs))));
        }
    });
}

fn toast(ui: &MainWindow, msg: &str, bad: bool) {
    let gen_ = ST.with(|st| {
        let mut st = st.borrow_mut();
        st.toast_gen += 1;
        st.toast_gen
    });
    ui.set_toast_text(msg.into());
    ui.set_toast_bad(bad);
    let weak = ui.as_weak();
    slint::Timer::single_shot(Duration::from_secs(5), move || {
        if let Some(ui) = weak.upgrade()
            && ST.with(|st| st.borrow().toast_gen) == gen_
        {
            ui.set_toast_text("".into());
        }
    });
}

fn show_window(ui: &MainWindow, core: &Core) {
    // Slint exposes no xdg-activation, so an already visible window is raised by re-mapping it:
    // a fresh toplevel is what the compositor focuses.
    if core.visible.load(Ordering::SeqCst) && ui.window().is_visible() {
        let _ = ui.hide();
    }
    let _ = ui.show();
    // On Wayland a window shown from an idle (hidden) loop is not mapped until another event
    // arrives; a one-shot redraw request provides it.
    let w = ui.as_weak();
    slint::Timer::single_shot(Duration::from_millis(100), move || {
        if let Some(ui) = w.upgrade() {
            ui.window().request_redraw();
        }
    });
    core.set_visible(true);
    render(ui);
}

/// Runs an allow-listed `grove` subcommand off the UI thread; `done` runs on the UI thread.
fn run_cli(
    ctx: &Ctx,
    ui: &MainWindow,
    args: Vec<String>,
    stdin: String,
    done: impl FnOnce(&MainWindow, bool, String) + Send + 'static,
) {
    ui.set_busy(true);
    let (core, weak) = (ctx.core.clone(), ctx.weak.clone());
    std::thread::spawn(move || {
        let r = core.bridge.cli(&args, &stdin);
        let ok = r["ok"] == true;
        let text = r["output"].as_str().unwrap_or("").to_string();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = weak.upgrade() {
                ui.set_busy(false);
                done(&ui, ok, text);
            }
        });
        core.wake();
    });
}

fn out(ok: bool, text: String) -> Out {
    Out {
        ok,
        text: text.into(),
    }
}

fn scan(ctx: &Ctx, ui: &MainWindow, group: String) {
    ui.set_import_note("Scanning…".into());
    ui.set_import_out(out(true, String::new()));
    ST.with(|st| st.borrow().picks.set_vec(vec![]));
    run_cli(
        ctx,
        ui,
        vec!["import".into(), group],
        String::new(),
        |ui, ok, text| {
            ui.set_import_note("".into());
            if !ok {
                ui.set_import_out(out(false, text));
                return;
            }
            let names = model::parse_import_listing(&text);
            if names.is_empty() {
                ui.set_import_note(
                    "Nothing to import: every remote repository is already on this machine.".into(),
                );
            }
            ST.with(|st| {
                st.borrow().picks.set_vec(
                    names
                        .into_iter()
                        .map(|n| Pick {
                            name: n.into(),
                            checked: false,
                        })
                        .collect::<Vec<_>>(),
                )
            });
        },
    );
}

fn first_or(v: &str, list: &ModelRc<SharedString>) -> String {
    if v.is_empty() {
        list.row_data(0).map(|x| x.to_string()).unwrap_or_default()
    } else {
        v.to_string()
    }
}

fn wire(ui: &MainWindow, ctx: &Ctx) {
    ST.with(|st| ui.set_picks(ModelRc::from(st.borrow().picks.clone())));

    ui.on_view_changed({
        let c = ctx.clone();
        move |_| c.core.wake()
    });
    ui.on_open_repo({
        let w = ui.as_weak();
        move |p| {
            if let Some(ui) = w.upgrade() {
                ui.set_selected_path(p);
                ui.set_view("repo".into());
                render(&ui);
            }
        }
    });
    ui.on_sync_repo({
        let c = ctx.clone();
        move |p| c.core.sync(Some(p.as_str()), c.weak.clone())
    });
    ui.on_sync_all({
        let c = ctx.clone();
        move || c.core.sync(None, c.weak.clone())
    });
    ui.on_toggle_pause({
        let c = ctx.clone();
        move || c.core.toggle_pause()
    });
    ui.on_filter_changed({
        let w = ui.as_weak();
        move |t| {
            ST.with(|st| st.borrow_mut().filter = t.to_string());
            if let Some(ui) = w.upgrade() {
                render(&ui);
            }
        }
    });

    ui.on_create_repo({
        let c = ctx.clone();
        move |group, name, public| {
            let Some(ui) = c.weak.upgrade() else { return };
            let group = model::group_name(&first_or(group.as_str(), &ui.get_groups())).to_string();
            let mut args = vec!["new".to_string(), group, name.trim().to_string()];
            if public {
                args.push("--public".into());
            }
            ui.set_new_out(out(true, String::new()));
            run_cli(&c, &ui, args, String::new(), |ui, ok, text| {
                ui.set_new_out(out(ok, text));
                if ok {
                    toast(ui, "Repository created", false);
                }
            });
        }
    });

    ui.on_scan_import({
        let c = ctx.clone();
        move |group| {
            let Some(ui) = c.weak.upgrade() else { return };
            let group = model::group_name(&first_or(group.as_str(), &ui.get_groups())).to_string();
            scan(&c, &ui, group);
        }
    });
    ui.on_pick_toggled(|i, checked| {
        ST.with(|st| {
            let m = st.borrow().picks.clone();
            if let Some(mut p) = m.row_data(i as usize) {
                p.checked = checked;
                m.set_row_data(i as usize, p);
            }
        });
    });
    ui.on_import_selected({
        let c = ctx.clone();
        move |group| {
            let Some(ui) = c.weak.upgrade() else { return };
            let group = model::group_name(&first_or(group.as_str(), &ui.get_groups())).to_string();
            let picked: Vec<String> = ST.with(|st| {
                st.borrow()
                    .picks
                    .iter()
                    .filter(|p| p.checked)
                    .map(|p| p.name.to_string())
                    .collect()
            });
            if picked.is_empty() {
                return;
            }
            let mut args = vec!["import".to_string(), group.clone()];
            args.extend(picked);
            let c2 = c.clone();
            run_cli(&c, &ui, args, String::new(), move |ui, ok, text| {
                ui.set_import_out(out(ok, text));
                if ok {
                    scan(&c2, ui, group);
                }
            });
        }
    });

    ui.on_resolve({
        let w = ui.as_weak();
        move |path, mode, branch| {
            let Some(ui) = w.upgrade() else { return };
            let (flags, label): (Vec<String>, String) = match mode.as_str() {
                "save" => {
                    let b = branch.trim().to_string();
                    if b.is_empty() {
                        toast(&ui, "Enter a branch name first", true);
                        return;
                    }
                    (
                        vec!["--save-as".into(), b.clone()],
                        format!("Keep your commits on branch “{b}” and move this branch to the remote tip."),
                    )
                }
                "remote" => (vec!["--take-remote".into()], "Discard your local commits and take the remote tip.".into()),
                _ => (vec!["--keep-local".into()], "Overwrite the remote branch with your local commits.".into()),
            };
            let mut args = vec!["resolve".to_string(), path.to_string()];
            args.extend(flags);
            ST.with(|st| st.borrow_mut().pending = Some(args));
            ui.set_confirm_text(label.into());
        }
    });
    ui.on_confirm_cancel({
        let w = ui.as_weak();
        move || {
            ST.with(|st| st.borrow_mut().pending = None);
            if let Some(ui) = w.upgrade() {
                ui.set_confirm_text("".into());
            }
        }
    });
    ui.on_confirm_accept({
        let c = ctx.clone();
        move || {
            let Some(ui) = c.weak.upgrade() else { return };
            ui.set_confirm_text("".into());
            let Some(args) = ST.with(|st| st.borrow_mut().pending.take()) else {
                return;
            };
            let path = args[1].clone();
            run_cli(&c, &ui, args, String::new(), move |ui, ok, text| {
                ST.with(|st| st.borrow_mut().outputs.insert(path, (ok, text)));
                render(ui);
            });
        }
    });

    ui.on_run_settings({
        let c = ctx.clone();
        move |what| {
            let Some(ui) = c.weak.upgrade() else { return };
            let args: Vec<String> = what.split('-').map(String::from).collect();
            let args = if args[0] == "config" {
                vec!["config-repo-init".to_string()]
            } else {
                args
            };
            run_cli(&c, &ui, args, String::new(), |ui, ok, text| {
                ui.set_settings_out(out(ok, text))
            });
        }
    });
    ui.on_login({
        let c = ctx.clone();
        move |provider, token| {
            let Some(ui) = c.weak.upgrade() else { return };
            let provider = first_or(provider.as_str(), &ui.get_providers());
            run_cli(
                &c,
                &ui,
                vec!["login".into(), provider],
                format!("{token}\n"),
                |ui, ok, text| {
                    ui.set_settings_out(out(ok, text));
                },
            );
        }
    });

    ui.on_onboard({
        let c = ctx.clone();
        move |machine, root, kind, provider, url, group, crepo, token| {
            let Some(ui) = c.weak.upgrade() else { return };
            let mut init: Vec<String> = [
                "init",
                "--machine",
                &machine,
                "--root",
                &root,
                "--provider",
                &provider,
                "--kind",
                &kind,
                "--group",
                &group,
                "--config-repo",
                &crepo,
            ]
            .iter()
            .map(|x| x.to_string())
            .collect();
            if !url.is_empty() {
                init.push("--url".into());
                init.push(url.to_string());
            }
            let token = format!("{token}\n");
            ui.set_busy(true);
            ui.set_onboard_out(out(true, String::new()));
            let (core, weak) = (c.core.clone(), c.weak.clone());
            std::thread::spawn(move || {
                let mut steps: Vec<(Vec<String>, String)> = Vec::new();
                // A previous attempt may already have written the config.
                if core.bridge.config_json().is_none() {
                    steps.push((init, String::new()));
                }
                steps.push((vec!["login".into(), provider.to_string()], token));
                steps.push((vec!["bootstrap".into()], String::new()));
                let mut lines: Vec<String> = Vec::new();
                let mut all_ok = true;
                for (args, stdin) in steps {
                    let r = core.bridge.cli(&args, &stdin);
                    let ok = r["ok"] == true;
                    lines.push(format!(
                        "$ grove {}\n{}",
                        args[0],
                        r["output"].as_str().unwrap_or("")
                    ));
                    let text = lines.join("\n\n");
                    let w = weak.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = w.upgrade() {
                            ui.set_onboard_out(out(ok, text));
                        }
                    });
                    if !ok {
                        all_ok = false;
                        break;
                    }
                }
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = weak.upgrade() {
                        ui.set_busy(false);
                        if all_ok {
                            toast(&ui, "All set", false);
                            ui.set_view("repos".into());
                        }
                    }
                });
                core.wake();
            });
        }
    });
}

// ---- worker: daemon polling -----------------------------------------------------------------

fn worker(
    core: Arc<Core>,
    rx: Receiver<()>,
    tray: Option<tray::TrayHandle>,
    weak: Weak<MainWindow>,
) {
    let mut last_states: HashMap<String, String> = HashMap::new();
    let mut last_agg: Option<Aggregate> = None;
    let mut last_run = Instant::now() - MIN_GAP;
    loop {
        let interval = if core.visible.load(Ordering::SeqCst) {
            POLL_VISIBLE
        } else {
            POLL_HIDDEN
        };
        match rx.recv_timeout(interval) {
            Ok(_) | Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
        // Coalesce bursts of events.
        let gap = last_run.elapsed();
        if gap < MIN_GAP {
            std::thread::sleep(MIN_GAP - gap);
        }
        while rx.try_recv().is_ok() {}
        last_run = Instant::now();

        let visible = core.visible.load(Ordering::SeqCst);
        let st = core.bridge.ipc(&json!({"v": 1, "cmd": "status"}));
        let status = (st["ok"] == true).then(|| st["data"].clone());
        let agg = model::aggregate(status.as_ref());
        core.paused
            .store(agg.state == TrayState::Paused, Ordering::SeqCst);

        let mut alerts = Vec::new();
        if let Some(s_) = &status {
            for r in s_["repos"].as_array().into_iter().flatten() {
                for w in r["worktrees"].as_array().into_iter().flatten() {
                    let path = s(r, "path");
                    let branch = s(w, "branch");
                    let key = format!("{path}|{branch}");
                    let n = model::state_name(w["state"].as_str().unwrap_or("")).to_string();
                    if let Some(prev) = last_states.get(&key)
                        && *prev != n
                        && (n == "diverged" || n == "error")
                    {
                        let title = if n == "diverged" {
                            "Diverged"
                        } else {
                            "Sync error"
                        };
                        alerts.push((
                            title.to_string(),
                            format!("{} · {branch}", model::short(&path)),
                        ));
                    }
                    last_states.insert(key, n);
                }
            }
        }
        if !visible {
            for (t, b) in &alerts {
                let _ = std::process::Command::new("notify-send")
                    .args(["-a", "Grove", "-i", "grove", t, b])
                    .spawn()
                    .map(|mut c| std::thread::spawn(move || c.wait()));
            }
        }

        if let Some(t) = &tray
            && last_agg.as_ref() != Some(&agg)
        {
            tray::set_state(t, &agg);
        }
        last_agg = Some(agg);

        if visible {
            let log = if status.is_some() {
                let l = core
                    .bridge
                    .ipc(&json!({"v": 1, "cmd": "log", "limit": 200}));
                l["data"].as_array().cloned().unwrap_or_default()
            } else {
                Vec::new()
            };
            let snap = Snapshot {
                status,
                log,
                config: core.bridge.config_json(),
                alerts,
            };
            let w = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = w.upgrade() {
                    for (t, b) in &snap.alerts {
                        toast(&ui, &format!("{t}: {b}"), true);
                    }
                    ST.with(|st| st.borrow_mut().snap = Some(snap));
                    render(&ui);
                }
            });
        } else {
            // Keep the last status for the moment the window opens; drop the heavy parts.
            let snap = Snapshot {
                status,
                log: Vec::new(),
                config: core.bridge.config_json(),
                alerts: Vec::new(),
            };
            let _ = slint::invoke_from_event_loop(move || {
                ST.with(|st| st.borrow_mut().snap = Some(snap))
            });
        }
    }
}

/// Wakes the worker whenever the daemon emits an activity event.
fn subscriber(core: Arc<Core>) {
    loop {
        if let Ok(mut c) = UnixStream::connect(&core.bridge.socket) {
            if writeln!(c, r#"{{"v":1,"cmd":"subscribe"}}"#).is_ok() {
                for line in BufReader::new(c).lines() {
                    if line.is_err() {
                        break;
                    }
                    core.wake();
                }
            }
            core.wake(); // connection ended: the daemon went away
        }
        std::thread::sleep(RECONNECT);
    }
}

// ---- entry ----------------------------------------------------------------------------------

fn default_grove_bin() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("grove")))
        .filter(|p| p.exists())
        .unwrap_or_else(|| "grove".into())
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let arg = |name: &str| {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!(
            "grove-gui [--hidden] [--config PATH] [--socket PATH] [--grove-bin PATH] [--view NAME]\n  --hidden  start in the tray without opening the window"
        );
        return Ok(());
    }
    let hidden = args.iter().any(|a| a == "--hidden");
    let inst_path = arg("--instance-socket")
        .map(PathBuf::from)
        .unwrap_or_else(instance::default_socket_path);
    let listener = match instance::acquire(&inst_path, !hidden).context("single-instance check")? {
        instance::Role::Secondary => return Ok(()),
        instance::Role::Primary(l) => l,
    };

    let bridge = Bridge {
        grove_bin: arg("--grove-bin")
            .map(PathBuf::from)
            .unwrap_or_else(default_grove_bin),
        config: arg("--config").map(PathBuf::from),
        socket: arg("--socket")
            .map(PathBuf::from)
            .unwrap_or_else(grove_daemon::ipc::default_socket_path),
    };
    let (tx, rx) = mpsc::channel();
    let core = Arc::new(Core {
        bridge,
        tx: Mutex::new(tx),
        paused: AtomicBool::new(false),
        visible: AtomicBool::new(false),
        tray_ok: AtomicBool::new(false),
    });

    slint::set_xdg_app_id("grove").ok();
    let ui = MainWindow::new().context("create window")?;
    let ctx = Ctx {
        core: core.clone(),
        weak: ui.as_weak(),
    };
    wire(&ui, &ctx);
    if let Some(v) = arg("--view") {
        // Start on a given view (`repo:<path>` for a repository); handy for screenshots.
        match v.split_once(':') {
            Some(("repo", p)) => {
                ui.set_selected_path(p.into());
                ui.set_view("repo".into());
            }
            _ => ui.set_view(v.into()),
        }
    }

    // Tray actions arrive on the tray's thread.
    let on_action: Arc<dyn Fn(Action) + Send + Sync> = {
        let ctx = ctx.clone();
        Arc::new(move |a| match a {
            Action::Show => {
                let c = ctx.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = c.weak.upgrade() {
                        show_window(&ui, &c.core);
                    }
                });
            }
            Action::SyncNow => ctx.core.sync(None, ctx.weak.clone()),
            Action::TogglePause => ctx.core.toggle_pause(),
            Action::Quit => {
                let _ = slint::invoke_from_event_loop(|| {
                    let _ = slint::quit_event_loop();
                });
            }
        })
    };
    let tray = match tray::spawn(on_action) {
        Ok(h) => {
            core.tray_ok.store(true, Ordering::SeqCst);
            Some(h)
        }
        Err(e) => {
            eprintln!("grove-gui: no system tray ({e}); closing the window will quit");
            None
        }
    };

    ui.window().on_close_requested({
        let c = ctx.clone();
        move || {
            c.core.set_visible(false);
            if !c.core.tray_ok.load(Ordering::SeqCst) {
                let _ = slint::quit_event_loop();
            }
            slint::CloseRequestResponse::HideWindow
        }
    });

    {
        let (c, w) = (core.clone(), ui.as_weak());
        std::thread::spawn(move || worker(c, rx, tray, w));
    }
    {
        let c = core.clone();
        std::thread::spawn(move || subscriber(c));
    }
    {
        let c = ctx.clone();
        std::thread::spawn(move || {
            instance::serve(listener, move || {
                let c = c.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = c.weak.upgrade() {
                        show_window(&ui, &c.core);
                    }
                });
            })
        });
    }

    if !hidden || !core.tray_ok.load(Ordering::SeqCst) {
        show_window(&ui, &core);
    } else {
        core.wake();
    }
    slint::run_event_loop_until_quit().context("event loop")?;
    let _ = std::fs::remove_file(&inst_path);
    Ok(())
}
