use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::crypto::SHA256_LEN;

pub const DEFAULT_MAX_FILE_BYTES: u64 = 2 * 1024 * 1024 * 1024;
pub const DEFAULT_CHUNK_BYTES: usize = 512 * 1024;

#[derive(Debug, Clone)]
pub struct FileTransferConfig {
    pub max_file_bytes: u64,
    pub chunk_bytes: usize,
}

impl Default for FileTransferConfig {
    fn default() -> Self {
        Self {
            max_file_bytes: DEFAULT_MAX_FILE_BYTES,
            chunk_bytes: DEFAULT_CHUNK_BYTES,
        }
    }
}

impl FileTransferConfig {
    pub fn bounded_chunk_bytes(&self) -> usize {
        self.chunk_bytes.clamp(1, crate::lan::MAX_FILE_CHUNK_BYTES)
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct FileOffer {
    pub transfer_id: String,
    pub source_device_id: String,
    pub target_device_id: String,
    pub file_name: String,
    pub size_bytes: u64,
    pub modified_unix_millis: Option<i64>,
    pub mime_type: Option<String>,
    pub sha256_hex: String,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct SentFile {
    pub transfer_id: String,
    pub file_name: String,
    pub bytes_sent: u64,
    pub sha256_hex: String,
}

#[derive(Debug, Clone)]
pub struct ReceivedFile {
    pub transfer_id: String,
    pub file_name: String,
    pub path: PathBuf,
    pub size_bytes: u64,
    pub sha256_hex: String,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct ReceivedFileRecord {
    pub transfer_id: String,
    pub file_name: String,
    pub path: String,
    pub size_bytes: u64,
    pub sha256_hex: String,
}

impl From<ReceivedFile> for ReceivedFileRecord {
    fn from(value: ReceivedFile) -> Self {
        Self {
            transfer_id: value.transfer_id,
            file_name: value.file_name,
            path: value.path.to_string_lossy().into_owned(),
            size_bytes: value.size_bytes,
            sha256_hex: value.sha256_hex,
        }
    }
}

#[derive(Debug, Clone)]
pub struct SendFileRequest {
    pub source_device_id: String,
    pub source_device_name: String,
    pub target_device_id: String,
    pub source_path: PathBuf,
    pub mime_type: Option<String>,
    pub config: FileTransferConfig,
}

#[derive(Debug, thiserror::Error)]
pub enum FileTransferError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("path is not a regular file")]
    NotRegularFile,
    #[error("file exceeds configured limit: {size} > {limit}")]
    FileTooLarge { size: u64, limit: u64 },
    #[error("invalid file name: {name}")]
    InvalidFileName { name: String },
    #[error("reserved file name: {name}")]
    ReservedFileName { name: String },
    #[error("invalid sha256: {sha256_hex}")]
    InvalidSha256 { sha256_hex: String },
    #[error("unexpected chunk offset: expected {expected}, got {got}")]
    OffsetMismatch { expected: u64, got: u64 },
    #[error("size mismatch: expected {expected}, got {got}")]
    SizeMismatch { expected: u64, got: u64 },
    #[error("hash mismatch: expected {expected}, got {got}")]
    HashMismatch { expected: String, got: String },
    #[error("transfer rejected: {reason}")]
    Rejected { reason: String },
    #[error("transfer canceled: {reason}")]
    Canceled { reason: String },
    #[error("unexpected lan frame: {frame}")]
    UnexpectedFrame { frame: String },
    #[error("peer device mismatch: expected {expected}, got {got}")]
    PeerMismatch { expected: String, got: String },
}

pub struct OutgoingFile {
    file: File,
    pub offer: FileOffer,
    chunk_bytes: usize,
}

impl OutgoingFile {
    pub fn open(
        source_device_id: String,
        target_device_id: String,
        path: &Path,
        mime_type: Option<String>,
        config: FileTransferConfig,
    ) -> Result<Self, FileTransferError> {
        let metadata = fs::metadata(path)?;
        if !metadata.is_file() {
            return Err(FileTransferError::NotRegularFile);
        }
        if metadata.len() > config.max_file_bytes {
            return Err(FileTransferError::FileTooLarge {
                size: metadata.len(),
                limit: config.max_file_bytes,
            });
        }
        let file_name = path
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or_else(|| FileTransferError::InvalidFileName {
                name: path.display().to_string(),
            })
            .and_then(sanitize_file_name)?;
        let sha256_hex = sha256_file_hex(path)?;
        let modified_unix_millis = metadata
            .modified()
            .ok()
            .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis().min(i64::MAX as u128) as i64);
        let file = File::open(path)?;

        Ok(Self {
            file,
            offer: FileOffer {
                transfer_id: uuid::Uuid::new_v4().to_string(),
                source_device_id,
                target_device_id,
                file_name,
                size_bytes: metadata.len(),
                modified_unix_millis,
                mime_type,
                sha256_hex,
            },
            chunk_bytes: config.bounded_chunk_bytes(),
        })
    }

    pub fn read_chunk(&mut self, buf: &mut Vec<u8>) -> Result<usize, FileTransferError> {
        buf.resize(self.chunk_bytes, 0);
        let read = self.file.read(buf)?;
        buf.truncate(read);
        Ok(read)
    }
}

pub struct IncomingFileWriter {
    offer: FileOffer,
    file: Option<File>,
    final_path: PathBuf,
    part_path: PathBuf,
    written: u64,
    hasher: Sha256,
    finished: bool,
}

impl IncomingFileWriter {
    pub fn accept(
        offer: FileOffer,
        destination_dir: &Path,
        config: FileTransferConfig,
    ) -> Result<Self, FileTransferError> {
        validate_offer(&offer, &config)?;
        fs::create_dir_all(destination_dir)?;
        let final_path = unique_destination_path(destination_dir, &offer.file_name)?;
        let part_path = part_path_for(&final_path, &offer.transfer_id);
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&part_path)?;

        Ok(Self {
            offer,
            file: Some(file),
            final_path,
            part_path,
            written: 0,
            hasher: Sha256::new(),
            finished: false,
        })
    }

    pub fn transfer_id(&self) -> &str {
        &self.offer.transfer_id
    }

    pub fn write_chunk(&mut self, offset: u64, data: &[u8]) -> Result<(), FileTransferError> {
        if offset != self.written {
            return Err(FileTransferError::OffsetMismatch {
                expected: self.written,
                got: offset,
            });
        }
        let next = self.written.saturating_add(data.len() as u64);
        if next > self.offer.size_bytes {
            return Err(FileTransferError::SizeMismatch {
                expected: self.offer.size_bytes,
                got: next,
            });
        }
        let Some(file) = &mut self.file else {
            return Err(FileTransferError::Canceled {
                reason: "writer already closed".into(),
            });
        };
        file.write_all(data)?;
        self.hasher.update(data);
        self.written = next;
        Ok(())
    }

    pub fn finish(mut self) -> Result<ReceivedFile, FileTransferError> {
        if self.written != self.offer.size_bytes {
            return Err(FileTransferError::SizeMismatch {
                expected: self.offer.size_bytes,
                got: self.written,
            });
        }

        if let Some(mut file) = self.file.take() {
            file.flush()?;
            file.sync_all()?;
        }

        let got = hex_digest(self.hasher.clone().finalize());
        if got != self.offer.sha256_hex {
            return Err(FileTransferError::HashMismatch {
                expected: self.offer.sha256_hex.clone(),
                got,
            });
        }

        fs::rename(&self.part_path, &self.final_path)?;
        self.finished = true;
        Ok(ReceivedFile {
            transfer_id: self.offer.transfer_id.clone(),
            file_name: self.offer.file_name.clone(),
            path: self.final_path.clone(),
            size_bytes: self.written,
            sha256_hex: self.offer.sha256_hex.clone(),
        })
    }
}

impl Drop for IncomingFileWriter {
    fn drop(&mut self) {
        if !self.finished {
            self.file.take();
            let _ = fs::remove_file(&self.part_path);
        }
    }
}

pub fn validate_offer(
    offer: &FileOffer,
    config: &FileTransferConfig,
) -> Result<(), FileTransferError> {
    if offer.size_bytes > config.max_file_bytes {
        return Err(FileTransferError::FileTooLarge {
            size: offer.size_bytes,
            limit: config.max_file_bytes,
        });
    }
    sanitize_file_name(&offer.file_name)?;
    if !is_valid_sha256_hex(&offer.sha256_hex) {
        return Err(FileTransferError::InvalidSha256 {
            sha256_hex: offer.sha256_hex.clone(),
        });
    }
    if offer.transfer_id.trim().is_empty() {
        return Err(FileTransferError::InvalidFileName {
            name: "empty transfer id".into(),
        });
    }
    Ok(())
}

pub fn sanitize_file_name(name: &str) -> Result<String, FileTransferError> {
    if name.is_empty() || name == "." || name == ".." {
        return Err(FileTransferError::InvalidFileName {
            name: name.to_string(),
        });
    }
    if name
        .chars()
        .any(|c| c == '/' || c == '\\' || c.is_control())
    {
        return Err(FileTransferError::InvalidFileName {
            name: name.to_string(),
        });
    }
    let trimmed = name.trim_matches([' ', '.']);
    if trimmed.is_empty() {
        return Err(FileTransferError::InvalidFileName {
            name: name.to_string(),
        });
    }
    let stem = trimmed
        .split_once('.')
        .map(|(s, _)| s)
        .unwrap_or(trimmed)
        .to_ascii_uppercase();
    if is_windows_reserved_name(&stem) {
        return Err(FileTransferError::ReservedFileName {
            name: name.to_string(),
        });
    }
    Ok(trimmed.to_string())
}

pub fn unique_destination_path(
    destination_dir: &Path,
    file_name: &str,
) -> Result<PathBuf, FileTransferError> {
    let safe_name = sanitize_file_name(file_name)?;
    let candidate = destination_dir.join(&safe_name);
    if !candidate.exists() {
        return Ok(candidate);
    }

    let (stem, ext) = split_name_ext(&safe_name);
    for i in 1..10_000 {
        let next_name = match ext {
            Some(ext) => format!("{stem} ({i}).{ext}"),
            None => format!("{stem} ({i})"),
        };
        let next = destination_dir.join(next_name);
        if !next.exists() {
            return Ok(next);
        }
    }
    Err(FileTransferError::Io(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "too many same-name files",
    )))
}

pub fn sha256_file_hex(path: &Path) -> Result<String, FileTransferError> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; DEFAULT_CHUNK_BYTES];
    loop {
        let read = file.read(&mut buf)?;
        if read == 0 {
            break;
        }
        hasher.update(&buf[..read]);
    }
    Ok(hex_digest(hasher.finalize()))
}

fn part_path_for(final_path: &Path, transfer_id: &str) -> PathBuf {
    let file_name = final_path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("incoming");
    let safe_id: String = transfer_id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .take(64)
        .collect();
    final_path.with_file_name(format!("{file_name}.{safe_id}.part"))
}

fn split_name_ext(name: &str) -> (&str, Option<&str>) {
    if let Some((stem, ext)) = name.rsplit_once('.') {
        if !stem.is_empty() && !ext.is_empty() {
            return (stem, Some(ext));
        }
    }
    (name, None)
}

fn is_windows_reserved_name(stem: &str) -> bool {
    matches!(
        stem,
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
    )
}

fn is_valid_sha256_hex(s: &str) -> bool {
    s.len() == SHA256_LEN * 2 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn hex_digest(digest: impl AsRef<[u8]>) -> String {
    let bytes = digest.as_ref();
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(hex_nibble(b >> 4));
        s.push(hex_nibble(b & 0x0f));
    }
    s
}

fn hex_nibble(n: u8) -> char {
    match n {
        0..=9 => (b'0' + n) as char,
        10..=15 => (b'a' + (n - 10)) as char,
        _ => unreachable!(),
    }
}
