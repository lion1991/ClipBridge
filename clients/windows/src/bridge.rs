//! Owns the `clipbridge_core::Client` and bridges Windows clipboard events
//! to it. Counterpart of the Mac `BridgeCoordinator` and the Android
//! `ClipBridgeAccessibilityService`.

use std::{
    sync::{Arc, Mutex},
    thread::JoinHandle,
    time::{Duration, SystemTime, UNIX_EPOCH},
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

pub struct Bridge {
    client: Option<Arc<Client>>,
    listener: Option<Arc<BridgeListener>>,
    poller: Option<JoinHandle<()>>,
    poll_stop: Arc<Mutex<bool>>,
    last_sent: Arc<Mutex<Option<String>>>,
    last_received: Arc<Mutex<Option<String>>>,
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
            last_sent: Arc::new(Mutex::new(None)),
            last_received: Arc::new(Mutex::new(None)),
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
            last_received: self.last_received.clone(),
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
            let last_sent = self.last_sent.clone();
            let last_received = self.last_received.clone();
            let client = self.client.clone();
            let device_name = device_name();

            match ClipboardListener::start(move || {
                handle_clipboard_change(&client, &last_sent, &last_received, &device_name);
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
        let last_sent = self.last_sent.clone();
        let last_received = self.last_received.clone();
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
                            try_publish(&client, &last_sent, &last_received, &device_name, text);
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
    last_sent: &Arc<Mutex<Option<String>>>,
    last_received: &Arc<Mutex<Option<String>>>,
    device_name: &str,
) {
    let Some(text) = read_clipboard_text() else { return };
    if text.is_empty() {
        return;
    }
    try_publish(client, last_sent, last_received, device_name, text);
}

fn try_publish(
    client: &Option<Arc<Client>>,
    last_sent: &Arc<Mutex<Option<String>>>,
    last_received: &Arc<Mutex<Option<String>>>,
    device_name: &str,
    text: String,
) {
    let received = last_received.lock().ok().and_then(|g| g.clone());
    let sent = last_sent.lock().ok().and_then(|g| g.clone());
    if Some(&text) == received.as_ref() || Some(&text) == sent.as_ref() {
        return;
    }
    if let Ok(mut g) = last_sent.lock() {
        *g = Some(text.clone());
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
    last_received: Arc<Mutex<Option<String>>>,
}

impl ClipListener for BridgeListener {
    fn on_clip(&self, payload: ClipPayload) {
        if payload.kind != ClipKind::Text {
            return;
        }
        if let Ok(mut g) = self.last_received.lock() {
            *g = Some(payload.content.clone());
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
