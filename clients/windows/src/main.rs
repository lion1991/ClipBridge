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
use tokio::sync::mpsc;

use crate::bridge::{Bridge, UiState};
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
    let bridge = Bridge::new(state_tx);

    tauri::Builder::default()
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
