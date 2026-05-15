use std::net::SocketAddr;
use std::time::Duration;

use clipbridge_core::{
    crypto, encrypt,
    protocol::{ClientMessage, ClipKind, ClipPayload, ServerMessage},
};
use clipbridge_relay::{app, BlobStore, Hub};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

async fn spawn_relay() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let blobs = BlobStore::new(8 * 1024 * 1024, Duration::from_secs(60), 4 * 1024 * 1024);
    let router = app(Hub::new(), blobs);
    tokio::spawn(async move {
        axum::serve(
            listener,
            router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    });
    // Tiny pause to ensure the listener is accepting; usually not needed but
    // protects against very fast test machines hitting connect-before-accept.
    tokio::time::sleep(Duration::from_millis(20)).await;
    addr
}

async fn connect(
    addr: SocketAddr,
) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>> {
    let url = format!("ws://{addr}/ws");
    let (ws, _) = tokio_tungstenite::connect_async(url).await.unwrap();
    ws
}

async fn send_json<S>(ws: &mut S, msg: &ClientMessage)
where
    S: futures_util::SinkExt<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
{
    let s = serde_json::to_string(msg).unwrap();
    ws.send(Message::Text(s.into())).await.unwrap();
}

async fn next_server_msg<S>(ws: &mut S) -> ServerMessage
where
    S: futures_util::StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>>
        + Unpin,
{
    loop {
        let frame = ws.next().await.expect("stream ended").unwrap();
        if let Message::Text(t) = frame {
            return serde_json::from_str(&t).unwrap();
        }
    }
}

#[tokio::test]
async fn publish_is_received_and_decrypts() {
    let addr = spawn_relay().await;
    let mut a = connect(addr).await;
    let mut b = connect(addr).await;

    let group_id = "group-test-1".to_string();
    let key = [42u8; crypto::KEY_LEN];

    send_json(
        &mut a,
        &ClientMessage::Join {
            group_id: group_id.clone(),
            device_id: "A".into(),
        },
    )
    .await;
    send_json(
        &mut b,
        &ClientMessage::Join {
            group_id: group_id.clone(),
            device_id: "B".into(),
        },
    )
    .await;

    matches!(next_server_msg(&mut a).await, ServerMessage::Joined { .. });
    matches!(next_server_msg(&mut b).await, ServerMessage::Joined { .. });

    let payload = ClipPayload {
        kind: ClipKind::Text,
        content: "hello from A".into(),
        device_name: "Mac of A".into(),
        ts: 1234,
        image: None,
    };
    let plaintext = serde_json::to_vec(&payload).unwrap();
    let (ciphertext, nonce) = encrypt(&key, &plaintext).unwrap();

    send_json(
        &mut a,
        &ClientMessage::Publish {
            group_id: group_id.clone(),
            ciphertext,
            nonce: nonce.to_vec(),
            ts: 1234,
        },
    )
    .await;

    // B should receive a Clip
    let received = tokio::time::timeout(Duration::from_secs(2), next_server_msg(&mut b))
        .await
        .expect("B did not receive in time");

    let ServerMessage::Clip {
        ciphertext, nonce, ..
    } = received
    else {
        panic!("expected Clip, got {received:?}");
    };

    let decrypted = clipbridge_core::decrypt(&key, &nonce, &ciphertext).unwrap();
    let payload: ClipPayload = serde_json::from_slice(&decrypted).unwrap();
    assert_eq!(payload.content, "hello from A");
    assert_eq!(payload.kind, ClipKind::Text);
}

#[tokio::test]
async fn sender_does_not_echo_to_self() {
    let addr = spawn_relay().await;
    let mut a = connect(addr).await;

    let group_id = "group-no-echo".to_string();
    let key = [9u8; crypto::KEY_LEN];

    send_json(
        &mut a,
        &ClientMessage::Join {
            group_id: group_id.clone(),
            device_id: "solo".into(),
        },
    )
    .await;
    matches!(next_server_msg(&mut a).await, ServerMessage::Joined { .. });

    let (ct, nonce) = encrypt(&key, b"x").unwrap();
    send_json(
        &mut a,
        &ClientMessage::Publish {
            group_id,
            ciphertext: ct,
            nonce: nonce.to_vec(),
            ts: 1,
        },
    )
    .await;

    // Wait briefly — A should NOT receive its own clip.
    let res = tokio::time::timeout(Duration::from_millis(300), next_server_msg(&mut a)).await;
    assert!(res.is_err(), "sender should not receive its own clip");
}

#[tokio::test]
async fn fetch_recent_returns_cached_clips() {
    let addr = spawn_relay().await;
    let mut a = connect(addr).await;

    let group_id = "group-recent".to_string();
    let key = [3u8; crypto::KEY_LEN];

    send_json(
        &mut a,
        &ClientMessage::Join {
            group_id: group_id.clone(),
            device_id: "A".into(),
        },
    )
    .await;
    matches!(next_server_msg(&mut a).await, ServerMessage::Joined { .. });

    for i in 0..2u64 {
        let (ct, nonce) = encrypt(&key, format!("clip-{i}").as_bytes()).unwrap();
        send_json(
            &mut a,
            &ClientMessage::Publish {
                group_id: group_id.clone(),
                ciphertext: ct,
                nonce: nonce.to_vec(),
                ts: i,
            },
        )
        .await;
    }

    // Tiny delay so publishes settle into the cache.
    tokio::time::sleep(Duration::from_millis(50)).await;

    send_json(
        &mut a,
        &ClientMessage::FetchRecent {
            group_id: group_id.clone(),
        },
    )
    .await;

    let reply = tokio::time::timeout(Duration::from_secs(2), next_server_msg(&mut a))
        .await
        .unwrap();
    let ServerMessage::Recent { clips } = reply else {
        panic!("expected Recent, got {reply:?}");
    };
    assert_eq!(clips.len(), 2);
    let plain0 = clipbridge_core::decrypt(&key, &clips[0].nonce, &clips[0].ciphertext).unwrap();
    assert_eq!(plain0, b"clip-0");
}

/// Wait for the next `LanPeers` push, skipping any other server messages
/// (e.g. the `Joined` ack). Returns `None` on timeout.
async fn next_lan_peers<S>(ws: &mut S, within: Duration) -> Option<Vec<clipbridge_core::protocol::LanPeer>>
where
    S: futures_util::StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>>
        + Unpin,
{
    tokio::time::timeout(within, async {
        loop {
            if let ServerMessage::LanPeers { peers } = next_server_msg(ws).await {
                return peers;
            }
        }
    })
    .await
    .ok()
}

#[tokio::test]
async fn rendezvous_introduces_same_egress_peers() {
    let addr = spawn_relay().await;
    let mut a = connect(addr).await;
    let mut b = connect(addr).await;
    let group_id = "rv-group".to_string();

    for (ws, did) in [(&mut a, "A"), (&mut b, "B")] {
        send_json(
            ws,
            &ClientMessage::Join {
                group_id: group_id.clone(),
                device_id: did.into(),
            },
        )
        .await;
    }
    matches!(next_server_msg(&mut a).await, ServerMessage::Joined { .. });
    matches!(next_server_msg(&mut b).await, ServerMessage::Joined { .. });

    // A opts into rendezvous first: it's alone, so the snapshot is empty.
    send_json(
        &mut a,
        &ClientMessage::LanAdvertise {
            group_id: group_id.clone(),
            device_id: "A".into(),
            candidates: vec!["192.168.1.10:5000".into()],
        },
    )
    .await;
    assert_eq!(
        next_lan_peers(&mut a, Duration::from_secs(2)).await.unwrap(),
        vec![]
    );

    // B opts in from the same egress IP (loopback) — B should immediately
    // learn A, and A should be re-pushed a snapshot now containing B.
    send_json(
        &mut b,
        &ClientMessage::LanAdvertise {
            group_id: group_id.clone(),
            device_id: "B".into(),
            candidates: vec!["192.168.1.11:6000".into()],
        },
    )
    .await;

    let b_sees = next_lan_peers(&mut b, Duration::from_secs(2)).await.unwrap();
    assert_eq!(b_sees.len(), 1);
    assert_eq!(b_sees[0].device_id, "A");
    assert_eq!(b_sees[0].candidates, vec!["192.168.1.10:5000".to_string()]);

    let a_sees = next_lan_peers(&mut a, Duration::from_secs(2)).await.unwrap();
    assert_eq!(a_sees.len(), 1);
    assert_eq!(a_sees[0].device_id, "B");
}

/// Mixed-fleet safety: a client that never sends `LanAdvertise` (an old
/// build) must never be pushed a `LanPeers` message, even when a new
/// rendezvous-capable peer joins the same group and egress.
#[tokio::test]
async fn old_client_never_receives_lan_peers() {
    let addr = spawn_relay().await;
    let mut old = connect(addr).await;
    let mut newc = connect(addr).await;
    let group_id = "mixed-fleet".to_string();

    for (ws, did) in [(&mut old, "old"), (&mut newc, "new")] {
        send_json(
            ws,
            &ClientMessage::Join {
                group_id: group_id.clone(),
                device_id: did.into(),
            },
        )
        .await;
    }
    matches!(next_server_msg(&mut old).await, ServerMessage::Joined { .. });
    matches!(next_server_msg(&mut newc).await, ServerMessage::Joined { .. });

    // The new client opts in; the old one stays silent (as an old build
    // would — it doesn't know the message exists).
    send_json(
        &mut newc,
        &ClientMessage::LanAdvertise {
            group_id: group_id.clone(),
            device_id: "new".into(),
            candidates: vec!["192.168.1.20:7000".into()],
        },
    )
    .await;
    // The new client itself gets a (self-only, empty) snapshot — proving
    // the relay is alive and processed the advertise.
    assert_eq!(
        next_lan_peers(&mut newc, Duration::from_secs(2)).await.unwrap(),
        vec![]
    );

    // The old connection must NOT have been pushed anything.
    assert!(
        next_lan_peers(&mut old, Duration::from_millis(500))
            .await
            .is_none(),
        "old client must never receive LanPeers"
    );
}
