use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use clipbridge_core::{Client, ClipKind, ClipListener, ClipPayload, ConnectionState, GroupKey};
use clipbridge_relay::{app, BlobStore, Hub};
use tokio::net::TcpListener;

async fn spawn_relay() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let blobs = BlobStore::new(8 * 1024 * 1024, Duration::from_secs(60), 4 * 1024 * 1024);
    let router = app(Hub::new(), blobs);
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    addr
}

#[derive(Default)]
struct Capture {
    clips: Mutex<Vec<ClipPayload>>,
    states: Mutex<Vec<String>>,
}

impl ClipListener for Capture {
    fn on_clip(&self, payload: ClipPayload) {
        self.clips.lock().unwrap().push(payload);
    }
    fn on_state(&self, state: ConnectionState) {
        let label = match state {
            ConnectionState::Connecting => "connecting".to_string(),
            ConnectionState::Connected => "connected".to_string(),
            ConnectionState::Disconnected => "disconnected".to_string(),
            ConnectionState::Error { message } => format!("error:{message}"),
        };
        self.states.lock().unwrap().push(label);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn two_clients_round_trip_through_relay() {
    let addr = spawn_relay().await;
    let relay_url = format!("ws://{addr}");
    let key = GroupKey::random().0.to_vec();
    let group_id = "shared".to_string();

    let cap_a: Arc<Capture> = Arc::new(Capture::default());
    let cap_b: Arc<Capture> = Arc::new(Capture::default());

    let client_a = Client::new(
        relay_url.clone(),
        group_id.clone(),
        key.clone(),
        "device-A".into(),
        cap_a.clone(),
    )
    .unwrap();
    let client_b = Client::new(
        relay_url.clone(),
        group_id.clone(),
        key.clone(),
        "device-B".into(),
        cap_b.clone(),
    )
    .unwrap();

    // Wait until both have registered Connected at least once.
    for _ in 0..50 {
        let a_ok = cap_a
            .states
            .lock()
            .unwrap()
            .iter()
            .any(|s| s == "connected");
        let b_ok = cap_b
            .states
            .lock()
            .unwrap()
            .iter()
            .any(|s| s == "connected");
        if a_ok && b_ok {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let payload = ClipPayload {
        kind: ClipKind::Text,
        content: "from A to B".into(),
        device_name: "Mac of A".into(),
        ts: 7,
        image: None,
    };
    client_a.send_clip(payload.clone()).unwrap();

    // Wait for B to receive.
    let mut received = None;
    for _ in 0..50 {
        if let Some(c) = cap_b.clips.lock().unwrap().first().cloned() {
            received = Some(c);
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let received = received.expect("B did not receive a clip");
    assert_eq!(received.content, "from A to B");
    assert_eq!(received.kind, ClipKind::Text);

    // A must not have echoed its own clip.
    assert!(cap_a.clips.lock().unwrap().is_empty(), "A received its own echo");

    client_a.stop();
    client_b.stop();
}

#[tokio::test(flavor = "multi_thread")]
async fn image_round_trip_through_blob_store() {
    let addr = spawn_relay().await;
    let relay_url = format!("ws://{addr}");
    let key = GroupKey::random().0.to_vec();
    let group_id = "img-shared".to_string();

    let cap_a: Arc<Capture> = Arc::new(Capture::default());
    let cap_b: Arc<Capture> = Arc::new(Capture::default());

    let client_a = Client::new(
        relay_url.clone(),
        group_id.clone(),
        key.clone(),
        "device-A".into(),
        cap_a.clone(),
    )
    .unwrap();
    let client_b = Client::new(
        relay_url.clone(),
        group_id.clone(),
        key.clone(),
        "device-B".into(),
        cap_b.clone(),
    )
    .unwrap();

    for _ in 0..50 {
        let a_ok = cap_a
            .states
            .lock()
            .unwrap()
            .iter()
            .any(|s| s == "connected");
        let b_ok = cap_b
            .states
            .lock()
            .unwrap()
            .iter()
            .any(|s| s == "connected");
        if a_ok && b_ok {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // 64KB pseudo-image — large enough to exercise the network path,
    // small enough to keep the test under a second.
    let image_bytes: Vec<u8> = (0..64 * 1024).map(|i| (i % 251) as u8).collect();
    let client_a_clone = client_a.clone();
    let send_image_bytes = image_bytes.clone();
    tokio::task::spawn_blocking(move || {
        client_a_clone
            .send_image(
                send_image_bytes,
                "image/png".into(),
                640,
                480,
                "Mac of A".into(),
                42,
            )
            .unwrap();
    })
    .await
    .unwrap();

    // Wait for B to receive the image meta.
    let mut received = None;
    for _ in 0..50 {
        if let Some(c) = cap_b.clips.lock().unwrap().first().cloned() {
            received = Some(c);
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let received = received.expect("B did not receive an image meta");
    assert_eq!(received.kind, ClipKind::Image);
    let meta = received.image.expect("expected image meta");
    assert_eq!(meta.width, 640);
    assert_eq!(meta.size_bytes, image_bytes.len() as u64);

    // B fetches the actual bytes and decrypts.
    let client_b_clone = client_b.clone();
    let plain = tokio::task::spawn_blocking(move || client_b_clone.fetch_image(meta).unwrap())
        .await
        .unwrap();
    assert_eq!(plain, image_bytes);

    client_a.stop();
    client_b.stop();
}
