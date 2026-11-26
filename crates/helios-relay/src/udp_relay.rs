//! Integrated UDP NAT Discovery (STUN-like observed address echo) and
//! Encrypted QUIC UDP Relay Fallback running inside `helios-relay`.
//!
//! Because QUIC traffic is end-to-end encrypted with TLS 1.3 and pinned peer
//! certificate fingerprints, this relay only ever forwards opaque encrypted UDP
//! datagrams when direct P2P UDP hole-punching is blocked by symmetric NAT or
//! restrictive firewalls.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use helios_proto::udp::{
    DecodedUdpPacket, ROLE_CLIENT, ROLE_HOST, decode_udp_packet, encode_observed_addr,
    encode_relay_quic,
};
use tokio::net::UdpSocket;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};
use uuid::Uuid;

#[derive(Debug, Default)]
struct SessionUdpPeers {
    host_addr: Option<SocketAddr>,
    client_addr: Option<SocketAddr>,
    last_seen: Option<Instant>,
}

/// Spawn the UDP rendezvous & QUIC relay socket task.
/// Returns the actual bound `SocketAddr` (useful when binding `:0` in tests)
/// and a `JoinHandle` for the background server loop.
pub async fn spawn_udp_relay(
    bind_addr: SocketAddr,
) -> Result<(SocketAddr, tokio::task::JoinHandle<()>)> {
    let socket = Arc::new(UdpSocket::bind(bind_addr).await?);
    let local_addr = socket.local_addr()?;
    info!(%local_addr, "started integrated QUIC UDP rendezvous & relay listener");

    let sessions: Arc<RwLock<HashMap<Uuid, SessionUdpPeers>>> =
        Arc::new(RwLock::new(HashMap::new()));

    // Periodic stale session cleanup
    let cleanup_sessions = sessions.clone();
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(60));
        loop {
            ticker.tick().await;
            let now = Instant::now();
            cleanup_sessions.write().await.retain(|_, entry| {
                entry
                    .last_seen
                    .is_some_and(|t| now.duration_since(t) < Duration::from_secs(300))
            });
        }
    });

    let handle = tokio::spawn(async move {
        let mut buf = vec![0u8; 65536];
        loop {
            let (n, src_addr) = match socket.recv_from(&mut buf).await {
                Ok(v) => v,
                Err(err) => {
                    warn!("UDP relay recv_from error: {err}");
                    continue;
                }
            };

            let pkt = &buf[..n];
            match decode_udp_packet(pkt) {
                Some(DecodedUdpPacket::Register {
                    session_id,
                    role_byte,
                }) => {
                    {
                        let mut map = sessions.write().await;
                        let entry = map.entry(session_id).or_default();
                        if role_byte == ROLE_HOST {
                            entry.host_addr = Some(src_addr);
                        } else if role_byte == ROLE_CLIENT {
                            entry.client_addr = Some(src_addr);
                        }
                        entry.last_seen = Some(Instant::now());
                    }
                    debug!(%session_id, role_byte, %src_addr, "registered UDP peer & echoing observed NAT address");
                    let reply = encode_observed_addr(src_addr);
                    let _ = socket.send_to(&reply, src_addr).await;
                }

                Some(DecodedUdpPacket::RelayQuic {
                    session_id,
                    role_byte,
                    payload,
                }) => {
                    let target_addr = {
                        let mut map = sessions.write().await;
                        let entry = map.entry(session_id).or_default();
                        entry.last_seen = Some(Instant::now());
                        if role_byte == ROLE_HOST {
                            entry.host_addr = Some(src_addr);
                            entry.client_addr
                        } else if role_byte == ROLE_CLIENT {
                            entry.client_addr = Some(src_addr);
                            entry.host_addr
                        } else {
                            None
                        }
                    };

                    if let Some(dst) = target_addr {
                        let forwarded = encode_relay_quic(session_id, role_byte, payload);
                        let _ = socket.send_to(&forwarded, dst).await;
                    }
                }

                _ => {}
            }
        }
    });

    Ok((local_addr, handle))
}
