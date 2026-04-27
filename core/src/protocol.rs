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
///
/// `image` and any other typed-payload sidecar fields are additive: old
/// clients that don't know about them simply skip the unknown variant via
/// the `kind` discriminant. New clients receiving an old payload (no
/// `image` field) get `None` thanks to `serde(default)`.
#[derive(Debug, Clone, Serialize, Deserialize, uniffi::Record)]
pub struct ClipPayload {
    pub kind: ClipKind,
    /// Text content. For non-text kinds this is empty (kept non-optional so
    /// the FFI surface stays stable for existing Swift/Kotlin call sites).
    #[serde(default)]
    pub content: String,
    pub device_name: String,
    pub ts: u64,
    /// Present iff `kind == Image`. Carries the metadata needed to fetch
    /// and verify the encrypted blob from the relay's blob endpoint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<ImageMeta>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, uniffi::Enum)]
#[serde(rename_all = "snake_case")]
pub enum ClipKind {
    Text,
    Image,
}

/// Metadata for an image clip. The actual image bytes live in the relay's
/// blob store, addressed by `sha256_hex` (which is the SHA-256 of the
/// ciphertext, not the plaintext — keeping the relay blind to whether two
/// uploads contain the same image). Bytes are end-to-end encrypted with
/// the group key.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, uniffi::Record)]
pub struct ImageMeta {
    pub mime_type: String,
    pub width: u32,
    pub height: u32,
    /// Plaintext byte length. Lets the receiver render a placeholder /
    /// progress bar before the blob arrives.
    pub size_bytes: u64,
    /// Hex-encoded SHA-256 of the *ciphertext* stored in the blob endpoint.
    /// Doubles as the blob URL key and a local-cache lookup key.
    pub sha256_hex: String,
    /// Random 12-byte nonce used to encrypt the blob, base64-encoded.
    /// Required by ChaCha20-Poly1305 — must be unique per encryption.
    pub nonce_b64: String,
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

    /// Ensure new clients can still decode payloads minted by older versions
    /// that had no `image` field at all.
    #[test]
    fn legacy_text_payload_deserializes() {
        let json = r#"{"kind":"text","content":"hi","device_name":"mac","ts":1}"#;
        let p: ClipPayload = serde_json::from_str(json).unwrap();
        assert_eq!(p.kind, ClipKind::Text);
        assert_eq!(p.content, "hi");
        assert!(p.image.is_none());
    }

    #[test]
    fn image_payload_round_trip() {
        let p = ClipPayload {
            kind: ClipKind::Image,
            content: String::new(),
            device_name: "mac".into(),
            ts: 1,
            image: Some(ImageMeta {
                mime_type: "image/png".into(),
                width: 1920,
                height: 1080,
                size_bytes: 4096,
                sha256_hex: "ba7816bf".into(),
                nonce_b64: "AAAAAAAAAAAAAAAA".into(),
            }),
        };
        let s = serde_json::to_string(&p).unwrap();
        let back: ClipPayload = serde_json::from_str(&s).unwrap();
        assert_eq!(back.kind, ClipKind::Image);
        assert_eq!(back.image.as_ref().unwrap().width, 1920);
    }
}
