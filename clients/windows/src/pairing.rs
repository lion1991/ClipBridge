//! Mirrors the pairing-config layout used by the Mac and Android clients
//! so the same QR / JSON works across platforms.

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use directories::ProjectDirs;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use std::{fs, path::PathBuf};
use uuid::Uuid;

pub const DEFAULT_RELAY_URL: &str = "wss://clip.wrlog.cn";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairingConfig {
    pub relay_url: String,
    pub group_id: String,
    /// 32-byte ChaCha20-Poly1305 key, base64url (no padding).
    pub key: String,
}

impl PairingConfig {
    pub fn make_new() -> Self {
        let mut key = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut key);
        Self {
            relay_url: DEFAULT_RELAY_URL.to_string(),
            group_id: Uuid::new_v4().to_string(),
            key: URL_SAFE_NO_PAD.encode(key),
        }
    }

    pub fn key_bytes(&self) -> Option<Vec<u8>> {
        URL_SAFE_NO_PAD.decode(self.key.as_bytes()).ok()
    }

    pub fn is_valid(&self) -> bool {
        self.key_bytes().map(|b| b.len() == 32).unwrap_or(false)
            && !self.relay_url.is_empty()
            && !self.group_id.is_empty()
    }
}

pub struct Store;

impl Store {
    fn path() -> PathBuf {
        let proj = ProjectDirs::from("com", "ClipBridge", "ClipBridge")
            .expect("no project directory available");
        let dir = proj.config_dir();
        std::fs::create_dir_all(dir).ok();
        dir.join("pairing.json")
    }

    pub fn load() -> Option<PairingConfig> {
        let raw = fs::read_to_string(Self::path()).ok()?;
        serde_json::from_str(&raw).ok()
    }

    pub fn save(cfg: &PairingConfig) -> std::io::Result<()> {
        let json = serde_json::to_string_pretty(cfg)?;
        fs::write(Self::path(), json)
    }

    pub fn clear() {
        let _ = fs::remove_file(Self::path());
    }

    pub fn device_id() -> String {
        let proj = ProjectDirs::from("com", "ClipBridge", "ClipBridge")
            .expect("no project directory available");
        let path = proj.config_dir().join("device_id");
        if let Ok(existing) = fs::read_to_string(&path) {
            let trimmed = existing.trim();
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }
        let id = Uuid::new_v4().to_string();
        let _ = fs::create_dir_all(proj.config_dir());
        let _ = fs::write(&path, &id);
        id
    }
}

// Re-export common types for convenience.
#[allow(dead_code)]
pub fn config_path() -> PathBuf {
    Store::path()
}

#[allow(dead_code)]
pub fn store_dir() -> Option<PathBuf> {
    ProjectDirs::from("com", "ClipBridge", "ClipBridge")
        .map(|p| p.config_dir().to_path_buf())
}
