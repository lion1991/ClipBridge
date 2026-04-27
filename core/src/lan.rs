//! Opportunistic LAN peer-to-peer transport.
//!
//! Discovery uses mDNS / DNS-SD (`_clipbridge._tcp.local.`) so devices on
//! the same subnet find each other without any server. Each peer listens
//! on a random TCP port advertised in the SRV record; the TXT record
//! carries a SHA-256 fingerprint of the group_id so peers can filter
//! themselves to the right group without leaking the raw id over multicast.
//!
//! Frames over TCP are length-prefixed and AEAD-encrypted with the same
//! group key as the relay path — no separate TLS / cert plumbing, and
//! "decrypts cleanly" implicitly proves the peer holds the group key.
//!
//! This module is *opportunistic*: every send is also written to the relay
//! WebSocket. The receiving `Client` deduplicates on `(sender_device_id,
//! ts)` so users never see doubled clips. If LAN is blocked (client
//! isolation, multicast filtering, peers on different SSIDs) the relay
//! path keeps working unchanged — LAN failures are silent.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, mpsc, Mutex};

use crate::crypto::{decrypt, encrypt, KEY_LEN, NONCE_LEN};
use crate::protocol::ClipPayload;

const SERVICE_TYPE: &str = "_clipbridge._tcp.local.";
const PROTO_VERSION: u32 = 1;
/// Cap on a single decrypted frame. Image *bytes* never travel via LAN in
/// v1 (only the metadata sidecar inside `ClipPayload`), so 1 MiB is more
/// than enough for any text clip plus its envelope.
const FRAME_MAX: usize = 1024 * 1024;
/// Hard cap on the broadcast channel — if a peer is wedged we'd rather
/// drop a clip on the LAN side than back-pressure the sender. The relay
/// path will still deliver it.
const OUT_BUFFER: usize = 32;

/// One-line wire message for the LAN socket. Goes through AEAD before
/// hitting the wire, so the same `LanMessage` discriminant doubles as the
/// authentication signal — only group members can produce a valid frame.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum LanMessage {
    /// First frame on every connection. Tells the other side which device
    /// we are so they can drop self-loops and feed dedup. `device_name` is
    /// the human-friendly label shown in the receiver's UI peer list —
    /// `serde(default)` so older peers that don't send it still parse.
    Hello {
        device_id: String,
        version: u32,
        #[serde(default)]
        device_name: String,
    },
    Clip {
        sender_device_id: String,
        ts: u64,
        payload: ClipPayload,
    },
}

#[derive(Debug, Clone)]
pub struct IncomingLanClip {
    pub sender_device_id: String,
    pub ts: u64,
    pub payload: ClipPayload,
}

#[derive(Debug, Clone)]
struct OutgoingLan {
    sender_device_id: String,
    ts: u64,
    payload: ClipPayload,
}

/// 16-hex-char fingerprint of `group_id` for the TXT record. We never put
/// the raw group_id on the wire — anyone on the same WiFi can sniff TXT
/// records, and the id is meant to stay opaque to non-members.
pub fn group_fingerprint(group_id: &str) -> String {
    let d = Sha256::digest(group_id.as_bytes());
    let mut s = String::with_capacity(16);
    for b in &d[..8] {
        s.push(hex_nibble(b >> 4));
        s.push(hex_nibble(b & 0x0f));
    }
    s
}

fn hex_nibble(n: u8) -> char {
    match n {
        0..=9 => (b'0' + n) as char,
        10..=15 => (b'a' + (n - 10)) as char,
        _ => unreachable!(),
    }
}

/// True for addresses we should never attempt to dial as-is. Currently
/// only IPv6 link-local (`fe80::/10`) — those need a `%scope` suffix to
/// route, and we don't track scope ids when consuming mDNS results.
/// Apple devices in particular pepper their mDNS announcements with
/// `awdl0` / `utun` link-local v6 addresses; without this filter we'd
/// occasionally pick one and the dial would silently fail.
fn is_unroutable(a: &IpAddr) -> bool {
    match a {
        IpAddr::V6(v6) => is_v6_link_local(v6),
        IpAddr::V4(_) => false,
    }
}

/// Manual `fe80::/10` test — `Ipv6Addr::is_unicast_link_local` only
/// stabilized recently and we want to keep working on older toolchains.
fn is_v6_link_local(v6: &Ipv6Addr) -> bool {
    let o = v6.octets();
    o[0] == 0xfe && (o[1] & 0xc0) == 0x80
}

#[derive(Debug, thiserror::Error)]
pub enum LanError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("mdns: {0}")]
    Mdns(String),
}

impl From<mdns_sd::Error> for LanError {
    fn from(e: mdns_sd::Error) -> Self {
        LanError::Mdns(e.to_string())
    }
}

/// Live registry of peers we currently have a fully-handshaked LAN session
/// to, keyed by mDNS instance fullname (so two processes on one device —
/// e.g. iOS main app + keyboard — count as separate entries). The values
/// are the human-friendly device names the peers sent in their Hello.
pub type PeerRegistry = Arc<std::sync::Mutex<HashMap<String, String>>>;

/// Owns the mDNS daemon, accept loop, and per-peer connection tasks. Drop
/// to tear everything down — the broadcast sender closes, peer tasks see
/// EOF and exit, and the daemon's own thread is shut down by `Drop`.
pub struct LanNode {
    out_tx: broadcast::Sender<OutgoingLan>,
    /// Live count of peers currently in a Hello-completed session. Bumped
    /// when `run_peer` finishes its handshake, decremented when its
    /// task ends. Read by the FFI getter so the UI can show "LAN: N".
    peer_count: Arc<AtomicUsize>,
    /// fullname → peer device_name. Populated alongside `peer_count` so
    /// the FFI layer can also render "局域网: Mac, iPhone" instead of
    /// just a count, which makes mesh asymmetry visible across devices.
    peers: PeerRegistry,
    /// Kept alive so the daemon and its background thread stay up. The
    /// `mdns_sd::ServiceDaemon::Drop` impl unregisters the service and
    /// stops the daemon.
    _daemon: ServiceDaemon,
    /// Forwarder thread that bridges flume's sync receiver to a tokio
    /// channel. Joined when its event_tx closes (i.e. when daemon drops).
    _forwarder: Option<JoinHandle<()>>,
}

impl LanNode {
    /// Bind a TCP listener on a random port, register an mDNS service for
    /// it, browse for other peers, and start forwarding clips.
    ///
    /// Must be called from within a tokio runtime context — spawns long-
    /// lived tasks via `tokio::spawn`. `peer_count` and `peers` are
    /// shared with the owner so they can be polled from outside the
    /// runtime (FFI getters). `device_name` is sent to peers in our
    /// Hello so they can render us in their UI peer list.
    pub async fn spawn(
        group_id: String,
        device_id: String,
        device_name: String,
        key: [u8; KEY_LEN],
        inbound: mpsc::UnboundedSender<IncomingLanClip>,
        peer_count: Arc<AtomicUsize>,
        peers: PeerRegistry,
    ) -> Result<Self, LanError> {
        let listener = TcpListener::bind(("0.0.0.0", 0)).await?;
        let port = listener.local_addr()?.port();

        let daemon = ServiceDaemon::new()?;
        let fingerprint = group_fingerprint(&group_id);

        // Properties (TXT record) — only the things peers need before they
        // open a TCP connection. The raw group_id is intentionally absent.
        let mut props: HashMap<String, String> = HashMap::new();
        props.insert("v".into(), PROTO_VERSION.to_string());
        props.insert("gid".into(), fingerprint.clone());
        props.insert("did".into(), device_id.clone());

        // mDNS instance names must be unique within the service type. iOS
        // runs the keyboard extension in a separate process from the main
        // app but they share the same `device_id` (per App Group), so we
        // tack on a per-process random suffix to keep registrations from
        // colliding. Receivers dedup on the `did` TXT property + clip ts,
        // so seeing the same logical device under two instances is fine.
        let mut suffix = [0u8; 4];
        rand::thread_rng().fill_bytes(&mut suffix);
        let suffix = format!("{:02x}{:02x}{:02x}{:02x}", suffix[0], suffix[1], suffix[2], suffix[3]);
        let instance = format!("{}-{}", short_id(&device_id), suffix);
        let hostname = format!("clipbridge-{}.local.", instance);
        let service = ServiceInfo::new(
            SERVICE_TYPE,
            &instance,
            &hostname,
            "",
            port,
            Some(props),
        )?
        .enable_addr_auto();
        daemon.register(service)?;

        let browse_rx = daemon.browse(SERVICE_TYPE)?;

        // mdns-sd's browse() returns a sync flume::Receiver. Forward it
        // into a tokio channel via a tiny std thread so the main loop can
        // `select!` on it without blocking the runtime worker.
        let (event_tx, event_rx) = mpsc::unbounded_channel::<ServiceEvent>();
        let forwarder = std::thread::Builder::new()
            .name("clipbridge-mdns-forwarder".into())
            .spawn(move || {
                while let Ok(ev) = browse_rx.recv() {
                    if event_tx.send(ev).is_err() {
                        break; // tokio side gone — daemon being torn down
                    }
                }
            })
            .ok();

        let (out_tx, _) = broadcast::channel::<OutgoingLan>(OUT_BUFFER);
        // `peer_count` is provided by the caller so the FFI side can read
        // it from outside the tokio runtime that owns this LanNode.

        // Shared state for outbound dedup of peer connections. Keyed by
        // mDNS instance fullname (not device_id) so that two processes
        // running on the same physical device — e.g. the iOS main app and
        // its keyboard extension — both get a connection.
        let outbound_peers: Arc<Mutex<HashMap<String, ()>>> = Arc::new(Mutex::new(HashMap::new()));

        // Accept loop: inbound connections from peers that found us first.
        // Inbound peers don't have a known mDNS fullname (we didn't dial
        // them), so we synthesize one from the connection's remote addr
        // for the purpose of the peer registry. It's just a unique key.
        {
            let key = key;
            let device_id = device_id.clone();
            let device_name = device_name.clone();
            let inbound = inbound.clone();
            let out_tx = out_tx.clone();
            let peer_count = peer_count.clone();
            let peers = peers.clone();
            tokio::spawn(async move {
                loop {
                    let (stream, addr) = match listener.accept().await {
                        Ok(s) => s,
                        Err(e) => {
                            tracing::warn!(?e, "lan accept failed");
                            tokio::time::sleep(Duration::from_secs(1)).await;
                            continue;
                        }
                    };
                    let key = key;
                    let device_id = device_id.clone();
                    let device_name = device_name.clone();
                    let inbound = inbound.clone();
                    let out_rx = out_tx.subscribe();
                    let peer_count = peer_count.clone();
                    let peers = peers.clone();
                    let registry_key = format!("inbound:{addr}");
                    tokio::spawn(async move {
                        if let Err(e) = run_peer(
                            stream,
                            addr,
                            key,
                            device_id,
                            device_name,
                            inbound,
                            out_rx,
                            None,
                            peer_count,
                            peers,
                            registry_key,
                        )
                        .await
                        {
                            tracing::debug!(?e, %addr, "lan peer (inbound) ended");
                        }
                    });
                }
            });
        }

        // Discover loop: react to mDNS events. For each remote peer in the
        // same group, open a TCP connection and start a session.
        {
            let key = key;
            let device_id = device_id.clone();
            let device_name = device_name.clone();
            let fingerprint = fingerprint.clone();
            let inbound = inbound.clone();
            let out_tx = out_tx.clone();
            let outbound_peers = outbound_peers.clone();
            let peer_count = peer_count.clone();
            let peers = peers.clone();
            tokio::spawn(async move {
                let mut event_rx = event_rx;
                while let Some(event) = event_rx.recv().await {
                    let info = match event {
                        ServiceEvent::ServiceResolved(info) => info,
                        _ => continue,
                    };
                    let props = info.get_properties();
                    let peer_gid = props.get_property_val_str("gid").unwrap_or("");
                    let peer_did = props.get_property_val_str("did").unwrap_or("");
                    if peer_gid != fingerprint {
                        continue; // different group on the same LAN
                    }
                    if peer_did.is_empty() {
                        continue;
                    }
                    // Lexicographic tiebreak: only the side with the larger
                    // device_id initiates. Otherwise both sides try to
                    // connect simultaneously and we end up with two
                    // sessions per peer pair. Equal device_ids (same
                    // physical device, e.g. iOS main app vs keyboard
                    // extension) also short-circuit here — there's no
                    // value in cross-talking with our own twin.
                    if device_id.as_str() <= peer_did {
                        continue;
                    }
                    let fullname = info.get_fullname().to_string();
                    {
                        let mut p = outbound_peers.lock().await;
                        if p.contains_key(&fullname) {
                            continue;
                        }
                        p.insert(fullname.clone(), ());
                    }
                    let port = info.get_port();
                    // mDNS gives us a HashSet of advertised addresses with
                    // unstable iteration order. iOS in particular publishes
                    // both an IPv4 and an IPv6 link-local on `awdl0`/`utun`;
                    // the link-local needs a %scope to dial and we don't
                    // track that, so blindly grabbing `iter().next()` would
                    // sometimes pick the unconnectable one and the pair
                    // would never link up. Filter & sort: drop link-local
                    // IPv6, then IPv4 first, IPv6 global second.
                    let mut candidates: Vec<SocketAddr> = info
                        .get_addresses()
                        .iter()
                        .copied()
                        .filter(|a| !is_unroutable(a))
                        .map(|a| SocketAddr::new(a, port))
                        .collect();
                    candidates.sort_by_key(|sa| match sa.ip() {
                        IpAddr::V4(_) => 0,
                        IpAddr::V6(_) => 1,
                    });
                    if candidates.is_empty() {
                        continue;
                    }
                    let key = key;
                    let device_id = device_id.clone();
                    let device_name = device_name.clone();
                    let inbound = inbound.clone();
                    let out_rx = out_tx.subscribe();
                    let outbound_peers = outbound_peers.clone();
                    let peer_did_owned = peer_did.to_string();
                    let peer_count = peer_count.clone();
                    let peers_for_run = peers.clone();
                    let registry_key = fullname.clone();
                    tokio::spawn(async move {
                        // Try each candidate address with a short timeout.
                        // First success wins; on failure we leave the
                        // outbound_peers entry behind so the next mDNS
                        // re-resolve (~30s) can try again.
                        let mut connected: Option<(TcpStream, SocketAddr)> = None;
                        for cand in &candidates {
                            match tokio::time::timeout(
                                Duration::from_secs(3),
                                TcpStream::connect(cand),
                            )
                            .await
                            {
                                Ok(Ok(s)) => {
                                    connected = Some((s, *cand));
                                    break;
                                }
                                Ok(Err(e)) => {
                                    tracing::debug!(?e, %cand, "lan dial failed, trying next");
                                }
                                Err(_) => {
                                    tracing::debug!(%cand, "lan dial timed out, trying next");
                                }
                            }
                        }
                        let res = if let Some((stream, sock)) = connected {
                            run_peer(
                                stream,
                                sock,
                                key,
                                device_id,
                                device_name,
                                inbound,
                                out_rx,
                                Some(peer_did_owned.clone()),
                                peer_count,
                                peers_for_run,
                                registry_key,
                            )
                            .await
                        } else {
                            Err(LanError::Io(std::io::Error::new(
                                std::io::ErrorKind::ConnectionRefused,
                                "no advertised address was reachable",
                            )))
                        };
                        if let Err(e) = res {
                            tracing::debug!(?e, "lan peer (outbound) ended");
                        }
                        outbound_peers.lock().await.remove(&fullname);
                    });
                }
            });
        }

        Ok(Self {
            out_tx,
            peer_count,
            peers,
            _daemon: daemon,
            _forwarder: forwarder,
        })
    }

    /// Push a clip to every currently-connected peer. Lossy by design: if
    /// the broadcast buffer is full or there are no peers we silently drop
    /// — relay is the source of truth.
    pub fn broadcast(&self, sender_device_id: String, ts: u64, payload: ClipPayload) {
        let _ = self.out_tx.send(OutgoingLan {
            sender_device_id,
            ts,
            payload,
        });
    }

    /// Number of peers currently in a fully-handshaked LAN session.
    /// Polled by the FFI layer to drive a "LAN: N" status badge.
    pub fn peer_count(&self) -> usize {
        self.peer_count.load(Ordering::Relaxed)
    }

    /// Snapshot of currently-connected peers as their human-friendly
    /// device names. UI uses this to render "局域网: Mac, iPhone" so
    /// users can spot mesh asymmetry at a glance (e.g. Android sees
    /// both Mac and iOS but Mac and iOS only see Android → we know
    /// the missing edge is Mac↔iOS).
    pub fn peer_names(&self) -> Vec<String> {
        let g = match self.peers.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        g.values().cloned().collect()
    }
}

/// First 8 chars of the device_id, lowercased. Just enough uniqueness for
/// an mDNS instance label / hostname, and short enough to keep DNS happy
/// (instance names cap at 63 bytes per RFC 6763).
fn short_id(device_id: &str) -> String {
    let s: String = device_id.chars().filter(|c| c.is_ascii_alphanumeric()).take(12).collect();
    s.to_lowercase()
}

/// RAII guard that bumps `peer_count` on construction, registers the
/// peer's name, and reverses both on drop. Even if `run_peer` returns
/// early or panics the counter and registry stay consistent.
struct PeerSessionGuard {
    count: Arc<AtomicUsize>,
    peers: PeerRegistry,
    key: String,
}
impl PeerSessionGuard {
    fn new(count: Arc<AtomicUsize>, peers: PeerRegistry, key: String, name: String) -> Self {
        count.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut g) = peers.lock() {
            g.insert(key.clone(), name);
        }
        Self { count, peers, key }
    }
}
impl Drop for PeerSessionGuard {
    fn drop(&mut self) {
        self.count.fetch_sub(1, Ordering::Relaxed);
        if let Ok(mut g) = self.peers.lock() {
            g.remove(&self.key);
        }
    }
}

/// Drive one TCP session: send Hello, then concurrently push outbound
/// clips and pull inbound frames until either side closes.
async fn run_peer(
    stream: TcpStream,
    addr: SocketAddr,
    key: [u8; KEY_LEN],
    self_device_id: String,
    self_device_name: String,
    inbound: mpsc::UnboundedSender<IncomingLanClip>,
    mut out_rx: broadcast::Receiver<OutgoingLan>,
    expected_peer: Option<String>,
    peer_count: Arc<AtomicUsize>,
    peers: PeerRegistry,
    registry_key: String,
) -> Result<(), LanError> {
    let _ = stream.set_nodelay(true);
    let (mut reader, mut writer) = stream.into_split();

    // Send our Hello first. Order doesn't matter — both sides do the same.
    let hello = LanMessage::Hello {
        device_id: self_device_id.clone(),
        version: PROTO_VERSION,
        device_name: self_device_name,
    };
    write_frame(&mut writer, &key, &hello).await?;

    // Read the peer's Hello to learn its device_id and friendly name.
    let (peer_did, peer_name) = match tokio::time::timeout(Duration::from_secs(5), read_frame(&mut reader, &key)).await {
        Ok(Ok(Some(LanMessage::Hello { device_id, device_name, .. }))) => (device_id, device_name),
        Ok(Ok(Some(_))) => {
            return Err(LanError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "first frame was not Hello",
            )));
        }
        Ok(Ok(None)) | Ok(Err(_)) | Err(_) => {
            return Err(LanError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "hello handshake failed",
            )));
        }
    };
    if let Some(expected) = expected_peer {
        // We initiated this connection because mDNS told us peer_did was
        // at this address. If the actual handshake says otherwise, bail —
        // someone else is squatting on that port.
        if short_id(&peer_did) != short_id(&expected) {
            return Err(LanError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "peer device_id mismatch",
            )));
        }
    }
    if peer_did == self_device_id {
        return Ok(()); // self-connect (shouldn't happen, but cheap to guard)
    }
    // Fall back to a short device_id snippet if the peer is on an older
    // build that doesn't send `device_name` (Hello field is `default`).
    let display_name = if peer_name.trim().is_empty() {
        short_id(&peer_did)
    } else {
        peer_name
    };
    // Only bump the public counter / registry once we have a real
    // handshake. Drop happens automatically when this function returns
    // or panics, so the count and the name list stay consistent.
    let _peer_guard = PeerSessionGuard::new(peer_count, peers, registry_key, display_name.clone());
    tracing::info!(%addr, peer = %display_name, "lan peer up");

    // Outbound + inbound run concurrently until either errors or EOF.
    let send_task = async move {
        loop {
            match out_rx.recv().await {
                Ok(out) => {
                    // Don't echo a clip back to the device that originally
                    // sent it. (We only know the *original* sender here,
                    // which may be a third device in larger groups.)
                    if out.sender_device_id == peer_did {
                        continue;
                    }
                    let msg = LanMessage::Clip {
                        sender_device_id: out.sender_device_id,
                        ts: out.ts,
                        payload: out.payload,
                    };
                    if let Err(e) = write_frame(&mut writer, &key, &msg).await {
                        return Err::<(), LanError>(e);
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(skipped = n, "lan broadcast lagged");
                }
                Err(broadcast::error::RecvError::Closed) => return Ok(()),
            }
        }
    };

    let recv_task = async move {
        loop {
            match read_frame(&mut reader, &key).await? {
                Some(LanMessage::Clip {
                    sender_device_id,
                    ts,
                    payload,
                }) => {
                    let _ = inbound.send(IncomingLanClip {
                        sender_device_id,
                        ts,
                        payload,
                    });
                }
                Some(LanMessage::Hello { .. }) => {
                    // Spurious second Hello — ignore.
                }
                None => return Ok::<(), LanError>(()),
            }
        }
    };

    tokio::select! {
        r = send_task => r,
        r = recv_task => r,
    }
}

/// Wire frame: `len:u32 BE | nonce:12 | ciphertext` where `len` covers
/// nonce+ciphertext. AEAD = ChaCha20-Poly1305 with the group key.
async fn write_frame<W: AsyncWriteExt + Unpin>(
    w: &mut W,
    key: &[u8; KEY_LEN],
    msg: &LanMessage,
) -> Result<(), LanError> {
    let plain = serde_json::to_vec(msg).map_err(|e| {
        LanError::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    })?;
    let (cipher, nonce) = encrypt(key, &plain).map_err(|e| {
        LanError::Io(std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))
    })?;
    let total = NONCE_LEN + cipher.len();
    if total > FRAME_MAX {
        return Err(LanError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "frame too large for LAN",
        )));
    }
    w.write_u32(total as u32).await?;
    w.write_all(&nonce).await?;
    w.write_all(&cipher).await?;
    w.flush().await?;
    Ok(())
}

async fn read_frame<R: AsyncReadExt + Unpin>(
    r: &mut R,
    key: &[u8; KEY_LEN],
) -> Result<Option<LanMessage>, LanError> {
    let len = match r.read_u32().await {
        Ok(n) => n as usize,
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(LanError::Io(e)),
    };
    if len > FRAME_MAX || len <= NONCE_LEN {
        return Err(LanError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "frame size out of range",
        )));
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf).await?;
    let (nonce, cipher) = buf.split_at(NONCE_LEN);
    let plain = decrypt(key, nonce, cipher).map_err(|_| {
        LanError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "AEAD decrypt failed (wrong group key?)",
        ))
    })?;
    let msg: LanMessage = serde_json::from_slice(&plain).map_err(|e| {
        LanError::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    })?;
    Ok(Some(msg))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::ClipKind;
    use tokio::io::duplex;

    #[test]
    fn fingerprint_is_stable_and_short() {
        let a = group_fingerprint("group-1");
        let b = group_fingerprint("group-1");
        let c = group_fingerprint("group-2");
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(a.len(), 16);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[tokio::test]
    async fn frame_round_trip() {
        let key = [9u8; KEY_LEN];
        let (mut a, mut b) = duplex(64 * 1024);
        let msg = LanMessage::Clip {
            sender_device_id: "dev-A".into(),
            ts: 12345,
            payload: ClipPayload {
                kind: ClipKind::Text,
                content: "hello LAN".into(),
                device_name: "mac".into(),
                ts: 12345,
                image: None,
            },
        };
        write_frame(&mut a, &key, &msg).await.unwrap();
        let got = read_frame(&mut b, &key).await.unwrap().unwrap();
        match got {
            LanMessage::Clip { ts, payload, .. } => {
                assert_eq!(ts, 12345);
                assert_eq!(payload.content, "hello LAN");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[tokio::test]
    async fn frame_with_wrong_key_fails() {
        let (mut a, mut b) = duplex(4096);
        let msg = LanMessage::Hello {
            device_id: "x".into(),
            version: 1,
            device_name: "x-name".into(),
        };
        write_frame(&mut a, &[1u8; KEY_LEN], &msg).await.unwrap();
        let r = read_frame(&mut b, &[2u8; KEY_LEN]).await;
        assert!(r.is_err());
    }

    /// Two nodes on localhost discover each other via mDNS and exchange
    /// one clip. Marked `#[ignore]` because:
    ///   - macOS requires per-binary "Local Network" privacy permission;
    ///     each `cargo test` rebuild gets a new binary path and the
    ///     permission must be re-granted in System Settings, so the test
    ///     silently fails on a fresh build (mdns-sd's own integration
    ///     tests fail for the same reason on macOS Sonoma+).
    ///   - Sandboxed CI envs typically block multicast.
    /// Run manually with `cargo test -p clipbridge-core --lib lan --
    /// --ignored` after granting Local Network permission to the test
    /// binary if you need to verify discovery end-to-end.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore]
    async fn two_nodes_discover_and_exchange() {
        let _ = tracing_subscriber::fmt::try_init();
        let key = [42u8; KEY_LEN];
        let group = "test-group-".to_string() + &uuid::Uuid::new_v4().to_string();

        // device_id_a < device_id_b so the lexicographic tiebreak picks
        // node B as the initiator. (Either way works, but pinning the
        // direction makes the test fully deterministic.)
        let did_a = format!("aaaa-{}", uuid::Uuid::new_v4());
        let did_b = format!("zzzz-{}", uuid::Uuid::new_v4());

        let (a_tx, mut a_rx) = mpsc::unbounded_channel::<IncomingLanClip>();
        let (b_tx, mut b_rx) = mpsc::unbounded_channel::<IncomingLanClip>();

        let count_a = Arc::new(AtomicUsize::new(0));
        let count_b = Arc::new(AtomicUsize::new(0));
        let peers_a: PeerRegistry = Arc::new(std::sync::Mutex::new(HashMap::new()));
        let peers_b: PeerRegistry = Arc::new(std::sync::Mutex::new(HashMap::new()));
        let node_a = LanNode::spawn(
            group.clone(),
            did_a.clone(),
            "node-A".into(),
            key,
            a_tx,
            count_a,
            peers_a,
        )
        .await
        .expect("spawn A");
        let node_b = LanNode::spawn(
            group.clone(),
            did_b.clone(),
            "node-B".into(),
            key,
            b_tx,
            count_b,
            peers_b,
        )
        .await
        .expect("spawn B");

        // Discovery is asynchronous; poll-broadcast until B receives.
        let payload = ClipPayload {
            kind: ClipKind::Text,
            content: "hi from A".into(),
            device_name: "A".into(),
            ts: 1,
            image: None,
        };

        let got_b = tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                node_a.broadcast(did_a.clone(), 1, payload.clone());
                tokio::select! {
                    Some(c) = b_rx.recv() => return c,
                    _ = tokio::time::sleep(Duration::from_millis(300)) => {}
                }
            }
        })
        .await
        .expect("B never received clip from A");
        assert_eq!(got_b.payload.content, "hi from A");
        assert_eq!(got_b.sender_device_id, did_a);

        // Now the reverse direction (connection is bidirectional).
        let payload2 = ClipPayload {
            kind: ClipKind::Text,
            content: "hi from B".into(),
            device_name: "B".into(),
            ts: 2,
            image: None,
        };
        let got_a = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                node_b.broadcast(did_b.clone(), 2, payload2.clone());
                tokio::select! {
                    Some(c) = a_rx.recv() => return c,
                    _ = tokio::time::sleep(Duration::from_millis(300)) => {}
                }
            }
        })
        .await
        .expect("A never received clip from B");
        assert_eq!(got_a.payload.content, "hi from B");
        assert_eq!(got_a.sender_device_id, did_b);
    }
}
