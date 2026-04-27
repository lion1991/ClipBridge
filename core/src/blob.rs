//! HTTP client for the relay's blob endpoint.
//!
//! Uses async `reqwest::Client` under the hood, but exposes a sync API so
//! it can be called directly from FFI (Swift / Kotlin / Tauri Rust) on the
//! caller's background thread. Each call spawns a short-lived OS thread
//! with its own current-thread runtime to drive the request — this avoids
//! the "runtime in runtime" panic when invoked from tests that already run
//! inside a tokio runtime, and avoids dragging the heavyweight
//! `reqwest::blocking` runtime (which can't be dropped from async ctx).
//!
//! Per-call thread spawn is acceptable because image uploads are
//! low-frequency (one per copy event) and dwarfed by the network round-
//! trip itself.

use std::time::Duration;

use reqwest::{Client as HttpClient, StatusCode};

/// Per-call HTTP timeout. Long enough for a 32MB upload over slow LTE
/// (~30s at 1MB/s with overhead) without blocking forever when the relay
/// is unreachable.
const HTTP_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, thiserror::Error)]
pub enum BlobError {
    #[error("http: {0}")]
    Http(String),
    #[error("blob not found")]
    NotFound,
    #[error("blob too large for relay (status 413)")]
    TooLarge,
    #[error("relay rejected upload: {0}")]
    Rejected(String),
    #[error("worker thread join failed")]
    JoinPanic,
}

#[derive(Clone)]
pub struct BlobClient {
    http: HttpClient,
    base: String,
}

impl BlobClient {
    pub fn new(relay_url: &str) -> Result<Self, BlobError> {
        let http = HttpClient::builder()
            .timeout(HTTP_TIMEOUT)
            .build()
            .map_err(|e| BlobError::Http(e.to_string()))?;
        Ok(Self {
            http,
            base: http_base(relay_url),
        })
    }

    pub fn upload(
        &self,
        group_id: &str,
        sha256_hex: &str,
        ciphertext: Vec<u8>,
    ) -> Result<(), BlobError> {
        let url = format!("{}/blob/{}/{}", self.base, group_id, sha256_hex);
        let http = self.http.clone();
        run_blocking(async move {
            let resp = http
                .put(&url)
                .body(ciphertext)
                .send()
                .await
                .map_err(|e| BlobError::Http(e.to_string()))?;
            match resp.status() {
                StatusCode::NO_CONTENT | StatusCode::OK => Ok(()),
                StatusCode::PAYLOAD_TOO_LARGE => Err(BlobError::TooLarge),
                other => Err(BlobError::Rejected(format!(
                    "{} {}",
                    other.as_u16(),
                    resp.text().await.unwrap_or_default()
                ))),
            }
        })
    }

    pub fn download(&self, group_id: &str, sha256_hex: &str) -> Result<Vec<u8>, BlobError> {
        let url = format!("{}/blob/{}/{}", self.base, group_id, sha256_hex);
        let http = self.http.clone();
        run_blocking(async move {
            let resp = http
                .get(&url)
                .send()
                .await
                .map_err(|e| BlobError::Http(e.to_string()))?;
            match resp.status() {
                StatusCode::OK => {
                    let bytes = resp
                        .bytes()
                        .await
                        .map_err(|e| BlobError::Http(e.to_string()))?;
                    Ok(bytes.to_vec())
                }
                StatusCode::NOT_FOUND => Err(BlobError::NotFound),
                other => Err(BlobError::Rejected(format!("{}", other.as_u16()))),
            }
        })
    }
}

/// Run an async future to completion, blocking the calling thread without
/// requiring an ambient tokio runtime. Spawns a dedicated OS thread that
/// owns its own current-thread runtime.
fn run_blocking<F, T>(fut: F) -> Result<T, BlobError>
where
    F: std::future::Future<Output = Result<T, BlobError>> + Send + 'static,
    T: Send + 'static,
{
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| BlobError::Http(format!("runtime: {e}")))?;
        rt.block_on(fut)
    })
    .join()
    .map_err(|_| BlobError::JoinPanic)?
}

/// Convert any of `ws://` `wss://` `http://` `https://` (or scheme-less
/// `host:port`) into the http(s) base used for blob requests. Mirrors
/// the WS-side normalization in `client.rs::session`.
pub fn http_base(relay_url: &str) -> String {
    let trimmed = relay_url.trim_end_matches('/');
    if let Some(rest) = trimmed.strip_prefix("wss://") {
        return format!("https://{rest}");
    }
    if let Some(rest) = trimmed.strip_prefix("ws://") {
        return format!("http://{rest}");
    }
    if trimmed.starts_with("https://") || trimmed.starts_with("http://") {
        return trimmed.to_string();
    }
    format!("http://{trimmed}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_base_normalization() {
        assert_eq!(http_base("ws://r.example/"), "http://r.example");
        assert_eq!(http_base("wss://r.example"), "https://r.example");
        assert_eq!(http_base("https://r.example/x"), "https://r.example/x");
        assert_eq!(http_base("r.example:8787"), "http://r.example:8787");
    }
}
