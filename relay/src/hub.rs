use std::{
    collections::{HashMap, HashSet, VecDeque},
    net::{IpAddr, SocketAddr},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use clipbridge_core::protocol::{LanCandidate, LanPeer, RecentClip};
use dashmap::DashMap;
use tokio::sync::{broadcast, mpsc};

const RECENT_CAP: usize = 3;
const RECENT_TTL: Duration = Duration::from_secs(5 * 60);
const BROADCAST_CAP: usize = 32;
pub(crate) const MAX_LAN_CANDIDATES: usize = 32;

#[derive(Clone)]
pub struct Hub {
    inner: Arc<DashMap<String, Group>>,
    /// Monotonic per-connection id source. Used to key rendezvous presence
    /// so a device's reconnect (or its second process) is a distinct slot.
    conn_seq: Arc<AtomicU64>,
}

/// One rendezvous-capable connection: a client that sent `LanAdvertise`.
struct RvConn {
    device_id: String,
    candidates: Vec<String>,
    candidate_networks: Vec<LanCandidate>,
    /// Per-connection mailbox. The connection's ws task drains this and
    /// writes each snapshot out as `ServerMessage::LanPeers`.
    tx: mpsc::UnboundedSender<Vec<LanPeer>>,
}

struct Group {
    tx: broadcast::Sender<RecentClip>,
    cache: parking_lot_compat::Mutex<VecDeque<(Instant, RecentClip)>>,
    /// Rendezvous presence: egress IP -> (conn_id -> RvConn). Only conns
    /// that opted in via `LanAdvertise` appear here. Grouping by egress IP
    /// is the "very likely same LAN" heuristic — correctness still rests on
    /// the clients' own Hello + group-key handshake when they dial.
    presence: parking_lot_compat::Mutex<HashMap<IpAddr, HashMap<u64, RvConn>>>,
}

impl Hub {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(DashMap::new()),
            conn_seq: Arc::new(AtomicU64::new(1)),
        }
    }

    /// A fresh per-connection id (see `conn_seq`).
    pub fn next_conn_id(&self) -> u64 {
        self.conn_seq.fetch_add(1, Ordering::Relaxed)
    }

    /// Get the group, creating it on first touch. Uses dashmap's per-key
    /// `entry` (which holds the shard write lock across the check and the
    /// insert) rather than a `contains_key` + `insert` pair: the latter is
    /// a TOCTOU race under a multi-threaded runtime — two connections for a
    /// not-yet-existing group could both insert, the second replacing the
    /// first `Group` and orphaning its broadcast channel + recent-cache, so
    /// a subscriber on the losing channel never receives publishes. The
    /// production relay is single-threaded (`current_thread`) so it never
    /// hit this, but it's a real correctness bug under any multi-threaded
    /// deployment and the multi-thread integration tests flaked on it.
    fn entry(&self, group_id: &str) -> dashmap::mapref::one::RefMut<'_, String, Group> {
        self.inner.entry(group_id.to_string()).or_insert_with(|| {
            let (tx, _) = broadcast::channel(BROADCAST_CAP);
            Group {
                tx,
                cache: parking_lot_compat::Mutex::new(VecDeque::with_capacity(RECENT_CAP)),
                presence: parking_lot_compat::Mutex::new(HashMap::new()),
            }
        })
    }

    pub fn subscribe(&self, group_id: &str) -> broadcast::Receiver<RecentClip> {
        self.entry(group_id).tx.subscribe()
    }

    pub fn publish(&self, group_id: &str, clip: RecentClip) {
        let group = self.entry(group_id);
        {
            let mut cache = group.cache.lock();
            let now = Instant::now();
            cache.retain(|(t, _)| now.duration_since(*t) < RECENT_TTL);
            if cache.len() == RECENT_CAP {
                cache.pop_front();
            }
            cache.push_back((now, clip.clone()));
        }
        let _ = group.tx.send(clip);
    }

    pub fn recent(&self, group_id: &str) -> Vec<RecentClip> {
        let group = self.entry(group_id);
        let mut cache = group.cache.lock();
        let now = Instant::now();
        cache.retain(|(t, _)| now.duration_since(*t) < RECENT_TTL);
        cache.iter().map(|(_, c)| c.clone()).collect()
    }

    /// Register or refresh a rendezvous connection and push a fresh peer
    /// snapshot to every connection sharing its egress IP (including the
    /// caller, so it learns peers that were already present).
    pub(crate) fn rendezvous_upsert(&self, update: RendezvousUpdate) {
        let RendezvousUpdate {
            group_id,
            egress,
            conn_id,
            device_id,
            candidates,
            candidate_networks,
            tx,
        } = update;
        let group = self.entry(&group_id);
        let mut p = group.presence.lock();
        p.entry(egress).or_default().insert(
            conn_id,
            RvConn {
                device_id,
                candidates,
                candidate_networks,
                tx,
            },
        );
        push_egress(&p, egress);
    }

    /// Drop a rendezvous connection (on socket close) and re-push the
    /// shrunken snapshot to the rest so they purge the departed peer.
    pub fn rendezvous_remove(&self, group_id: &str, egress: IpAddr, conn_id: u64) {
        let group = self.entry(group_id);
        let mut p = group.presence.lock();
        if let Some(conns) = p.get_mut(&egress) {
            conns.remove(&conn_id);
            let empty = conns.is_empty();
            if empty {
                p.remove(&egress);
            }
        }
        push_egress(&p, egress);
    }
}

impl Default for Hub {
    fn default() -> Self {
        Self::new()
    }
}

pub(crate) struct RendezvousUpdate {
    pub group_id: String,
    pub egress: IpAddr,
    pub conn_id: u64,
    pub device_id: String,
    pub candidates: Vec<String>,
    pub candidate_networks: Vec<LanCandidate>,
    pub tx: mpsc::UnboundedSender<Vec<LanPeer>>,
}

/// Recompute and deliver each connection's tailored `LanPeers` snapshot
/// for one egress group. A connection never sees itself; peers are deduped
/// by `device_id` (a device may briefly hold two conns across a reconnect)
/// with the most recently inserted candidates winning. Sends are over
/// unbounded mpsc, so this never blocks while holding the presence lock.
fn push_egress(p: &HashMap<IpAddr, HashMap<u64, RvConn>>, egress: IpAddr) {
    let Some(conns) = p.get(&egress) else {
        return;
    };
    for (&cid, c) in conns.iter() {
        let mut by_did: HashMap<&str, (u64, &RvConn)> = HashMap::new();
        for (&oid, oc) in conns.iter() {
            if oid == cid {
                continue;
            }
            by_did
                .entry(oc.device_id.as_str())
                .and_modify(|latest| {
                    if oid > latest.0 {
                        *latest = (oid, oc);
                    }
                })
                .or_insert((oid, oc));
        }
        let peers: Vec<LanPeer> = by_did
            .into_iter()
            .filter_map(|(did, (_, peer))| {
                let candidates = candidates_for_receiver(c, peer);
                (!candidates.is_empty()).then_some(LanPeer {
                    device_id: did.to_string(),
                    candidates,
                })
            })
            .collect();
        // Receiver gone just means that conn's task already exited; the
        // next remove() will clean its slot.
        let _ = c.tx.send(peers);
    }
}

pub(crate) fn sanitize_lan_advertise(
    candidates: Vec<String>,
    candidate_networks: Vec<LanCandidate>,
) -> (Vec<String>, Vec<LanCandidate>) {
    let mut sanitized_candidates = Vec::new();
    let mut seen_candidates = HashSet::new();
    for candidate in candidates {
        if sanitized_candidates.len() >= MAX_LAN_CANDIDATES {
            break;
        }
        let Ok(addr) = candidate.parse::<SocketAddr>() else {
            continue;
        };
        if !is_relay_lan_candidate_ip(&addr.ip()) {
            continue;
        }
        let normalized = addr.to_string();
        if seen_candidates.insert(normalized.clone()) {
            sanitized_candidates.push(normalized);
        }
    }

    let accepted: HashSet<&str> = sanitized_candidates.iter().map(String::as_str).collect();
    let mut sanitized_networks = Vec::new();
    let mut seen_networks = HashSet::new();
    for candidate in candidate_networks {
        if sanitized_networks.len() >= MAX_LAN_CANDIDATES {
            break;
        }
        let Ok(addr) = candidate.addr.parse::<SocketAddr>() else {
            continue;
        };
        let normalized = addr.to_string();
        if !accepted.contains(normalized.as_str())
            || !valid_prefix_len(&addr.ip(), candidate.prefix_len)
            || !seen_networks.insert(normalized.clone())
        {
            continue;
        }
        sanitized_networks.push(LanCandidate {
            addr: normalized,
            prefix_len: candidate.prefix_len,
        });
    }

    (sanitized_candidates, sanitized_networks)
}

fn is_relay_lan_candidate_ip(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_private() || v4.is_link_local(),
        IpAddr::V6(v6) => (v6.octets()[0] & 0xfe) == 0xfc,
    }
}

fn valid_prefix_len(ip: &IpAddr, prefix_len: u8) -> bool {
    match ip {
        IpAddr::V4(_) => prefix_len <= 32,
        IpAddr::V6(_) => prefix_len <= 128,
    }
}

fn candidates_for_receiver(receiver: &RvConn, peer: &RvConn) -> Vec<String> {
    if receiver.candidate_networks.is_empty() || peer.candidate_networks.is_empty() {
        return peer.candidates.clone();
    }
    let mut out = Vec::new();
    for candidate in &peer.candidate_networks {
        if peer
            .candidates
            .iter()
            .any(|legacy| legacy == &candidate.addr)
            && receiver
                .candidate_networks
                .iter()
                .any(|receiver_candidate| mutual_subnet(receiver_candidate, candidate))
            && !out.contains(&candidate.addr)
        {
            out.push(candidate.addr.clone());
        }
    }
    out
}

fn mutual_subnet(a: &LanCandidate, b: &LanCandidate) -> bool {
    let Some(a_addr) = parse_candidate_addr(&a.addr) else {
        return false;
    };
    let Some(b_addr) = parse_candidate_addr(&b.addr) else {
        return false;
    };
    ip_in_prefix(b_addr.ip(), a_addr.ip(), a.prefix_len)
        && ip_in_prefix(a_addr.ip(), b_addr.ip(), b.prefix_len)
}

fn parse_candidate_addr(addr: &str) -> Option<SocketAddr> {
    addr.parse().ok()
}

fn ip_in_prefix(target: IpAddr, network_ip: IpAddr, prefix_len: u8) -> bool {
    match (target, network_ip) {
        (IpAddr::V4(target), IpAddr::V4(network_ip)) if prefix_len <= 32 => {
            let mask = if prefix_len == 0 {
                0
            } else {
                u32::MAX << (32 - prefix_len)
            };
            (u32::from(target) & mask) == (u32::from(network_ip) & mask)
        }
        (IpAddr::V6(target), IpAddr::V6(network_ip)) if prefix_len <= 128 => {
            let mask = if prefix_len == 0 {
                0
            } else {
                u128::MAX << (128 - prefix_len)
            };
            (u128::from(target) & mask) == (u128::from(network_ip) & mask)
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    fn ip(n: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(10, 0, 0, n))
    }

    fn rv(
        group_id: &str,
        egress: IpAddr,
        conn_id: u64,
        device_id: &str,
        candidates: Vec<String>,
        tx: mpsc::UnboundedSender<Vec<LanPeer>>,
    ) -> RendezvousUpdate {
        rv_with_networks(group_id, egress, conn_id, device_id, candidates, vec![], tx)
    }

    fn rv_with_networks(
        group_id: &str,
        egress: IpAddr,
        conn_id: u64,
        device_id: &str,
        candidates: Vec<String>,
        candidate_networks: Vec<LanCandidate>,
        tx: mpsc::UnboundedSender<Vec<LanPeer>>,
    ) -> RendezvousUpdate {
        RendezvousUpdate {
            group_id: group_id.into(),
            egress,
            conn_id,
            device_id: device_id.into(),
            candidates,
            candidate_networks,
            tx,
        }
    }

    #[tokio::test]
    async fn upsert_pushes_existing_peers_to_newcomer_and_notifies_rest() {
        let hub = Hub::new();
        let egress = ip(1);

        // A joins first — no peers yet.
        let (a_tx, mut a_rx) = mpsc::unbounded_channel();
        hub.rendezvous_upsert(rv("g", egress, 1, "A", vec!["1.1.1.1:10".into()], a_tx));
        assert_eq!(a_rx.recv().await.unwrap().len(), 0);

        // B joins same egress — B learns A immediately, A is re-notified.
        let (b_tx, mut b_rx) = mpsc::unbounded_channel();
        hub.rendezvous_upsert(rv("g", egress, 2, "B", vec!["2.2.2.2:20".into()], b_tx));

        let b_sees = b_rx.recv().await.unwrap();
        assert_eq!(b_sees.len(), 1);
        assert_eq!(b_sees[0].device_id, "A");
        assert_eq!(b_sees[0].candidates, vec!["1.1.1.1:10".to_string()]);

        let a_sees = a_rx.recv().await.unwrap();
        assert_eq!(a_sees.len(), 1);
        assert_eq!(a_sees[0].device_id, "B");
    }

    #[tokio::test]
    async fn different_egress_ips_are_isolated() {
        let hub = Hub::new();
        let (a_tx, mut a_rx) = mpsc::unbounded_channel();
        let (b_tx, mut b_rx) = mpsc::unbounded_channel();
        hub.rendezvous_upsert(rv("g", ip(1), 1, "A", vec!["a:1".into()], a_tx));
        let _ = a_rx.recv().await;
        // B is on a different egress IP — must not see A and must not
        // trigger a push to A.
        hub.rendezvous_upsert(rv("g", ip(2), 2, "B", vec!["b:2".into()], b_tx));
        assert_eq!(b_rx.recv().await.unwrap().len(), 0);
        assert!(
            a_rx.try_recv().is_err(),
            "A should not be notified about a different-egress peer"
        );
    }

    #[tokio::test]
    async fn remove_repushes_shrunken_snapshot() {
        let hub = Hub::new();
        let egress = ip(1);
        let (a_tx, mut a_rx) = mpsc::unbounded_channel();
        let (b_tx, mut b_rx) = mpsc::unbounded_channel();
        hub.rendezvous_upsert(rv("g", egress, 1, "A", vec!["a:1".into()], a_tx));
        let _ = a_rx.recv().await;
        hub.rendezvous_upsert(rv("g", egress, 2, "B", vec!["b:2".into()], b_tx));
        let _ = a_rx.recv().await; // A learns B
        let _ = b_rx.recv().await;

        hub.rendezvous_remove("g", egress, 2);
        // A gets a fresh snapshot with B gone.
        assert_eq!(a_rx.recv().await.unwrap().len(), 0);
    }

    #[tokio::test]
    async fn peers_deduped_by_device_id_uses_highest_conn_id_candidates() {
        let hub = Hub::new();
        let egress = ip(1);
        let (obs_tx, mut obs_rx) = mpsc::unbounded_channel();
        hub.rendezvous_upsert(rv("g", egress, 1, "obs", vec!["o:1".into()], obs_tx));
        let _ = obs_rx.recv().await;

        // Same device_id "D" reconnects on a later conn id; the observer
        // must see exactly one entry for D and it must use the highest
        // conn_id's candidates, independent of HashMap iteration order.
        for conn_id in 2..130 {
            let (d_tx, _d_rx) = mpsc::unbounded_channel();
            hub.rendezvous_upsert(rv(
                "g",
                egress,
                conn_id,
                "D",
                vec![format!("old:{conn_id}")],
                d_tx,
            ));
            let _ = obs_rx.recv().await;
        }

        let (d_tx, _d_rx) = mpsc::unbounded_channel();
        hub.rendezvous_upsert(rv("g", egress, 130, "D", vec!["new:130".into()], d_tx));
        let snap = obs_rx.recv().await.unwrap();
        let d_entries: Vec<_> = snap.iter().filter(|p| p.device_id == "D").collect();
        assert_eq!(d_entries.len(), 1, "duplicate device_id must collapse");
        assert_eq!(d_entries[0].candidates, vec!["new:130".to_string()]);
    }

    fn lc(addr: &str, prefix_len: u8) -> LanCandidate {
        LanCandidate {
            addr: addr.into(),
            prefix_len,
        }
    }

    #[tokio::test]
    async fn candidate_networks_filter_to_mutual_subnets() {
        let hub = Hub::new();
        let egress = ip(1);
        let (a_tx, mut a_rx) = mpsc::unbounded_channel();
        let (b_tx, mut b_rx) = mpsc::unbounded_channel();

        hub.rendezvous_upsert(rv_with_networks(
            "g",
            egress,
            1,
            "A",
            vec!["192.168.1.10:5000".into(), "10.211.55.2:5000".into()],
            vec![lc("192.168.1.10:5000", 24), lc("10.211.55.2:5000", 24)],
            a_tx,
        ));
        let _ = a_rx.recv().await;
        hub.rendezvous_upsert(rv_with_networks(
            "g",
            egress,
            2,
            "B",
            vec!["192.168.1.11:6000".into(), "10.37.129.2:6000".into()],
            vec![lc("192.168.1.11:6000", 24), lc("10.37.129.2:6000", 24)],
            b_tx,
        ));

        let b_sees = b_rx.recv().await.unwrap();
        assert_eq!(b_sees[0].candidates, vec!["192.168.1.10:5000".to_string()]);
        let a_sees = a_rx.recv().await.unwrap();
        assert_eq!(a_sees[0].candidates, vec!["192.168.1.11:6000".to_string()]);
    }

    #[test]
    fn sanitize_lan_advertise_bounds_and_validates_candidates() {
        let mut candidates = vec![
            "not-a-socket".to_string(),
            "8.8.8.8:53".to_string(),
            "192.168.1.1:5000".to_string(),
            "192.168.1.1:5000".to_string(),
        ];
        for host in 2..=40 {
            candidates.push(format!("192.168.1.{host}:5000"));
        }
        let mut candidate_networks: Vec<LanCandidate> =
            candidates.iter().map(|addr| lc(addr, 24)).collect();
        candidate_networks.push(lc("192.168.1.2:5000", 40));
        candidate_networks.push(lc("192.168.1.250:5000", 24));
        candidate_networks.push(lc("[fd00::1]:5000", 129));

        let (candidates, candidate_networks) =
            sanitize_lan_advertise(candidates, candidate_networks);

        assert_eq!(candidates.len(), MAX_LAN_CANDIDATES);
        assert_eq!(candidates[0], "192.168.1.1:5000");
        assert!(!candidates.contains(&"8.8.8.8:53".to_string()));
        assert_eq!(
            candidates
                .iter()
                .collect::<std::collections::HashSet<_>>()
                .len(),
            candidates.len()
        );
        assert!(candidate_networks
            .iter()
            .all(|candidate| candidates.contains(&candidate.addr)));
        assert!(!candidate_networks
            .iter()
            .any(|candidate| candidate.addr == "192.168.1.250:5000"));
        assert!(!candidate_networks
            .iter()
            .any(|candidate| candidate.prefix_len > 32 && !candidate.addr.starts_with('[')));
        assert!(!candidate_networks
            .iter()
            .any(|candidate| candidate.prefix_len > 128 && candidate.addr.starts_with('[')));
    }
}

// Tiny shim so we don't pull in parking_lot just for a Mutex; std works fine here.
mod parking_lot_compat {
    use std::sync::{Mutex as StdMutex, MutexGuard};

    pub struct Mutex<T>(StdMutex<T>);
    impl<T> Mutex<T> {
        pub fn new(v: T) -> Self {
            Self(StdMutex::new(v))
        }
        pub fn lock(&self) -> MutexGuard<'_, T> {
            self.0.lock().expect("poisoned")
        }
    }
}
