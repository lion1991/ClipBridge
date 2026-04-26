pub mod client;
pub mod crypto;
pub mod group;
pub mod protocol;

pub use client::{Client, ClientError, ClipListener, ConnectionState, FfiError};
pub use crypto::{decrypt, encrypt, CryptoError, NONCE_LEN};
pub use group::{GroupConfig, GroupId, GroupKey};
pub use protocol::{ClientMessage, ClipKind, ClipPayload, ServerMessage};

uniffi::setup_scaffolding!();
