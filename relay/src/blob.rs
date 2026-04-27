//! In-memory blob store for image (and future file) payloads.
//!
//! The relay is intentionally dumb: clients PUT a ciphertext keyed by its
//! own SHA-256, and any other client in the group GETs it back. The relay
//! never decrypts and doesn't know which device a blob belongs to — it
//! only sees the group id (used as a tenancy boundary so two unrelated
//! groups can't collide on the same hash).
//!
//! Eviction is dual-bound: total bytes across all groups stay under
//! `budget_bytes`, and any single entry is dropped after `ttl`. Both are
//! configured via env vars in `lib.rs::blob_store_from_env`.

use std::{
    sync::atomic::{AtomicU64, Ordering},
    sync::Arc,
    time::{Duration, Instant},
};

use bytes::Bytes;
use clipbridge_core::sha256_hex;
use dashmap::DashMap;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BlobKey {
    pub group_id: String,
    pub sha256_hex: String,
}

struct Entry {
    inserted_at: Instant,
    bytes: Bytes,
}

#[derive(Clone)]
pub struct BlobStore {
    inner: Arc<DashMap<BlobKey, Entry>>,
    total_bytes: Arc<AtomicU64>,
    budget_bytes: u64,
    ttl: Duration,
    max_blob_bytes: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum PutError {
    #[error("blob exceeds per-blob max ({max} bytes)")]
    TooLarge { max: usize },
    #[error("body sha256 does not match path hash")]
    HashMismatch,
}

impl BlobStore {
    pub fn new(budget_bytes: u64, ttl: Duration, max_blob_bytes: usize) -> Self {
        Self {
            inner: Arc::new(DashMap::new()),
            total_bytes: Arc::new(AtomicU64::new(0)),
            budget_bytes,
            ttl,
            max_blob_bytes,
        }
    }

    pub fn max_blob_bytes(&self) -> usize {
        self.max_blob_bytes
    }

    /// Insert a blob. The caller must have validated the hash matches.
    pub fn put(&self, key: BlobKey, body: Bytes) -> Result<(), PutError> {
        if body.len() > self.max_blob_bytes {
            return Err(PutError::TooLarge {
                max: self.max_blob_bytes,
            });
        }
        if sha256_hex(&body) != key.sha256_hex {
            return Err(PutError::HashMismatch);
        }
        // Idempotent: if already present, refresh nothing — a second PUT
        // with the same hash is by definition the same bytes.
        if self.inner.contains_key(&key) {
            return Ok(());
        }
        let len = body.len() as u64;
        self.evict_until_room_for(len);
        if self
            .inner
            .insert(
                key,
                Entry {
                    inserted_at: Instant::now(),
                    bytes: body,
                },
            )
            .is_none()
        {
            self.total_bytes.fetch_add(len, Ordering::Relaxed);
        }
        Ok(())
    }

    pub fn get(&self, key: &BlobKey) -> Option<Bytes> {
        let entry = self.inner.get(key)?;
        if entry.inserted_at.elapsed() >= self.ttl {
            // Expired between fetch and now — drop it lazily.
            drop(entry);
            self.remove(key);
            return None;
        }
        Some(entry.bytes.clone())
    }

    fn remove(&self, key: &BlobKey) {
        if let Some((_, entry)) = self.inner.remove(key) {
            self.total_bytes
                .fetch_sub(entry.bytes.len() as u64, Ordering::Relaxed);
        }
    }

    /// Evict expired entries first, then if still over budget, evict
    /// oldest-first until there's room for `incoming`. Cheap O(n) walk —
    /// the relay holds tens of entries at most, not millions.
    fn evict_until_room_for(&self, incoming: u64) {
        let now = Instant::now();
        let expired: Vec<BlobKey> = self
            .inner
            .iter()
            .filter(|e| now.duration_since(e.inserted_at) >= self.ttl)
            .map(|e| e.key().clone())
            .collect();
        for k in expired {
            self.remove(&k);
        }
        while self.total_bytes.load(Ordering::Relaxed) + incoming > self.budget_bytes {
            let oldest = self
                .inner
                .iter()
                .min_by_key(|e| e.inserted_at)
                .map(|e| e.key().clone());
            match oldest {
                Some(k) => self.remove(&k),
                None => break,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(g: &str, h: &str) -> BlobKey {
        BlobKey {
            group_id: g.into(),
            sha256_hex: h.into(),
        }
    }

    #[test]
    fn put_then_get_round_trip() {
        let s = BlobStore::new(1024, Duration::from_secs(60), 512);
        let body = Bytes::from_static(b"hello");
        let h = sha256_hex(&body);
        s.put(key("g", &h), body.clone()).unwrap();
        assert_eq!(s.get(&key("g", &h)).unwrap(), body);
    }

    #[test]
    fn rejects_hash_mismatch() {
        let s = BlobStore::new(1024, Duration::from_secs(60), 512);
        let err = s
            .put(key("g", "deadbeef"), Bytes::from_static(b"hello"))
            .unwrap_err();
        assert!(matches!(err, PutError::HashMismatch));
    }

    #[test]
    fn rejects_oversize() {
        let s = BlobStore::new(1024, Duration::from_secs(60), 4);
        let body = Bytes::from_static(b"hello");
        let h = sha256_hex(&body);
        let err = s.put(key("g", &h), body).unwrap_err();
        assert!(matches!(err, PutError::TooLarge { .. }));
    }

    #[test]
    fn evicts_oldest_under_budget_pressure() {
        let s = BlobStore::new(8, Duration::from_secs(60), 8);
        let a = Bytes::from_static(b"aaaa");
        let b = Bytes::from_static(b"bbbb");
        let c = Bytes::from_static(b"cccc");
        s.put(key("g", &sha256_hex(&a)), a.clone()).unwrap();
        std::thread::sleep(Duration::from_millis(2));
        s.put(key("g", &sha256_hex(&b)), b.clone()).unwrap();
        std::thread::sleep(Duration::from_millis(2));
        s.put(key("g", &sha256_hex(&c)), c.clone()).unwrap();
        // budget 8, three 4-byte entries → oldest (a) is evicted to make
        // room for c; b and c stay together at the budget cap.
        assert!(s.get(&key("g", &sha256_hex(&a))).is_none());
        assert!(s.get(&key("g", &sha256_hex(&b))).is_some());
        assert!(s.get(&key("g", &sha256_hex(&c))).is_some());
        assert_eq!(s.total_bytes.load(Ordering::Relaxed), 8);
    }

    #[test]
    fn idempotent_put_keeps_one_copy() {
        let s = BlobStore::new(1024, Duration::from_secs(60), 512);
        let body = Bytes::from_static(b"hello");
        let h = sha256_hex(&body);
        s.put(key("g", &h), body.clone()).unwrap();
        s.put(key("g", &h), body.clone()).unwrap();
        assert_eq!(s.total_bytes.load(Ordering::Relaxed), body.len() as u64);
    }
}
