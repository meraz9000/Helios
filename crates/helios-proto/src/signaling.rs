use std::net::SocketAddr;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::crypto::DeviceId;

/// Role a device is announcing on the signaling channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceRole {
    /// Remote machine being controlled (e.g., always-on Windows PC).
    Host,
    /// Viewing/controlling machine (e.g., macOS laptop).
    Client,
}

/// Summary of an online Host visible to authenticated Clients.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HostSummary {
    pub device_id: DeviceId,
    pub name: String,
    pub os: String,
    pub online: bool,
    pub active_sessions: u32,
}

/// Classification of a UDP endpoint advertised during QUIC NAT traversal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndpointKind {
    LocalLan,
    ObservedPublic,
}

/// Candidate UDP `ip:port` for direct P2P QUIC hole-punching.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UdpEndpoint {
    pub addr: SocketAddr,
    pub kind: EndpointKind,
}

/// JSON messages exchanged over TLS WebSocket (`/ws`) between devices and
/// the DigitalOcean `helios-relay` server.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SignalingMessage {
    /// Sent by relay immediately upon WebSocket connect.
    Challenge {
        nonce: String,
        protocol_version: u32,
    },

    /// Sent by Host or Client to prove possession of its Ed25519 private key.
    Authenticate {
        name: String,
        os: String,
        role: DeviceRole,
        public_key_b64: String,
        signature_b64: String,
        /// Optional one-time pairing token when enrolling a new device.
        #[serde(skip_serializing_if = "Option::is_none")]
        pairing_token: Option<String>,
    },

    /// Relay confirms authentication and returns the droplet's UDP rendezvous/relay endpoint.
    AuthOk {
        device_id: DeviceId,
        /// Droplet `host:port` (e.g., `relay.yourdomain.com:3478` or `127.0.0.1:3478`)
        /// used for NAT public `ip:port` discovery and encrypted QUIC relay fallback.
        udp_relay_addr: String,
    },

    /// Relay rejects authentication or request.
    Error {
        code: String,
        message: String,
    },

    /// Lightweight keepalive sent by idle background Windows Host (~30s interval).
    Heartbeat {
        timestamp_ms: u64,
    },

    /// Heartbeat acknowledgement.
    HeartbeatAck {
        timestamp_ms: u64,
    },

    /// Client asks relay for all authorized online Hosts.
    ListHosts,

    /// Relay replies with online Hosts.
    HostList {
        hosts: Vec<HostSummary>,
    },

    /// Client requests a new remote desktop session to a specific Host.
    RequestSession {
        target_host_id: DeviceId,
    },

    /// Relay notifies the always-on Host service to wake its capture agent
    /// and prepare a QUIC session.
    IncomingSession {
        session_id: Uuid,
        client_id: DeviceId,
        client_name: String,
        udp_relay_addr: String,
    },

    /// Relay informs the Client that the session has been dispatched to the Host.
    SessionCreated {
        session_id: Uuid,
        host_id: DeviceId,
        udp_relay_addr: String,
    },

    /// Exchanged between Host and Client via relay to pin the Host's ephemeral
    /// TLS 1.3 certificate SHA-256 fingerprint and share UDP hole-punch candidates.
    QuicPeerHello {
        session_id: Uuid,
        /// Hex-encoded SHA-256 of the peer's DER TLS 1.3 certificate (pinned E2E).
        cert_sha256_hex: String,
        /// Candidate UDP `ip:port` endpoints for direct P2P hole-punching.
        endpoints: Vec<UdpEndpoint>,
    },

    /// Host or Client signals whether it fell back to droplet UDP relay or connected P2P.
    EndSession {
        session_id: Uuid,
        reason: String,
    },
}
