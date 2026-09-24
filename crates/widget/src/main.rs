//! pereprava-widget: Tauri v2 + Svelte status widget.
//!
//! A menu-bar (tray) icon plus a compact Svelte dashboard. The Rust side owns
//! no MTP logic — it reads the status file published by `pereprava watch` and
//! drives the same `open` / `umount` actions the CLI offers.
//!
//! This crate sits outside the workspace safety lints because Tauri/AppKit
//! plumbing is platform-glue code.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use serde::{Deserialize, Serialize};
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager};

/// Status file written atomically by `pereprava watch`.
const STATUS_FILE: &str = "/tmp/pereprava-status.json";

/// Snapshot of the watcher state, mirrored 1:1 into the status file.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Status {
    #[serde(default)]
    state: String,
    #[serde(default)]
    model: String,
    #[serde(default)]
    mounted: String,
    #[serde(default)]
    rx: u64,
    #[serde(default)]
    tx: u64,
    #[serde(default)]
    speed_rx: u64,
    #[serde(default)]
    speed_tx: u64,
    /// Unix seconds when the daemon wrote this snapshot.
    #[serde(default)]
    ts: u64,
}

/// A snapshot older than this means the daemon is gone: the last values are
/// history, not current state, so they must not be shown as "attached".
const STALE_AFTER_SECS: u64 = 15;

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

fn read_status() -> Status {
    let raw = match std::fs::read_to_string(STATUS_FILE) {
        Ok(r) => r,
        Err(_) => return Status::default(),
    };
    let status: Status = match serde_json::from_str(&raw) {
        Ok(s) => s,
        Err(_) => return Status::default(),
    };
    // Freshness: a dead daemon must not keep showing a live volume.
    if status.ts != 0 && now_secs().saturating_sub(status.ts) > STALE_AFTER_SECS {
        return Status {
            state: "stale".to_string(),
            ts: status.ts,
            ..Status::default()
        };
    }
    status
}

/// POSIX single-quote escaping so a mount path can never break the shell.
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn open_mounted() -> Result<(), String> {
    let path = read_status().mounted;
    if path.is_empty() {
        return Err("том не смонтирован".into());
    }
    // Spawn (do not wait): `open` returns immediately and we do not want the
    // command handler blocked on a slow launch.
    std::process::Command::new("open")
        .arg(&path)
        .spawn()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Unmounts the current volume, escalating the way the CLI daemon does.
///
/// Blocking on `osascript` waits for the administrator dialog, so every
/// entry point that can reach this runs it off the UI thread.
async fn unmount_mounted_async() -> Result<(), String> {
    let path = read_status().mounted;
    if path.is_empty() {
        return Err("том не смонтирован".into());
    }
    let q = sh_quote(&path);
    // Plain umount first (clean detach), then -f, then diskutil for the
    // wedged-NFS cases the CLI relies on.
    let script =
        format!("/sbin/umount {q} 2>/dev/null || /sbin/umount -f {q} 2>/dev/null || /usr/sbin/diskutil unmount force {q}");
    let escaped = script.replace('\\', "\\\\").replace('"', "\\\"");
    let out = tauri::async_runtime::spawn_blocking(move || {
        std::process::Command::new("/usr/bin/osascript")
            .arg("-e")
            .arg(format!(
                "do shell script \"{escaped}\" with administrator privileges"
            ))
            .output()
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        Err(if err.is_empty() {
            "не удалось размонтировать".into()
        } else {
            err
        })
    }
}

#[tauri::command]
fn get_status() -> Status {
    read_status()
}

#[tauri::command]
fn open_volume() -> Result<(), String> {
    open_mounted()
}

#[tauri::command]
async fn unmount_volume() -> Result<(), String> {
    unmount_mounted_async().await
}

fn show_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
    }
}

/// Quits the widget.
///
/// The LaunchAgent is configured with `KeepAlive = { SuccessfulExit: false }`
/// (restart on crash only), so a clean `exit(0)` from here is a real quit and
/// is not immediately respawned by launchd.
fn quit_app(app: &AppHandle) {
    app.exit(0);
}

fn main() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            get_status,
            open_volume,
            unmount_volume
        ])
        .setup(|app| {
            // Menu-bar app: no Dock icon.
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);

            let show = MenuItem::with_id(app, "show", "Показать виджет", true, None::<&str>)?;
            let open = MenuItem::with_id(
                app,
                "open_volume",
                "Открыть том в Finder",
                true,
                None::<&str>,
            )?;
            let unmount = MenuItem::with_id(app, "unmount", "Размонтировать", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "Выход", true, None::<&str>)?;
            let sep = PredefinedMenuItem::separator(app)?;
            let menu = Menu::with_items(app, &[&show, &open, &unmount, &sep, &quit])?;

            let icon = tauri::image::Image::from_bytes(include_bytes!("../assets/tray.png"))?;
            TrayIconBuilder::with_id("main")
                .icon(icon)
                .icon_as_template(true)
                .tooltip("pereprava")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "show" => show_window(app),
                    "open_volume" => {
                        let _ = open_mounted();
                    }
                    "unmount" => {
                        // Off the UI thread: waits on the admin dialog.
                        let handle = app.clone();
                        tauri::async_runtime::spawn(async move {
                            let _ = unmount_mounted_async().await;
                            let _ = handle;
                        });
                    }
                    "quit" => quit_app(app),
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        show_window(tray.app_handle());
                    }
                })
                .build(app)?;

            // Push the watcher status to the UI once a second.
            let handle = app.handle().clone();
            std::thread::spawn(move || {
                loop {
                    let _ = handle.emit("status", read_status());
                    std::thread::sleep(std::time::Duration::from_millis(1000));
                }
            });

            // Closing the dashboard must not kill the tray app: hide instead.
            if let Some(window) = app.get_webview_window("main") {
                let handle = app.handle().clone();
                window.on_window_event(move |event| {
                    if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                        api.prevent_close();
                        if let Some(w) = handle.get_webview_window("main") {
                            let _ = w.hide();
                        }
                    }
                });
            }

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running pereprava widget");
}
