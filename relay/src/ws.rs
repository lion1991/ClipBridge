use std::net::SocketAddr;
use std::time::Duration;

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        ConnectInfo, State,
    },
    response::IntoResponse,
};
use clipbridge_core::protocol::{ClientMessage, LanPeer, RecentClip, ServerMessage};
use futures_util::{SinkExt, StreamExt};

use crate::hub::Hub;

pub async fn ws_handler(
    ws: WebSocketUpgrade,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    State(hub): State<Hub>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, hub, peer.ip()))
}

async fn handle_socket(socket: WebSocket, hub: Hub, egress: std::net::IpAddr) {
    let (mut ws_tx, mut ws_rx) = socket.split();

    // Per-connection state
    let mut group: Option<String> = None;
    let mut device_id: Option<String> = None;
    let mut rx: Option<tokio::sync::broadcast::Receiver<RecentClip>> = None;

    // Rendezvous state. `rv_group` is set once the client opts in via
    // `LanAdvertise`; it's what we use to deregister on disconnect. The
    // mailbox carries per-connection `LanPeers` snapshots pushed by the
    // hub whenever this egress group's membership changes.
    let conn_id = hub.next_conn_id();
    let mut rv_group: Option<String> = None;
    let (rv_tx, mut rv_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<LanPeer>>();

    // Keep idle connections from accumulating: ping every 30s, drop the
    // socket if no inbound frame for 60s.
    const PING_EVERY: Duration = Duration::from_secs(30);
    const IDLE_TIMEOUT: Duration = Duration::from_secs(60);
    let mut ping_interval = tokio::time::interval(PING_EVERY);
    ping_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    ping_interval.tick().await;
    let mut last_seen = tokio::time::Instant::now();

    loop {
        let idle_deadline = last_seen + IDLE_TIMEOUT;
        tokio::select! {
            biased;
            _ = tokio::time::sleep_until(idle_deadline) => {
                tracing::debug!(?device_id, "idle timeout, closing socket");
                break;
            }
            _ = ping_interval.tick() => {
                if ws_tx.send(Message::Ping(Vec::new())).await.is_err() { break; }
            }
            // Inbound from client
            incoming = ws_rx.next() => {
                last_seen = tokio::time::Instant::now();
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
                    Ok(ClientMessage::LanAdvertise { group_id, device_id: did, candidates }) => {
                        let Some(joined_device_id) = device_id.clone() else {
                            let _ = ws_tx.send(Message::Text(serde_json::to_string(&ServerMessage::Error { reason: "join first".into() }).unwrap())).await;
                            continue;
                        };
                        if group.as_deref() != Some(group_id.as_str()) {
                            let _ = ws_tx.send(Message::Text(serde_json::to_string(&ServerMessage::Error { reason: "group mismatch".into() }).unwrap())).await;
                            continue;
                        }
                        if did != joined_device_id {
                            let _ = ws_tx.send(Message::Text(serde_json::to_string(&ServerMessage::Error { reason: "device mismatch".into() }).unwrap())).await;
                            continue;
                        }
                        // Opt in to relay-assisted rendezvous. Register (or
                        // refresh, if re-sent after a network change) under
                        // this connection's egress IP; the hub immediately
                        // pushes us whatever same-LAN peers already exist
                        // and notifies the rest that we joined. `LanPeers`
                        // only ever reaches connections that reach here, so
                        // old clients (which never send this) are unaffected.
                        tracing::info!(
                            device_id = %joined_device_id,
                            %egress,
                            candidate_count = candidates.len(),
                            candidates = ?candidates,
                            "lan advertise received"
                        );
                        rv_group = Some(group_id.clone());
                        hub.rendezvous_upsert(
                            &group_id,
                            egress,
                            conn_id,
                            joined_device_id,
                            candidates,
                            rv_tx.clone(),
                        );
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
            // Rendezvous snapshot pushed by the hub (membership changed).
            rv = rv_rx.recv() => {
                let Some(peers) = rv else { continue };
                let msg = ServerMessage::LanPeers { peers };
                let _ = ws_tx.send(Message::Text(serde_json::to_string(&msg).unwrap())).await;
            }
        }
    }

    // Socket closed: drop our rendezvous slot so same-LAN peers stop being
    // told to dial us and purge our stale candidates.
    if let Some(g) = rv_group {
        hub.rendezvous_remove(&g, egress, conn_id);
    }
}
