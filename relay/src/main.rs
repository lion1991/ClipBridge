use std::net::SocketAddr;

use clipbridge_relay::{app, Hub};
use tracing_subscriber::EnvFilter;

// Single-threaded runtime: the relay is purely I/O-bound (a handful of
// WebSocket sockets, no blocking work) so a worker pool buys us nothing,
// while it does fail to start on hosts whose Docker still ships the old
// default seccomp profile that blocks `clone3` (the syscall glibc 2.34+
// uses to spawn threads). Avoiding the spawn altogether keeps the image
// portable across older VMs.
#[tokio::main(flavor = "current_thread")]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let hub = Hub::new();
    let router = app(hub);

    let addr: SocketAddr = std::env::var("CLIPBRIDGE_BIND")
        .unwrap_or_else(|_| "0.0.0.0:8787".into())
        .parse()
        .expect("invalid CLIPBRIDGE_BIND");

    tracing::info!(%addr, "clipbridge-relay listening");
    let listener = tokio::net::TcpListener::bind(addr).await.expect("bind");
    axum::serve(listener, router).await.expect("serve");
}
