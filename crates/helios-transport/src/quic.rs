//! End-to-End Encrypted **QUIC (`quinn`)** Transport with:
//! 1. **Pinned TLS 1.3 Certificates**: Host generates an ephemeral cert whose SHA-256
//!    fingerprint is exchanged over Ed25519-authenticated signaling; Client verifies
//!    the exact fingerprint so the DigitalOcean relay cannot inspect or MITM traffic.
//! 2. **Direct UDP Hole-Punching First**: Both peers discover their public NAT `ip:port`
//!    via `helios-relay`'s UDP socket, probe each other directly, and hand the punched
//!    UDP socket directly to `quinn::Endpoint` (zero droplet bandwidth!).
//! 3. **Automatic DigitalOcean UDP Relay Fallback**: If symmetric NAT or strict firewalls
//!    block direct UDP, encrypted QUIC datagrams automatically tunnel through the droplet's
//!    UDP relay socket (`HLP1`) with zero code changes to upper layers.
//! 4. **Prioritized Multi-Channel Delivery**:
//!    - Reliable bidirectional QUIC stream (`helios-ctrl`) for mouse/keyboard input & probes
//!    - Unreliable QUIC Datagrams (RFC 9221) for small dirty-tile updates + per-frame
//!      unidirectional QUIC streams for larger frames (zero head-of-line blocking!).

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use bytes::Bytes;
use helios_proto::udp::{
    DecodedUdpPacket, ROLE_CLIENT, ROLE_HOST, decode_udp_packet, encode_punch_probe,
    encode_register, encode_relay_quic,
};
use helios_proto::{
    ControlMessage, EndpointKind, MediaMessage, SignalingMessage, UdpEndpoint,
};
use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use quinn::{ClientConfig, Connection, Endpoint, EndpointConfig, ServerConfig, TransportConfig, VarInt};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use tokio::net::UdpSocket;
use tokio::sync::{Mutex, mpsc};
use tokio::time::timeout;
use tracing::{debug, info, warn};
use uuid::Uuid;

const HELIOS_ALPN: &[u8] = b"helios-quic-v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerRole {
    /// Windows Host: acts as QUIC Server (presents pinned ephemeral TLS 1.3 cert).
    Host,
    /// macOS Client: acts as QUIC Client (verifies Host's pinned TLS 1.3 cert).
    Client,
}

impl PeerRole {
    fn udp_role_byte(self) -> u8 {
        match self {
            Self::Host => ROLE_HOST,
            Self::Client => ROLE_CLIENT,
        }
    }
}

#[derive(Debug)]
pub enum PeerEvent {
    Ready { using_relay: bool },
    Control(ControlMessage),
    Media(MediaMessage),
    Disconnected,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct QuicTransportStats {
    pub rtt_ms: f32,
    pub lost_packets: u64,
    pub using_relay: bool,
}

struct EphemeralCert {
    cert_der: CertificateDer<'static>,
    key_der: PrivateKeyDer<'static>,
    sha256_hex: String,
}

fn generate_ephemeral_cert() -> Result<EphemeralCert> {
    let cert = rcgen::generate_simple_self_signed(vec!["helios.local".to_string()])?;
    let cert_bytes = cert.cert.der().to_vec();
    let sha256_hex = hex::encode(Sha256::digest(&cert_bytes));
    let key_bytes = cert.key_pair.serialize_der();
    Ok(EphemeralCert {
        cert_der: CertificateDer::from(cert_bytes),
        key_der: PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key_bytes)),
        sha256_hex,
    })
}

/// Custom rustls verifier that pins the exact SHA-256 fingerprint of the Host's
/// ephemeral TLS 1.3 certificate exchanged over Ed25519-authenticated signaling.
#[derive(Debug)]
struct PinnedFingerprintVerifier {
    expected_sha256_hex: String,
    provider: Arc<rustls::crypto::CryptoProvider>,
}

impl ServerCertVerifier for PinnedFingerprintVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        let actual = hex::encode(Sha256::digest(end_entity.as_ref()));
        if actual == self.expected_sha256_hex {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General(format!(
                "Helios TLS cert fingerprint mismatch: expected {}, got {}",
                self.expected_sha256_hex, actual
            )))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// Pre-connection UDP rendezvous state: discovers public NAT address via the
/// DigitalOcean droplet, performs simultaneous UDP hole-punching, and establishes
/// a pinned TLS 1.3 QUIC connection (P2P first, automatic droplet relay fallback).
pub struct QuicRendezvous {
    session_id: Uuid,
    role: PeerRole,
    socket: UdpSocket,
    relay_udp_addr: Option<SocketAddr>,
    cert: EphemeralCert,
    local_endpoints: Vec<UdpEndpoint>,
}

impl QuicRendezvous {
    /// Bind a local UDP socket, register with the DigitalOcean UDP rendezvous socket
    /// to discover our external NAT `ip:port`, and prepare our `QuicPeerHello` message.
    pub async fn prepare(
        session_id: Uuid,
        role: PeerRole,
        udp_relay_addr_str: &str,
    ) -> Result<(Self, SignalingMessage)> {
        let socket = UdpSocket::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0))
            .await
            .context("failed to bind local QUIC UDP socket")?;
        let local_port = socket.local_addr()?.port();

        let mut local_endpoints = Vec::new();
        // Discover local LAN IPv4 by probing route (no packets actually sent)
        if let Some(lan_ip) = detect_local_ipv4() {
            local_endpoints.push(UdpEndpoint {
                addr: SocketAddr::new(lan_ip, local_port),
                kind: EndpointKind::LocalLan,
            });
        }
        local_endpoints.push(UdpEndpoint {
            addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), local_port),
            kind: EndpointKind::LocalLan,
        });

        let relay_udp_addr = resolve_udp_addr(udp_relay_addr_str).await;
        if let Some(relay_addr) = relay_udp_addr {
            if let Some(observed) =
                discover_observed_addr(&socket, relay_addr, session_id, role.udp_role_byte()).await
            {
                debug!(%session_id, ?role, %observed, "discovered public NAT UDP endpoint");
                if !local_endpoints.iter().any(|e| e.addr == observed) {
                    local_endpoints.push(UdpEndpoint {
                        addr: observed,
                        kind: EndpointKind::ObservedPublic,
                    });
                }
            } else {
                warn!(%session_id, %relay_addr, "timed out discovering observed NAT address from droplet");
            }
        }

        let cert = generate_ephemeral_cert()?;
        let hello = SignalingMessage::QuicPeerHello {
            session_id,
            cert_sha256_hex: cert.sha256_hex.clone(),
            endpoints: local_endpoints.clone(),
        };

        Ok((
            Self {
                session_id,
                role,
                socket,
                relay_udp_addr,
                cert,
                local_endpoints,
            },
            hello,
        ))
    }

    pub fn local_endpoints(&self) -> &[UdpEndpoint] {
        &self.local_endpoints
    }

    /// Complete NAT hole-punching (or fall back to the DigitalOcean UDP relay) and
    /// open the E2E-encrypted QUIC connection with pinned TLS 1.3 verification.
    pub async fn establish(
        self,
        remote_cert_sha256_hex: String,
        remote_endpoints: Vec<UdpEndpoint>,
        force_relay: bool,
    ) -> Result<HeliosPeer> {
        let punched_peer = if !force_relay && !remote_endpoints.is_empty() {
            attempt_udp_hole_punch(&self.socket, self.session_id, &remote_endpoints).await
        } else {
            None
        };

        let (std_socket, connect_target, using_relay, proxy_task) =
            if let Some(direct_addr) = punched_peer {
                info!(
                    session_id = %self.session_id,
                    role = ?self.role,
                    %direct_addr,
                    "direct P2P UDP hole-punch succeeded — starting direct QUIC"
                );
                let std_sock = self.socket.into_std()?;
                std_sock.set_nonblocking(true)?;
                (std_sock, direct_addr, false, None)
            } else {
                let relay_addr = self.relay_udp_addr.ok_or_else(|| {
                    anyhow!("direct P2P UDP hole-punch failed and no droplet UDP relay is configured")
                })?;
                info!(
                    session_id = %self.session_id,
                    role = ?self.role,
                    %relay_addr,
                    "switching to DigitalOcean encrypted QUIC UDP relay fallback"
                );
                let (std_sock, proxy_addr, task) = spawn_relay_udp_proxy(
                    self.socket,
                    relay_addr,
                    self.session_id,
                    self.role.udp_role_byte(),
                )
                .await?;
                (std_sock, proxy_addr, true, Some(task))
            };

        let transport_cfg = build_low_latency_transport_config();

        let (endpoint, connection) = match self.role {
            PeerRole::Host => {
                let server_cfg = build_server_config(&self.cert, transport_cfg)?;
                let runtime = quinn::default_runtime()
                    .ok_or_else(|| anyhow!("no async runtime available for quinn"))?;
                let endpoint = Endpoint::new(
                    EndpointConfig::default(),
                    Some(server_cfg),
                    std_socket,
                    runtime,
                )?;

                let incoming = timeout(Duration::from_secs(15), endpoint.accept())
                    .await
                    .context("timed out waiting for Client QUIC handshake")?
                    .ok_or_else(|| anyhow!("QUIC endpoint closed before client connected"))?;
                let conn = incoming.await.context("Host QUIC TLS handshake failed")?;
                (endpoint, conn)
            }
            PeerRole::Client => {
                let client_cfg = build_client_config(remote_cert_sha256_hex, transport_cfg)?;
                let runtime = quinn::default_runtime()
                    .ok_or_else(|| anyhow!("no async runtime available for quinn"))?;
                let mut endpoint =
                    Endpoint::new(EndpointConfig::default(), None, std_socket, runtime)?;
                endpoint.set_default_client_config(client_cfg);

                // Brief retry window so Host's Quinn server endpoint is bound on its punched socket
                let conn = timeout(Duration::from_secs(15), async {
                    let connecting = endpoint.connect(connect_target, "helios.local")?;
                    connecting.await.context("Client QUIC TLS handshake failed")
                })
                .await
                .context("timed out connecting QUIC to Host")??;
                (endpoint, conn)
            }
        };

        info!(
            session_id = %self.session_id,
            role = ?self.role,
            using_relay,
            rtt_ms = connection.rtt().as_secs_f32() * 1000.0,
            "encrypted QUIC TLS 1.3 session established"
        );

        HeliosPeer::from_connection(
            self.session_id,
            self.role,
            endpoint,
            connection,
            using_relay,
            proxy_task,
        )
        .await
    }
}

/// Clone-able handle for sending prioritized QUIC control messages, media frames,
/// and reading native QUIC RTT/loss stats concurrently without locking `recv_event()`.
#[derive(Clone)]
pub struct HeliosPeerSender {
    pub session_id: Uuid,
    pub using_relay: bool,
    endpoint: Endpoint,
    connection: Connection,
    ctrl_send: Arc<Mutex<quinn::SendStream>>,
    tasks: Arc<Mutex<Vec<tokio::task::JoinHandle<()>>>>,
}

impl HeliosPeerSender {
    /// Send a high-priority control/input message on the dedicated reliable QUIC control stream.
    pub async fn send_control(&self, msg: &ControlMessage) -> Result<()> {
        let payload = msg.encode()?;
        let len = (payload.len() as u32).to_le_bytes();
        let mut send = self.ctrl_send.lock().await;
        send.write_all(&len).await?;
        send.write_all(&payload).await?;
        Ok(())
    }

    /// Send a video or dirty-tile frame using QUIC Datagrams when small enough (zero-retransmit
    /// lowest latency) or an independent per-frame unidirectional QUIC stream when > MTU.
    pub async fn send_media(&self, msg: &MediaMessage) -> Result<()> {
        let payload = msg.encode()?;
        if let Some(max_dgram) = self.connection.max_datagram_size()
            && payload.len() <= max_dgram
        {
            self.connection.send_datagram(Bytes::from(payload))?;
            return Ok(());
        }

        let mut uni = self.connection.open_uni().await?;
        uni.write_all(&payload).await?;
        uni.finish()?;
        Ok(())
    }

    /// Read native QUIC path RTT and packet loss counters directly from Quinn.
    pub fn quic_stats(&self) -> QuicTransportStats {
        let stats = self.connection.stats();
        QuicTransportStats {
            rtt_ms: self.connection.rtt().as_secs_f32() * 1000.0,
            lost_packets: stats.path.lost_packets,
            using_relay: self.using_relay,
        }
    }

    pub async fn close(&self) {
        self.connection.close(VarInt::from_u32(0), b"session_ended");
        self.endpoint.close(VarInt::from_u32(0), b"session_ended");
        let tasks = self.tasks.lock().await;
        for t in tasks.iter() {
            t.abort();
        }
    }
}

/// Active encrypted QUIC session with prioritized Control stream and zero-HOL Media delivery.
pub struct HeliosPeer {
    pub session_id: Uuid,
    pub using_relay: bool,
    sender: HeliosPeerSender,
    event_rx: mpsc::UnboundedReceiver<PeerEvent>,
}

impl HeliosPeer {
    async fn from_connection(
        session_id: Uuid,
        role: PeerRole,
        endpoint: Endpoint,
        connection: Connection,
        using_relay: bool,
        proxy_task: Option<tokio::task::JoinHandle<()>>,
    ) -> Result<Self> {
        let (mut ctrl_send, mut ctrl_recv) = match role {
            PeerRole::Client => {
                let (mut s, r) = connection.open_bi().await?;
                s.write_all(b"HCTL").await?;
                (s, r)
            }
            PeerRole::Host => {
                let (s, mut r) = timeout(Duration::from_secs(10), connection.accept_bi())
                    .await
                    .context("timed out waiting for Client control stream")??;
                let mut magic = [0u8; 4];
                r.read_exact(&mut magic).await?;
                if &magic != b"HCTL" {
                    bail!("unexpected QUIC control stream magic");
                }
                (s, r)
            }
        };

        let _ = ctrl_send.flush().await;
        let ctrl_send = Arc::new(Mutex::new(ctrl_send));
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let _ = event_tx.send(PeerEvent::Ready { using_relay });

        let mut tasks = Vec::new();
        if let Some(pt) = proxy_task {
            tasks.push(pt);
        }

        // 1. Control stream reader (length-prefixed reliable ControlMessages)
        let ev_ctrl = event_tx.clone();
        tasks.push(tokio::spawn(async move {
            loop {
                let mut len_buf = [0u8; 4];
                if ctrl_recv.read_exact(&mut len_buf).await.is_err() {
                    break;
                }
                let len = u32::from_le_bytes(len_buf) as usize;
                if len > 4 * 1024 * 1024 {
                    warn!("dropping oversized control frame ({len} bytes)");
                    break;
                }
                let mut payload = vec![0u8; len];
                if ctrl_recv.read_exact(&mut payload).await.is_err() {
                    break;
                }
                match ControlMessage::decode(&payload) {
                    Ok(msg) => {
                        if ev_ctrl.send(PeerEvent::Control(msg)).is_err() {
                            break;
                        }
                    }
                    Err(err) => warn!("failed to decode QUIC control message: {err}"),
                }
            }
            let _ = ev_ctrl.send(PeerEvent::Disconnected);
        }));

        // 2. Unreliable QUIC Datagram reader (lowest-latency small dirty-tile updates)
        let conn_dgram = connection.clone();
        let ev_dgram = event_tx.clone();
        tasks.push(tokio::spawn(async move {
            while let Ok(dgram) = conn_dgram.read_datagram().await {
                match MediaMessage::decode(&dgram) {
                    Ok(msg) => {
                        if ev_dgram.send(PeerEvent::Media(msg)).is_err() {
                            break;
                        }
                    }
                    Err(err) => warn!("failed to decode QUIC media datagram: {err}"),
                }
            }
        }));

        // 3. Per-frame Unidirectional QUIC Stream reader (for frames > MTU without HOL blocking)
        let conn_uni = connection.clone();
        let ev_uni = event_tx.clone();
        tasks.push(tokio::spawn(async move {
            while let Ok(mut recv) = conn_uni.accept_uni().await {
                let ev_tx = ev_uni.clone();
                tokio::spawn(async move {
                    if let Ok(bytes) = recv.read_to_end(16 * 1024 * 1024).await {
                        match MediaMessage::decode(&bytes) {
                            Ok(msg) => {
                                let _ = ev_tx.send(PeerEvent::Media(msg));
                            }
                            Err(err) => warn!("failed to decode QUIC uni media frame: {err}"),
                        }
                    }
                });
            }
        }));

        let sender = HeliosPeerSender {
            session_id,
            using_relay,
            endpoint,
            connection,
            ctrl_send,
            tasks: Arc::new(Mutex::new(tasks)),
        };

        Ok(Self {
            session_id,
            using_relay,
            sender,
            event_rx,
        })
    }

    /// Split into a lock-free clone-able sender handle and the inbound event receiver.
    pub fn split(self) -> (HeliosPeerSender, mpsc::UnboundedReceiver<PeerEvent>) {
        (self.sender, self.event_rx)
    }

    pub async fn send_control(&self, msg: &ControlMessage) -> Result<()> {
        self.sender.send_control(msg).await
    }

    pub async fn send_media(&self, msg: &MediaMessage) -> Result<()> {
        self.sender.send_media(msg).await
    }

    pub async fn recv_event(&mut self) -> Option<PeerEvent> {
        self.event_rx.recv().await
    }

    pub fn quic_stats(&self) -> QuicTransportStats {
        self.sender.quic_stats()
    }

    pub async fn close(&self) {
        self.sender.close().await;
    }
}

fn build_low_latency_transport_config() -> Arc<TransportConfig> {
    let mut transport = TransportConfig::default();
    transport.keep_alive_interval(Some(Duration::from_secs(5)));
    transport.max_idle_timeout(Some(Duration::from_secs(20).try_into().unwrap()));
    transport.datagram_receive_buffer_size(Some(2 * 1024 * 1024));
    transport.datagram_send_buffer_size(1024 * 1024);
    Arc::new(transport)
}

fn build_server_config(
    cert: &EphemeralCert,
    transport: Arc<TransportConfig>,
) -> Result<ServerConfig> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut tls = rustls::ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_no_client_auth()
        .with_single_cert(vec![cert.cert_der.clone()], cert.key_der.clone_key())?;
    tls.alpn_protocols = vec![HELIOS_ALPN.to_vec()];

    let quic_tls = QuicServerConfig::try_from(tls)?;
    let mut cfg = ServerConfig::with_crypto(Arc::new(quic_tls));
    cfg.transport_config(transport);
    Ok(cfg)
}

fn build_client_config(
    expected_sha256_hex: String,
    transport: Arc<TransportConfig>,
) -> Result<ClientConfig> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let verifier = Arc::new(PinnedFingerprintVerifier {
        expected_sha256_hex,
        provider: provider.clone(),
    });
    let mut tls = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    tls.alpn_protocols = vec![HELIOS_ALPN.to_vec()];

    let quic_tls = QuicClientConfig::try_from(tls)?;
    let mut cfg = ClientConfig::new(Arc::new(quic_tls));
    cfg.transport_config(transport);
    Ok(cfg)
}

fn detect_local_ipv4() -> Option<IpAddr> {
    let sock = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    sock.connect("8.8.8.8:80").ok()?;
    let ip = sock.local_addr().ok()?.ip();
    if ip.is_ipv4() && !ip.is_loopback() {
        Some(ip)
    } else {
        None
    }
}

async fn resolve_udp_addr(addr_str: &str) -> Option<SocketAddr> {
    if let Ok(sock) = addr_str.parse::<SocketAddr>() {
        return Some(sock);
    }
    tokio::net::lookup_host(addr_str)
        .await
        .ok()?
        .find(|a| a.is_ipv4())
}

async fn discover_observed_addr(
    socket: &UdpSocket,
    relay_addr: SocketAddr,
    session_id: Uuid,
    role_byte: u8,
) -> Option<SocketAddr> {
    let reg = encode_register(session_id, role_byte);
    let mut buf = [0u8; 512];
    for _ in 0..3 {
        let _ = socket.send_to(&reg, relay_addr).await;
        if let Ok(Ok((n, _))) = timeout(Duration::from_millis(300), socket.recv_from(&mut buf)).await
            && let Some(DecodedUdpPacket::ObservedAddr(addr)) = decode_udp_packet(&buf[..n])
        {
            return Some(addr);
        }
    }
    None
}

/// Simultaneous UDP hole-punching across remote LAN & public endpoints.
/// Returns the first remote `SocketAddr` that completes a bidirectional probe/ack.
async fn attempt_udp_hole_punch(
    socket: &UdpSocket,
    session_id: Uuid,
    remote_endpoints: &[UdpEndpoint],
) -> Option<SocketAddr> {
    let probe = encode_punch_probe(session_id, false);
    let ack = encode_punch_probe(session_id, true);
    let deadline = tokio::time::Instant::now() + Duration::from_millis(1200);
    let mut ticker = tokio::time::interval(Duration::from_millis(75));
    let mut buf = [0u8; 2048];
    let mut confirmed_addr: Option<SocketAddr> = None;

    while tokio::time::Instant::now() < deadline {
        tokio::select! {
            _ = ticker.tick() => {
                for ep in remote_endpoints {
                    let _ = socket.send_to(&probe, ep.addr).await;
                }
                if let Some(addr) = confirmed_addr {
                    // Send one final ack before returning so the remote peer also unblocks immediately
                    let _ = socket.send_to(&ack, addr).await;
                    return Some(addr);
                }
            }
            res = socket.recv_from(&mut buf) => {
                if let Ok((n, src)) = res {
                    match decode_udp_packet(&buf[..n]) {
                        Some(DecodedUdpPacket::PunchProbe { session_id: sid }) if sid == session_id => {
                            let _ = socket.send_to(&ack, src).await;
                            let _ = socket.send_to(&ack, src).await;
                            confirmed_addr = Some(src);
                        }
                        Some(DecodedUdpPacket::PunchAck { session_id: sid }) if sid == session_id => {
                            let _ = socket.send_to(&ack, src).await;
                            return Some(src);
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    confirmed_addr
}

/// Local loopback UDP proxy that transparently encapsulates Quinn's encrypted QUIC
/// datagrams into `HLP1` relay packets routed through the DigitalOcean droplet.
async fn spawn_relay_udp_proxy(
    outer_socket: UdpSocket,
    relay_addr: SocketAddr,
    session_id: Uuid,
    role_byte: u8,
) -> Result<(
    std::net::UdpSocket,
    SocketAddr,
    tokio::task::JoinHandle<()>,
)> {
    // Re-register with the relay to guarantee our NAT mapping is fresh
    let reg = encode_register(session_id, role_byte);
    let _ = outer_socket.send_to(&reg, relay_addr).await;

    let quinn_sock = UdpSocket::bind("127.0.0.1:0").await?;
    let quinn_addr = quinn_sock.local_addr()?;

    let proxy_sock = UdpSocket::bind("127.0.0.1:0").await?;
    let proxy_addr = proxy_sock.local_addr()?;

    let outer = Arc::new(outer_socket);
    let proxy = Arc::new(proxy_sock);

    let outer_tx = outer.clone();
    let proxy_rx = proxy.clone();
    let t1 = tokio::spawn(async move {
        let mut buf = vec![0u8; 65536];
        loop {
            match proxy_rx.recv_from(&mut buf).await {
                Ok((n, _)) => {
                    let wrapped = encode_relay_quic(session_id, role_byte, &buf[..n]);
                    if outer_tx.send_to(&wrapped, relay_addr).await.is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    let outer_rx = outer;
    let proxy_tx = proxy;
    let handle = tokio::spawn(async move {
        let mut buf = vec![0u8; 65536];
        loop {
            match outer_rx.recv_from(&mut buf).await {
                Ok((n, _)) => {
                    if let Some(DecodedUdpPacket::RelayQuic {
                        session_id: sid,
                        payload,
                        ..
                    }) = decode_udp_packet(&buf[..n])
                        && sid == session_id
                        && proxy_tx.send_to(payload, quinn_addr).await.is_err()
                    {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        t1.abort();
    });

    let std_quinn_sock = quinn_sock.into_std()?;
    std_quinn_sock.set_nonblocking(true)?;
    Ok((std_quinn_sock, proxy_addr, handle))
}
