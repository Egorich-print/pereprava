//! pereprava-menubar: bridge icon in the macOS status bar.
//!
//! Reads `/tmp/pereprava-status.json` (written by `pereprava watch`) once a
//! second and renders connection state + live transfer speeds. The menu
//! offers open-volume / unmount / quit.
//!
//! This crate intentionally sits outside the workspace safety lints: talking
//! to AppKit requires `unsafe` FFI. The unsafe surface is confined to UI
//! plumbing here; business logic lives in pereprava-core / nfs-mount.

#![allow(unsafe_code)]

mod json;

use objc2::rc::Retained;
use objc2::runtime::{NSObject, Sel};
use objc2::{define_class, msg_send, sel, ClassType};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSMenu, NSMenuItem, NSRunningApplication,
    NSStatusBar, NSStatusItem,
};
use objc2_foundation::{MainThreadMarker, NSString, ns_string};
use std::path::PathBuf;
use std::sync::OnceLock;

const STATUS_FILE: &str = "/tmp/pereprava-status.json";

/// Raw pointers to UI elements owned for the whole app lifetime; shared with
/// the timer callback. They are never freed until process exit.
#[derive(Clone, Copy)]
struct Ui {
    item: *mut NSStatusItem,
    menu: *mut NSMenu,
    info: *mut NSMenuItem,
    speed: *mut NSMenuItem,
    volume_line: *mut NSMenuItem,
}
unsafe impl Send for Ui {}
unsafe impl Sync for Ui {}

static UI: OnceLock<Ui> = OnceLock::new();
static MOUNT_PATH: OnceLock<String> = OnceLock::new();

fn main() {
    let mtm = MainThreadMarker::new().expect("menubar must run on the main thread");

    let app = unsafe { NSApplication::sharedApplication(mtm) };
    unsafe {
        app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    }

    // --- status item -------------------------------------------------------
    let status_item = unsafe { NSStatusBar::systemStatusBar().statusItemWithLength(-1.0) };
    if let Some(button) = unsafe { status_item.button(mtm) } {
        button.setTitle(ns_string!("🚧"));
    }

    // --- menu --------------------------------------------------------------
    let menu = unsafe { NSMenu::new(mtm) };
    menu.setAutoenablesItems(false);

    let target = MenuTarget::new(mtm);

    let info_item = make_text_item(&menu, "pereprava: запуск…");
    let speed_item = make_text_item(&menu, "скорость: —");

    let open_item = make_action_item(&menu, "Открыть том", &target, sel!(openVolume:));
    let unmount_item = make_action_item(&menu, "Размонтировать", &target, sel!(unmountAction:));
    add_separator(&menu);
    let _quit = make_action_item(&menu, "Выход", &target, sel!(quitAction:));

    unsafe { status_item.setMenu(Some(&menu)) };

    let _ = MOUNT_PATH.set(String::new());
    let _ = UI.set(Ui {
        item: Retained::into_raw(status_item),
        menu: Retained::into_raw(menu),
        info: Retained::into_raw(info_item),
        speed: Retained::into_raw(speed_item),
        volume_line: Retained::into_raw(open_item),
    });
    let _ = unmount_item;

    schedule_tick(mtm);
    unsafe { app.run() };
}

/// Registers the one-second refresh timer on the main runloop.
fn schedule_tick(mtm: MainThreadMarker) {
    use std::ptr::NonNull;
    let block = block2::RcBlock::new(move |_t: NonNull<objc2_foundation::NSTimer>| tick());
    unsafe {
        let _: Retained<objc2_foundation::NSTimer> =
            objc2_foundation::NSTimer::scheduledTimerWithTimeInterval_repeats_block(
                1.0, true, &block,
            );
    }
}

/// One UI refresh: read status file, repaint icon + menu lines.
fn tick() {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let Some(ui) = UI.get() else { return };
    let raw = std::fs::read_to_string(STATUS_FILE).unwrap_or_default();
    let st = json::Status::parse(&raw);

    let (icon, line1): (&str, String) = match st.state.as_str() {
        "attached" => {
            let busy = st.speed_tx + st.speed_rx > 0;
            (
                if busy { "🌉⇅" } else { "🌉" },
                format!(
                    "{}{} · ▲ {} ▼ {}",
                    if st.model.is_empty() {
                        "телефон"
                    } else {
                        &st.model
                    },
                    if st.mounted.is_empty() {
                        ""
                    } else {
                        " · том готов"
                    },
                    fmt_rate(st.speed_tx),
                    fmt_rate(st.speed_rx),
                ),
            )
        }
        "gone" => ("💤".into(), "телефон отключён — настройки сохранены".into()),
        _ => (
            "🚧",
            "жду телефон (кабель + режим «Передача файлов»)".into(),
        ),
    };

    unsafe {
        if let Some(btn) = (&*ui.item).button(mtm) {
            btn.setTitle(NSString::from_str(icon).as_ref());
        }
        (&*ui.info).setTitle(NSString::from_str(&line1).as_ref());
        (&*ui.speed).setTitle(
            NSString::from_str(&format!(
                "том: {}",
                if st.mounted.is_empty() {
                    "—"
                } else {
                    &st.mounted
                }
            ))
            .as_ref(),
        );
        if !st.mounted.is_empty() {
            let _ = MOUNT_PATH.set(st.mounted.clone());
        }
    }
}

fn fmt_rate(bps: u64) -> String {
    if bps == 0 {
        return "—".into();
    }
    let mib = bps as f64 / (1024.0 * 1024.0);
    if mib >= 1.0 {
        format!("{mib:.1} MiB/s")
    } else {
        format!("{:.0} KiB/s", bps as f64 / 1024.0)
    }
}

fn make_text_item(menu: &NSMenu, title: &str) -> Retained<NSMenuItem> {
    let mtm = MainThreadMarker::new().expect("main");
    let item = unsafe { NSMenuItem::new(mtm) };
    unsafe {
        item.setTitle(&NSString::from_str(title));
        item.setEnabled(false);
        menu.addItem(&item);
    }
    item
}

fn make_action_item(
    menu: &NSMenu,
    title: &str,
    target: &MenuTarget,
    action: Sel,
) -> Retained<NSMenuItem> {
    let mtm = MainThreadMarker::new().expect("main");
    let item = unsafe { NSMenuItem::new(mtm) };
    unsafe {
        item.setTitle(&NSString::from_str(title));
        item.setTarget(Some(target));
        item.setAction(Some(action));
        menu.addItem(&item);
    }
    item
}

fn add_separator(menu: &NSMenu) {
    let mtm = MainThreadMarker::new().expect("main");
    let sep = unsafe { NSMenuItem::separatorItem(mtm) };
    unsafe { menu.addItem(&sep) };
}

// ---------------------------------------------------------------------------
// Action target class
// ---------------------------------------------------------------------------

struct MenuTargetIvars;

define_class!(
    // SAFETY: NSObject has no subclassing requirements; no Drop override.
    #[unsafe(super(NSObject))]
    #[name = "PerepravaMenuTarget"]
    #[ivars = MenuTargetIvars]
    struct MenuTarget;

    impl MenuTarget {
        #[unsafe(method(openVolume:))]
        fn __open_volume(&self, _sender: *mut NSObject) {
            open_volume_impl();
        }

        #[unsafe(method(unmountAction:))]
        fn __unmount_action(&self, _sender: *mut NSObject) {
            let Some(path) = MOUNT_PATH.get() else { return };
            if path.is_empty() {
                return;
            }
            let script =
                format!("do shell script \"umount '{path}'\" with administrator privileges");
            std::thread::spawn(move || {
                let _ = std::process::Command::new("osascript")
                    .arg("-e")
                    .arg(&script)
                    .status();
            });
        }

        #[unsafe(method(quitAction:))]
        fn __quit_action(&self, _sender: *mut NSObject) {
            let mtm = MainThreadMarker::new().expect("main");
            let app: Retained<NSRunningApplication> =
                unsafe { NSRunningApplication::currentApplication() };
            app.terminate();
        }
    }
);

impl MenuTarget {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = mtm.alloc().set_ivars(MenuTargetIvars);
        unsafe { msg_send![super(this), init] }
    }
}

fn open_volume_impl() {
    let Some(path) = MOUNT_PATH.get() else { return };
    if path.is_empty() {
        return;
    }
    let p = PathBuf::from(path);
    std::thread::spawn(move || {
        let _ = std::process::Command::new("open").arg(&p).status();
    });
}
