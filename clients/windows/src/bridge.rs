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
        self.spawn_poller();
        Ok(())
    }

    pub fn stop(&mut self) {
        if let Some(client) = self.client.take() {
            client.stop();
        }
        self.listener = None;
        if let Ok(mut s) = self.poll_stop.lock() {
            *s = true;
        }
        if let Some(handle) = self.poller.take() {
            let _ = handle.join();
        }
        // Reset for the next start.
        self.poll_stop = Arc::new(Mutex::new(false));
        let _ = self.state_tx.send(UiState::Idle);
    }

    fn spawn_poller(&mut self) {
        let stop_flag = self.poll_stop.clone();
        let last_sent = self.last_sent.clone();
        let last_received = self.last_received.clone();
        let client_for_poller = self.client.clone();

        let device_name = sys_info::hostname()
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "Windows".to_string());

        // A single thread polls the clipboard ~2x per second. arboard returns
        // the current text; we compare against the last seen value to avoid
        // republishing our own writes.
        let handle = std::thread::Builder::new()
            .name("clipbridge-clipboard-poller".into())
            .spawn(move || {
                let mut clipboard = match arboard::Clipboard::new() {
                    Ok(c) => c,
                    Err(e) => {
                        tracing_eprintln(format!("clipboard init failed: {e}"));
                        return;
                    }
                };
                let mut last_seen: Option<String> = None;
                loop {
                    if stop_flag.lock().map(|s| *s).unwrap_or(true) {
                        break;
                    }
                    let text = clipboard.get_text().ok();
                    if let Some(ref t) = text {
                        if !t.is_empty() && Some(t) != last_seen.as_ref() {
                            last_seen = Some(t.clone());

                            let received = last_received.lock().ok().and_then(|g| g.clone());
                            let sent = last_sent.lock().ok().and_then(|g| g.clone());
                            if Some(t) == received.as_ref() || Some(t) == sent.as_ref() {
                                continue;
                            }
                            if let Ok(mut g) = last_sent.lock() {
                                *g = Some(t.clone());
                            }

                            let now = SystemTime::now()
                                .duration_since(UNIX_EPOCH)
                                .map(|d| d.as_millis() as u64)
                                .unwrap_or(0);
                            let payload = ClipPayload {
                                kind: ClipKind::Text,
                                content: t.clone(),
                                device_name: device_name.clone(),
                                ts: now,
                            };
                            if let Some(c) = client_for_poller.as_ref() {
                                let _ = c.send_clip(payload);
                            }
                        }
                    }
                    std::thread::sleep(Duration::from_millis(500));
                }
            })
            .expect("spawn poller");
        self.poller = Some(handle);
    }
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
        // Mark as "received" so the polling thread doesn't re-publish our
        // own write back to the relay, then push to the system clipboard.
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

mod sys_info {
    pub fn hostname() -> std::io::Result<String> {
        let host = std::env::var("COMPUTERNAME")
            .or_else(|_| std::env::var("HOSTNAME"))
            .unwrap_or_else(|_| "Windows".to_string());
        Ok(host)
    }
}

fn tracing_eprintln(msg: String) {
    eprintln!("[clipbridge] {msg}");
}
