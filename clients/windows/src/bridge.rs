//! Owns the `clipbridge_core::Client` and bridges Windows clipboard events
//! to it. Counterpart of the Mac `BridgeCoordinator` and the Android
//! `ClipBridgeAccessibilityService`.

use std::{
    sync::{Arc, Mutex},
    thread::JoinHandle,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use clipbridge_core::{
    Client, ClipKind, ClipListener, ClipPayload, ConnectionState,
};
use serde::Serialize;
use tokio::sync::mpsc;

#[cfg(windows)]
use crate::clipboard_listener::ClipboardListener;
use crate::pairing::{PairingConfig, Store};

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum UiState {
    Idle,
    Connecting,
    Connected,
    Disconnected,
    Error { message: String },
}

/// Window during which a clipboard change matching the most recent remote
/// write is treated as our own echo and skipped. Long enough that the OS
/// `WM_CLIPBOARDUPDATE` (or 500 ms poll tick) fires while we still know it's
/// an echo; short enough that the user can re-copy the same text on purpose.
const ECHO_WINDOW: Duration = Duration::from_secs(10);

pub struct Bridge {
    client: Option<Arc<Client>>,
    listener: Option<Arc<BridgeListener>>,
    poller: Option<JoinHandle<()>>,
    poll_stop: Arc<Mutex<bool>>,
    expected_echo: Arc<Mutex<Option<(String, Instant)>>>,
    state_tx: mpsc::UnboundedSender<UiState>,

    // Native clipboard listener (Windows only). Held here so its Drop
    // signals the worker thread to stop on `stop()`.
    #[cfg(windows)]
    native_listener: Option<ClipboardListener>,
}

impl Bridge {
    pub fn new(state_tx: mpsc::UnboundedSender<UiState>) -> Self {
        Self {
            client: None,
            listener: None,
            poller: None,
            poll_stop: Arc::new(Mutex::new(false)),
            expected_echo: Arc::new(Mutex::new(None)),
            state_tx,
            #[cfg(windows)]
            native_listener: None,
        }
    }

    pub fn start(&mut self, cfg: &PairingConfig) -> Result<(), String> {
        self.stop();
        let key = cfg.key_bytes().ok_or_else(|| "密钥无效".to_string())?;
        let device_id = Store::device_id();

        let listener = Arc::new(BridgeListener {
            state_tx: self.state_tx.clone(),
            expected_echo: self.expected_echo.clone(),
        });

        let client = Client::new(
            cfg.relay_url.clone(),
            cfg.group_id.clone(),
            key,
            device_id,
            listener.clone() as Arc<dyn ClipListener>,
        )
        .map_err(|e| format!("客户端启动失败:{e}"))?;

        self.listener = Some(listener);
        self.client = Some(client);
        self.spawn_clipboard_handler();
        Ok(())
    }

    pub fn stop(&mut self) {
        if let Some(client) = self.client.take() {
            client.stop();
        }
        self.listener = None;

        // Tear down whichever clipboard input we're using.
        #[cfg(windows)]
        {
            self.native_listener = None;
        }
        if let Ok(mut s) = self.poll_stop.lock() {
            *s = true;
        }
        if let Some(handle) = self.poller.take() {
            let _ = handle.join();
        }
        self.poll_stop = Arc::new(Mutex::new(false));

        let _ = self.state_tx.send(UiState::Idle);
    }

    /// Prefer the OS-native event-driven listener
    /// (`AddClipboardFormatListener`). If creating the message-only window
    /// or registering the listener fails for any reason, fall back to the
    /// 500 ms polling loop so the app still works.
    fn spawn_clipboard_handler(&mut self) {
        #[cfg(windows)]
        {
            let expected_echo = self.expected_echo.clone();
            let client = self.client.clone();
            let device_name = device_name();

            match ClipboardListener::start(move || {
                handle_clipboard_change(&client, &expected_echo, &device_name);
            }) {
                Ok(l) => {
                    self.native_listener = Some(l);
                    return;
                }
                Err(e) => {
                    eprintln!(
                        "[clipbridge] native clipboard listener unavailable, falling back to polling: {e}"
                    );
                }
            }
        }
        self.spawn_poller();
    }

    fn spawn_poller(&mut self) {
        let stop_flag = self.poll_stop.clone();
        let expected_echo = self.expected_echo.clone();
        let client = self.client.clone();
        let device_name = device_name();

        let handle = std::thread::Builder::new()
            .name("clipbridge-clipboard-poller".into())
            .spawn(move || {
                let mut last_seen: Option<String> = None;
                loop {
                    if stop_flag.lock().map(|s| *s).unwrap_or(true) {
                        break;
                    }
                    if let Some(text) = read_clipboard_text() {
                        if !text.is_empty() && Some(&text) != last_seen.as_ref() {
                            last_seen = Some(text.clone());
                            try_publish(&client, &expected_echo, &device_name, text);
                        }
                    }
                    std::thread::sleep(Duration::from_millis(500));
                }
            })
            .expect("spawn poller");
        self.poller = Some(handle);
    }
}

/// Called from `WM_CLIPBOARDUPDATE` (or, in fallback, from the poller).
fn handle_clipboard_change(
    client: &Option<Arc<Client>>,
    expected_echo: &Arc<Mutex<Option<(String, Instant)>>>,
    device_name: &str,
) {
    let Some(text) = read_clipboard_text() else { return };
    if text.is_empty() {
        return;
    }
    try_publish(client, expected_echo, device_name, text);
}

fn try_publish(
    client: &Option<Arc<Client>>,
    expected_echo: &Arc<Mutex<Option<(String, Instant)>>>,
    device_name: &str,
    text: String,
) {
    // If this change matches the most recent remote write (within the echo
    // window), it's our own `arboard` set firing the listener — skip without
    // republishing. Outside the window, treat it as a real user copy so they
    // can re-share the same text on purpose.
    if let Ok(e) = expected_echo.lock() {
        if let Some((s, t)) = e.as_ref() {
            if s == &text && t.elapsed() < ECHO_WINDOW {
                return;
            }
        }
    }

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let payload = ClipPayload {
        kind: ClipKind::Text,
        content: text,
        device_name: device_name.to_string(),
        ts: now,
    };
    if let Some(c) = client.as_ref() {
        let _ = c.send_clip(payload);
    }
}

fn read_clipboard_text() -> Option<String> {
    // arboard sometimes fails transiently when another app holds the
    // clipboard open; treat that as "no change yet" and let the next event /
    // poll succeed.
    let mut clipboard = arboard::Clipboard::new().ok()?;
    clipboard.get_text().ok()
}

fn device_name() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "Windows".to_string())
}

struct BridgeListener {
    state_tx: mpsc::UnboundedSender<UiState>,
    expected_echo: Arc<Mutex<Option<(String, Instant)>>>,
}

impl ClipListener for BridgeListener {
    fn on_clip(&self, payload: ClipPayload) {
        if payload.kind != ClipKind::Text {
            return;
        }
        // Mark this content as "expected echo" *before* writing so the
        // WM_CLIPBOARDUPDATE callback (which fires after `set_text`) can
        // recognise its own write and skip republishing.
        if let Ok(mut g) = self.expected_echo.lock() {
            *g = Some((payload.content.clone(), Instant::now()));
        }
        if let Ok(mut clipboard) = arboard::Clipboard::new() {
            let _ = clipboard.set_text(&payload.content);
        }
    }

    fn on_state(&self, state: ConnectionState) {
        let mapped = match state {
            ConnectionState::Connecting => UiState::Connecting,
            ConnectionState::Connected => UiState::Connected,
            ConnectionState::Disconnected => UiState::Disconnected,
            ConnectionState::Error { message } => UiState::Error { message },
        };
        let _ = self.state_tx.send(mapped);
    }
}
