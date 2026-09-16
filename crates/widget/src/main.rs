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
}

fn read_status() -> Status {
    std::fs::read_to_string(STATUS_FILE)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
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
    std::process::Command::new("open")
        .arg(&path)
        .spawn()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

fn unmount_mounted() -> Result<(), String> {
    let path = read_status().mounted;
    if path.is_empty() {
        return Err("том не смонтирован".into());
    }
    let script = format!(
        "/sbin/umount {} || /sbin/umount -f {}",
        sh_quote(&path),
        sh_quote(&path)
    );
    let escaped = script.replace('\\', "\\\\").replace('"', "\\\"");
    let out = std::process::Command::new("/usr/bin/osascript")
        .arg("-e")
        .arg(format!(
            "do shell script \"{escaped}\" with administrator privileges"
        ))
        .output()
        .map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
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
fn unmount_volume() -> Result<(), String> {
    unmount_mounted()
}

fn show_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.set_focus();
    }
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
                    "open_volume" => drop(open_mounted()),
                    "unmount" => drop(unmount_mounted()),
                    "quit" => app.exit(0),
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

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running pereprava widget");
}
