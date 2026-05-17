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
use crate::file_transfer::{
    FileTransferConfig, ReceivedFile, ReceivedFileRecord, SendFileRequest, SentFile,
};
use crate::lan::{
    candidates_for_peer, lan_fetch_blob, lan_send_file, peer_records, unique_peer_names, BlobCache,
    IncomingLanClip, LanNode, LanNodeConfig, LanPeerRecord, PeerAddrs, PeerRegistry,
    SharedBlobCache, SharedFileReceiveDir,
};
use crate::protocol::{
    ClientMessage, ClipKind, ClipPayload, ImageMeta, LanCandidate, ServerMessage,
};

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
    #[error("no LAN candidates for peer: {device_id}")]
    NoLanPeer { device_id: String },
    #[error("file transfer failed: {reason}")]
    FileTransfer { reason: String },
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
    device_id: String,
    device_name: String,
    blob: BlobClient,
    /// Ciphertext we've recently sent or fetched, content-addressed by
    /// sha256(ciphertext). Lets `fetch_image` pull image bytes straight
    /// from a LAN peer instead of round-tripping the relay, and lets this
    /// device re-serve to other peers. Shared with the LAN node so its
    /// blob-serve task reads the same store.
    blob_cache: SharedBlobCache,
    /// mDNS instance → dialable addresses for every resolved group peer,
    /// populated by the LAN node from mDNS. `fetch_image` iterates its
    /// values to find a peer to pull image bytes from.
    peer_addrs: PeerAddrs,
    /// Number of LAN peers currently in a fully-handshaked session.
    /// Updated by the per-peer task in `lan.rs`; read here for the
    /// `lan_peer_count()` getter the UI polls.
    lan_peers: Arc<AtomicUsize>,
    /// Live snapshot of connected peers' device names. Same source of
    /// truth as `lan_peers` (count == registry.len()), kept in parallel
    /// so the FFI getter can hand back a `Vec<String>` for the UI to
    /// render "局域网: Mac, iPhone".
    lan_peer_names: PeerRegistry,
    file_receive_dir: SharedFileReceiveDir,
    received_files: Arc<Mutex<VecDeque<ReceivedFileRecord>>>,
}

#[uniffi::export]
impl Client {
    /// Spawn a background thread that connects to the relay, joins the group,
    /// and forwards encrypted clips. The provided `listener` is invoked for
    /// each decrypted incoming clip and on connection-state transitions.
    ///
    /// `device_name` is the human-readable label shown to other peers in
    /// LAN status badges (and already what we attach to outgoing clip
    /// payloads). Pass the same string the platform uses for clip
    /// `device_name` so peers render us with a consistent identity.
    #[uniffi::constructor]
    pub fn new(
        relay_url: String,
        group_id: String,
        key: Vec<u8>,
        device_id: String,
        device_name: String,
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
        let lan_peer_names: PeerRegistry =
            Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
        let blob_cache = BlobCache::new();
        let peer_addrs: PeerAddrs =
            Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
        let file_receive_dir: SharedFileReceiveDir = Arc::new(std::sync::Mutex::new(None));
        let received_files = Arc::new(Mutex::new(VecDeque::new()));
        let shared = Arc::new(Shared {
            key: key_arr,
            group_id: group_id.clone(),
            device_id: device_id.clone(),
            device_name: device_name.clone(),
            blob,
            lan_peers: lan_peers.clone(),
            lan_peer_names: lan_peer_names.clone(),
            blob_cache: blob_cache.clone(),
            peer_addrs: peer_addrs.clone(),
            file_receive_dir: file_receive_dir.clone(),
            received_files: received_files.clone(),
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
                rt.block_on(run(ClientRun {
                    relay_url: worker_relay,
                    group_id: worker_group,
                    key: key_arr,
                    device_id,
                    device_name,
                    listener,
                    cmd_rx,
                    lan_peers,
                    lan_peer_names,
                    blob_cache,
                    peer_addrs,
                    file_receive_dir,
                    received_files,
                }));
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
        let (ciphertext, nonce) =
            encrypt(&self.shared.key, &image_bytes).map_err(ClientError::from)?;
        let sha = sha256_hex(&ciphertext);
        // Seed the LAN cache so co-LAN peers can pull these bytes from us
        // directly instead of round-tripping the relay. We still upload to
        // the relay below — non-LAN peers, late joiners and the relay's
        // recent-cache all depend on it.
        self.shared
            .blob_cache
            .insert(sha.clone(), Arc::new(ciphertext.clone()));
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
        // LAN-first: if a peer is in a live session, try pulling the bytes
        // straight off the local network. Address comes from the mDNS-fed
        // map; we attempt peers one at a time so a `BlobMiss` from one
        // still lets us try the next. Any failure falls through to the
        // relay path below — the optimization is invisible to callers.
        if self.shared.lan_peers.load(Ordering::Relaxed) > 0 {
            // Ciphertext ≈ plaintext + AEAD tag; cap with slack so a
            // buggy/hostile peer can't stream us unbounded memory. sha256
            // is verified before we trust the bytes regardless.
            let max_bytes = (meta.size_bytes as usize).saturating_add(1024);
            let peers: Vec<Vec<std::net::SocketAddr>> = self
                .shared
                .peer_addrs
                .lock()
                .map(|g| g.values().map(|entry| entry.candidates.clone()).collect())
                .unwrap_or_default();
            for candidates in peers {
                let Some(ciphertext) = lan_fetch_blob(
                    candidates,
                    self.shared.key,
                    self.shared.device_id.clone(),
                    self.shared.device_name.clone(),
                    meta.sha256_hex.clone(),
                    max_bytes,
                ) else {
                    continue;
                };
                if sha256_hex(&ciphertext) != meta.sha256_hex {
                    continue; // corrupted/mismatched — try next peer, then relay
                }
                if let Ok(plain) = decrypt(&self.shared.key, &nonce, &ciphertext) {
                    // Re-seed our cache so we can serve the next peer too.
                    self.shared
                        .blob_cache
                        .insert(meta.sha256_hex.clone(), Arc::new(ciphertext));
                    return Ok(plain);
                }
            }
        }

        let ciphertext = self
            .shared
            .blob
            .download(&self.shared.group_id, &meta.sha256_hex)
            .map_err(ClientError::from)?;
        let plain = decrypt(&self.shared.key, &nonce, &ciphertext).map_err(ClientError::from)?;
        // Seed the cache from the relay download too, so a device that
        // only had relay reach can still hand the bytes to LAN peers.
        self.shared
            .blob_cache
            .insert(meta.sha256_hex.clone(), Arc::new(ciphertext));
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

    /// Snapshot of currently-connected peers' device names, one entry per
    /// logical peer (deduped on device_id so a reconnect transient or an
    /// iOS app+keyboard pair doesn't surface twice). Order is not stable
    /// across calls (HashMap iteration). Empty vec = no LAN peers
    /// (relay-only). UI uses this to render the actual peer list, which
    /// makes mesh asymmetry obvious — if Mac shows ["Android"] and
    /// Android shows ["Mac", "iPhone"], the missing edge is Mac↔iPhone.
    pub fn lan_peers(&self) -> Vec<String> {
        unique_peer_names(&self.shared.lan_peer_names)
    }

    pub fn lan_peer_records(&self) -> Vec<LanPeerRecord> {
        peer_records(&self.shared.lan_peer_names, &self.shared.peer_addrs)
    }

    pub fn send_file_to_peer(
        &self,
        target_device_id: String,
        source_path: String,
        mime_type: Option<String>,
    ) -> Result<SentFile, FfiError> {
        let candidates = candidates_for_peer(&self.shared.peer_addrs, &target_device_id);
        if candidates.is_empty() {
            return Err(FfiError::NoLanPeer {
                device_id: target_device_id,
            });
        }
        lan_send_file(
            candidates,
            self.shared.key,
            SendFileRequest {
                source_device_id: self.shared.device_id.clone(),
                source_device_name: self.shared.device_name.clone(),
                target_device_id,
                source_path: std::path::PathBuf::from(source_path),
                mime_type,
                config: FileTransferConfig::default(),
            },
        )
        .map_err(|e| FfiError::FileTransfer {
            reason: e.to_string(),
        })
    }

    pub fn set_file_receive_dir(&self, dir: String) {
        let mut g = self
            .shared
            .file_receive_dir
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        *g = if dir.trim().is_empty() {
            None
        } else {
            Some(std::path::PathBuf::from(dir))
        };
    }

    pub fn take_received_files(&self) -> Vec<ReceivedFileRecord> {
        let mut g = self
            .shared
            .received_files
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        g.drain(..).collect()
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

struct ClientRun {
    relay_url: String,
    group_id: String,
    key: [u8; KEY_LEN],
    device_id: String,
    device_name: String,
    listener: Arc<dyn ClipListener>,
    cmd_rx: mpsc::UnboundedReceiver<Cmd>,
    lan_peers: Arc<AtomicUsize>,
    lan_peer_names: PeerRegistry,
    blob_cache: SharedBlobCache,
    peer_addrs: PeerAddrs,
    file_receive_dir: SharedFileReceiveDir,
    received_files: Arc<Mutex<VecDeque<ReceivedFileRecord>>>,
}

async fn run(config: ClientRun) {
    let ClientRun {
        relay_url,
        group_id,
        key,
        device_id,
        device_name,
        listener,
        mut cmd_rx,
        lan_peers,
        lan_peer_names,
        blob_cache,
        peer_addrs,
        file_receive_dir,
        received_files,
    } = config;

    // Receive-side dedup. The same (sender_device_id, ts) pair may arrive
    // both from the relay WS and from a LAN peer; whichever lands first
    // wins, the other is silently dropped. Shared via Arc<Mutex<>> because
    // the LAN inbound task and the WS session task both insert into it.
    let dedup = Arc::new(Mutex::new(DedupCache::new(128, Duration::from_secs(10))));

    // Try to bring up the LAN node. Failure here (e.g. mDNS daemon
    // refused, port bind failed in a sandboxed test env) is non-fatal:
    // we just degrade to relay-only and log it.
    let (lan_in_tx, lan_in_rx) = mpsc::unbounded_channel::<IncomingLanClip>();
    let (file_in_tx, mut file_in_rx) = mpsc::unbounded_channel::<ReceivedFile>();
    let lan = match LanNode::spawn(LanNodeConfig {
        group_id: group_id.clone(),
        device_id: device_id.clone(),
        device_name: device_name.clone(),
        key,
        inbound: lan_in_tx,
        peer_count: lan_peers,
        peers: lan_peer_names,
        blob_cache,
        peer_addrs,
        file_receive_dir,
        file_inbound: file_in_tx,
    })
    .await
    {
        Ok(n) => {
            tracing::info!("lan transport up");
            Some(Arc::new(n))
        }
        Err(e) => {
            tracing::warn!(?e, "lan transport disabled");
            None
        }
    };

    {
        let received_files = received_files.clone();
        tokio::spawn(async move {
            while let Some(file) = file_in_rx.recv().await {
                let mut g = received_files.lock().unwrap_or_else(|p| p.into_inner());
                if g.len() >= 128 {
                    g.pop_front();
                }
                g.push_back(file.into());
            }
        });
    }

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
            SessionCtx {
                relay_url: &relay_url,
                group_id: &group_id,
                key: &key,
                device_id: &device_id,
                listener: &listener,
                lan: lan.as_deref(),
                dedup: &dedup,
            },
            &mut cmd_rx,
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

const LAN_ADVERTISE_REFRESH_EVERY: Duration = Duration::from_secs(5);

fn normalize_lan_candidate_networks(mut candidates: Vec<LanCandidate>) -> Vec<LanCandidate> {
    candidates.sort_by(|a, b| a.addr.cmp(&b.addr).then(a.prefix_len.cmp(&b.prefix_len)));
    candidates.dedup_by(|a, b| a.addr == b.addr && a.prefix_len == b.prefix_len);
    candidates
}

fn lan_advertise_refresh_needed(last: Option<&[LanCandidate]>, current: &[LanCandidate]) -> bool {
    match last {
        Some(last) => last != current,
        None => !current.is_empty(),
    }
}

struct SessionCtx<'a> {
    relay_url: &'a str,
    group_id: &'a str,
    key: &'a [u8; KEY_LEN],
    device_id: &'a str,
    listener: &'a Arc<dyn ClipListener>,
    lan: Option<&'a LanNode>,
    dedup: &'a Arc<Mutex<DedupCache>>,
}

async fn session(
    ctx: SessionCtx<'_>,
    cmd_rx: &mut mpsc::UnboundedReceiver<Cmd>,
) -> Result<SessionExit, Box<dyn std::error::Error + Send + Sync>> {
    let SessionCtx {
        relay_url,
        group_id,
        key,
        device_id,
        listener,
        lan,
        dedup,
    } = ctx;

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
    let mut lan_advertise_error_pending = false;
    let mut lan_advertise_disabled = false;
    let mut last_lan_advertise: Option<Vec<LanCandidate>> = None;

    let join = ClientMessage::Join {
        group_id: group_id.to_string(),
        device_id: device_id.to_string(),
    };
    ws.send(Message::Text(serde_json::to_string(&join)?))
        .await?;

    // Pull whatever is still in the relay's recent-cache so devices that
    // joined late or just reconnected after a network blip don't miss the
    // last few clips. The relay caches up to 3 clips for 5 minutes.
    let fetch = ClientMessage::FetchRecent {
        group_id: group_id.to_string(),
    };
    ws.send(Message::Text(serde_json::to_string(&fetch)?))
        .await?;

    // Opt into relay-assisted LAN rendezvous: tell the relay our dialable
    // private candidates so it can introduce us to same-LAN group peers
    // without waiting on mDNS. Sending this is also what marks the
    // connection rendezvous-capable — old relays just reply with an error
    // we ignore, and never push us `LanPeers`. Skipped entirely if the LAN
    // node didn't start (multicast/socket blocked) or has no private IPs.
    if let Some(lan) = lan {
        let candidate_networks =
            normalize_lan_candidate_networks(lan.advertise_candidate_networks());
        let candidates: Vec<String> = candidate_networks.iter().map(|c| c.addr.clone()).collect();
        if !candidates.is_empty() {
            let adv = ClientMessage::LanAdvertise {
                group_id: group_id.to_string(),
                device_id: device_id.to_string(),
                candidates,
                candidate_networks: candidate_networks.clone(),
            };
            ws.send(Message::Text(serde_json::to_string(&adv)?)).await?;
            lan_advertise_error_pending = true;
            last_lan_advertise = Some(candidate_networks);
        }
    }

    listener.on_state(ConnectionState::Connected);

    // Heartbeat: ping every 30s, force-reconnect if no inbound frame for 60s.
    // The latter catches NAT idle timeouts and silent network switches (Wi-Fi
    // ↔ cellular) where TCP stays "open" but never delivers data again.
    const PING_EVERY: Duration = Duration::from_secs(30);
    const IDLE_TIMEOUT: Duration = Duration::from_secs(60);
    let mut ping_interval = tokio::time::interval(PING_EVERY);
    ping_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    ping_interval.tick().await; // consume the immediate first tick
    let mut lan_advertise_interval = tokio::time::interval(LAN_ADVERTISE_REFRESH_EVERY);
    lan_advertise_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    lan_advertise_interval.tick().await; // consume the immediate first tick
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
                ws.send(Message::Ping(Vec::new())).await?;
            }
            _ = lan_advertise_interval.tick(), if lan.is_some() && !lan_advertise_disabled => {
                if let Some(lan) = lan {
                    let candidate_networks =
                        normalize_lan_candidate_networks(lan.advertise_candidate_networks());
                    if lan_advertise_refresh_needed(
                        last_lan_advertise.as_deref(),
                        &candidate_networks,
                    ) {
                        let candidates: Vec<String> =
                            candidate_networks.iter().map(|c| c.addr.clone()).collect();
                        let adv = ClientMessage::LanAdvertise {
                            group_id: group_id.to_string(),
                            device_id: device_id.to_string(),
                            candidates,
                            candidate_networks: candidate_networks.clone(),
                        };
                        ws.send(Message::Text(serde_json::to_string(&adv)?)).await?;
                        lan_advertise_error_pending = true;
                        last_lan_advertise = Some(candidate_networks);
                    }
                }
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
                        ws.send(Message::Text(serde_json::to_string(&msg)?)).await?;
                    }
                    Cmd::FetchRecent => {
                        let msg = ClientMessage::FetchRecent {
                            group_id: group_id.to_string(),
                        };
                        ws.send(Message::Text(serde_json::to_string(&msg)?)).await?;
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
                        // Rendezvous updates need the LAN node (not in
                        // `handle_server`'s reach), and a full snapshot
                        // each time, so intercept before dispatch.
                        if let ServerMessage::LanPeers { peers } = parsed {
                            lan_advertise_error_pending = false;
                            if let Some(lan) = lan {
                                lan.ingest_relay_peers(peers).await;
                            }
                        } else {
                            handle_server(
                                parsed,
                                key,
                                device_id,
                                listener,
                                dedup,
                                &mut lan_advertise_error_pending,
                                &mut lan_advertise_disabled,
                            );
                        }
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
    lan_advertise_error_pending: &mut bool,
    lan_advertise_disabled: &mut bool,
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
            if *lan_advertise_error_pending {
                *lan_advertise_error_pending = false;
                if is_lan_advertise_compat_error(&reason) {
                    *lan_advertise_disabled = true;
                    tracing::debug!(
                        %reason,
                        "ignoring optional LanAdvertise compatibility error from old relay"
                    );
                    return;
                }
            }
            listener.on_state(ConnectionState::Error { message: reason });
        }
        // Intercepted in `session()` before dispatch (needs the LAN node).
        ServerMessage::LanPeers { .. } => {}
    }
}

fn is_lan_advertise_compat_error(reason: &str) -> bool {
    reason.contains("bad message:")
        && reason.contains("unknown variant")
        && reason.contains("lan_advertise")
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
        if self.entries.iter().any(|(s, t, _)| s == sender && *t == ts) {
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

    #[derive(Default)]
    struct CaptureListener {
        states: Mutex<Vec<String>>,
    }

    impl ClipListener for CaptureListener {
        fn on_clip(&self, _payload: ClipPayload) {}

        fn on_state(&self, state: ConnectionState) {
            let label = match state {
                ConnectionState::Connecting => "connecting".to_string(),
                ConnectionState::Connected => "connected".to_string(),
                ConnectionState::Disconnected => "disconnected".to_string(),
                ConnectionState::Error { message } => format!("error:{message}"),
            };
            self.states.lock().unwrap().push(label);
        }
    }

    #[test]
    fn handle_server_suppresses_pending_lan_advertise_compat_error() {
        let capture = Arc::new(CaptureListener::default());
        let listener: Arc<dyn ClipListener> = capture.clone();
        let dedup = Arc::new(Mutex::new(DedupCache::new(8, Duration::from_secs(60))));
        let mut lan_advertise_error_pending = true;
        let mut lan_advertise_disabled = false;

        handle_server(
            ServerMessage::Error {
                reason: "bad message: unknown variant `lan_advertise`, expected one of `join`, `publish`, `fetch_recent`".into(),
            },
            &[0; KEY_LEN],
            "device",
            &listener,
            &dedup,
            &mut lan_advertise_error_pending,
            &mut lan_advertise_disabled,
        );

        assert!(capture.states.lock().unwrap().is_empty());
        assert!(!lan_advertise_error_pending);
        assert!(lan_advertise_disabled);
    }

    #[test]
    fn handle_server_reports_non_compat_error_while_lan_advertise_pending() {
        let capture = Arc::new(CaptureListener::default());
        let listener: Arc<dyn ClipListener> = capture.clone();
        let dedup = Arc::new(Mutex::new(DedupCache::new(8, Duration::from_secs(60))));
        let mut lan_advertise_error_pending = true;
        let mut lan_advertise_disabled = false;

        handle_server(
            ServerMessage::Error {
                reason: "group mismatch".into(),
            },
            &[0; KEY_LEN],
            "device",
            &listener,
            &dedup,
            &mut lan_advertise_error_pending,
            &mut lan_advertise_disabled,
        );

        assert_eq!(
            capture.states.lock().unwrap().as_slice(),
            ["error:group mismatch"]
        );
        assert!(!lan_advertise_error_pending);
        assert!(!lan_advertise_disabled);
    }

    fn client_for_file_tests(peer_addrs: PeerAddrs, peer_names: PeerRegistry) -> Client {
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel::<Cmd>();
        Client {
            cmd_tx,
            thread: Mutex::new(None),
            shared: Arc::new(Shared {
                key: [31u8; KEY_LEN],
                group_id: "group".into(),
                device_id: "source".into(),
                device_name: "Source".into(),
                blob: BlobClient::new("http://127.0.0.1:1").unwrap(),
                blob_cache: BlobCache::new(),
                peer_addrs,
                lan_peers: Arc::new(AtomicUsize::new(0)),
                lan_peer_names: peer_names,
                file_receive_dir: Arc::new(std::sync::Mutex::new(None)),
                received_files: Arc::new(Mutex::new(VecDeque::new())),
            }),
        }
    }

    #[test]
    fn client_exposes_lan_peer_records_with_device_ids() {
        let peer_names: PeerRegistry =
            Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
        peer_names
            .lock()
            .unwrap()
            .insert("relay:target".into(), ("target".into(), "Target".into()));
        let peer_addrs: PeerAddrs =
            Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
        peer_addrs.lock().unwrap().insert(
            "relay:target".into(),
            crate::lan::PeerAddrEntry {
                device_id: "target".into(),
                candidates: vec!["127.0.0.1:5555".parse().unwrap()],
            },
        );
        let client = client_for_file_tests(peer_addrs, peer_names);

        let records = client.lan_peer_records();

        assert_eq!(records.len(), 1);
        assert_eq!(records[0].device_id, "target");
        assert_eq!(records[0].display_name, "Target");
        assert_eq!(records[0].candidate_count, 1);
    }

    #[tokio::test]
    async fn client_sends_file_to_peer_device_id() {
        let root = std::env::temp_dir().join(format!(
            "clipbridge-client-file-send-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("client-send.txt");
        std::fs::write(&source, b"from client").unwrap();
        let destination = root.join("received");
        std::fs::create_dir_all(&destination).unwrap();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let receiver = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            crate::lan::receive_file_from_stream(
                stream,
                [31u8; KEY_LEN],
                "target".into(),
                "Target".into(),
                destination,
                crate::file_transfer::FileTransferConfig::default(),
            )
            .await
            .unwrap()
        });

        let peer_addrs: PeerAddrs =
            Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
        peer_addrs.lock().unwrap().insert(
            "relay:target".into(),
            crate::lan::PeerAddrEntry {
                device_id: "target".into(),
                candidates: vec![addr],
            },
        );
        let client = client_for_file_tests(
            peer_addrs,
            Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        );

        let sent = tokio::task::spawn_blocking(move || {
            client.send_file_to_peer(
                "target".into(),
                source.to_string_lossy().into_owned(),
                Some("text/plain".into()),
            )
        })
        .await
        .unwrap()
        .unwrap();
        let received = receiver.await.unwrap();

        assert_eq!(sent.bytes_sent, 11);
        assert_eq!(received.file_name, "client-send.txt");
        assert_eq!(
            std::fs::read_to_string(received.path).unwrap(),
            "from client"
        );
    }

    fn lc(addr: &str, prefix_len: u8) -> crate::protocol::LanCandidate {
        crate::protocol::LanCandidate {
            addr: addr.into(),
            prefix_len,
        }
    }

    #[test]
    fn lan_advertise_refresh_detects_real_candidate_changes_only() {
        let first = normalize_lan_candidate_networks(vec![
            lc("192.168.1.10:5000", 24),
            lc("10.0.0.2:5000", 24),
        ]);
        let reordered_duplicate = normalize_lan_candidate_networks(vec![
            lc("10.0.0.2:5000", 24),
            lc("192.168.1.10:5000", 24),
            lc("192.168.1.10:5000", 24),
        ]);
        let changed = normalize_lan_candidate_networks(vec![
            lc("192.168.2.10:5000", 24),
            lc("10.0.0.2:5000", 24),
        ]);

        assert!(lan_advertise_refresh_needed(None, &first));
        assert!(!lan_advertise_refresh_needed(
            Some(&first),
            &reordered_duplicate
        ));
        assert!(lan_advertise_refresh_needed(Some(&first), &changed));
        assert!(lan_advertise_refresh_needed(Some(&first), &[]));
        assert!(!lan_advertise_refresh_needed(None, &[]));
    }
}
