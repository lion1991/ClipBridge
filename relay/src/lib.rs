pub mod hub;
pub mod ws;

use axum::{routing::get, Router};

pub use hub::Hub;

pub fn app(hub: Hub) -> Router {
    Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/ws", get(ws::ws_handler))
        .with_state(hub)
}
