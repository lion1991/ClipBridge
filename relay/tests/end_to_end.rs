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
        axum::serve(listener, router).await.unwrap();
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
