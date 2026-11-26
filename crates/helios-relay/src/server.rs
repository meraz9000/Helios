use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use futures_util::{SinkExt, StreamExt};
use helios_proto::crypto::generate_nonce_b64;
use helios_proto::{
    DeviceId, DeviceRole, HostSummary, PROTOCOL_VERSION, SignalingMessage, verify_signature,
};
use serde_json::json;
use tokio::sync::{RwLock, mpsc};
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::store::DeviceStore;

#[derive(Debug, Clone)]
pub struct RelayConfig {
    /// Public `host:port` of the droplet's integrated QUIC UDP rendezvous & relay socket
    /// (e.g., `relay.yourdomain.com:3478` or `127.0.0.1:3478`).
    pub udp_relay_addr: String,
    /// If true, any valid Ed25519 signature is accepted without prior pairing (dev/test only!).
    pub allow_open_enrollment: bool,
}

impl Default for RelayConfig {
    fn default() -> Self {
        Self {
            udp_relay_addr: "127.0.0.1:3478".into(),
            allow_open_enrollment: false,
        }
    }
}

struct ConnectedPeer {
    device_id: DeviceId,
    name: String,
    os: String,
    role: DeviceRole,
    tx: mpsc::UnboundedSender<SignalingMessage>,
    active_sessions: u32,
}

struct ActiveSession {
    host_id: DeviceId,
    client_id: DeviceId,
}

pub struct RelayState {
    pub config: RelayConfig,
    pub store: RwLock<DeviceStore>,
    peers: RwLock<HashMap<DeviceId, ConnectedPeer>>,
    sessions: RwLock<HashMap<Uuid, ActiveSession>>,
}

impl RelayState {
    pub fn new(config: RelayConfig, store: DeviceStore) -> Arc<Self> {
        Arc::new(Self {
            config,
            store: RwLock::new(store),
            peers: RwLock::new(HashMap::new()),
            sessions: RwLock::new(HashMap::new()),
        })
    }
}

pub fn build_router(state: Arc<RelayState>) -> Router {
    Router::new()
        .route("/health", get(health_handler))
        .route("/ws", get(ws_upgrade_handler))
        .with_state(state)
}

async fn health_handler(State(state): State<Arc<RelayState>>) -> impl IntoResponse {
    let peers = state.peers.read().await;
    let online_hosts = peers
        .values()
        .filter(|p| p.role == DeviceRole::Host)
        .count();
    let online_clients = peers
        .values()
        .filter(|p| p.role == DeviceRole::Client)
        .count();
    let sessions = state.sessions.read().await.len();

    Json(json!({
        "status": "ok",
        "transport": "quic",
        "protocol_version": PROTOCOL_VERSION,
        "udp_relay_addr": state.config.udp_relay_addr,
        "online_hosts": online_hosts,
        "online_clients": online_clients,
        "active_sessions": sessions,
    }))
}

async fn ws_upgrade_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<RelayState>>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

async fn send_ws_json(
    sender: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    msg: &SignalingMessage,
) -> Result<(), ()> {
    let text = serde_json::to_string(msg).map_err(|_| ())?;
    sender.send(Message::Text(text.into())).await.map_err(|_| ())
}

async fn handle_socket(socket: WebSocket, state: Arc<RelayState>) {
    let (mut ws_tx, mut ws_rx) = socket.split();

    // 1. Send cryptographic challenge nonce
    let nonce = generate_nonce_b64();
    let challenge = SignalingMessage::Challenge {
        nonce: nonce.clone(),
        protocol_version: PROTOCOL_VERSION,
    };
    if send_ws_json(&mut ws_tx, &challenge).await.is_err() {
        return;
    }

    // 2. Wait for Authenticate response
    let auth_msg = match ws_rx.next().await {
        Some(Ok(Message::Text(txt))) => serde_json::from_str::<SignalingMessage>(&txt).ok(),
        _ => None,
    };

    let (name, os, role, public_key_b64, signature_b64, pairing_token) = match auth_msg {
        Some(SignalingMessage::Authenticate {
            name,
            os,
            role,
            public_key_b64,
            signature_b64,
            pairing_token,
        }) => (name, os, role, public_key_b64, signature_b64, pairing_token),
        _ => {
            let _ = send_ws_json(
                &mut ws_tx,
                &SignalingMessage::Error {
                    code: "auth_required".into(),
                    message: "Expected Authenticate message in response to Challenge".into(),
                },
            )
            .await;
            return;
        }
    };

    // 3. Verify Ed25519 signature over challenge nonce
    let device_id = match verify_signature(&public_key_b64, nonce.as_bytes(), &signature_b64) {
        Ok(id) => id,
        Err(err) => {
            warn!("rejected invalid Ed25519 signature: {err}");
            let _ = send_ws_json(
                &mut ws_tx,
                &SignalingMessage::Error {
                    code: "invalid_signature".into(),
                    message: "Ed25519 challenge signature verification failed".into(),
                },
            )
            .await;
            return;
        }
    };

    // 4. Enforce pairing allowlist
    let authorized = {
        let store = state.store.read().await;
        store.is_authorized(&device_id, &public_key_b64)
    };

    if !authorized && !state.config.allow_open_enrollment {
        let enrolled = if let Some(tok) = pairing_token.as_deref() {
            let mut store = state.store.write().await;
            store
                .enroll_with_token(
                    tok,
                    device_id.clone(),
                    name.clone(),
                    os.clone(),
                    role,
                    public_key_b64.clone(),
                )
                .unwrap_or(false)
        } else {
            false
        };

        if !enrolled {
            warn!(%device_id, %name, "unauthorized device rejected");
            let _ = send_ws_json(
                &mut ws_tx,
                &SignalingMessage::Error {
                    code: "unauthorized".into(),
                    message: format!(
                        "Device {device_id} is not paired. Pass a valid --pairing-token to enroll."
                    ),
                },
            )
            .await;
            return;
        }
        info!(%device_id, %name, ?role, "enrolled new paired device");
    }

    // 5. Register active peer channel
    let (tx, mut rx) = mpsc::unbounded_channel::<SignalingMessage>();
    if send_ws_json(
        &mut ws_tx,
        &SignalingMessage::AuthOk {
            device_id: device_id.clone(),
            udp_relay_addr: state.config.udp_relay_addr.clone(),
        },
    )
    .await
    .is_err()
    {
        return;
    }

    {
        let mut peers = state.peers.write().await;
        peers.insert(
            device_id.clone(),
            ConnectedPeer {
                device_id: device_id.clone(),
                name: name.clone(),
                os: os.clone(),
                role,
                tx: tx.clone(),
                active_sessions: 0,
            },
        );
    }
    info!(%device_id, %name, ?role, "device connected to QUIC relay coordinator");

    let writer = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if send_ws_json(&mut ws_tx, &msg).await.is_err() {
                break;
            }
        }
    });

    while let Some(frame) = ws_rx.next().await {
        let Ok(msg) = frame else { break };
        match msg {
            Message::Text(txt) => match serde_json::from_str::<SignalingMessage>(&txt) {
                Ok(sig) => handle_signaling_message(&state, &device_id, sig).await,
                Err(err) => {
                    debug!(%device_id, "ignoring malformed signaling JSON: {err}");
                }
            },
            Message::Close(_) => break,
            Message::Ping(_) | Message::Pong(_) | Message::Binary(_) => {}
        }
    }

    writer.abort();
    cleanup_peer(&state, &device_id).await;
    info!(%device_id, "device disconnected from relay");
}

async fn handle_signaling_message(
    state: &Arc<RelayState>,
    sender_id: &DeviceId,
    msg: SignalingMessage,
) {
    match msg {
        SignalingMessage::Heartbeat { .. } => {
            let now_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64;
            if let Some(peer) = state.peers.read().await.get(sender_id) {
                let _ = peer.tx.send(SignalingMessage::HeartbeatAck {
                    timestamp_ms: now_ms,
                });
            }
        }

        SignalingMessage::ListHosts => {
            let peers = state.peers.read().await;
            let hosts: Vec<HostSummary> = peers
                .values()
                .filter(|p| p.role == DeviceRole::Host)
                .map(|p| HostSummary {
                    device_id: p.device_id.clone(),
                    name: p.name.clone(),
                    os: p.os.clone(),
                    online: true,
                    active_sessions: p.active_sessions,
                })
                .collect();
            if let Some(peer) = peers.get(sender_id) {
                let _ = peer.tx.send(SignalingMessage::HostList { hosts });
            }
        }

        SignalingMessage::RequestSession {
            target_host_id,
            access_token,
        } => {
            let mut peers = state.peers.write().await;
            let Some(client_name) = peers.get(sender_id).map(|p| p.name.clone()) else {
                return;
            };
            let host_exists = peers
                .get(&target_host_id)
                .is_some_and(|p| p.role == DeviceRole::Host);

            if !host_exists {
                if let Some(client) = peers.get(sender_id) {
                    let _ = client.tx.send(SignalingMessage::Error {
                        code: "host_offline".into(),
                        message: format!("Host {target_host_id} is not currently connected"),
                    });
                }
                return;
            }

            let session_id = Uuid::new_v4();
            let udp_relay_addr = state.config.udp_relay_addr.clone();

            if let Some(host) = peers.get_mut(&target_host_id) {
                host.active_sessions += 1;
                let _ = host.tx.send(SignalingMessage::IncomingSession {
                    session_id,
                    client_id: sender_id.clone(),
                    client_name,
                    udp_relay_addr: udp_relay_addr.clone(),
                    access_token,
                });
            }
            if let Some(client) = peers.get(sender_id) {
                let _ = client.tx.send(SignalingMessage::SessionCreated {
                    session_id,
                    host_id: target_host_id.clone(),
                    udp_relay_addr,
                });
            }

            state.sessions.write().await.insert(
                session_id,
                ActiveSession {
                    host_id: target_host_id.clone(),
                    client_id: sender_id.clone(),
                },
            );
            info!(%session_id, client = %sender_id, host = %target_host_id, "dispatched QUIC session signal");
        }

        SignalingMessage::QuicPeerHello { session_id, .. } => {
            relay_to_session_peer(state, sender_id, session_id, msg).await;
        }

        SignalingMessage::EndSession { session_id, reason } => {
            let removed = state.sessions.write().await.remove(&session_id);
            if let Some(sess) = removed {
                let target = if &sess.host_id == sender_id {
                    &sess.client_id
                } else {
                    &sess.host_id
                };
                let mut peers = state.peers.write().await;
                if let Some(host) = peers.get_mut(&sess.host_id) {
                    host.active_sessions = host.active_sessions.saturating_sub(1);
                }
                if let Some(other) = peers.get(target) {
                    let _ = other.tx.send(SignalingMessage::EndSession {
                        session_id,
                        reason,
                    });
                }
            }
        }

        _ => {}
    }
}

async fn relay_to_session_peer(
    state: &Arc<RelayState>,
    sender_id: &DeviceId,
    session_id: Uuid,
    msg: SignalingMessage,
) {
    let target_id = {
        let sessions = state.sessions.read().await;
        let Some(sess) = sessions.get(&session_id) else {
            return;
        };
        if &sess.host_id == sender_id {
            sess.client_id.clone()
        } else if &sess.client_id == sender_id {
            sess.host_id.clone()
        } else {
            return;
        }
    };

    if let Some(target) = state.peers.read().await.get(&target_id) {
        let _ = target.tx.send(msg);
    }
}

async fn cleanup_peer(state: &Arc<RelayState>, device_id: &DeviceId) {
    state.peers.write().await.remove(device_id);

    let affected: Vec<(Uuid, DeviceId)> = {
        let mut sessions = state.sessions.write().await;
        let mut to_remove = Vec::new();
        for (sid, sess) in sessions.iter() {
            if &sess.host_id == device_id {
                to_remove.push((*sid, sess.client_id.clone()));
            } else if &sess.client_id == device_id {
                to_remove.push((*sid, sess.host_id.clone()));
            }
        }
        for (sid, _) in &to_remove {
            sessions.remove(sid);
        }
        to_remove
    };

    let mut peers = state.peers.write().await;
    for (session_id, other_id) in affected {
        if let Some(other) = peers.get_mut(&other_id) {
            if other.role == DeviceRole::Host {
                other.active_sessions = other.active_sessions.saturating_sub(1);
            }
            let _ = other.tx.send(SignalingMessage::EndSession {
                session_id,
                reason: "peer_disconnected".into(),
            });
        }
    }
}
