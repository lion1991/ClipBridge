use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    response::IntoResponse,
};
use clipbridge_core::protocol::{ClientMessage, RecentClip, ServerMessage};
use futures_util::{SinkExt, StreamExt};

use crate::hub::Hub;

pub async fn ws_handler(ws: WebSocketUpgrade, State(hub): State<Hub>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, hub))
}

async fn handle_socket(socket: WebSocket, hub: Hub) {
    let (mut ws_tx, mut ws_rx) = socket.split();

    // Per-connection state
    let mut group: Option<String> = None;
    let mut device_id: Option<String> = None;
    let mut rx: Option<tokio::sync::broadcast::Receiver<RecentClip>> = None;

    loop {
        tokio::select! {
            // Inbound from client
            incoming = ws_rx.next() => {
                let Some(Ok(msg)) = incoming else { break };
                let text = match msg {
                    Message::Text(t) => t,
                    Message::Binary(_) => continue,
                    Message::Ping(p) => { let _ = ws_tx.send(Message::Pong(p)).await; continue; }
                    Message::Pong(_) => continue,
                    Message::Close(_) => break,
                };
                let parsed: Result<ClientMessage, _> = serde_json::from_str(&text);
                match parsed {
                    Ok(ClientMessage::Join { group_id, device_id: did }) => {
                        rx = Some(hub.subscribe(&group_id));
                        let reply = ServerMessage::Joined { group_id: group_id.clone() };
                        device_id = Some(did);
                        group = Some(group_id);
                        let _ = ws_tx.send(Message::Text(serde_json::to_string(&reply).unwrap())).await;
                    }
                    Ok(ClientMessage::Publish { group_id, ciphertext, nonce, ts }) => {
                        let Some(did) = device_id.clone() else {
                            let _ = ws_tx.send(Message::Text(serde_json::to_string(&ServerMessage::Error { reason: "join first".into() }).unwrap())).await;
                            continue;
                        };
                        if group.as_deref() != Some(group_id.as_str()) {
                            let _ = ws_tx.send(Message::Text(serde_json::to_string(&ServerMessage::Error { reason: "group mismatch".into() }).unwrap())).await;
                            continue;
                        }
                        let clip = RecentClip { ciphertext, nonce, ts, sender_device_id: did };
                        hub.publish(&group_id, clip);
                    }
                    Ok(ClientMessage::FetchRecent { group_id }) => {
                        if group.as_deref() != Some(group_id.as_str()) {
                            let _ = ws_tx.send(Message::Text(serde_json::to_string(&ServerMessage::Error { reason: "group mismatch".into() }).unwrap())).await;
                            continue;
                        }
                        let clips = hub.recent(&group_id);
                        let reply = ServerMessage::Recent { clips };
                        let _ = ws_tx.send(Message::Text(serde_json::to_string(&reply).unwrap())).await;
                    }
                    Err(e) => {
                        let reply = ServerMessage::Error { reason: format!("bad message: {e}") };
                        let _ = ws_tx.send(Message::Text(serde_json::to_string(&reply).unwrap())).await;
                    }
                }
            }
            // Outbound from broadcast
            broadcast = async {
                match &mut rx {
                    Some(r) => r.recv().await.ok(),
                    None => std::future::pending::<Option<RecentClip>>().await,
                }
            } => {
                let Some(clip) = broadcast else { continue };
                if Some(&clip.sender_device_id) == device_id.as_ref() {
                    continue; // don't echo to sender
                }
                let msg = ServerMessage::Clip {
                    ciphertext: clip.ciphertext,
                    nonce: clip.nonce,
                    ts: clip.ts,
                    sender_device_id: clip.sender_device_id,
                };
                let _ = ws_tx.send(Message::Text(serde_json::to_string(&msg).unwrap())).await;
            }
        }
    }
}
