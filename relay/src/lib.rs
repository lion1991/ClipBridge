pub mod blob;
pub mod blob_routes;
pub mod hub;
pub mod ws;

use std::time::Duration;

use axum::{
    extract::DefaultBodyLimit,
    routing::{get, put},
    Router,
};
use tower_http::limit::RequestBodyLimitLayer;

pub use blob::BlobStore;
pub use hub::Hub;

/// Build the relay router. The `Hub` carries WS state; the `BlobStore`
/// carries opaque ciphertext for image / file payloads.
pub fn app(hub: Hub, blobs: BlobStore) -> Router {
    let max = blobs.max_blob_bytes();
    let blob_router = Router::new()
        .route("/blob/:group_id/:sha256", put(blob_routes::put_blob))
        .route("/blob/:group_id/:sha256", get(blob_routes::get_blob))
        // Axum's default 2MB cap is much smaller than a normal screenshot;
        // raise it to the per-blob max so PUT bodies aren't truncated.
        .layer(DefaultBodyLimit::disable())
        .layer(RequestBodyLimitLayer::new(max))
        .with_state(blobs);

    Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/ws", get(ws::ws_handler))
        .with_state(hub)
        .merge(blob_router)
}

/// Read blob-store knobs from env (called by `main`). Defaults are tuned
/// for a single-tenant hobby relay; bump them when you have headroom.
pub fn blob_store_from_env() -> BlobStore {
    let budget = parse_env_u64("CLIPBRIDGE_BLOB_BUDGET_BYTES", 256 * 1024 * 1024);
    let ttl_secs = parse_env_u64("CLIPBRIDGE_BLOB_TTL_SECS", 300);
    let max_blob = parse_env_u64("CLIPBRIDGE_BLOB_MAX_BYTES", 32 * 1024 * 1024);
    BlobStore::new(budget, Duration::from_secs(ttl_secs), max_blob as usize)
}

fn parse_env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}
