//! Tauri shell: serves `gui/ui` and exposes the same three calls the dev bridge offers
//! (`ipc`, `cli`, `config`) as Tauri commands, backed by the shared `grove-bridge` library.
//! Also a tray icon with open / pause-resume / quit so the window can be closed.

use grove_bridge::Bridge;
use serde_json::{Value, json};
use tauri::menu::{Menu, MenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{Manager, WindowEvent};

fn bridge() -> Bridge {
    Bridge {
        grove_bin: std::env::var_os("GROVE_BIN")
            .map(Into::into)
            .unwrap_or_else(|| "grove".into()),
        config: None,
        socket: grove_daemon::ipc::default_socket_path(),
    }
}

#[tauri::command]
fn ipc(req: Value) -> Value {
    bridge().ipc(&req)
}

#[tauri::command]
fn cli(args: Vec<String>, stdin: Option<String>) -> Value {
    bridge().cli(&args, &stdin.unwrap_or_default())
}

#[tauri::command]
fn config() -> Value {
    match bridge().config_json() {
        Some(c) => json!({"exists": true, "config": c}),
        None => json!({"exists": false}),
    }
}

fn main() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![ipc, cli, config])
        .setup(|app| {
            let open = MenuItem::with_id(app, "open", "Open Grove", true, None::<&str>)?;
            let pause = MenuItem::with_id(app, "pause", "Pause / resume sync", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "Quit GUI (daemon keeps running)", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&open, &pause, &quit])?;
            TrayIconBuilder::new()
                .icon(app.default_window_icon().cloned().expect("window icon"))
                .menu(&menu)
                .on_menu_event(|app, ev| match ev.id.as_ref() {
                    "open" => {
                        if let Some(w) = app.get_webview_window("main") {
                            let _ = w.show();
                            let _ = w.set_focus();
                        }
                    }
                    "pause" => {
                        let b = bridge();
                        let paused = b.ipc(&json!({"v":1,"cmd":"status"}))["data"]["paused"]
                            .as_bool()
                            .unwrap_or(false);
                        b.ipc(&json!({"v":1,"cmd": if paused {"resume"} else {"pause"}}));
                    }
                    "quit" => app.exit(0),
                    _ => {}
                })
                .build(app)?;
            Ok(())
        })
        // Closing the window hides it; the tray keeps the app (and its notifications) alive.
        .on_window_event(|w, ev| {
            if let WindowEvent::CloseRequested { api, .. } = ev {
                api.prevent_close();
                let _ = w.hide();
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running grove-gui");
}
