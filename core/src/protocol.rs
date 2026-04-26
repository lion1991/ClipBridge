use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use serde::{Deserialize, Serialize};

use crate::group::GroupId;

/// Wire messages from client to relay.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    /// Subscribe to a group's broadcast channel.
    Join { group_id: GroupId, device_id: String },
    /// Broadcast an encrypted clip to all other group members.
    Publish {
        group_id: GroupId,
        #[serde(with = "b64")]
        ciphertext: Vec<u8>,
        #[serde(with = "b64")]
        nonce: Vec<u8>,
        ts: u64,
    },
    /// Pull recently cached clips (used by iOS keyboard on activation).
    FetchRecent { group_id: GroupId },
}

/// Wire messages from relay to client.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    Joined { group_id: GroupId },
    Clip {
        #[serde(with = "b64")]
        ciphertext: Vec<u8>,
        #[serde(with = "b64")]
        nonce: Vec<u8>,
        ts: u64,
        sender_device_id: String,
    },
    Recent { clips: Vec<RecentClip> },
    Error { reason: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecentClip {
    #[serde(with = "b64")]
    pub ciphertext: Vec<u8>,
    #[serde(with = "b64")]
    pub nonce: Vec<u8>,
    pub ts: u64,
    pub sender_device_id: String,
}

/// Decrypted payload (after group key opens the ciphertext).
#[derive(Debug, Clone, Serialize, Deserialize, uniffi::Record)]
pub struct ClipPayload {
    pub kind: ClipKind,
    pub content: String,
    pub device_name: String,
    pub ts: u64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, uniffi::Enum)]
#[serde(rename_all = "snake_case")]
pub enum ClipKind {
    Text,
    Image,
}

mod b64 {
    use super::*;
    use serde::{Deserializer, Serializer};

    pub fn serialize<S: Serializer>(v: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&B64.encode(v))
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let s: String = Deserialize::deserialize(d)?;
        B64.decode(s.as_bytes()).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_publish() {
        let m = ClientMessage::Publish {
            group_id: "g1".into(),
            ciphertext: vec![1, 2, 3],
            nonce: vec![0; 12],
            ts: 42,
        };
        let s = serde_json::to_string(&m).unwrap();
        let back: ClientMessage = serde_json::from_str(&s).unwrap();
        match back {
            ClientMessage::Publish { ts, .. } => assert_eq!(ts, 42),
            _ => panic!("wrong variant"),
        }
    }
}
