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

use std::collections::{HashMap, VecDeque};
use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, mpsc, Mutex, Notify};

use crate::crypto::{decrypt, encrypt, KEY_LEN, NONCE_LEN};
use crate::file_transfer::{
    FileOffer, FileTransferConfig, FileTransferError, IncomingFileWriter, OutgoingFile,
    ReceivedFile, SendFileRequest, SentFile,
};
use crate::protocol::{ClipPayload, LanCandidate};

const SERVICE_TYPE: &str = "_clipbridge._tcp.local.";
const PROTO_VERSION: u32 = 1;
/// Cap on a single decrypted frame. Image *bytes* never travel via LAN in
/// v1 (only the metadata sidecar inside `ClipPayload`), so 1 MiB is more
/// than enough for any text clip plus its envelope.
const FRAME_MAX: usize = 1024 * 1024;
pub const MAX_FILE_CHUNK_BYTES: usize = 512 * 1024;
/// Hard cap on the broadcast channel — if a peer is wedged we'd rather
/// drop a clip on the LAN side than back-pressure the sender. The relay
/// path will still deliver it.
const OUT_BUFFER: usize = 32;
/// How often each peer task sends a no-op Ping, and how long since any
/// inbound frame we wait before declaring the link dead. The 3× ratio
/// gives one missed ping margin before tearing the connection down.
const PING_INTERVAL: Duration = Duration::from_secs(15);
const IDLE_TIMEOUT: Duration = Duration::from_secs(45);
/// How often the reconciler re-tries dialing known-but-not-connected
/// peers. mDNS only emits `ServiceResolved` once per service unless the
/// records change, so without this loop a TCP drop never reconnects.
const RECONNECT_INTERVAL: Duration = Duration::from_secs(5);
const DIAL_TIMEOUT: Duration = Duration::from_secs(1);
/// Raw bytes per `BlobChunk` before base64 + JSON + AEAD framing. 512 KiB
/// inflates to ~700 KiB base64, comfortably under `FRAME_MAX` once the
/// envelope is added. Blob bytes ride a *dedicated* short-lived TCP
/// connection so this never head-of-line-blocks clipboard frames.
const BLOB_CHUNK: usize = 512 * 1024;
/// Bounds on the in-memory ciphertext cache that backs LAN blob serving.
/// Keyed by sha256(ciphertext) — same address the relay blob store uses —
/// so a peer can re-serve an image it sent or recently fetched without the
/// bytes ever touching the relay. Evict oldest until both caps hold.
const BLOB_CACHE_MAX_BYTES: usize = 64 * 1024 * 1024;
const BLOB_CACHE_MAX_ENTRIES: usize = 32;

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
    /// Liveness keep-alive. Sent every `PING_INTERVAL` regardless of clip
    /// traffic so each side can detect a silently-dead peer via the
    /// `IDLE_TIMEOUT`. No payload — just the wakeup. Older peers without
    /// this variant fail JSON parse and we drop them, which is fine —
    /// they'd reconnect on the next mDNS event with a matching version.
    Ping,
    /// Sent by a blob requester as its first post-Hello frame on a
    /// *dedicated* connection (never on the shared control link). Asks the
    /// peer to stream the ciphertext it has cached under `sha256_hex`
    /// (= sha256 of the relay-blob ciphertext, the universal image
    /// address). Old peers that don't know this variant fail JSON parse
    /// and drop only that throwaway connection — the requester then falls
    /// back to the relay, so the optimization degrades safely.
    BlobRequest {
        sha256_hex: String,
    },
    /// One slice of the requested ciphertext. `last == true` marks the
    /// final chunk (an empty `data` with `last` is valid for a 0-byte
    /// blob, though images never are).
    BlobChunk {
        #[serde(with = "b64")]
        data: Vec<u8>,
        last: bool,
    },
    /// The serving peer doesn't have `sha256_hex` cached. Requester aborts
    /// immediately and falls back to the relay rather than waiting out a
    /// timeout.
    BlobMiss {
        sha256_hex: String,
    },
    FileOffer {
        offer: FileOffer,
    },
    FileAccept {
        transfer_id: String,
    },
    FileReject {
        transfer_id: String,
        reason: String,
    },
    FileChunk {
        transfer_id: String,
        offset: u64,
        #[serde(with = "b64")]
        data: Vec<u8>,
    },
    FileComplete {
        transfer_id: String,
    },
    FileCancel {
        transfer_id: String,
        reason: String,
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

/// Base64 codec for the `BlobChunk::data` field. serde_json would otherwise
/// render a `Vec<u8>` as a JSON array of integers (~4x blowup); base64 is
/// ~1.34x and the frame budget is sized around it. Mirrors the helper in
/// `protocol.rs` (kept local to avoid widening that module's visibility).
mod b64 {
    use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(v: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&B64.encode(v))
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let s: String = Deserialize::deserialize(d)?;
        B64.decode(s.as_bytes()).map_err(serde::de::Error::custom)
    }
}

/// One resolved LAN address set for a peer instance. The key in
/// [`PeerAddrs`] remains the mDNS fullname or `relay:<device_id>` namespace,
/// while this value carries the stable logical device id needed by file
/// transfer UI and targeted dialing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerAddrEntry {
    pub device_id: String,
    pub display_name: Option<String>,
    pub candidates: Vec<SocketAddr>,
}

/// Stable peer record for UI target selection.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct LanPeerRecord {
    pub device_id: String,
    pub display_name: String,
    pub candidate_count: u32,
}

/// mDNS instance `fullname` / relay namespace → dialable addresses for
/// every group peer resolved, in *both* directions (unlike `known_peers`,
/// which the control-link tiebreak only populates on the larger-id side).
/// The blob/file paths are symmetric — a receiver pulls or sends bytes
/// from/to whoever has candidates regardless of who dials the control link.
pub type PeerAddrs = Arc<std::sync::Mutex<HashMap<String, PeerAddrEntry>>>;
pub type SharedFileReceiveDir = Arc<std::sync::Mutex<Option<PathBuf>>>;

/// Snapshot stable peer records with one row per logical device.
pub fn peer_records(peers: &PeerRegistry, addrs: &PeerAddrs) -> Vec<LanPeerRecord> {
    let mut names: HashMap<String, String> = HashMap::new();
    if let Ok(g) = peers.lock() {
        let mut entries: Vec<_> = g.iter().collect();
        entries.sort_by(|(a, _), (b, _)| a.cmp(b));
        for (_, (did, name)) in entries {
            names.entry(did.clone()).or_insert_with(|| name.clone());
        }
    }

    let mut by_device: HashMap<String, (String, u32)> = HashMap::new();
    if let Ok(g) = addrs.lock() {
        for entry in g.values() {
            let display_name = names
                .get(&entry.device_id)
                .cloned()
                .or_else(|| non_empty_display_name(entry.display_name.as_deref()))
                .unwrap_or_else(|| short_id(&entry.device_id));
            let rec = by_device
                .entry(entry.device_id.clone())
                .or_insert((display_name, 0));
            rec.1 = rec.1.saturating_add(entry.candidates.len() as u32);
        }
    }

    for (did, name) in names {
        by_device.entry(did).or_insert((name, 0));
    }

    let mut out: Vec<LanPeerRecord> = by_device
        .into_iter()
        .map(
            |(device_id, (display_name, candidate_count))| LanPeerRecord {
                device_id,
                display_name,
                candidate_count,
            },
        )
        .collect();
    out.sort_by(|a, b| {
        a.display_name
            .cmp(&b.display_name)
            .then_with(|| a.device_id.cmp(&b.device_id))
    });
    out
}

fn non_empty_display_name(value: Option<&str>) -> Option<String> {
    let trimmed = value?.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

pub fn candidates_for_peer(addrs: &PeerAddrs, device_id: &str) -> Vec<SocketAddr> {
    let mut out = Vec::new();
    if let Ok(g) = addrs.lock() {
        for entry in g.values() {
            if entry.device_id == device_id {
                for cand in &entry.candidates {
                    if !out.contains(cand) {
                        out.push(*cand);
                    }
                }
            }
        }
    }
    out
}

/// Bounded in-memory cache of group-key ciphertext keyed by
/// sha256(ciphertext) — the same address the relay blob endpoint uses.
/// Populated when this device sends an image (so it can serve LAN peers
/// without the relay) and when it fetches one (so it can re-serve to the
/// next peer, spreading load across the mesh). Eviction is oldest-first
/// until both the byte and entry caps hold.
pub struct BlobCache {
    inner: std::sync::Mutex<BlobCacheInner>,
}

struct BlobCacheInner {
    map: HashMap<String, Arc<Vec<u8>>>,
    order: VecDeque<String>,
    bytes: usize,
}

pub type SharedBlobCache = Arc<BlobCache>;

impl BlobCache {
    pub fn new() -> SharedBlobCache {
        Arc::new(Self {
            inner: std::sync::Mutex::new(BlobCacheInner {
                map: HashMap::new(),
                order: VecDeque::new(),
                bytes: 0,
            }),
        })
    }

    pub fn get(&self, sha256_hex: &str) -> Option<Arc<Vec<u8>>> {
        let g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        g.map.get(sha256_hex).cloned()
    }

    pub fn insert(&self, sha256_hex: String, data: Arc<Vec<u8>>) {
        let mut g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        if g.map.contains_key(&sha256_hex) {
            return; // content-addressed: same key ⇒ identical bytes
        }
        g.bytes += data.len();
        g.order.push_back(sha256_hex.clone());
        g.map.insert(sha256_hex, data);
        while g.bytes > BLOB_CACHE_MAX_BYTES || g.map.len() > BLOB_CACHE_MAX_ENTRIES {
            let Some(old) = g.order.pop_front() else {
                break;
            };
            if let Some(v) = g.map.remove(&old) {
                g.bytes -= v.len();
            }
        }
    }
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

/// True for addresses worth advertising to the relay as LAN rendezvous
/// candidates: RFC1918 private IPv4, IPv4 link-local (169.254/16), and
/// IPv6 ULA (`fc00::/7`). Public addresses are deliberately withheld — the
/// relay only ever learns private space (the privacy bound the user
/// accepted). IPv6 link-local is excluded for the same reason
/// `is_unroutable` drops it: it needs a `%scope` we can't carry across the
/// relay path.
fn is_advertisable_private(a: &IpAddr) -> bool {
    match a {
        IpAddr::V4(v4) => v4.is_private() || v4.is_link_local(),
        IpAddr::V6(v6) => (v6.octets()[0] & 0xfe) == 0xfc,
    }
}

#[derive(Debug, Clone)]
struct LocalNetwork {
    ip: IpAddr,
    netmask: IpAddr,
    prefix_len: u8,
}

fn local_private_networks() -> Vec<LocalNetwork> {
    let Ok(ifaces) = if_addrs::get_if_addrs() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for iface in ifaces {
        if iface.is_loopback() {
            continue;
        }
        match iface.addr {
            if_addrs::IfAddr::V4(v4) => {
                let ip = IpAddr::V4(v4.ip);
                if is_advertisable_private(&ip) {
                    out.push(LocalNetwork {
                        ip,
                        netmask: IpAddr::V4(v4.netmask),
                        prefix_len: v4.prefixlen,
                    });
                }
            }
            if_addrs::IfAddr::V6(v6) => {
                let ip = IpAddr::V6(v6.ip);
                if is_advertisable_private(&ip) {
                    out.push(LocalNetwork {
                        ip,
                        netmask: IpAddr::V6(v6.netmask),
                        prefix_len: v6.prefixlen,
                    });
                }
            }
        }
    }
    out
}

fn same_subnet(candidate: &IpAddr, local: &LocalNetwork) -> bool {
    match (candidate, &local.ip, &local.netmask) {
        (IpAddr::V4(candidate), IpAddr::V4(local_ip), IpAddr::V4(mask)) => {
            (u32::from(*candidate) & u32::from(*mask)) == (u32::from(*local_ip) & u32::from(*mask))
        }
        (IpAddr::V6(candidate), IpAddr::V6(local_ip), IpAddr::V6(mask)) => {
            (u128::from(*candidate) & u128::from(*mask))
                == (u128::from(*local_ip) & u128::from(*mask))
        }
        _ => false,
    }
}

fn sort_candidates_for_dial(candidates: &mut [SocketAddr], local_networks: &[LocalNetwork]) {
    candidates.sort_by_key(|sa| {
        let same_subnet_rank = if local_networks
            .iter()
            .any(|local| same_subnet(&sa.ip(), local))
        {
            0
        } else {
            1
        };
        let family_rank = match sa.ip() {
            IpAddr::V4(_) => 0,
            IpAddr::V6(_) => 1,
        };
        (same_subnet_rank, family_rank)
    });
}

fn is_local_interface_candidate(candidate: &SocketAddr, local_networks: &[LocalNetwork]) -> bool {
    local_networks
        .iter()
        .any(|local| candidate.ip() == local.ip)
}

fn drop_local_interface_candidates(
    candidates: &mut Vec<SocketAddr>,
    local_networks: &[LocalNetwork],
) {
    candidates.retain(|candidate| !is_local_interface_candidate(candidate, local_networks));
}

/// Enumerate this host's advertisable private interface addresses as
/// `ip:port` strings for `ClientMessage::LanAdvertise`. Loopback,
/// public, and link-local-v6 addresses are filtered out. Returns empty
/// (rather than erroring) if interface enumeration fails — the caller
/// just skips advertising and relies on mDNS / relay.
pub fn local_private_candidates(port: u16) -> Vec<String> {
    local_private_candidate_networks(port)
        .into_iter()
        .map(|c| c.addr)
        .collect()
}

/// Same candidates as `local_private_candidates`, annotated with each
/// interface prefix so a new relay can drop VPN/virtual candidates that
/// cannot be on the receiver's subnet. Kept additive on the wire: old
/// relays ignore this field and still use the string-only `candidates`.
pub fn local_private_candidate_networks(port: u16) -> Vec<LanCandidate> {
    let mut out: Vec<String> = Vec::new();
    let mut candidates: Vec<LanCandidate> = Vec::new();
    for local in local_private_networks() {
        let s = SocketAddr::new(local.ip, port).to_string();
        if !out.contains(&s) {
            out.push(s.clone());
            candidates.push(LanCandidate {
                addr: s,
                prefix_len: local.prefix_len,
            });
        }
    }
    candidates
}

#[derive(Debug, thiserror::Error)]
pub enum LanError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("mdns: {0}")]
    Mdns(String),
    #[error("file transfer: {0}")]
    FileTransfer(#[from] FileTransferError),
}

impl From<mdns_sd::Error> for LanError {
    fn from(e: mdns_sd::Error) -> Self {
        LanError::Mdns(e.to_string())
    }
}

/// Live registry of peers we currently have a fully-handshaked LAN session
/// to, keyed by a per-session id (mDNS instance fullname for outbound,
/// `inbound:{remote_addr}` for inbound) so reconnects from a new source
/// port don't clobber a still-alive old session's entry. The value is
/// `(peer_device_id, device_name)`: keep the device id around so UI
/// getters can dedupe transient overlaps (e.g. peer reconnected before
/// the old session's idle timeout fired) and intentional same-device
/// multi-process registrations (iOS main app + keyboard share a did).
pub type PeerRegistry = Arc<std::sync::Mutex<HashMap<String, (String, String)>>>;

/// Snapshot the registry as a list of display names with one entry per
/// logical peer (deduped by device_id). Used by the FFI getters that
/// drive the UI's "局域网: A, B" line — users care about logical peers,
/// not raw TCP sessions.
pub fn unique_peer_names(reg: &PeerRegistry) -> Vec<String> {
    let g = match reg.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut out: Vec<String> = Vec::new();
    for (did, name) in g.values() {
        if seen.insert(did.clone()) {
            out.push(name.clone());
        }
    }
    out
}

/// Information cached from an mDNS `ServiceResolved` event. The reconciler
/// loop scans this every `RECONNECT_INTERVAL` and re-dials any entry that
/// isn't currently in `outbound_peers`, so a dropped TCP recovers without
/// waiting for mDNS to re-announce (which it may never do for a stable
/// peer once the initial cache is hot).
#[derive(Debug, Clone)]
struct KnownPeer {
    /// Device id from the TXT record. Carried so the dial side can
    /// confirm the handshake matches who mDNS thought we were dialing.
    peer_did: String,
    /// Already-filtered, IPv4-first list of addresses to attempt in order.
    candidates: Vec<SocketAddr>,
}

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
    /// TCP port the LAN listener is bound to. Advertised to the relay so
    /// rendezvous peers learn where to dial us.
    port: u16,
    /// This device's id, kept for the control-link tiebreak when ingesting
    /// relay-learned peers (mirrors the mDNS discover path's tiebreak).
    self_device_id: String,
    /// The same `known_peers` map the mDNS discover loop feeds and the
    /// reconciler dials from. `ingest_relay_peers` writes relay-learned
    /// entries here under a `relay:` key namespace so they get dialed
    /// exactly like mDNS-discovered peers.
    known_peers: Arc<Mutex<HashMap<String, KnownPeer>>>,
    /// The same map `fetch_image` reads for blob-fetch addresses. Relay-
    /// learned entries are added/removed here too so LAN image pull works
    /// for peers found via the relay, not just via mDNS.
    peer_addrs: PeerAddrs,
    /// Wakes the reconciler immediately when relay/mDNS learns a new peer.
    /// Without this, startup can sit relay-only until the next 5s tick.
    reconcile_notify: Arc<Notify>,
}

pub(crate) struct LanNodeConfig {
    pub group_id: String,
    pub device_id: String,
    pub device_name: String,
    pub key: [u8; KEY_LEN],
    pub inbound: mpsc::UnboundedSender<IncomingLanClip>,
    pub peer_count: Arc<AtomicUsize>,
    pub peers: PeerRegistry,
    pub blob_cache: SharedBlobCache,
    pub peer_addrs: PeerAddrs,
    pub file_receive_dir: SharedFileReceiveDir,
    pub file_inbound: mpsc::UnboundedSender<ReceivedFile>,
    pub lan_active: Arc<AtomicBool>,
    pub lan_mode_notify: Arc<Notify>,
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
    pub(crate) async fn spawn(config: LanNodeConfig) -> Result<Self, LanError> {
        let LanNodeConfig {
            group_id,
            device_id,
            device_name,
            key,
            inbound,
            peer_count,
            peers,
            blob_cache,
            peer_addrs,
            file_receive_dir,
            file_inbound,
            lan_active,
            lan_mode_notify,
        } = config;

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
        let suffix = format!(
            "{:02x}{:02x}{:02x}{:02x}",
            suffix[0], suffix[1], suffix[2], suffix[3]
        );
        let instance = format!("{}-{}", short_id(&device_id), suffix);
        let hostname = format!("clipbridge-{}.local.", instance);
        let service = ServiceInfo::new(SERVICE_TYPE, &instance, &hostname, "", port, Some(props))?
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

        // Currently-dialing-or-connected outbound peers. Keyed by mDNS
        // instance fullname so two processes on one device count as
        // separate entries (iOS main app vs keyboard extension).
        let outbound_peers: Arc<Mutex<HashMap<String, ()>>> = Arc::new(Mutex::new(HashMap::new()));
        // Long-lived cache of every peer we've ever resolved via mDNS in
        // this group, scrubbed only on `ServiceRemoved`. The reconciler
        // loop scans this every few seconds and re-dials anything not
        // currently in `outbound_peers` — without this we'd never recover
        // from a TCP drop because mDNS rarely re-emits `ServiceResolved`
        // for an unchanged service.
        let known_peers: Arc<Mutex<HashMap<String, KnownPeer>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let reconcile_notify = Arc::new(Notify::new());

        // Accept loop: inbound connections from peers that found us first.
        // Inbound peers don't have a known mDNS fullname (we didn't dial
        // them), so we synthesize one from the connection's remote addr
        // for the purpose of the peer registry. It's just a unique key.
        {
            let accept_key = key;
            let device_id = device_id.clone();
            let device_name = device_name.clone();
            let inbound = inbound.clone();
            let out_tx = out_tx.clone();
            let peer_count = peer_count.clone();
            let peers = peers.clone();
            let blob_cache = blob_cache.clone();
            let file_receive_dir = file_receive_dir.clone();
            let file_inbound = file_inbound.clone();
            let lan_active = lan_active.clone();
            let lan_mode_notify = lan_mode_notify.clone();
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
                    let device_id = device_id.clone();
                    let device_name = device_name.clone();
                    let inbound = inbound.clone();
                    let out_rx = out_tx.subscribe();
                    let peer_count = peer_count.clone();
                    let peers = peers.clone();
                    let blob_cache = blob_cache.clone();
                    let file_receive_dir = file_receive_dir.clone();
                    let file_inbound = file_inbound.clone();
                    let lan_active = lan_active.clone();
                    let lan_mode_notify = lan_mode_notify.clone();
                    let registry_key = format!("inbound:{addr}");
                    tokio::spawn(async move {
                        let peer = PeerRunContext {
                            key: accept_key,
                            self_device_id: device_id,
                            self_device_name: device_name,
                            inbound,
                            out_rx,
                            expected_peer: None,
                            peer_count,
                            peers,
                            blob_cache,
                            registry_key,
                            file_receive_dir,
                            file_inbound,
                            lan_active,
                            lan_mode_notify,
                        };
                        if let Err(e) = run_peer(stream, addr, peer).await {
                            tracing::debug!(?e, %addr, "lan peer (inbound) ended");
                        }
                    });
                }
            });
        }

        // Discover loop: feed `known_peers` from mDNS events. The actual
        // dialing happens in the reconciler below — keeping discovery and
        // (re)connection separate is what lets us recover from TCP drops
        // even when mDNS doesn't re-emit `ServiceResolved`.
        {
            let device_id = device_id.clone();
            let fingerprint = fingerprint.clone();
            let known_peers = known_peers.clone();
            let peer_addrs = peer_addrs.clone();
            let reconcile_notify = reconcile_notify.clone();
            tokio::spawn(async move {
                let mut event_rx = event_rx;
                while let Some(event) = event_rx.recv().await {
                    match event {
                        ServiceEvent::ServiceResolved(info) => {
                            let props = info.get_properties();
                            let peer_gid = props.get_property_val_str("gid").unwrap_or("");
                            let peer_did = props.get_property_val_str("did").unwrap_or("");
                            if peer_gid != fingerprint {
                                continue;
                            }
                            if peer_did.is_empty() {
                                continue;
                            }
                            let port = info.get_port();
                            let local_networks = local_private_networks();
                            // mDNS gives us a HashSet of addresses with
                            // unstable iteration order. iOS in particular
                            // publishes IPv6 link-local on awdl0/utun that
                            // need %scope to dial. Filter & sort: drop
                            // link-local v6, IPv4 first, global v6 second.
                            let mut candidates: Vec<SocketAddr> = info
                                .get_addresses()
                                .iter()
                                .copied()
                                .filter(|a| !is_unroutable(a))
                                .map(|a| SocketAddr::new(a, port))
                                .collect();
                            drop_local_interface_candidates(&mut candidates, &local_networks);
                            sort_candidates_for_dial(&mut candidates, &local_networks);
                            if candidates.is_empty() {
                                continue;
                            }
                            let fullname = info.get_fullname().to_string();
                            // Cache addresses on *both* sides regardless of
                            // the control-link tiebreak below: the blob
                            // fetcher dials whoever has the bytes, which can
                            // be the side that never initiates the control
                            // connection. Keyed by the mDNS instance
                            // `fullname` (not `did`) so the same device's
                            // multiple processes (iOS app + keyboard share a
                            // did) stay distinct entries, and so a
                            // `ServiceRemoved` — which only carries the
                            // fullname — can purge the right one.
                            if let Ok(mut a) = peer_addrs.lock() {
                                a.insert(
                                    fullname.clone(),
                                    PeerAddrEntry {
                                        device_id: peer_did.to_string(),
                                        display_name: None,
                                        candidates: candidates.clone(),
                                    },
                                );
                            }
                            // Lexicographic tiebreak: only the side with the
                            // larger device_id initiates the *control* link.
                            // Equal ids (same physical device — iOS main app
                            // vs keyboard) also short-circuit here.
                            if device_id.as_str() <= peer_did {
                                continue;
                            }
                            let mut g = known_peers.lock().await;
                            g.insert(
                                fullname,
                                KnownPeer {
                                    peer_did: peer_did.to_string(),
                                    candidates,
                                },
                            );
                            reconcile_notify.notify_one();
                        }
                        ServiceEvent::ServiceRemoved(_, fullname) => {
                            // mDNS TTL expired with no re-announcement —
                            // peer is presumed gone. Stop reconnect retries
                            // and drop its cached blob-fetch addresses so
                            // `fetch_image` doesn't burn a dial timeout on a
                            // stale instance before falling back to relay.
                            known_peers.lock().await.remove(&fullname);
                            if let Ok(mut a) = peer_addrs.lock() {
                                a.remove(&fullname);
                            }
                        }
                        _ => {}
                    }
                }
            });
        }

        // Reconciler: every RECONNECT_INTERVAL, dial any known peer that
        // doesn't currently have an in-flight or live outbound session.
        // This is what makes "I disconnected and now I'm back" work
        // automatically — the peer task's disconnect cleanup removes the
        // outbound_peers entry, and the next reconciler tick re-dials.
        {
            let reconnect_key = key;
            let device_id_for_recon = device_id.clone();
            let device_name_for_recon = device_name.clone();
            let inbound = inbound.clone();
            let out_tx = out_tx.clone();
            let outbound_peers = outbound_peers.clone();
            let peer_count = peer_count.clone();
            let peers = peers.clone();
            let known_peers = known_peers.clone();
            let blob_cache = blob_cache.clone();
            let reconcile_notify = reconcile_notify.clone();
            let file_receive_dir = file_receive_dir.clone();
            let file_inbound = file_inbound.clone();
            let lan_active = lan_active.clone();
            let lan_mode_notify = lan_mode_notify.clone();
            tokio::spawn(async move {
                let mut interval = tokio::time::interval(RECONNECT_INTERVAL);
                interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                interval.tick().await; // burn the immediate first tick
                loop {
                    tokio::select! {
                        _ = interval.tick() => {}
                        _ = reconcile_notify.notified() => {}
                        _ = lan_mode_notify.notified() => {}
                    }
                    if !lan_active.load(Ordering::Relaxed) {
                        continue;
                    }
                    let snapshot: Vec<(String, KnownPeer)> = {
                        let g = known_peers.lock().await;
                        g.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
                    };
                    for (fullname, kp) in snapshot {
                        // Reserve the slot atomically with the live-check
                        // so two reconciler ticks (or a stale mDNS event
                        // path) can't race into duplicate dials.
                        {
                            let mut p = outbound_peers.lock().await;
                            if p.contains_key(&fullname) {
                                continue;
                            }
                            p.insert(fullname.clone(), ());
                        }
                        let device_id = device_id_for_recon.clone();
                        let device_name = device_name_for_recon.clone();
                        let inbound = inbound.clone();
                        let out_rx = out_tx.subscribe();
                        let outbound_peers = outbound_peers.clone();
                        let peer_count = peer_count.clone();
                        let peers_for_run = peers.clone();
                        let blob_cache = blob_cache.clone();
                        let file_receive_dir = file_receive_dir.clone();
                        let file_inbound = file_inbound.clone();
                        let lan_active = lan_active.clone();
                        let lan_mode_notify = lan_mode_notify.clone();
                        let registry_key = fullname.clone();
                        let peer_did_owned = kp.peer_did.clone();
                        let candidates = kp.candidates.clone();
                        tokio::spawn(async move {
                            let peer = PeerRunContext {
                                key: reconnect_key,
                                self_device_id: device_id,
                                self_device_name: device_name,
                                inbound,
                                out_rx,
                                expected_peer: Some(peer_did_owned),
                                peer_count,
                                peers: peers_for_run,
                                blob_cache,
                                registry_key,
                                file_receive_dir,
                                file_inbound,
                                lan_active,
                                lan_mode_notify,
                            };
                            let res = dial_and_run(candidates, peer).await;
                            if let Err(e) = res {
                                tracing::debug!(?e, %fullname, "lan peer (outbound) ended");
                            }
                            outbound_peers.lock().await.remove(&fullname);
                        });
                    }
                }
            });
        }

        Ok(Self {
            out_tx,
            peer_count,
            peers,
            _daemon: daemon,
            _forwarder: forwarder,
            port,
            self_device_id: device_id,
            known_peers,
            peer_addrs,
            reconcile_notify,
        })
    }

    /// Port the LAN TCP listener is bound to.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// This host's advertisable private candidates for `LanAdvertise`.
    pub fn advertise_candidates(&self) -> Vec<String> {
        local_private_candidates(self.port)
    }

    /// This host's advertisable private candidates with interface prefix
    /// metadata for relay-side subnet filtering.
    pub fn advertise_candidate_networks(&self) -> Vec<LanCandidate> {
        local_private_candidate_networks(self.port)
    }

    /// Merge a relay-pushed peer snapshot into the same `known_peers` /
    /// `peer_addrs` maps the mDNS path feeds, so relay-discovered peers get
    /// dialed and blob-fetched exactly like mDNS ones.
    ///
    /// `LanPeers` is a *full snapshot*: every previously relay-learned
    /// entry (the `relay:` key namespace) absent from `peers` is purged, so
    /// a departed peer simply stops being dialed — the relay analogue of
    /// mDNS `ServiceRemoved`. mDNS-keyed entries (instance fullnames) are
    /// left untouched. Peers already known via mDNS are skipped for the
    /// control link to avoid a duplicate reciprocal connection.
    pub async fn ingest_relay_peers(&self, peers: Vec<crate::protocol::LanPeer>) {
        // Take the tokio lock first (the only `.await`), then the sync
        // peer_addrs lock — never hold the std mutex across an await.
        let mut known = self.known_peers.lock().await;
        if let Ok(mut a) = self.peer_addrs.lock() {
            apply_relay_snapshot(&self.self_device_id, peers, &mut a, &mut known);
        }
        self.reconcile_notify.notify_one();
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
        unique_peer_names(&self.peers)
    }
}

/// First 8 chars of the device_id, lowercased. Just enough uniqueness for
/// an mDNS instance label / hostname, and short enough to keep DNS happy
/// (instance names cap at 63 bytes per RFC 6763).
fn short_id(device_id: &str) -> String {
    let s: String = device_id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(12)
        .collect();
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
    fn new(
        count: Arc<AtomicUsize>,
        peers: PeerRegistry,
        key: String,
        peer_did: String,
        name: String,
    ) -> Self {
        count.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut g) = peers.lock() {
            g.insert(key.clone(), (peer_did, name));
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

/// Pure core of `LanNode::ingest_relay_peers`, split out so the snapshot
/// semantics can be unit-tested without spawning an mDNS daemon.
///
/// Replaces the entire `relay:` key namespace in both maps with `peers`:
/// `peer_addrs` gets every parsed peer (the blob fetcher dials whoever has
/// the bytes regardless of the control-link tiebreak); `known_peers` gets
/// only peers we should *initiate* the control link to — i.e. our
/// device_id is lexicographically greater (same tiebreak as the mDNS
/// path) and the peer isn't already covered by an mDNS entry. Non-`relay:`
/// keys (mDNS instance fullnames) are left untouched.
fn apply_relay_snapshot(
    self_device_id: &str,
    peers: Vec<crate::protocol::LanPeer>,
    peer_addrs: &mut HashMap<String, PeerAddrEntry>,
    known_peers: &mut HashMap<String, KnownPeer>,
) {
    let local_networks = local_private_networks();
    apply_relay_snapshot_with_local_networks(
        self_device_id,
        peers,
        peer_addrs,
        known_peers,
        &local_networks,
    );
}

fn apply_relay_snapshot_with_local_networks(
    self_device_id: &str,
    peers: Vec<crate::protocol::LanPeer>,
    peer_addrs: &mut HashMap<String, PeerAddrEntry>,
    known_peers: &mut HashMap<String, KnownPeer>,
    local_networks: &[LocalNetwork],
) {
    // device_id -> relay-provided name + dialable candidates, parsed & filtered.
    let mut desired: Vec<(String, Option<String>, Vec<SocketAddr>)> = Vec::new();
    for p in peers {
        if p.device_id == self_device_id {
            continue; // never rendezvous with ourselves
        }
        let mut cands: Vec<SocketAddr> = Vec::new();
        for s in &p.candidates {
            if let Ok(sa) = s.parse::<SocketAddr>() {
                if !is_unroutable(&sa.ip()) && !cands.contains(&sa) {
                    cands.push(sa);
                }
            }
        }
        drop_local_interface_candidates(&mut cands, local_networks);
        sort_candidates_for_dial(&mut cands, local_networks);
        if !cands.is_empty() {
            desired.push((
                p.device_id,
                non_empty_display_name(Some(&p.device_name)),
                cands,
            ));
        }
    }

    peer_addrs.retain(|k, _| !k.starts_with("relay:"));
    for (did, display_name, cands) in &desired {
        peer_addrs.insert(
            format!("relay:{did}"),
            PeerAddrEntry {
                device_id: did.clone(),
                display_name: display_name.clone(),
                candidates: cands.clone(),
            },
        );
    }

    known_peers.retain(|k, _| !k.starts_with("relay:"));
    let mdns_dids: std::collections::HashSet<String> =
        known_peers.values().map(|kp| kp.peer_did.clone()).collect();
    for (peer_did, _, cands) in desired {
        if self_device_id <= peer_did.as_str() {
            continue;
        }
        if mdns_dids.contains(&peer_did) {
            continue; // mDNS already covers this peer's control link
        }
        known_peers.insert(
            format!("relay:{peer_did}"),
            KnownPeer {
                peer_did,
                candidates: cands,
            },
        );
    }
}

struct PeerRunContext {
    key: [u8; KEY_LEN],
    self_device_id: String,
    self_device_name: String,
    inbound: mpsc::UnboundedSender<IncomingLanClip>,
    out_rx: broadcast::Receiver<OutgoingLan>,
    expected_peer: Option<String>,
    peer_count: Arc<AtomicUsize>,
    peers: PeerRegistry,
    blob_cache: SharedBlobCache,
    registry_key: String,
    file_receive_dir: SharedFileReceiveDir,
    file_inbound: mpsc::UnboundedSender<ReceivedFile>,
    lan_active: Arc<AtomicBool>,
    lan_mode_notify: Arc<Notify>,
}

/// Try each candidate address in order with a short per-attempt timeout,
/// then hand the first connected stream to `run_peer`. Used by both the
/// initial-discovery and reconciler paths so dial logic stays in one
/// place.
async fn dial_and_run(candidates: Vec<SocketAddr>, peer: PeerRunContext) -> Result<(), LanError> {
    let mut connected: Option<(TcpStream, SocketAddr)> = None;
    for cand in &candidates {
        match tokio::time::timeout(DIAL_TIMEOUT, TcpStream::connect(cand)).await {
            Ok(Ok(s)) => {
                connected = Some((s, *cand));
                break;
            }
            Ok(Err(e)) => tracing::debug!(?e, %cand, "lan dial failed, trying next"),
            Err(_) => tracing::debug!(%cand, "lan dial timed out, trying next"),
        }
    }
    let (stream, sock) = connected.ok_or_else(|| {
        LanError::Io(std::io::Error::new(
            std::io::ErrorKind::ConnectionRefused,
            "no advertised address was reachable",
        ))
    })?;
    run_peer(stream, sock, peer).await
}

/// Drive one TCP session: send Hello, then concurrently push outbound
/// clips and pull inbound frames until either side closes.
async fn run_peer(
    stream: TcpStream,
    addr: SocketAddr,
    peer: PeerRunContext,
) -> Result<(), LanError> {
    let PeerRunContext {
        key,
        self_device_id,
        self_device_name,
        inbound,
        mut out_rx,
        expected_peer,
        peer_count,
        peers,
        blob_cache,
        registry_key,
        file_receive_dir,
        file_inbound,
        lan_active,
        lan_mode_notify,
    } = peer;

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
    let (peer_did, peer_name) =
        match tokio::time::timeout(Duration::from_secs(5), read_frame(&mut reader, &key)).await {
            Ok(Ok(Some(LanMessage::Hello {
                device_id,
                device_name,
                ..
            }))) => (device_id, device_name),
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

    // Classify this connection. A blob requester sends `BlobRequest` as its
    // first post-Hello frame; a control peer sends nothing on its own until
    // its first ping. We send an immediate Ping so a *new* control peer can
    // be classified (and registered) within an RTT instead of waiting out
    // PING_INTERVAL; a blob connection's BlobRequest still wins the race.
    // Timeout ⇒ assume control peer (an old build that doesn't immediate-
    // ping — costs a few seconds of registration lag during rollout only).
    // Serving a blob here never touches the peer registry/count: it's a
    // throwaway connection, not a mesh edge.
    write_frame(&mut writer, &key, &LanMessage::Ping).await?;
    let pending: Option<LanMessage> =
        match tokio::time::timeout(Duration::from_secs(5), read_frame(&mut reader, &key)).await {
            Ok(Ok(Some(LanMessage::BlobRequest { sha256_hex }))) => {
                return serve_blob(&mut writer, &key, &blob_cache, &sha256_hex).await;
            }
            Ok(Ok(Some(LanMessage::FileOffer { offer }))) => {
                let receive_dir = file_receive_dir
                    .lock()
                    .ok()
                    .and_then(|dir| dir.as_ref().cloned());
                let Some(receive_dir) = receive_dir else {
                    reject_file_offer(
                        &mut writer,
                        &key,
                        &offer.transfer_id,
                        "file receiving is not enabled",
                    )
                    .await?;
                    return Ok(());
                };
                let received = receive_offered_file(
                    &mut reader,
                    &mut writer,
                    offer,
                    ReceiveOfferContext {
                        key: &key,
                        self_device_id: &self_device_id,
                        peer_device_id: &peer_did,
                        destination_dir: receive_dir,
                        config: FileTransferConfig::default(),
                    },
                )
                .await?;
                let _ = file_inbound.send(received);
                return Ok(());
            }
            Ok(Ok(Some(other))) => Some(other),
            Ok(Ok(None)) => return Ok(()), // peer closed right after Hello
            Ok(Err(e)) => return Err(e),
            Err(_) => None, // timed out: treat as a (possibly old) control peer
        };

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
    let _peer_guard = PeerSessionGuard::new(
        peer_count,
        peers,
        registry_key,
        peer_did.clone(),
        display_name.clone(),
    );
    tracing::info!(%addr, peer = %display_name, "lan peer up");

    // Single select! for outbound writes (clips + ping), inbound reads,
    // and an idle deadline. This keeps writes and reads serialized which
    // is fine over a short-lived lock-step LAN connection, and lets the
    // idle check share state with both sides.
    let mut ping_interval = tokio::time::interval(PING_INTERVAL);
    ping_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    ping_interval.tick().await; // burn the immediate first tick
    let mut last_seen = tokio::time::Instant::now();

    // The classify read above may have already consumed the peer's first
    // real frame (typically its immediate Ping, or a clip that beat it).
    // Process it before the select loop so it isn't dropped.
    if let Some(LanMessage::Clip {
        sender_device_id,
        ts,
        payload,
    }) = &pending
    {
        let _ = inbound.send(IncomingLanClip {
            sender_device_id: sender_device_id.clone(),
            ts: *ts,
            payload: payload.clone(),
        });
    }
    if matches!(pending, Some(LanMessage::Ping)) && !lan_active.load(Ordering::Relaxed) {
        write_frame(&mut writer, &key, &LanMessage::Ping).await?;
    }

    loop {
        let idle_deadline = last_seen + IDLE_TIMEOUT;
        tokio::select! {
            biased;
            // Idle check first so a saturated read loop can't keep us
            // hanging on a dead peer past the deadline.
            _ = tokio::time::sleep_until(idle_deadline) => {
                tracing::debug!(peer = %display_name, "lan idle timeout, closing");
                return Err(LanError::Io(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "no inbound frame within idle window",
                )));
            }
            _ = ping_interval.tick(), if lan_active.load(Ordering::Relaxed) => {
                write_frame(&mut writer, &key, &LanMessage::Ping).await?;
            }
            _ = lan_mode_notify.notified() => {
                tracing::debug!(
                    peer = %display_name,
                    active = lan_active.load(Ordering::Relaxed),
                    "lan active mode changed"
                );
            }
            out = out_rx.recv(), if lan_active.load(Ordering::Relaxed) => {
                match out {
                    Ok(out) => {
                        // Don't echo a clip back to the device that
                        // originally sent it.
                        if out.sender_device_id == peer_did {
                            continue;
                        }
                        let msg = LanMessage::Clip {
                            sender_device_id: out.sender_device_id,
                            ts: out.ts,
                            payload: out.payload,
                        };
                        write_frame(&mut writer, &key, &msg).await?;
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(skipped = n, "lan broadcast lagged");
                    }
                    Err(broadcast::error::RecvError::Closed) => return Ok(()),
                }
            }
            frame = read_frame(&mut reader, &key) => {
                last_seen = tokio::time::Instant::now();
                match frame? {
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
                    Some(LanMessage::Ping) => {
                        // Active peers send their own periodic Ping. In
                        // inactive Android standby we stay passive, but echo
                        // a Ping so a desktop probe can prove we're alive on
                        // the LAN without re-enabling discovery/reconnects.
                        if !lan_active.load(Ordering::Relaxed) {
                            write_frame(&mut writer, &key, &LanMessage::Ping).await?;
                        }
                    }
                    Some(LanMessage::BlobRequest { .. })
                    | Some(LanMessage::BlobChunk { .. })
                    | Some(LanMessage::BlobMiss { .. })
                    | Some(LanMessage::FileOffer { .. })
                    | Some(LanMessage::FileAccept { .. })
                    | Some(LanMessage::FileReject { .. })
                    | Some(LanMessage::FileChunk { .. })
                    | Some(LanMessage::FileComplete { .. })
                    | Some(LanMessage::FileCancel { .. }) => {
                        // Bulk-transfer traffic only ever rides a dedicated,
                        // freshly classified connection. Seeing it on an
                        // established control link means a buggy/old/hostile
                        // peer — ignore rather than tear down clip sync.
                    }
                    None => return Ok(()),
                }
            }
        }
    }
}

pub async fn send_file_to_stream(
    stream: TcpStream,
    key: [u8; KEY_LEN],
    request: SendFileRequest,
) -> Result<SentFile, LanError> {
    let SendFileRequest {
        source_device_id,
        source_device_name,
        target_device_id,
        source_path,
        mime_type,
        config,
    } = request;
    let mut outgoing = OutgoingFile::open(
        source_device_id.clone(),
        target_device_id.clone(),
        &source_path,
        mime_type,
        config,
    )?;
    let offer = outgoing.offer.clone();

    let _ = stream.set_nodelay(true);
    let (mut reader, mut writer) = stream.into_split();
    write_hello(&mut writer, &key, &source_device_id, &source_device_name).await?;
    let (peer_device_id, _) = read_hello(&mut reader, &key).await?;
    if peer_device_id != target_device_id {
        return Err(FileTransferError::PeerMismatch {
            expected: target_device_id,
            got: peer_device_id,
        }
        .into());
    }

    write_frame(
        &mut writer,
        &key,
        &LanMessage::FileOffer {
            offer: offer.clone(),
        },
    )
    .await?;

    loop {
        match read_frame(&mut reader, &key).await? {
            Some(LanMessage::FileAccept { transfer_id }) if transfer_id == offer.transfer_id => {
                break;
            }
            Some(LanMessage::FileReject { reason, .. }) => {
                return Err(FileTransferError::Rejected { reason }.into());
            }
            Some(LanMessage::Ping) | Some(LanMessage::Hello { .. }) => continue,
            Some(other) => {
                return Err(FileTransferError::UnexpectedFrame {
                    frame: frame_label(&other).into(),
                }
                .into());
            }
            None => {
                return Err(FileTransferError::Canceled {
                    reason: "peer closed before accepting file offer".into(),
                }
                .into());
            }
        }
    }

    let mut offset = 0u64;
    let mut buf = Vec::new();
    loop {
        let read = outgoing.read_chunk(&mut buf)?;
        if read == 0 {
            break;
        }
        write_frame(
            &mut writer,
            &key,
            &LanMessage::FileChunk {
                transfer_id: offer.transfer_id.clone(),
                offset,
                data: buf.clone(),
            },
        )
        .await?;
        offset += read as u64;
    }
    write_frame(
        &mut writer,
        &key,
        &LanMessage::FileComplete {
            transfer_id: offer.transfer_id.clone(),
        },
    )
    .await?;

    Ok(SentFile {
        transfer_id: offer.transfer_id,
        file_name: offer.file_name,
        bytes_sent: offset,
        sha256_hex: offer.sha256_hex,
    })
}

async fn send_file_to_candidates(
    candidates: &[SocketAddr],
    key: [u8; KEY_LEN],
    request: SendFileRequest,
) -> Result<SentFile, LanError> {
    let mut connected: Option<TcpStream> = None;
    for cand in candidates {
        match tokio::time::timeout(DIAL_TIMEOUT, TcpStream::connect(cand)).await {
            Ok(Ok(s)) => {
                connected = Some(s);
                break;
            }
            Ok(Err(e)) => tracing::debug!(?e, %cand, "file transfer dial failed, trying next"),
            Err(_) => tracing::debug!(%cand, "file transfer dial timed out, trying next"),
        }
    }
    let stream = connected.ok_or_else(|| {
        LanError::Io(std::io::Error::new(
            std::io::ErrorKind::ConnectionRefused,
            "no advertised address was reachable for file transfer",
        ))
    })?;
    send_file_to_stream(stream, key, request).await
}

pub fn lan_send_file(
    candidates: Vec<SocketAddr>,
    key: [u8; KEY_LEN],
    request: SendFileRequest,
) -> Result<SentFile, LanError> {
    if candidates.is_empty() {
        return Err(LanError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "no LAN candidates for file transfer",
        )));
    }
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        rt.block_on(send_file_to_candidates(&candidates, key, request))
    })
    .join()
    .map_err(|_| {
        LanError::Io(std::io::Error::other(
            "file transfer worker thread panicked",
        ))
    })?
}

pub async fn receive_file_from_stream(
    stream: TcpStream,
    key: [u8; KEY_LEN],
    self_device_id: String,
    self_device_name: String,
    destination_dir: PathBuf,
    config: FileTransferConfig,
) -> Result<ReceivedFile, LanError> {
    let _ = stream.set_nodelay(true);
    let (mut reader, mut writer) = stream.into_split();
    write_hello(&mut writer, &key, &self_device_id, &self_device_name).await?;
    let (peer_device_id, _) = read_hello(&mut reader, &key).await?;

    let offer = match read_frame(&mut reader, &key).await? {
        Some(LanMessage::FileOffer { offer }) => offer,
        Some(other) => {
            return Err(FileTransferError::UnexpectedFrame {
                frame: frame_label(&other).into(),
            }
            .into());
        }
        None => {
            return Err(FileTransferError::Canceled {
                reason: "peer closed before sending file offer".into(),
            }
            .into());
        }
    };

    receive_offered_file(
        &mut reader,
        &mut writer,
        offer,
        ReceiveOfferContext {
            key: &key,
            self_device_id: &self_device_id,
            peer_device_id: &peer_device_id,
            destination_dir,
            config,
        },
    )
    .await
}

struct ReceiveOfferContext<'a> {
    key: &'a [u8; KEY_LEN],
    self_device_id: &'a str,
    peer_device_id: &'a str,
    destination_dir: PathBuf,
    config: FileTransferConfig,
}

async fn receive_offered_file<R, W>(
    reader: &mut R,
    writer: &mut W,
    offer: FileOffer,
    ctx: ReceiveOfferContext<'_>,
) -> Result<ReceivedFile, LanError>
where
    R: AsyncReadExt + Unpin,
    W: AsyncWriteExt + Unpin,
{
    let ReceiveOfferContext {
        key,
        self_device_id,
        peer_device_id,
        destination_dir,
        config,
    } = ctx;
    if offer.target_device_id != self_device_id {
        let reason = "file offer targeted a different device".to_string();
        reject_file_offer(writer, key, &offer.transfer_id, &reason).await?;
        return Err(FileTransferError::PeerMismatch {
            expected: self_device_id.to_string(),
            got: offer.target_device_id,
        }
        .into());
    }
    if offer.source_device_id != peer_device_id {
        let reason = "file offer source did not match handshake".to_string();
        reject_file_offer(writer, key, &offer.transfer_id, &reason).await?;
        return Err(FileTransferError::PeerMismatch {
            expected: peer_device_id.to_string(),
            got: offer.source_device_id,
        }
        .into());
    }

    let mut incoming = match IncomingFileWriter::accept(offer.clone(), &destination_dir, config) {
        Ok(writer) => writer,
        Err(e) => {
            let reason = e.to_string();
            reject_file_offer(writer, key, &offer.transfer_id, &reason).await?;
            return Err(e.into());
        }
    };
    write_frame(
        writer,
        key,
        &LanMessage::FileAccept {
            transfer_id: offer.transfer_id.clone(),
        },
    )
    .await?;

    loop {
        match read_frame(reader, key).await? {
            Some(LanMessage::FileChunk {
                transfer_id,
                offset,
                data,
            }) if transfer_id == incoming.transfer_id() => {
                if let Err(e) = incoming.write_chunk(offset, &data) {
                    let reason = e.to_string();
                    let _ = write_frame(
                        writer,
                        key,
                        &LanMessage::FileCancel {
                            transfer_id,
                            reason,
                        },
                    )
                    .await;
                    return Err(e.into());
                }
            }
            Some(LanMessage::FileComplete { transfer_id })
                if transfer_id == incoming.transfer_id() =>
            {
                return incoming.finish().map_err(Into::into);
            }
            Some(LanMessage::FileCancel { reason, .. }) => {
                return Err(FileTransferError::Canceled { reason }.into());
            }
            Some(other) => {
                return Err(FileTransferError::UnexpectedFrame {
                    frame: frame_label(&other).into(),
                }
                .into());
            }
            None => {
                return Err(FileTransferError::Canceled {
                    reason: "peer closed before file transfer completed".into(),
                }
                .into());
            }
        }
    }
}

async fn write_hello<W: AsyncWriteExt + Unpin>(
    writer: &mut W,
    key: &[u8; KEY_LEN],
    device_id: &str,
    device_name: &str,
) -> Result<(), LanError> {
    write_frame(
        writer,
        key,
        &LanMessage::Hello {
            device_id: device_id.to_string(),
            version: PROTO_VERSION,
            device_name: device_name.to_string(),
        },
    )
    .await
}

async fn read_hello<R: AsyncReadExt + Unpin>(
    reader: &mut R,
    key: &[u8; KEY_LEN],
) -> Result<(String, String), LanError> {
    match read_frame(reader, key).await? {
        Some(LanMessage::Hello {
            device_id,
            device_name,
            ..
        }) => Ok((device_id, device_name)),
        Some(other) => Err(FileTransferError::UnexpectedFrame {
            frame: frame_label(&other).into(),
        }
        .into()),
        None => Err(FileTransferError::Canceled {
            reason: "peer closed during hello handshake".into(),
        }
        .into()),
    }
}

async fn reject_file_offer<W: AsyncWriteExt + Unpin>(
    writer: &mut W,
    key: &[u8; KEY_LEN],
    transfer_id: &str,
    reason: &str,
) -> Result<(), LanError> {
    write_frame(
        writer,
        key,
        &LanMessage::FileReject {
            transfer_id: transfer_id.to_string(),
            reason: reason.to_string(),
        },
    )
    .await
}

fn frame_label(msg: &LanMessage) -> &'static str {
    match msg {
        LanMessage::Hello { .. } => "hello",
        LanMessage::Clip { .. } => "clip",
        LanMessage::Ping => "ping",
        LanMessage::BlobRequest { .. } => "blob_request",
        LanMessage::BlobChunk { .. } => "blob_chunk",
        LanMessage::BlobMiss { .. } => "blob_miss",
        LanMessage::FileOffer { .. } => "file_offer",
        LanMessage::FileAccept { .. } => "file_accept",
        LanMessage::FileReject { .. } => "file_reject",
        LanMessage::FileChunk { .. } => "file_chunk",
        LanMessage::FileComplete { .. } => "file_complete",
        LanMessage::FileCancel { .. } => "file_cancel",
    }
}

/// Stream the cached ciphertext for `sha256_hex` as `BlobChunk` frames, or
/// a single `BlobMiss` if we don't have it. Runs on a dedicated throwaway
/// connection so chunking here never head-of-line-blocks clipboard frames.
async fn serve_blob<W: AsyncWriteExt + Unpin>(
    w: &mut W,
    key: &[u8; KEY_LEN],
    cache: &SharedBlobCache,
    sha256_hex: &str,
) -> Result<(), LanError> {
    let Some(bytes) = cache.get(sha256_hex) else {
        return write_frame(
            w,
            key,
            &LanMessage::BlobMiss {
                sha256_hex: sha256_hex.to_string(),
            },
        )
        .await;
    };
    let total = bytes.len();
    if total == 0 {
        return write_frame(
            w,
            key,
            &LanMessage::BlobChunk {
                data: Vec::new(),
                last: true,
            },
        )
        .await;
    }
    let mut off = 0;
    while off < total {
        let end = (off + BLOB_CHUNK).min(total);
        write_frame(
            w,
            key,
            &LanMessage::BlobChunk {
                data: bytes[off..end].to_vec(),
                last: end == total,
            },
        )
        .await?;
        off = end;
    }
    Ok(())
}

/// Dial a peer on a fresh connection, Hello, ask for `sha256_hex`, and
/// accumulate the streamed ciphertext. Returns `None` (→ caller falls back
/// to the relay) on any dial/handshake/timeout failure, a `BlobMiss`, or if
/// the stream exceeds `max_bytes` (cheap guard against a buggy/hostile
/// peer; the caller still verifies sha256 before trusting the bytes).
async fn fetch_blob(
    candidates: &[SocketAddr],
    key: &[u8; KEY_LEN],
    self_device_id: &str,
    self_device_name: &str,
    sha256_hex: &str,
    max_bytes: usize,
) -> Option<Vec<u8>> {
    let mut stream = None;
    for cand in candidates {
        if let Ok(Ok(s)) = tokio::time::timeout(DIAL_TIMEOUT, TcpStream::connect(cand)).await {
            stream = Some(s);
            break;
        }
    }
    let stream = stream?;
    let _ = stream.set_nodelay(true);
    let (mut reader, mut writer) = stream.into_split();

    write_frame(
        &mut writer,
        key,
        &LanMessage::Hello {
            device_id: self_device_id.to_string(),
            version: PROTO_VERSION,
            device_name: self_device_name.to_string(),
        },
    )
    .await
    .ok()?;
    write_frame(
        &mut writer,
        key,
        &LanMessage::BlobRequest {
            sha256_hex: sha256_hex.to_string(),
        },
    )
    .await
    .ok()?;

    let mut buf: Vec<u8> = Vec::new();
    loop {
        match tokio::time::timeout(Duration::from_secs(10), read_frame(&mut reader, key)).await {
            Ok(Ok(Some(LanMessage::BlobChunk { data, last }))) => {
                buf.extend_from_slice(&data);
                if buf.len() > max_bytes {
                    return None;
                }
                if last {
                    return Some(buf);
                }
            }
            Ok(Ok(Some(LanMessage::BlobMiss { .. }))) => return None,
            // The serving side immediate-pings before classifying our
            // request, and may echo a Hello; skip non-blob frames.
            Ok(Ok(Some(LanMessage::Ping)))
            | Ok(Ok(Some(LanMessage::Hello { .. })))
            | Ok(Ok(Some(LanMessage::Clip { .. }))) => continue,
            Ok(Ok(Some(LanMessage::BlobRequest { .. })))
            | Ok(Ok(Some(LanMessage::FileOffer { .. })))
            | Ok(Ok(Some(LanMessage::FileAccept { .. })))
            | Ok(Ok(Some(LanMessage::FileReject { .. })))
            | Ok(Ok(Some(LanMessage::FileChunk { .. })))
            | Ok(Ok(Some(LanMessage::FileComplete { .. })))
            | Ok(Ok(Some(LanMessage::FileCancel { .. }))) => return None,
            Ok(Ok(None)) | Ok(Err(_)) | Err(_) => return None,
        }
    }
}

/// Sync wrapper over [`fetch_blob`] for the FFI `fetch_image` path, which
/// runs on the host's calling thread outside any tokio runtime. Mirrors
/// `blob.rs`'s per-call thread + current-thread runtime trampoline so it
/// can't panic with "runtime in runtime".
pub fn lan_fetch_blob(
    candidates: Vec<SocketAddr>,
    key: [u8; KEY_LEN],
    self_device_id: String,
    self_device_name: String,
    sha256_hex: String,
    max_bytes: usize,
) -> Option<Vec<u8>> {
    if candidates.is_empty() {
        return None;
    }
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .ok()?;
        rt.block_on(fetch_blob(
            &candidates,
            &key,
            &self_device_id,
            &self_device_name,
            &sha256_hex,
            max_bytes,
        ))
    })
    .join()
    .ok()
    .flatten()
}

/// Wire frame: `len:u32 BE | nonce:12 | ciphertext` where `len` covers
/// nonce+ciphertext. AEAD = ChaCha20-Poly1305 with the group key.
async fn write_frame<W: AsyncWriteExt + Unpin>(
    w: &mut W,
    key: &[u8; KEY_LEN],
    msg: &LanMessage,
) -> Result<(), LanError> {
    let plain = serde_json::to_vec(msg)
        .map_err(|e| LanError::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, e)))?;
    let (cipher, nonce) =
        encrypt(key, &plain).map_err(|e| LanError::Io(std::io::Error::other(e.to_string())))?;
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
    let msg: LanMessage = serde_json::from_slice(&plain)
        .map_err(|e| LanError::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, e)))?;
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

    #[test]
    fn blob_cache_evicts_oldest_by_count() {
        let c = BlobCache::new();
        for i in 0..(BLOB_CACHE_MAX_ENTRIES + 3) {
            c.insert(format!("k{i}"), Arc::new(vec![0u8; 8]));
        }
        // Oldest keys evicted, newest retained.
        assert!(c.get("k0").is_none());
        assert!(c.get(&format!("k{}", BLOB_CACHE_MAX_ENTRIES + 2)).is_some());
    }

    #[test]
    fn blob_cache_dedups_same_key() {
        let c = BlobCache::new();
        c.insert("k".into(), Arc::new(vec![1, 2, 3]));
        c.insert("k".into(), Arc::new(vec![9, 9, 9, 9])); // ignored: content-addressed
        assert_eq!(*c.get("k").unwrap(), vec![1, 2, 3]);
    }

    #[test]
    fn advertisable_private_classification() {
        use std::net::{Ipv4Addr, Ipv6Addr};
        let v4 = |a, b, c, d| IpAddr::V4(Ipv4Addr::new(a, b, c, d));
        assert!(is_advertisable_private(&v4(192, 168, 1, 5))); // RFC1918
        assert!(is_advertisable_private(&v4(10, 0, 0, 1)));
        assert!(is_advertisable_private(&v4(172, 16, 9, 9)));
        assert!(is_advertisable_private(&v4(169, 254, 3, 3))); // link-local v4
        assert!(!is_advertisable_private(&v4(8, 8, 8, 8))); // public withheld
        assert!(is_advertisable_private(&IpAddr::V6(Ipv6Addr::new(
            0xfd00, 0, 0, 0, 0, 0, 0, 1
        )))); // ULA
        assert!(!is_advertisable_private(&IpAddr::V6(Ipv6Addr::new(
            0xfe80, 0, 0, 0, 0, 0, 0, 1
        )))); // link-local v6 excluded (needs %scope)
    }

    #[test]
    fn local_candidates_are_private_and_carry_port() {
        // Machine-dependent set, but every entry must parse, be private,
        // and carry the requested port.
        for s in local_private_candidates(54321) {
            let sa: SocketAddr = s.parse().expect("parses as ip:port");
            assert_eq!(sa.port(), 54321);
            assert!(is_advertisable_private(&sa.ip()), "{sa} not private");
            assert!(!sa.ip().is_loopback());
        }
    }

    fn lp(did: &str, cands: &[&str]) -> crate::protocol::LanPeer {
        crate::protocol::LanPeer {
            device_id: did.into(),
            device_name: format!("{did} name"),
            candidates: cands.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn relay_snapshot_tiebreak_and_peer_addrs() {
        let mut pa: HashMap<String, PeerAddrEntry> = HashMap::new();
        let mut kp: HashMap<String, KnownPeer> = HashMap::new();

        // self = "dev-m". Peer "dev-a" < self -> we initiate (in
        // known_peers). Peer "dev-z" > self -> they initiate (NOT in
        // known_peers) but still in peer_addrs for blob pull.
        apply_relay_snapshot(
            "dev-m",
            vec![
                lp("dev-a", &["192.168.1.2:5000"]),
                lp("dev-z", &["192.168.1.9:6000"]),
                lp("dev-m", &["192.168.1.1:4000"]), // ourselves: dropped
            ],
            &mut pa,
            &mut kp,
        );

        assert!(pa.contains_key("relay:dev-a"));
        assert!(pa.contains_key("relay:dev-z"));
        assert!(!pa.contains_key("relay:dev-m"));
        assert!(kp.contains_key("relay:dev-a"), "smaller peer: we dial");
        assert!(
            !kp.contains_key("relay:dev-z"),
            "larger peer dials us, not in known_peers"
        );
    }

    #[test]
    fn relay_snapshot_purges_departed_and_keeps_mdns() {
        let mut pa: HashMap<String, PeerAddrEntry> = HashMap::new();
        let mut kp: HashMap<String, KnownPeer> = HashMap::new();
        // Pre-existing mDNS entry (non-relay key) must survive.
        kp.insert(
            "Mac._clipbridge._tcp.local.".into(),
            KnownPeer {
                peer_did: "dev-mdns".into(),
                candidates: vec!["192.168.1.50:7000".parse().unwrap()],
            },
        );
        pa.insert(
            "Mac._clipbridge._tcp.local.".into(),
            PeerAddrEntry {
                device_id: "dev-mdns".into(),
                display_name: None,
                candidates: vec!["192.168.1.50:7000".parse().unwrap()],
            },
        );

        apply_relay_snapshot(
            "dev-m",
            vec![lp("dev-a", &["192.168.1.2:5000"])],
            &mut pa,
            &mut kp,
        );
        assert!(kp.contains_key("relay:dev-a"));
        assert!(kp.contains_key("Mac._clipbridge._tcp.local."));

        // Next snapshot no longer lists dev-a -> its relay entry is purged,
        // mDNS entry still untouched.
        apply_relay_snapshot("dev-m", vec![], &mut pa, &mut kp);
        assert!(!kp.contains_key("relay:dev-a"), "departed peer purged");
        assert!(!pa.contains_key("relay:dev-a"));
        assert!(
            kp.contains_key("Mac._clipbridge._tcp.local."),
            "mDNS survives"
        );
        assert!(pa.contains_key("Mac._clipbridge._tcp.local."));
    }

    #[test]
    fn relay_snapshot_skips_peer_already_known_via_mdns() {
        let mut pa: HashMap<String, PeerAddrEntry> = HashMap::new();
        let mut kp: HashMap<String, KnownPeer> = HashMap::new();
        kp.insert(
            "A._clipbridge._tcp.local.".into(),
            KnownPeer {
                peer_did: "dev-a".into(),
                candidates: vec!["192.168.1.2:5000".parse().unwrap()],
            },
        );
        // dev-a < self so normally we'd add it, but mDNS already covers
        // its control link -> no duplicate relay: entry in known_peers.
        apply_relay_snapshot(
            "dev-m",
            vec![lp("dev-a", &["192.168.1.2:9999"])],
            &mut pa,
            &mut kp,
        );
        assert!(!kp.contains_key("relay:dev-a"), "mDNS-covered peer skipped");
        // peer_addrs still gets it (blob fetch is tiebreak-independent).
        assert!(pa.contains_key("relay:dev-a"));
    }

    #[test]
    fn relay_snapshot_drops_unroutable_and_unparseable() {
        let mut pa: HashMap<String, PeerAddrEntry> = HashMap::new();
        let mut kp: HashMap<String, KnownPeer> = HashMap::new();
        apply_relay_snapshot(
            "dev-m",
            vec![
                lp("dev-a", &["not-an-addr", "[fe80::1]:5000"]), // both rejected
                lp("dev-b", &["garbage"]),
            ],
            &mut pa,
            &mut kp,
        );
        assert!(pa.is_empty(), "no usable candidates -> nothing added");
        assert!(kp.is_empty());
    }

    #[test]
    fn relay_snapshot_drops_candidates_on_local_interface_only() {
        let local_networks = vec![LocalNetwork {
            ip: "192.168.1.10".parse().unwrap(),
            netmask: "255.255.255.0".parse().unwrap(),
            prefix_len: 24,
        }];
        let mut pa: HashMap<String, PeerAddrEntry> = HashMap::new();
        let mut kp: HashMap<String, KnownPeer> = HashMap::new();

        apply_relay_snapshot_with_local_networks(
            "dev-m",
            vec![
                lp("old-self", &["192.168.1.10:5000"]),
                lp("dev-a", &["192.168.1.10:6000", "192.168.1.11:6000"]),
            ],
            &mut pa,
            &mut kp,
            &local_networks,
        );

        assert!(
            !pa.contains_key("relay:old-self"),
            "same host candidate should not appear as a stale peer"
        );
        let peer = pa.get("relay:dev-a").expect("same subnet peer remains");
        assert_eq!(peer.candidates, vec!["192.168.1.11:6000".parse().unwrap()]);
        assert!(kp.contains_key("relay:dev-a"));
    }

    #[test]
    fn peer_records_dedupe_by_device_id_and_keep_candidates() {
        let peers: PeerRegistry = Arc::new(std::sync::Mutex::new(HashMap::new()));
        peers.lock().unwrap().insert(
            "Mac._clipbridge._tcp.local.".into(),
            ("dev-a".into(), "Mac".into()),
        );
        peers.lock().unwrap().insert(
            "inbound:127.0.0.1:5555".into(),
            ("dev-a".into(), "Mac duplicate".into()),
        );
        peers
            .lock()
            .unwrap()
            .insert("relay:dev-b".into(), ("dev-b".into(), "Android".into()));

        let addrs: PeerAddrs = Arc::new(std::sync::Mutex::new(HashMap::new()));
        addrs.lock().unwrap().insert(
            "Mac._clipbridge._tcp.local.".into(),
            PeerAddrEntry {
                device_id: "dev-a".into(),
                display_name: None,
                candidates: vec!["192.168.1.20:5000".parse().unwrap()],
            },
        );
        addrs.lock().unwrap().insert(
            "relay:dev-b".into(),
            PeerAddrEntry {
                device_id: "dev-b".into(),
                display_name: None,
                candidates: vec!["192.168.1.30:5000".parse().unwrap()],
            },
        );

        let records = peer_records(&peers, &addrs);

        assert_eq!(records.len(), 2);
        assert_eq!(records[0].device_id, "dev-b");
        assert_eq!(records[0].display_name, "Android");
        assert_eq!(records[0].candidate_count, 1);
        assert_eq!(records[1].device_id, "dev-a");
        assert_eq!(records[1].display_name, "Mac");
        assert_eq!(
            candidates_for_peer(&addrs, "dev-a"),
            vec!["192.168.1.20:5000".parse().unwrap()]
        );
    }

    #[test]
    fn peer_records_use_relay_advertised_device_name_before_handshake() {
        let peers: PeerRegistry = Arc::new(std::sync::Mutex::new(HashMap::new()));
        let addrs: PeerAddrs = Arc::new(std::sync::Mutex::new(HashMap::new()));
        addrs.lock().unwrap().insert(
            "relay:android".into(),
            PeerAddrEntry {
                device_id: "8eb4001d9a41f00d".into(),
                display_name: Some("SM-S9380".into()),
                candidates: vec!["192.168.1.30:5000".parse().unwrap()],
            },
        );

        let records = peer_records(&peers, &addrs);

        assert_eq!(records.len(), 1);
        assert_eq!(records[0].display_name, "SM-S9380");
        assert_eq!(records[0].candidate_count, 1);
    }

    #[test]
    fn dial_candidates_prefer_same_subnet_addresses() {
        let local = vec![
            LocalNetwork {
                ip: "192.168.248.22".parse().unwrap(),
                netmask: "255.255.255.0".parse().unwrap(),
                prefix_len: 24,
            },
            LocalNetwork {
                ip: "10.37.129.2".parse().unwrap(),
                netmask: "255.255.255.0".parse().unwrap(),
                prefix_len: 24,
            },
        ];
        let mut candidates: Vec<SocketAddr> = vec![
            "10.211.55.2:49901".parse().unwrap(),
            "[fd7a:115c:a1e0::2f34:f655]:49901".parse().unwrap(),
            "192.168.248.40:49901".parse().unwrap(),
            "10.37.129.44:49901".parse().unwrap(),
        ];

        sort_candidates_for_dial(&mut candidates, &local);

        assert_eq!(candidates[0], "192.168.248.40:49901".parse().unwrap());
        assert_eq!(candidates[1], "10.37.129.44:49901".parse().unwrap());
    }

    /// `fetch_blob` over a real localhost socket against a hand-rolled
    /// server that speaks just enough of the protocol (`read Hello` →
    /// `read BlobRequest` → `serve_blob`). Multi-chunk payload exercises
    /// the chunk loop. No mDNS, no multicast — safe in sandboxed CI.
    #[tokio::test]
    async fn blob_round_trip_multi_chunk() {
        let key = [7u8; KEY_LEN];
        let sha = "deadbeef".to_string();
        let payload = vec![0xABu8; BLOB_CHUNK + BLOB_CHUNK / 2 + 17];

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let cache = BlobCache::new();
        cache.insert(sha.clone(), Arc::new(payload.clone()));

        tokio::spawn(async move {
            let (s, _) = listener.accept().await.unwrap();
            let (mut r, mut w) = s.into_split();
            // Requester sends Hello then BlobRequest.
            let _ = read_frame(&mut r, &key).await.unwrap().unwrap();
            match read_frame(&mut r, &key).await.unwrap().unwrap() {
                LanMessage::BlobRequest { sha256_hex } => {
                    serve_blob(&mut w, &key, &cache, &sha256_hex).await.unwrap();
                }
                _ => panic!("expected BlobRequest"),
            }
        });

        let got = fetch_blob(&[addr], &key, "me", "me-name", &sha, payload.len() + 1024).await;
        assert_eq!(got, Some(payload));
    }

    #[tokio::test]
    async fn blob_miss_returns_none() {
        let key = [3u8; KEY_LEN];
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let cache = BlobCache::new(); // empty → BlobMiss

        tokio::spawn(async move {
            let (s, _) = listener.accept().await.unwrap();
            let (mut r, mut w) = s.into_split();
            let _ = read_frame(&mut r, &key).await.unwrap().unwrap();
            match read_frame(&mut r, &key).await.unwrap().unwrap() {
                LanMessage::BlobRequest { sha256_hex } => {
                    serve_blob(&mut w, &key, &cache, &sha256_hex).await.unwrap();
                }
                _ => panic!("expected BlobRequest"),
            }
        });

        let got = fetch_blob(&[addr], &key, "me", "me-name", "nope", 4096).await;
        assert_eq!(got, None);
    }

    #[tokio::test]
    async fn blob_fetch_aborts_when_over_max_bytes() {
        let key = [5u8; KEY_LEN];
        let sha = "cafe".to_string();
        let payload = vec![1u8; BLOB_CHUNK * 2];
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let cache = BlobCache::new();
        cache.insert(sha.clone(), Arc::new(payload));

        tokio::spawn(async move {
            let (s, _) = listener.accept().await.unwrap();
            let (mut r, mut w) = s.into_split();
            let _ = read_frame(&mut r, &key).await.unwrap().unwrap();
            if let LanMessage::BlobRequest { sha256_hex } =
                read_frame(&mut r, &key).await.unwrap().unwrap()
            {
                let _ = serve_blob(&mut w, &key, &cache, &sha256_hex).await;
            }
        });

        // Cap below the payload size → fetch must bail (→ relay fallback).
        let got = fetch_blob(&[addr], &key, "me", "me-name", &sha, BLOB_CHUNK).await;
        assert_eq!(got, None);
    }

    #[tokio::test]
    async fn run_peer_classifies_file_offer_and_reports_received_file() {
        let key = [17u8; KEY_LEN];
        let root =
            std::env::temp_dir().join(format!("clipbridge-run-peer-file-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("run-peer.txt");
        std::fs::write(&source, b"via run_peer").unwrap();
        let destination = root.join("received");
        std::fs::create_dir_all(&destination).unwrap();

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (clip_tx, _clip_rx) = mpsc::unbounded_channel::<IncomingLanClip>();
        let (file_tx, mut file_rx) = mpsc::unbounded_channel::<ReceivedFile>();
        let file_receive_dir = Arc::new(std::sync::Mutex::new(Some(destination)));
        let server = tokio::spawn({
            let file_receive_dir = file_receive_dir.clone();
            async move {
                let (stream, peer_addr) = listener.accept().await.unwrap();
                let peer = PeerRunContext {
                    key,
                    self_device_id: "target".into(),
                    self_device_name: "Target".into(),
                    inbound: clip_tx,
                    out_rx: broadcast::channel::<OutgoingLan>(1).1,
                    expected_peer: Some("source".into()),
                    peer_count: Arc::new(AtomicUsize::new(0)),
                    peers: Arc::new(std::sync::Mutex::new(HashMap::new())),
                    blob_cache: BlobCache::new(),
                    registry_key: "test".into(),
                    file_receive_dir,
                    file_inbound: file_tx,
                    lan_active: Arc::new(AtomicBool::new(true)),
                    lan_mode_notify: Arc::new(Notify::new()),
                };
                run_peer(stream, peer_addr, peer).await.unwrap();
            }
        });

        let sent = tokio::task::spawn_blocking(move || {
            lan_send_file(
                vec![addr],
                key,
                SendFileRequest {
                    source_device_id: "source".into(),
                    source_device_name: "Source".into(),
                    target_device_id: "target".into(),
                    source_path: source,
                    mime_type: Some("text/plain".into()),
                    config: FileTransferConfig::default(),
                },
            )
        })
        .await
        .unwrap()
        .unwrap();

        let received = tokio::time::timeout(Duration::from_secs(2), file_rx.recv())
            .await
            .unwrap()
            .unwrap();
        server.await.unwrap();

        assert_eq!(sent.bytes_sent, 12);
        assert_eq!(received.file_name, "run-peer.txt");
        assert_eq!(
            std::fs::read_to_string(received.path).unwrap(),
            "via run_peer"
        );
    }

    #[tokio::test]
    async fn inactive_lan_peer_completes_handshake_and_answers_ping() {
        let key = [19u8; KEY_LEN];
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let lan_active = Arc::new(AtomicBool::new(false));
        let lan_mode_notify = Arc::new(Notify::new());

        let server = tokio::spawn({
            let lan_active = lan_active.clone();
            let lan_mode_notify = lan_mode_notify.clone();
            async move {
                let (stream, peer_addr) = listener.accept().await.unwrap();
                let (clip_tx, _clip_rx) = mpsc::unbounded_channel::<IncomingLanClip>();
                let (file_tx, _file_rx) = mpsc::unbounded_channel::<ReceivedFile>();
                let peer = PeerRunContext {
                    key,
                    self_device_id: "android".into(),
                    self_device_name: "Android".into(),
                    inbound: clip_tx,
                    out_rx: broadcast::channel::<OutgoingLan>(1).1,
                    expected_peer: Some("desktop".into()),
                    peer_count: Arc::new(AtomicUsize::new(0)),
                    peers: Arc::new(std::sync::Mutex::new(HashMap::new())),
                    blob_cache: BlobCache::new(),
                    registry_key: "probe".into(),
                    file_receive_dir: Arc::new(std::sync::Mutex::new(None)),
                    file_inbound: file_tx,
                    lan_active,
                    lan_mode_notify,
                };
                run_peer(stream, peer_addr, peer).await.unwrap();
            }
        });

        let stream = TcpStream::connect(addr).await.unwrap();
        let (mut reader, mut writer) = stream.into_split();
        write_frame(
            &mut writer,
            &key,
            &LanMessage::Hello {
                device_id: "desktop".into(),
                version: PROTO_VERSION,
                device_name: "Desktop".into(),
            },
        )
        .await
        .unwrap();

        let hello = tokio::time::timeout(Duration::from_secs(1), read_frame(&mut reader, &key))
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        match hello {
            LanMessage::Hello { device_id, .. } => assert_eq!(device_id, "android"),
            other => panic!("expected Hello, got {other:?}"),
        }

        let first_ping =
            tokio::time::timeout(Duration::from_secs(1), read_frame(&mut reader, &key))
                .await
                .unwrap()
                .unwrap()
                .unwrap();
        assert!(matches!(first_ping, LanMessage::Ping));

        write_frame(&mut writer, &key, &LanMessage::Ping)
            .await
            .unwrap();
        let ping_reply =
            tokio::time::timeout(Duration::from_secs(1), read_frame(&mut reader, &key))
                .await
                .unwrap()
                .unwrap()
                .unwrap();
        assert!(matches!(ping_reply, LanMessage::Ping));

        drop(writer);
        drop(reader);
        server.await.unwrap();
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
        let (a_file_tx, _a_file_rx) = mpsc::unbounded_channel::<ReceivedFile>();
        let (b_file_tx, _b_file_rx) = mpsc::unbounded_channel::<ReceivedFile>();

        let count_a = Arc::new(AtomicUsize::new(0));
        let count_b = Arc::new(AtomicUsize::new(0));
        let peers_a: PeerRegistry = Arc::new(std::sync::Mutex::new(HashMap::new()));
        let peers_b: PeerRegistry = Arc::new(std::sync::Mutex::new(HashMap::new()));
        let lan_active_a = Arc::new(AtomicBool::new(true));
        let lan_active_b = Arc::new(AtomicBool::new(true));
        let node_a = LanNode::spawn(LanNodeConfig {
            group_id: group.clone(),
            device_id: did_a.clone(),
            device_name: "node-A".into(),
            key,
            inbound: a_tx,
            peer_count: count_a,
            peers: peers_a,
            blob_cache: BlobCache::new(),
            peer_addrs: Arc::new(std::sync::Mutex::new(HashMap::new())),
            file_receive_dir: Arc::new(std::sync::Mutex::new(None)),
            file_inbound: a_file_tx,
            lan_active: lan_active_a,
            lan_mode_notify: Arc::new(Notify::new()),
        })
        .await
        .expect("spawn A");
        let node_b = LanNode::spawn(LanNodeConfig {
            group_id: group.clone(),
            device_id: did_b.clone(),
            device_name: "node-B".into(),
            key,
            inbound: b_tx,
            peer_count: count_b,
            peers: peers_b,
            blob_cache: BlobCache::new(),
            peer_addrs: Arc::new(std::sync::Mutex::new(HashMap::new())),
            file_receive_dir: Arc::new(std::sync::Mutex::new(None)),
            file_inbound: b_file_tx,
            lan_active: lan_active_b,
            lan_mode_notify: Arc::new(Notify::new()),
        })
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
