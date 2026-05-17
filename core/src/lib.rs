pub mod blob;
pub mod client;
pub mod crypto;
pub mod file_transfer;
pub mod group;
pub mod lan;
pub mod protocol;

pub use blob::{http_base, BlobClient, BlobError};
pub use client::{Client, ClientError, ClipListener, ConnectionState, FfiError};
pub use crypto::{decrypt, encrypt, sha256_hex, CryptoError, NONCE_LEN, SHA256_LEN};
pub use file_transfer::{
    FileOffer, FileTransferConfig, FileTransferError, ReceivedFile, ReceivedFileRecord,
    SendFileRequest, SentFile,
};
pub use group::{GroupConfig, GroupId, GroupKey};
pub use lan::LanPeerRecord;
pub use protocol::{ClientMessage, ClipKind, ClipPayload, ImageMeta, ServerMessage};

uniffi::setup_scaffolding!();
