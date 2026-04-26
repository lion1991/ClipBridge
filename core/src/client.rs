use std::sync::{Arc, Mutex, Once};
use std::thread::JoinHandle;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

use crate::crypto::{decrypt, encrypt, KEY_LEN};
use crate::protocol::{ClientMessage, ClipPayload, ServerMessage};

/// Rustls 0.23 refuses to pick a crypto provider on its own when more than
/// one (or none) is enabled across the dependency graph. We control this
/// explicitly: pure-Rust `ring` is enabled in Cargo.toml, and we also call
/// `install_default()` here so the panic can't sneak back in via a future
/// transitive dep that brings `aws-lc-rs`.
static CRYPTO_INIT: Once = Once::new();
fn ensure_crypto_provider() {
    CRYPTO_INIT.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

#[derive(Debug, Clone, uniffi::Enum)]
pub enum ConnectionState {
    Connecting,
    Connected,
    Disconnected,
    Error { message: String },
}

/// Foreign-implementable callback. UniFFI generates Swift/Kotlin protocols
/// that the host app implements; Rust calls into them on the worker thread.
#[uniffi::export(with_foreign)]
pub trait ClipListener: Send + Sync {
    fn on_clip(&self, payload: ClipPayload);
    fn on_state(&self, state: ConnectionState);
}

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("client already stopped")]
    Stopped,
    #[error("encrypt failed: {0}")]
    Encrypt(#[from] crate::crypto::CryptoError),
    #[error("serde: {0}")]
    Serde(#[from] serde_json::Error),
}

/// FFI-facing error variants — flat (no nested types) so UniFFI can bridge
/// them cleanly to Swift / Kotlin enums.
///
/// Field names here must not collide with `Throwable.message` on the Kotlin
/// side, so we use `reason` for the freeform internal-error string.
#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum FfiError {
    #[error("client stopped")]
    Stopped,
    #[error("group key must be {KEY_LEN} bytes, got {got}")]
    InvalidKey { got: u32 },
    #[error("internal: {reason}")]
    Internal { reason: String },
}

impl From<ClientError> for FfiError {
    fn from(e: ClientError) -> Self {
        match e {
            ClientError::Stopped => FfiError::Stopped,
            other => FfiError::Internal {
                reason: other.to_string(),
            },
        }
    }
}

enum Cmd {
    SendClip(ClipPayload),
    FetchRecent,
    Stop,
}

#[derive(uniffi::Object)]
pub struct Client {
    cmd_tx: mpsc::UnboundedSender<Cmd>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

#[uniffi::export]
impl Client {
    /// Spawn a background thread that connects to the relay, joins the group,
    /// and forwards encrypted clips. The provided `listener` is invoked for
    /// each decrypted incoming clip and on connection-state transitions.
    #[uniffi::constructor]
    pub fn new(
        relay_url: String,
        group_id: String,
        key: Vec<u8>,
        device_id: String,
        listener: Arc<dyn ClipListener>,
    ) -> Result<Arc<Self>, FfiError> {
        ensure_crypto_provider();
        if key.len() != KEY_LEN {
            return Err(FfiError::InvalidKey {
                got: key.len() as u32,
            });
        }
        let mut key_arr = [0u8; KEY_LEN];
        key_arr.copy_from_slice(&key);

        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<Cmd>();
        let thread = std::thread::Builder::new()
            .name("clipbridge-client".into())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("build runtime");
                rt.block_on(run(relay_url, group_id, key_arr, device_id, listener, cmd_rx));
            })
            .map_err(|e| FfiError::Internal {
                reason: format!("spawn thread: {e}"),
            })?;
        Ok(Arc::new(Self {
            cmd_tx,
            thread: Mutex::new(Some(thread)),
        }))
    }

    pub fn send_clip(&self, payload: ClipPayload) -> Result<(), FfiError> {
        self.cmd_tx
            .send(Cmd::SendClip(payload))
            .map_err(|_| FfiError::Stopped)
    }

    pub fn fetch_recent(&self) -> Result<(), FfiError> {
        self.cmd_tx
            .send(Cmd::FetchRecent)
            .map_err(|_| FfiError::Stopped)
    }

    /// Signal the worker thread to disconnect and wait for it to finish.
    pub fn stop(&self) {
        let _ = self.cmd_tx.send(Cmd::Stop);
        if let Some(t) = self.thread.lock().unwrap().take() {
            let _ = t.join();
        }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.cmd_tx.send(Cmd::Stop);
        if let Some(t) = self.thread.lock().unwrap().take() {
            let _ = t.join();
        }
    }
}

async fn run(
    relay_url: String,
    group_id: String,
    key: [u8; KEY_LEN],
    device_id: String,
    listener: Arc<dyn ClipListener>,
    mut cmd_rx: mpsc::UnboundedReceiver<Cmd>,
) {
    let mut backoff = Duration::from_secs(1);
    loop {
        listener.on_state(ConnectionState::Connecting);
        match session(&relay_url, &group_id, &key, &device_id, &listener, &mut cmd_rx).await {
            Ok(SessionExit::Stop) => {
                listener.on_state(ConnectionState::Disconnected);
                return;
            }
            Ok(SessionExit::Reconnect) => {
                listener.on_state(ConnectionState::Disconnected);
                backoff = Duration::from_secs(1);
            }
            Err(e) => {
                tracing::warn!(error = %e, "session error, will reconnect");
                listener.on_state(ConnectionState::Error {
                    message: e.to_string(),
                });
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(30));
            }
        }
    }
}

enum SessionExit {
    Stop,
    Reconnect,
}

async fn session(
    relay_url: &str,
    group_id: &str,
    key: &[u8; KEY_LEN],
    device_id: &str,
    listener: &Arc<dyn ClipListener>,
    cmd_rx: &mut mpsc::UnboundedReceiver<Cmd>,
) -> Result<SessionExit, Box<dyn std::error::Error + Send + Sync>> {
    // Accept any of ws:// wss:// http:// https:// — the user often pastes the
    // browser-style URL of the relay's reverse proxy.
    let normalized = relay_url.trim_end_matches('/');
    let normalized = match normalized {
        u if u.starts_with("https://") => format!("wss://{}", &u["https://".len()..]),
        u if u.starts_with("http://") => format!("ws://{}", &u["http://".len()..]),
        u => u.to_string(),
    };
    let url = format!("{normalized}/ws");
    tracing::info!(%url, "connecting");
    let (mut ws, _) = tokio_tungstenite::connect_async(&url).await?;

    let join = ClientMessage::Join {
        group_id: group_id.to_string(),
        device_id: device_id.to_string(),
    };
    ws.send(Message::Text(serde_json::to_string(&join)?.into()))
        .await?;

    // Pull whatever is still in the relay's recent-cache so devices that
    // joined late or just reconnected after a network blip don't miss the
    // last few clips. The relay caches up to 3 clips for 5 minutes.
    let fetch = ClientMessage::FetchRecent {
        group_id: group_id.to_string(),
    };
    ws.send(Message::Text(serde_json::to_string(&fetch)?.into()))
        .await?;

    listener.on_state(ConnectionState::Connected);

    // Heartbeat: ping every 30s, force-reconnect if no inbound frame for 60s.
    // The latter catches NAT idle timeouts and silent network switches (Wi-Fi
    // ↔ cellular) where TCP stays "open" but never delivers data again.
    const PING_EVERY: Duration = Duration::from_secs(30);
    const IDLE_TIMEOUT: Duration = Duration::from_secs(60);
    let mut ping_interval = tokio::time::interval(PING_EVERY);
    ping_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    ping_interval.tick().await; // consume the immediate first tick
    let mut last_seen = tokio::time::Instant::now();

    loop {
        let idle_deadline = last_seen + IDLE_TIMEOUT;
        tokio::select! {
            biased;
            _ = tokio::time::sleep_until(idle_deadline) => {
                tracing::warn!("idle for {IDLE_TIMEOUT:?}, reconnecting");
                return Ok(SessionExit::Reconnect);
            }
            _ = ping_interval.tick() => {
                ws.send(Message::Ping(Vec::new().into())).await?;
            }
            cmd = cmd_rx.recv() => {
                let Some(cmd) = cmd else {
                    return Ok(SessionExit::Stop);
                };
                match cmd {
                    Cmd::Stop => {
                        let _ = ws.close(None).await;
                        return Ok(SessionExit::Stop);
                    }
                    Cmd::SendClip(payload) => {
                        let plaintext = serde_json::to_vec(&payload)?;
                        let (ciphertext, nonce) = encrypt(key, &plaintext)?;
                        let msg = ClientMessage::Publish {
                            group_id: group_id.to_string(),
                            ciphertext,
                            nonce: nonce.to_vec(),
                            ts: payload.ts,
                        };
                        ws.send(Message::Text(serde_json::to_string(&msg)?.into())).await?;
                    }
                    Cmd::FetchRecent => {
                        let msg = ClientMessage::FetchRecent {
                            group_id: group_id.to_string(),
                        };
                        ws.send(Message::Text(serde_json::to_string(&msg)?.into())).await?;
                    }
                }
            }
            frame = ws.next() => {
                last_seen = tokio::time::Instant::now();
                let Some(frame) = frame else {
                    return Ok(SessionExit::Reconnect);
                };
                let frame = frame?;
                match frame {
                    Message::Text(t) => {
                        let parsed: ServerMessage = serde_json::from_str(&t)?;
                        handle_server(parsed, key, device_id, listener);
                    }
                    Message::Ping(p) => {
                        ws.send(Message::Pong(p)).await?;
                    }
                    Message::Close(_) => return Ok(SessionExit::Reconnect),
                    _ => {}
                }
            }
        }
    }
}

fn handle_server(
    msg: ServerMessage,
    key: &[u8; KEY_LEN],
    device_id: &str,
    listener: &Arc<dyn ClipListener>,
) {
    match msg {
        ServerMessage::Joined { .. } => {}
        ServerMessage::Clip {
            ciphertext, nonce, ..
        } => {
            if let Ok(plain) = decrypt(key, &nonce, &ciphertext) {
                if let Ok(payload) = serde_json::from_slice::<ClipPayload>(&plain) {
                    listener.on_clip(payload);
                }
            }
        }
        ServerMessage::Recent { clips } => {
            // Skip clips this device originally published — replaying them
            // would just re-write our own content to the local clipboard.
            // Cache is oldest-first, so iterating in order means the newest
            // clip wins on the OS clipboard.
            for c in clips {
                if c.sender_device_id == device_id {
                    continue;
                }
                if let Ok(plain) = decrypt(key, &c.nonce, &c.ciphertext) {
                    if let Ok(payload) = serde_json::from_slice::<ClipPayload>(&plain) {
                        listener.on_clip(payload);
                    }
                }
            }
        }
        ServerMessage::Error { reason } => {
            listener.on_state(ConnectionState::Error { message: reason });
        }
    }
}
