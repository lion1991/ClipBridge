use std::{
    collections::{HashMap, VecDeque},
    net::IpAddr,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use clipbridge_core::protocol::{LanPeer, RecentClip};
use dashmap::DashMap;
use tokio::sync::{broadcast, mpsc};

const RECENT_CAP: usize = 3;
const RECENT_TTL: Duration = Duration::from_secs(5 * 60);
const BROADCAST_CAP: usize = 32;

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
    pub fn rendezvous_upsert(
        &self,
        group_id: &str,
        egress: IpAddr,
        conn_id: u64,
        device_id: String,
        candidates: Vec<String>,
        tx: mpsc::UnboundedSender<Vec<LanPeer>>,
    ) {
        let group = self.entry(group_id);
        let mut p = group.presence.lock();
        p.entry(egress).or_default().insert(
            conn_id,
            RvConn {
                device_id,
                candidates,
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
        let mut by_did: HashMap<&str, &Vec<String>> = HashMap::new();
        for (&oid, oc) in conns.iter() {
            if oid == cid {
                continue;
            }
            by_did.insert(oc.device_id.as_str(), &oc.candidates);
        }
        let peers: Vec<LanPeer> = by_did
            .into_iter()
            .map(|(did, cands)| LanPeer {
                device_id: did.to_string(),
                candidates: cands.clone(),
            })
            .collect();
        // Receiver gone just means that conn's task already exited; the
        // next remove() will clean its slot.
        let _ = c.tx.send(peers);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    fn ip(n: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(10, 0, 0, n))
    }

    #[tokio::test]
    async fn upsert_pushes_existing_peers_to_newcomer_and_notifies_rest() {
        let hub = Hub::new();
        let egress = ip(1);

        // A joins first — no peers yet.
        let (a_tx, mut a_rx) = mpsc::unbounded_channel();
        hub.rendezvous_upsert("g", egress, 1, "A".into(), vec!["1.1.1.1:10".into()], a_tx);
        assert_eq!(a_rx.recv().await.unwrap().len(), 0);

        // B joins same egress — B learns A immediately, A is re-notified.
        let (b_tx, mut b_rx) = mpsc::unbounded_channel();
        hub.rendezvous_upsert("g", egress, 2, "B".into(), vec!["2.2.2.2:20".into()], b_tx);

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
        hub.rendezvous_upsert("g", ip(1), 1, "A".into(), vec!["a:1".into()], a_tx);
        let _ = a_rx.recv().await;
        // B is on a different egress IP — must not see A and must not
        // trigger a push to A.
        hub.rendezvous_upsert("g", ip(2), 2, "B".into(), vec!["b:2".into()], b_tx);
        assert_eq!(b_rx.recv().await.unwrap().len(), 0);
        assert!(a_rx.try_recv().is_err(), "A should not be notified about a different-egress peer");
    }

    #[tokio::test]
    async fn remove_repushes_shrunken_snapshot() {
        let hub = Hub::new();
        let egress = ip(1);
        let (a_tx, mut a_rx) = mpsc::unbounded_channel();
        let (b_tx, mut b_rx) = mpsc::unbounded_channel();
        hub.rendezvous_upsert("g", egress, 1, "A".into(), vec!["a:1".into()], a_tx);
        let _ = a_rx.recv().await;
        hub.rendezvous_upsert("g", egress, 2, "B".into(), vec!["b:2".into()], b_tx);
        let _ = a_rx.recv().await; // A learns B
        let _ = b_rx.recv().await;

        hub.rendezvous_remove("g", egress, 2);
        // A gets a fresh snapshot with B gone.
        assert_eq!(a_rx.recv().await.unwrap().len(), 0);
    }

    #[tokio::test]
    async fn peers_deduped_by_device_id() {
        let hub = Hub::new();
        let egress = ip(1);
        let (obs_tx, mut obs_rx) = mpsc::unbounded_channel();
        hub.rendezvous_upsert("g", egress, 1, "obs".into(), vec!["o:1".into()], obs_tx);
        let _ = obs_rx.recv().await;
        // Same device_id "D" reconnects on a second conn id; the observer
        // must see exactly one entry for D (latest candidates win).
        let (d1_tx, _d1_rx) = mpsc::unbounded_channel();
        hub.rendezvous_upsert("g", egress, 2, "D".into(), vec!["old:1".into()], d1_tx);
        let _ = obs_rx.recv().await;
        let (d2_tx, _d2_rx) = mpsc::unbounded_channel();
        hub.rendezvous_upsert("g", egress, 3, "D".into(), vec!["new:2".into()], d2_tx);
        let snap = obs_rx.recv().await.unwrap();
        let d_entries: Vec<_> = snap.iter().filter(|p| p.device_id == "D").collect();
        assert_eq!(d_entries.len(), 1, "duplicate device_id must collapse");
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
