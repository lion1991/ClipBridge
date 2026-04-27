// Hide console window in release builds — this is a tray-only app.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod bridge;
#[cfg(windows)]
mod clipboard_listener;
mod pairing;

use std::sync::Mutex;

use qrcode::{render::svg, QrCode};
use serde::Serialize;
use tauri::{
    image::Image,
    menu::{Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    AppHandle, Emitter, Manager, RunEvent, State, WindowEvent,
};
use tauri_plugin_dialog::{DialogExt, FilePath};
use tokio::sync::mpsc;

use crate::bridge::{Bridge, ImageHistoryEntry, UiState};
use crate::pairing::{PairingConfig, Store};

struct AppState {
    bridge: Mutex<Bridge>,
    last_state: Mutex<UiState>,
}

#[derive(Serialize)]
struct PairingDto {
    json: String,
    qr_svg: Option<String>,
}

fn main() {
    let (state_tx, mut state_rx) = mpsc::unbounded_channel::<UiState>();
    let (image_tx, mut image_rx) = mpsc::unbounded_channel::<ImageHistoryEntry>();
    let bridge = Bridge::new(state_tx, image_tx);

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(
            tauri_plugin_autostart::init(
                tauri_plugin_autostart::MacosLauncher::LaunchAgent,
                None,
            ),
        )
        .manage(AppState {
            bridge: Mutex::new(bridge),
            last_state: Mutex::new(UiState::Idle),
        })
        .invoke_handler(tauri::generate_handler![
            cmd_load_pairing,
            cmd_save_pairing,
            cmd_clear_pairing,
            cmd_generate_pairing,
            cmd_current_state,
            cmd_show_window,
            cmd_quit,
            cmd_recent_images,
            cmd_send_image_bytes,
            cmd_save_image_to_file,
        ])
        .setup(move |app| {
            // Tray icon + menu.
            let show_item = MenuItem::with_id(app, "show", "打开配对窗口…", true, None::<&str>)?;
            let quit_item = MenuItem::with_id(app, "quit", "退出 ClipBridge", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&show_item, &quit_item])?;

            let _tray = TrayIconBuilder::with_id("main")
                .icon(app.default_window_icon().unwrap().clone())
                .tooltip("ClipBridge")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "show" => show_main_window(app),
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
                        show_main_window(tray.app_handle());
                    }
                })
                .build(app)?;

            // Pump UiState updates from the bridge into the frontend.
            let app_handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                while let Some(state) = state_rx.recv().await {
                    if let Some(s) = app_handle.try_state::<AppState>() {
                        if let Ok(mut last) = s.last_state.lock() {
                            *last = state.clone();
                        }
                    }
                    let _ = app_handle.emit("connection-state", &state);
                }
            });

            // Same for image history events — the frontend appends each
            // entry to its in-memory list and re-renders the image tab.
            let app_handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                while let Some(entry) = image_rx.recv().await {
                    let _ = app_handle.emit("image-event", &entry);
                }
            });

            // Auto-start the bridge if a config is already saved.
            if let Some(cfg) = Store::load() {
                if cfg.is_valid() {
                    if let Some(s) = app.try_state::<AppState>() {
                        let _ = s.bridge.lock().map(|mut b| b.start(&cfg));
                    }
                }
            } else {
                show_main_window(app.handle());
            }

            Ok(())
        })
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                // Closing the X button just hides the window — the app keeps
                // running in the tray. User has to choose "退出" to actually
                // quit.
                let _ = window.hide();
                api.prevent_close();
            }
        })
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|_app, event| {
            if let RunEvent::ExitRequested { api, .. } = event {
                // Don't auto-quit when last window closes.
                api.prevent_exit();
            }
        });
}

fn show_main_window(app: &AppHandle) {
    if let Some(win) = app.get_webview_window("main") {
        let _ = win.show();
        let _ = win.set_focus();
    }
}

fn quit_app(app: &AppHandle) {
    if let Some(s) = app.try_state::<AppState>() {
        let _ = s.bridge.lock().map(|mut b| b.stop());
    }
    app.exit(0);
}

// ─── Commands ───

#[tauri::command]
fn cmd_load_pairing() -> Option<PairingDto> {
    Store::load().map(|cfg| {
        let json = serde_json::to_string_pretty(&cfg).unwrap_or_default();
        PairingDto {
            qr_svg: render_qr(&json),
            json,
        }
    })
}

#[tauri::command]
fn cmd_save_pairing(json: String, state: State<'_, AppState>) -> Result<PairingDto, String> {
    let cfg: PairingConfig =
        serde_json::from_str(&json).map_err(|e| format!("配对信息无效:{e}"))?;
    if !cfg.is_valid() {
        return Err("配对信息无效:缺少必要字段或密钥长度不符".to_string());
    }
    Store::save(&cfg).map_err(|e| format!("保存失败:{e}"))?;
    state
        .bridge
        .lock()
        .map_err(|e| e.to_string())?
        .start(&cfg)?;
    let pretty = serde_json::to_string_pretty(&cfg).unwrap_or(json);
    Ok(PairingDto {
        qr_svg: render_qr(&pretty),
        json: pretty,
    })
}

#[tauri::command]
fn cmd_clear_pairing(state: State<'_, AppState>) -> Result<(), String> {
    Store::clear();
    state.bridge.lock().map_err(|e| e.to_string())?.stop();
    Ok(())
}

#[tauri::command]
fn cmd_generate_pairing(state: State<'_, AppState>) -> Result<PairingDto, String> {
    let cfg = PairingConfig::make_new();
    Store::save(&cfg).map_err(|e| format!("保存失败:{e}"))?;
    state
        .bridge
        .lock()
        .map_err(|e| e.to_string())?
        .start(&cfg)?;
    let json = serde_json::to_string_pretty(&cfg).unwrap_or_default();
    Ok(PairingDto {
        qr_svg: render_qr(&json),
        json,
    })
}

#[tauri::command]
fn cmd_current_state(state: State<'_, AppState>) -> UiState {
    state
        .last_state
        .lock()
        .map(|s| s.clone())
        .unwrap_or(UiState::Idle)
}

#[tauri::command]
fn cmd_show_window(app: AppHandle) {
    show_main_window(&app);
}

#[tauri::command]
fn cmd_quit(app: AppHandle) {
    quit_app(&app);
}

/// Snapshot of the in-process image history. Used on window-open to
/// repaint after a hide/show — `image-event` covers live updates.
#[tauri::command]
fn cmd_recent_images(state: State<'_, AppState>) -> Vec<ImageHistoryEntry> {
    state
        .bridge
        .lock()
        .map(|b| b.recent_images())
        .unwrap_or_default()
}

/// Picker-driven send: frontend reads a File via FileReader, ships the
/// bytes here. Returns the new history entry so the UI can render
/// optimistically before the listener fires.
#[tauri::command]
fn cmd_send_image_bytes(
    bytes: Vec<u8>,
    state: State<'_, AppState>,
) -> Result<ImageHistoryEntry, String> {
    state
        .bridge
        .lock()
        .map_err(|e| e.to_string())?
        .send_image_bytes(bytes)
}

/// Save an image from history to a user-chosen file. The frontend passes
/// the entry id and a default file name; we open a Save dialog and write
/// the original PNG bytes verbatim (no re-encoding).
#[tauri::command]
async fn cmd_save_image_to_file(
    id: String,
    default_name: String,
    app: AppHandle,
) -> Result<Option<String>, String> {
    let bytes = {
        let state = app
            .try_state::<AppState>()
            .ok_or_else(|| "app state missing".to_string())?;
        let bridge = state.bridge.lock().map_err(|e| e.to_string())?;
        bridge
            .image_bytes_for(&id)
            .ok_or_else(|| "图片字节已过期 (历史已清)".to_string())?
    };

    let (tx, rx) = std::sync::mpsc::channel::<Option<FilePath>>();
    app.dialog()
        .file()
        .set_title("保存图片")
        .set_file_name(&default_name)
        .add_filter("PNG", &["png"])
        .save_file(move |maybe_path| {
            let _ = tx.send(maybe_path);
        });
    let chosen = rx.recv().map_err(|e| format!("dialog: {e}"))?;
    let Some(file_path) = chosen else { return Ok(None) };
    let path = file_path
        .as_path()
        .ok_or_else(|| "save dialog returned non-filesystem path".to_string())?
        .to_path_buf();
    std::fs::write(&path, &bytes).map_err(|e| format!("写入失败:{e}"))?;
    Ok(Some(path.display().to_string()))
}

fn render_qr(text: &str) -> Option<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    let code = QrCode::new(trimmed.as_bytes()).ok()?;
    let svg_str = code
        .render::<svg::Color>()
        .min_dimensions(240, 240)
        .quiet_zone(true)
        .dark_color(svg::Color("#111111"))
        .light_color(svg::Color("#ffffff"))
        .build();
    Some(svg_str)
}

// Suppress an unused-import warning from `Image` on some Tauri feature combos.
#[allow(dead_code)]
fn _force_link(_: Image) {}
