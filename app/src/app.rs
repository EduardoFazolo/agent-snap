//! Tray icon + popover window (Tauri v2). The popover is a transparent, undecorated window whose
//! HTML draws the NSPopover-like card; it is positioned under/over the tray icon on every show.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agent_snap_core::platform::{PermState, Permission, RectI};
use agent_snap_core::{prompt_for, Options};
use tauri::menu::{MenuBuilder, MenuItemBuilder};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, State, WindowEvent};
use tauri_plugin_positioner::{Position, WindowExt};

use crate::controller::{Controller, Phase, Snapshot};
use crate::{icons, platform};

const POPOVER: &str = "popover";
const TRAY: &str = "main";

struct Ui {
    /// Set once the page has called `ready`.
    ready: AtomicBool,
    /// When the popover was last hidden because it lost focus (a tray click right after = toggle off).
    hidden_at: Mutex<Option<Instant>>,
    recording_icon: AtomicBool,
}

fn popover_position() -> Position {
    if cfg!(target_os = "macos") {
        Position::TrayCenter
    } else {
        Position::TrayBottomCenter
    }
}

fn show_popover(app: &AppHandle) {
    let Some(win) = app.get_webview_window(POPOVER) else { return };
    let ui = app.state::<Arc<Ui>>();
    if ui.ready.load(Ordering::SeqCst) {
        // JS re-renders, measures and calls `resize(.., show=true)`.
        let _ = app.emit("popover:show", ());
    } else {
        let _ = win.as_ref().window().move_window(popover_position());
        let _ = win.show();
        let _ = win.set_focus();
    }
}

fn hide_popover(app: &AppHandle) {
    if let Some(win) = app.get_webview_window(POPOVER) {
        let _ = win.hide();
    }
}

fn set_tray_icon(app: &AppHandle, recording: bool) {
    let ui = app.state::<Arc<Ui>>();
    if ui.recording_icon.swap(recording, Ordering::SeqCst) == recording {
        return;
    }
    if let Some(tray) = app.tray_by_id(TRAY) {
        let icon = if recording { icons::record() } else { icons::camera() };
        let _ = tray.set_icon(Some(icon));
        let _ = tray.set_icon_as_template(!recording);
        let _ = tray.set_tooltip(Some(if recording { "agent-snap: recording" } else { "agent-snap" }));
    }
}

/// Tray rect (physical pixels, top-left origin) -> points, inset by 4pt like the Swift app.
fn tray_rect_points(app: &AppHandle, rect: &tauri::Rect) -> Option<RectI> {
    let pos = rect.position.to_physical::<f64>(1.0);
    let size = rect.size.to_physical::<f64>(1.0);
    let scale = app
        .monitor_from_point(pos.x, pos.y)
        .ok()
        .flatten()
        .or_else(|| app.primary_monitor().ok().flatten())
        .map(|m| m.scale_factor())
        .unwrap_or(1.0);
    let inset = 4.0;
    Some(RectI {
        x: (pos.x / scale - inset).round() as i32,
        y: (pos.y / scale - inset).round() as i32,
        w: (size.width / scale + 2.0 * inset).round() as i32,
        h: (size.height / scale + 2.0 * inset).round() as i32,
    })
}

// ---- commands ----

#[tauri::command]
fn ready(app: AppHandle, ui: State<'_, Arc<Ui>>) {
    ui.ready.store(true, Ordering::SeqCst);
    log::info!("popover ready");
    if let Some(win) = app.get_webview_window(POPOVER) {
        if win.is_visible().unwrap_or(false) {
            let _ = app.emit("popover:show", ());
        }
    }
}

#[tauri::command]
fn get_state(c: State<'_, Controller>) -> Snapshot {
    c.snapshot()
}

#[tauri::command]
fn get_options(c: State<'_, Controller>) -> Options {
    c.options()
}

#[tauri::command]
fn set_options(c: State<'_, Controller>, options: Options) -> Snapshot {
    if !c.is_busy() {
        c.set_options(options);
    }
    c.snapshot()
}

#[tauri::command]
fn start_recording(app: AppHandle, c: State<'_, Controller>) -> Result<(), String> {
    let perms = platform::permissions();
    if perms.state(Permission::ScreenRecording) != PermState::Granted || perms.state(Permission::Accessibility) != PermState::Granted {
        return Err("permissions missing".into());
    }
    if c.is_busy() {
        return Err("already recording".into());
    }
    hide_popover(&app);
    c.start();
    Ok(())
}

#[tauri::command]
fn stop_recording(c: State<'_, Controller>) {
    c.stop();
}

#[tauri::command]
fn copy_prompt(path: String) -> Result<(), String> {
    let text = prompt_for(std::path::Path::new(&path));
    arboard::Clipboard::new().and_then(|mut cb| cb.set_text(text)).map_err(|e| e.to_string())
}

#[tauri::command]
fn open_path(path: String) -> Result<(), String> {
    platform::open_path(&path).map_err(|e| e.to_string())
}

#[tauri::command]
fn reveal_path(path: String) -> Result<(), String> {
    platform::reveal_path(&path).map_err(|e| e.to_string())
}

#[tauri::command]
fn list_sessions(c: State<'_, Controller>) -> Vec<crate::sessions::SessionInfo> {
    c.refresh_sessions();
    c.snapshot().sessions
}

#[derive(serde::Serialize)]
struct Perms {
    screen: bool,
    ax: bool,
}

#[tauri::command]
fn permissions_state() -> Perms {
    let p = platform::permissions();
    Perms {
        screen: p.state(Permission::ScreenRecording) == PermState::Granted,
        ax: p.state(Permission::Accessibility) == PermState::Granted,
    }
}

fn which_perm(which: &str) -> Permission {
    if which == "ax" || which == "accessibility" {
        Permission::Accessibility
    } else {
        Permission::ScreenRecording
    }
}

#[tauri::command]
fn permissions_request(which: String) {
    let p = platform::permissions();
    p.request(which_perm(&which));
    p.open_settings(which_perm(&which));
}

#[tauri::command]
fn permissions_open_settings(which: String) {
    platform::permissions().open_settings(which_perm(&which));
}

/// Sizes the popover to its content (logical px), re-anchors it to the tray and optionally shows it.
#[tauri::command]
fn resize(app: AppHandle, width: f64, height: f64, show: Option<bool>) {
    let Some(win) = app.get_webview_window(POPOVER) else { return };
    let w = width.clamp(200.0, 800.0).round();
    let h = height.clamp(80.0, 1200.0).round();
    let _ = win.set_size(tauri::LogicalSize::new(w, h));
    let _ = win.as_ref().window().move_window(popover_position());
    if show.unwrap_or(false) {
        let _ = win.show();
        let _ = win.set_focus();
    }
}

#[tauri::command]
fn hide(app: AppHandle) {
    hide_popover(&app);
}

#[tauri::command]
fn quit(app: AppHandle) {
    app.exit(0);
}

// ---- app ----

pub fn run() -> anyhow::Result<()> {
    let controller = Controller::new();
    let ui = Arc::new(Ui { ready: AtomicBool::new(false), hidden_at: Mutex::new(None), recording_icon: AtomicBool::new(false) });

    let mut builder = tauri::Builder::default()
        .plugin(tauri_plugin_positioner::init())
        .manage(controller.clone())
        .manage(ui.clone())
        .invoke_handler(tauri::generate_handler![
            ready,
            get_state,
            get_options,
            set_options,
            start_recording,
            stop_recording,
            copy_prompt,
            open_path,
            reveal_path,
            list_sessions,
            permissions_state,
            permissions_request,
            permissions_open_settings,
            resize,
            hide,
            quit
        ])
        .on_window_event(|window, event| {
            if window.label() != POPOVER {
                return;
            }
            match event {
                WindowEvent::Focused(false) => {
                    let ui = window.state::<Arc<Ui>>();
                    *ui.hidden_at.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
                    let _ = window.hide();
                }
                WindowEvent::CloseRequested { api, .. } => {
                    api.prevent_close();
                    let _ = window.hide();
                }
                _ => {}
            }
        })
        .setup(move |app| {
            let handle = app.handle().clone();
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);

            // Push state changes to the page and swap the tray glyph.
            {
                let h = handle.clone();
                let c = controller.clone();
                controller.on_change(move |snap| {
                    let _ = h.emit("state", snap);
                    let recording = matches!(snap.phase, "recording" | "starting");
                    set_tray_icon(&h, recording);
                    if snap.phase == "done" {
                        show_popover(&h);
                    }
                    let _ = &c;
                });
            }

            // 1s tick for elapsed time.
            {
                let h = handle.clone();
                let c = controller.clone();
                std::thread::Builder::new()
                    .name("agent-snap-tick".into())
                    .spawn(move || loop {
                        std::thread::sleep(Duration::from_secs(1));
                        if c.phase() == Phase::Recording {
                            let _ = h.emit("state", c.snapshot());
                        }
                    })
                    .ok();
            }

            let open_item = MenuItemBuilder::with_id("open", "Open popover").build(app)?;
            let quit_item = MenuItemBuilder::with_id("quit", "Quit agent-snap").build(app)?;
            let menu = MenuBuilder::new(app).item(&open_item).separator().item(&quit_item).build()?;

            let c = controller.clone();
            TrayIconBuilder::with_id(TRAY)
                .icon(icons::camera())
                .icon_as_template(true)
                .tooltip("agent-snap")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, ev| match ev.id().as_ref() {
                    "open" => show_popover(app),
                    "quit" => app.exit(0),
                    _ => {}
                })
                .on_tray_icon_event(move |tray, event| {
                    let app = tray.app_handle();
                    tauri_plugin_positioner::on_tray_event(app, &event);
                    if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, rect, .. } = &event {
                        c.set_tray_rect(tray_rect_points(app, rect));
                        let ui = app.state::<Arc<Ui>>();
                        let visible = app.get_webview_window(POPOVER).and_then(|w| w.is_visible().ok()).unwrap_or(false);
                        let just_hidden = ui
                            .hidden_at
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .is_some_and(|t| t.elapsed() < Duration::from_millis(400));
                        if visible {
                            hide_popover(app);
                        } else if !just_hidden {
                            show_popover(app);
                        }
                    }
                })
                .build(app)?;
            log::info!("tray ready");
            Ok(())
        });

    // Keep running with no visible window.
    builder = builder.on_page_load(|_, _| {});
    let app = builder.build(tauri::generate_context!())?;
    app.run(|_app, event| {
        if let tauri::RunEvent::ExitRequested { api, code, .. } = event {
            if code.is_none() {
                api.prevent_exit();
            }
        }
    });
    Ok(())
}
