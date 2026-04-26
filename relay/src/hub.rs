use std::{
    collections::VecDeque,
    sync::Arc,
    time::{Duration, Instant},
};

use clipbridge_core::protocol::RecentClip;
use dashmap::DashMap;
use tokio::sync::broadcast;

const RECENT_CAP: usize = 3;
const RECENT_TTL: Duration = Duration::from_secs(5 * 60);
const BROADCAST_CAP: usize = 32;

#[derive(Clone)]
pub struct Hub {
    inner: Arc<DashMap<String, Group>>,
}

struct Group {
    tx: broadcast::Sender<RecentClip>,
    cache: parking_lot_compat::Mutex<VecDeque<(Instant, RecentClip)>>,
}

impl Hub {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(DashMap::new()),
        }
    }

    fn entry(&self, group_id: &str) -> dashmap::mapref::one::RefMut<'_, String, Group> {
        if !self.inner.contains_key(group_id) {
            let (tx, _) = broadcast::channel(BROADCAST_CAP);
            self.inner.insert(
                group_id.to_string(),
                Group {
                    tx,
                    cache: parking_lot_compat::Mutex::new(VecDeque::with_capacity(RECENT_CAP)),
                },
            );
        }
        self.inner.get_mut(group_id).expect("just inserted")
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
