use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use rand::RngCore;
use serde::{Deserialize, Serialize};

use crate::crypto::KEY_LEN;

pub type GroupId = String;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupKey(#[serde(with = "key_serde")] pub [u8; KEY_LEN]);

impl GroupKey {
    pub fn random() -> Self {
        let mut k = [0u8; KEY_LEN];
        rand::thread_rng().fill_bytes(&mut k);
        Self(k)
    }
}

mod key_serde {
    use super::*;
    use serde::{Deserializer, Serializer};

    pub fn serialize<S: Serializer>(k: &[u8; KEY_LEN], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&URL_SAFE_NO_PAD.encode(k))
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<[u8; KEY_LEN], D::Error> {
        let s: String = Deserialize::deserialize(d)?;
        let v = URL_SAFE_NO_PAD
            .decode(s.as_bytes())
            .map_err(serde::de::Error::custom)?;
        v.try_into()
            .map_err(|_| serde::de::Error::custom("group key must be 32 bytes"))
    }
}

/// What gets encoded into the pairing QR code.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupConfig {
    pub relay_url: String,
    pub group_id: GroupId,
    pub key: GroupKey,
}

impl GroupConfig {
    pub fn new(relay_url: impl Into<String>) -> Self {
        Self {
            relay_url: relay_url.into(),
            group_id: uuid::Uuid::new_v4().to_string(),
            key: GroupKey::random(),
        }
    }
}
