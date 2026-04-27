use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Once};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

use crate::blob::{BlobClient, BlobError};
use crate::crypto::{decrypt, encrypt, sha256_hex, KEY_LEN, NONCE_LEN};
use crate::lan::{IncomingLanClip, LanNode};
use crate::protocol::{ClientMessage, ClipKind, ClipPayload, ImageMeta, ServerMessage};

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
    #[error("blob: {0}")]
    Blob(#[from] crate::blob::BlobError),
    #[error("invalid image meta: {0}")]
    InvalidImageMeta(String),
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
    #[error("blob not found on relay (expired or never uploaded)")]
    BlobNotFound,
    #[error("blob exceeds relay size limit")]
    BlobTooLarge,
    #[error("internal: {reason}")]
    Internal { reason: String },
}

impl From<ClientError> for FfiError {
    fn from(e: ClientError) -> Self {
        match e {
            ClientError::Stopped => FfiError::Stopped,
            ClientError::Blob(BlobError::NotFound) => FfiError::BlobNotFound,
            ClientError::Blob(BlobError::TooLarge) => FfiError::BlobTooLarge,
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
    /// Shared between FFI methods (send_image / fetch_image, called on the
    /// host's calling thread) and the worker thread (which only needs key
    /// + group_id for WS framing). Cloning is cheap — Arc bumps a counter.
    shared: Arc<Shared>,
}

struct Shared {
    key: [u8; KEY_LEN],
    group_id: String,
    blob: BlobClient,
    /// Number of LAN peers currently in a fully-handshaked session.
    /// Updated by the per-peer task in `lan.rs`; read here for the
    /// `lan_peer_count()` getter the UI polls.
    lan_peers: Arc<AtomicUsize>,
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

        let blob = BlobClient::new(&relay_url).map_err(|e| FfiError::Internal {
            reason: format!("blob client: {e}"),
        })?;
        let lan_peers = Arc::new(AtomicUsize::new(0));
        let shared = Arc::new(Shared {
            key: key_arr,
            group_id: group_id.clone(),
            blob,
            lan_peers: lan_peers.clone(),
        });

        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<Cmd>();
        let worker_relay = relay_url.clone();
        let worker_group = group_id.clone();
        let thread = std::thread::Builder::new()
            .name("clipbridge-client".into())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("build runtime");
                rt.block_on(run(
                    worker_relay,
                    worker_group,
                    key_arr,
                    device_id,
                    listener,
                    cmd_rx,
                    lan_peers,
                ));
            })
            .map_err(|e| FfiError::Internal {
                reason: format!("spawn thread: {e}"),
            })?;
        Ok(Arc::new(Self {
            cmd_tx,
            thread: Mutex::new(Some(thread)),
            shared,
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

    /// Encrypt `image_bytes`, upload the ciphertext to the relay's blob
    /// endpoint, then queue a `Publish` carrying the resulting `ImageMeta`.
    /// Blocks the calling thread for the duration of the HTTP upload —
    /// hosts should call this from a background thread / coroutine.
    pub fn send_image(
        &self,
        image_bytes: Vec<u8>,
        mime_type: String,
        width: u32,
        height: u32,
        device_name: String,
        ts: u64,
    ) -> Result<(), FfiError> {
        let plaintext_len = image_bytes.len() as u64;
        let (ciphertext, nonce) = encrypt(&self.shared.key, &image_bytes).map_err(ClientError::from)?;
        let sha = sha256_hex(&ciphertext);
        self.shared
            .blob
            .upload(&self.shared.group_id, &sha, ciphertext)
            .map_err(ClientError::from)?;
        let meta = ImageMeta {
            mime_type,
            width,
            height,
            size_bytes: plaintext_len,
            sha256_hex: sha,
            nonce_b64: B64.encode(nonce),
        };
        let payload = ClipPayload {
            kind: ClipKind::Image,
            content: String::new(),
            device_name,
            ts,
            image: Some(meta),
        };
        self.cmd_tx
            .send(Cmd::SendClip(payload))
            .map_err(|_| FfiError::Stopped)
    }

    /// Download the ciphertext for `meta` from the relay and decrypt it
    /// with the group key. Blocking; safe to call from a background thread.
    pub fn fetch_image(&self, meta: ImageMeta) -> Result<Vec<u8>, FfiError> {
        let nonce = B64
            .decode(meta.nonce_b64.as_bytes())
            .map_err(|e| ClientError::InvalidImageMeta(format!("nonce_b64: {e}")))?;
        if nonce.len() != NONCE_LEN {
            return Err(ClientError::InvalidImageMeta(format!(
                "nonce length {} != {NONCE_LEN}",
                nonce.len()
            ))
            .into());
        }
        let ciphertext = self
            .shared
            .blob
            .download(&self.shared.group_id, &meta.sha256_hex)
            .map_err(ClientError::from)?;
        let plain = decrypt(&self.shared.key, &nonce, &ciphertext).map_err(ClientError::from)?;
        Ok(plain)
    }

    /// Number of LAN peers currently in a fully-handshaked session. The
    /// UI polls this every couple of seconds to render a transport badge
    /// ("LAN: 2 / 仅中继"). 0 means LAN is up but no one's discovered us
    /// yet, *or* the LAN transport failed to start (multicast blocked,
    /// permission denied) and we're relay-only.
    pub fn lan_peer_count(&self) -> u32 {
        self.shared.lan_peers.load(Ordering::Relaxed) as u32
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
    lan_peers: Arc<AtomicUsize>,
) {
    // Receive-side dedup. The same (sender_device_id, ts) pair may arrive
    // both from the relay WS and from a LAN peer; whichever lands first
    // wins, the other is silently dropped. Shared via Arc<Mutex<>> because
    // the LAN inbound task and the WS session task both insert into it.
    let dedup = Arc::new(Mutex::new(DedupCache::new(128, Duration::from_secs(10))));

    // Try to bring up the LAN node. Failure here (e.g. mDNS daemon
    // refused, port bind failed in a sandboxed test env) is non-fatal:
    // we just degrade to relay-only and log it.
    let (lan_in_tx, lan_in_rx) = mpsc::unbounded_channel::<IncomingLanClip>();
    let lan = match LanNode::spawn(group_id.clone(), device_id.clone(), key, lan_in_tx, lan_peers).await {
        Ok(n) => {
            tracing::info!("lan transport up");
            Some(Arc::new(n))
        }
        Err(e) => {
            tracing::warn!(?e, "lan transport disabled");
            None
        }
    };

    // Drain LAN inbound into the listener regardless of WS state, so a
    // brief relay outage doesn't also stall LAN delivery.
    {
        let listener = listener.clone();
        let dedup = dedup.clone();
        let self_id = device_id.clone();
        tokio::spawn(async move {
            let mut rx = lan_in_rx;
            while let Some(c) = rx.recv().await {
                if c.sender_device_id == self_id {
                    continue;
                }
                if !dedup.lock().unwrap().insert(&c.sender_device_id, c.ts) {
                    continue;
                }
                listener.on_clip(c.payload);
            }
        });
    }

    let mut backoff = Duration::from_secs(1);
    loop {
        listener.on_state(ConnectionState::Connecting);
        match session(
            &relay_url,
            &group_id,
            &key,
            &device_id,
            &listener,
            &mut cmd_rx,
            lan.as_deref(),
            &dedup,
        )
        .await
        {
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
    lan: Option<&LanNode>,
    dedup: &Arc<Mutex<DedupCache>>,
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
                        // Fire to LAN peers first — broadcast is non-blocking
                        // and lets us at least reach co-LAN devices even if
                        // the WS write below stalls.
                        if let Some(lan) = lan {
                            lan.broadcast(device_id.to_string(), payload.ts, payload.clone());
                        }
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
                        handle_server(parsed, key, device_id, listener, dedup);
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
    dedup: &Arc<Mutex<DedupCache>>,
) {
    match msg {
        ServerMessage::Joined { .. } => {}
        ServerMessage::Clip {
            ciphertext,
            nonce,
            ts,
            sender_device_id,
        } => {
            if sender_device_id == device_id {
                return; // shouldn't happen, but be defensive
            }
            if !dedup.lock().unwrap().insert(&sender_device_id, ts) {
                return; // LAN beat the relay (or vice versa)
            }
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
                if !dedup.lock().unwrap().insert(&c.sender_device_id, c.ts) {
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

/// Tiny LRU-ish cache to drop duplicate `(sender_device_id, ts)` deliveries.
/// Entries past `ttl` are considered stale and ignored on insert (so a
/// stale match doesn't reject a brand-new clip that happens to collide).
/// Bounded at `capacity` — we evict the oldest entry when full.
pub(crate) struct DedupCache {
    entries: VecDeque<(String, u64, Instant)>,
    capacity: usize,
    ttl: Duration,
}

impl DedupCache {
    pub fn new(capacity: usize, ttl: Duration) -> Self {
        Self {
            entries: VecDeque::with_capacity(capacity),
            capacity,
            ttl,
        }
    }

    /// Returns `true` if the key was inserted (i.e. *not* a recent duplicate).
    pub fn insert(&mut self, sender: &str, ts: u64) -> bool {
        let now = Instant::now();
        // Drop expired entries from the front. They're roughly time-
        // ordered (insert order) so this is cheap in the common case.
        while let Some((_, _, t)) = self.entries.front() {
            if now.duration_since(*t) > self.ttl {
                self.entries.pop_front();
            } else {
                break;
            }
        }
        if self
            .entries
            .iter()
            .any(|(s, t, _)| s == sender && *t == ts)
        {
            return false;
        }
        if self.entries.len() == self.capacity {
            self.entries.pop_front();
        }
        self.entries.push_back((sender.to_string(), ts, now));
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dedup_rejects_repeats_within_ttl() {
        let mut d = DedupCache::new(8, Duration::from_secs(5));
        assert!(d.insert("A", 1));
        assert!(!d.insert("A", 1));
        assert!(d.insert("A", 2));
        assert!(d.insert("B", 1));
    }

    #[test]
    fn dedup_evicts_oldest_when_full() {
        let mut d = DedupCache::new(2, Duration::from_secs(60));
        assert!(d.insert("A", 1));
        assert!(d.insert("A", 2));
        assert!(d.insert("A", 3)); // evicts (A,1)
        assert!(d.insert("A", 1)); // re-insert allowed because evicted
    }
}
