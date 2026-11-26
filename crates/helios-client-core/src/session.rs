use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Result, bail};
use helios_proto::{
    ClipboardSync, ControlMessage, DeviceId, DeviceIdentity, DeviceRole, HostSummary, InputEvent,
    MediaMessage, QualityMode, SessionTelemetry, SignalingMessage, VideoFramePacket,
    apply_tile_frame,
};
use helios_transport::{PeerEvent, PeerRole, QuicRendezvous, SignalingClient};
use tokio::sync::mpsc;
use tracing::{info, warn};

/// Callback invoked whenever the client's RGBA framebuffer is updated by a dirty-tile frame.
pub type DecodedFrameCallback = Box<dyn Fn(u32, u32, &[u8], &SessionTelemetry) + Send + Sync>;

/// Callback invoked whenever a hardware-encoded H.264/H.265/AV1 access unit arrives.
pub type EncodedVideoCallback = Box<dyn Fn(&VideoFramePacket, &SessionTelemetry) + Send + Sync>;

/// Connection lifecycle state reported to the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ConnectionState {
    Disconnected = 0,
    Connecting = 1,
    ConnectedP2P = 2,
    ConnectedRelay = 3,
    Error = 4,
}

pub type StateChangeCallback = Box<dyn Fn(ConnectionState, &str) + Send + Sync>;

/// Callback invoked when Windows sends a new cursor bitmap (decompressed RGBA bytes).
pub type CursorShapeCallback = Box<dyn Fn(u16, u16, u16, u16, &[u8]) + Send + Sync>;

/// Callback invoked when the Host replies to an on-demand clipboard pull.
pub type ClipboardCallback = Box<dyn Fn(&str) + Send + Sync>;

/// Callback bundle passed into `ClientSession::connect_with_callbacks`.
pub struct ClientCallbacks {
    pub on_rgba_frame: DecodedFrameCallback,
    pub on_video_frame: Option<EncodedVideoCallback>,
    pub on_state_change: Option<StateChangeCallback>,
    pub on_cursor_shape: Option<CursorShapeCallback>,
    pub on_clipboard: Option<ClipboardCallback>,
}

#[derive(Debug, Clone)]
pub struct ClientSessionConfig {
    pub relay_url: String,
    pub identity: DeviceIdentity,
    pub pairing_token: Option<String>,
    /// Unique Host Access Passcode shown by the Windows Host on install.
    pub access_token: Option<String>,
    pub initial_mode: QualityMode,
    /// Optional flag for testing droplet UDP relay fallback.
    pub force_relay: bool,
}

/// High-level client session handle for controlling a remote Windows Host over QUIC.
pub struct ClientSession {
    control_tx: mpsc::UnboundedSender<ControlMessage>,
    stop_tx: tokio::sync::watch::Sender<bool>,
}

impl ClientSession {
    /// Query the DigitalOcean relay for all currently online Windows Hosts.
    pub async fn list_online_hosts(cfg: &ClientSessionConfig) -> Result<Vec<HostSummary>> {
        let mut sig = SignalingClient::connect(
            &cfg.relay_url,
            &cfg.identity,
            DeviceRole::Client,
            cfg.pairing_token.clone(),
            Duration::from_secs(30),
        )
        .await?;

        sig.send(SignalingMessage::ListHosts)?;
        match tokio::time::timeout(Duration::from_secs(5), sig.recv()).await? {
            Some(SignalingMessage::HostList { hosts }) => Ok(hosts),
            Some(SignalingMessage::Error { code, message }) => {
                bail!("relay error ({code}): {message}");
            }
            other => bail!("unexpected reply to ListHosts: {other:?}"),
        }
    }

    /// Convenience wrapper for RGBA-only consumers (`helios-cli`).
    pub async fn connect_and_stream(
        cfg: ClientSessionConfig,
        target_host_id: DeviceId,
        on_frame: DecodedFrameCallback,
    ) -> Result<(Self, tokio::task::JoinHandle<Result<()>>)> {
        Self::connect_with_callbacks(
            cfg,
            target_host_id,
            ClientCallbacks {
                on_rgba_frame: on_frame,
                on_video_frame: None,
                on_state_change: None,
                on_cursor_shape: None,
                on_clipboard: None,
            },
        )
        .await
    }

    /// Connect to a specific Windows Host via the DigitalOcean relay, establish
    /// an encrypted QUIC P2P/Relay session, and invoke callbacks for decoded RGBA
    /// dirty-tile frames, encoded H.264/H.265 video packets, and connection state.
    pub async fn connect_with_callbacks(
        cfg: ClientSessionConfig,
        target_host_id: DeviceId,
        callbacks: ClientCallbacks,
    ) -> Result<(Self, tokio::task::JoinHandle<Result<()>>)> {
        let (control_tx, mut control_rx) = mpsc::unbounded_channel::<ControlMessage>();
        let (stop_tx, mut stop_rx) = tokio::sync::watch::channel(false);

        let notify_state = |cb: &Option<StateChangeCallback>, state: ConnectionState, msg: &str| {
            if let Some(f) = cb {
                f(state, msg);
            }
        };

        let task = tokio::spawn(async move {
            notify_state(
                &callbacks.on_state_change,
                ConnectionState::Connecting,
                "Authenticating with DigitalOcean relay...",
            );

            let run_result: Result<()> = async {
                let mut sig = SignalingClient::connect(
                    &cfg.relay_url,
                    &cfg.identity,
                    DeviceRole::Client,
                    cfg.pairing_token.clone(),
                    Duration::from_secs(30),
                )
                .await?;

                sig.send(SignalingMessage::RequestSession {
                    target_host_id: target_host_id.clone(),
                    access_token: cfg.access_token.clone(),
                })?;

                let (session_id, udp_relay_addr) = match tokio::time::timeout(
                    Duration::from_secs(10),
                    sig.recv(),
                )
                .await?
                {
                    Some(SignalingMessage::SessionCreated {
                        session_id,
                        udp_relay_addr,
                        ..
                    }) => (session_id, udp_relay_addr),
                    Some(SignalingMessage::Error { code, message }) => {
                        bail!("failed to start session ({code}): {message}");
                    }
                    other => bail!("unexpected signaling reply: {other:?}"),
                };

                notify_state(
                    &callbacks.on_state_change,
                    ConnectionState::Connecting,
                    "Negotiating direct QUIC P2P UDP tunnel...",
                );
                info!(%session_id, host = %target_host_id, "session created — initiating QUIC NAT rendezvous");

                let (rdv, hello_msg) =
                    QuicRendezvous::prepare(session_id, PeerRole::Client, &udp_relay_addr).await?;
                sig.send(hello_msg)?;

                let (remote_cert, remote_endpoints) = match tokio::time::timeout(
                    Duration::from_secs(10),
                    sig.recv(),
                )
                .await?
                {
                    Some(SignalingMessage::QuicPeerHello {
                        cert_sha256_hex,
                        endpoints,
                        ..
                    }) => (cert_sha256_hex, endpoints),
                    Some(SignalingMessage::EndSession { reason, .. }) => {
                        bail!("host ended session before QUIC handshake: {reason}");
                    }
                    other => bail!("expected QuicPeerHello from host, got {other:?}"),
                };

                let peer = rdv
                    .establish(remote_cert, remote_endpoints, cfg.force_relay)
                    .await?;
                let (peer_tx, mut peer_rx) = peer.split();
                let using_relay = peer_tx.using_relay;
                notify_state(
                    &callbacks.on_state_change,
                    if using_relay {
                        ConnectionState::ConnectedRelay
                    } else {
                        ConnectionState::ConnectedP2P
                    },
                    if using_relay {
                        "Connected via DigitalOcean Encrypted QUIC Relay"
                    } else {
                        "Connected Direct P2P (QUIC TLS 1.3)"
                    },
                );

                // Forward outbound control messages (input, quality switches, pings, clipboard)
                let peer_ctrl = peer_tx.clone();
                let initial_mode = cfg.initial_mode;
                let mut stop_rx_ctrl = stop_rx.clone();
                let ctrl_task = tokio::spawn(async move {
                    let _ = peer_ctrl
                        .send_control(&ControlMessage::SetQualityMode(initial_mode))
                        .await;

                    let mut ping_interval = tokio::time::interval(Duration::from_secs(1));
                    let mut seq = 1u64;
                    loop {
                        tokio::select! {
                            _ = stop_rx_ctrl.changed() => break,
                            Some(msg) = control_rx.recv() => {
                                if peer_ctrl.send_control(&msg).await.is_err() {
                                    break;
                                }
                            }
                            _ = ping_interval.tick() => {
                                let ts_us = SystemTime::now()
                                    .duration_since(UNIX_EPOCH)
                                    .unwrap_or_default()
                                    .as_micros() as u64;
                                let _ = peer_ctrl.send_control(&ControlMessage::Ping {
                                    seq,
                                    client_ts_us: ts_us,
                                }).await;
                                seq += 1;
                            }
                        }
                    }
                });

                // Decode incoming frames & maintain telemetry
                let mut fb_width = 0u32;
                let mut fb_height = 0u32;
                let mut fb_rgba: Vec<u8> = Vec::new();
                let mut telemetry = SessionTelemetry {
                    using_relay,
                    ..Default::default()
                };
                let mut window_start = Instant::now();
                let mut window_frames = 0u32;
                let mut window_bytes = 0usize;

                loop {
                    tokio::select! {
                        _ = stop_rx.changed() => break,
                        ev = peer_rx.recv() => {
                            let Some(ev) = ev else { break };
                            match ev {
                                PeerEvent::Ready { using_relay } => {
                                    telemetry.using_relay = using_relay;
                                }
                                PeerEvent::Control(ControlMessage::Pong { client_ts_us, .. }) => {
                                    let now_us = SystemTime::now()
                                        .duration_since(UNIX_EPOCH)
                                        .unwrap_or_default()
                                        .as_micros() as u64;
                                    telemetry.rtt_ms =
                                        now_us.saturating_sub(client_ts_us) as f32 / 1000.0;
                                }
                                PeerEvent::Control(ControlMessage::Cursor(
                                    helios_proto::CursorUpdate::Shape(shape),
                                )) => {
                                    if let Some(on_cursor) = &callbacks.on_cursor_shape {
                                        let max_bytes =
                                            (shape.width as usize) * (shape.height as usize) * 4;
                                        if let Ok(rgba) =
                                            zstd::bulk::decompress(&shape.rgba_zstd, max_bytes)
                                            && rgba.len() == max_bytes
                                        {
                                            on_cursor(
                                                shape.width,
                                                shape.height,
                                                shape.hotspot_x,
                                                shape.hotspot_y,
                                                &rgba,
                                            );
                                        }
                                    }
                                }
                                PeerEvent::Control(ControlMessage::Clipboard(clip)) => {
                                    if let Some(on_clip) = &callbacks.on_clipboard {
                                        on_clip(&clip.text);
                                    }
                                }
                                PeerEvent::Media(MediaMessage::DirtyTiles(df)) => {
                                    let payload_bytes: usize =
                                        df.tiles.iter().map(|t| t.rgba_zstd.len()).sum();
                                    update_rate_stats(
                                        &mut telemetry,
                                        &mut window_start,
                                        &mut window_frames,
                                        &mut window_bytes,
                                        payload_bytes,
                                    );
                                    if let Err(err) = apply_tile_frame(
                                        &df,
                                        &mut fb_width,
                                        &mut fb_height,
                                        &mut fb_rgba,
                                    ) {
                                        warn!("tile decode error: {err}");
                                        continue;
                                    }
                                    (callbacks.on_rgba_frame)(
                                        fb_width,
                                        fb_height,
                                        &fb_rgba,
                                        &telemetry,
                                    );
                                }
                                PeerEvent::Media(MediaMessage::VideoFrame(vf)) => {
                                    update_rate_stats(
                                        &mut telemetry,
                                        &mut window_start,
                                        &mut window_frames,
                                        &mut window_bytes,
                                        vf.payload.len(),
                                    );
                                    if let Some(on_video) = &callbacks.on_video_frame {
                                        on_video(&vf, &telemetry);
                                    }
                                }
                                PeerEvent::Disconnected => break,
                                _ => {}
                            }
                        }
                    }
                }

                let _ = sig.send(SignalingMessage::EndSession {
                    session_id,
                    reason: "client_closed".into(),
                });
                peer_tx.close().await;
                ctrl_task.abort();
                Ok(())
            }
            .await;

            match &run_result {
                Ok(()) => notify_state(
                    &callbacks.on_state_change,
                    ConnectionState::Disconnected,
                    "Disconnected",
                ),
                Err(err) => notify_state(
                    &callbacks.on_state_change,
                    ConnectionState::Error,
                    &err.to_string(),
                ),
            }

            run_result
        });

        Ok((Self { control_tx, stop_tx }, task))
    }

    pub fn send_input(&self, ev: InputEvent) -> Result<()> {
        self.control_tx
            .send(ControlMessage::Input(ev))
            .map_err(|_| anyhow::anyhow!("session closed"))
    }

    pub fn set_quality_mode(&self, mode: QualityMode) -> Result<()> {
        self.control_tx
            .send(ControlMessage::SetQualityMode(mode))
            .map_err(|_| anyhow::anyhow!("session closed"))
    }

    pub fn request_keyframe(&self) -> Result<()> {
        self.control_tx
            .send(ControlMessage::RequestKeyframe)
            .map_err(|_| anyhow::anyhow!("session closed"))
    }

    pub fn send_clipboard(&self, text: String) -> Result<()> {
        // Cap clipboard sync at 64 KB so slow links aren't saturated
        let trimmed = if text.len() > 65_536 {
            text.chars().take(65_536).collect()
        } else {
            text
        };
        self.control_tx
            .send(ControlMessage::Clipboard(ClipboardSync { text: trimmed }))
            .map_err(|_| anyhow::anyhow!("session closed"))
    }

    pub fn request_clipboard(&self) -> Result<()> {
        self.control_tx
            .send(ControlMessage::RequestClipboard)
            .map_err(|_| anyhow::anyhow!("session closed"))
    }

    pub fn switch_monitor(&self, monitor_index: u32) -> Result<()> {
        self.control_tx
            .send(ControlMessage::SwitchMonitor { monitor_index })
            .map_err(|_| anyhow::anyhow!("session closed"))
    }

    pub fn disconnect(&self) {
        let _ = self.stop_tx.send(true);
    }
}

fn update_rate_stats(
    telemetry: &mut SessionTelemetry,
    window_start: &mut Instant,
    window_frames: &mut u32,
    window_bytes: &mut usize,
    frame_bytes: usize,
) {
    *window_bytes += frame_bytes;
    *window_frames += 1;
    let elapsed = window_start.elapsed().as_secs_f32();
    if elapsed >= 0.5 {
        telemetry.fps = *window_frames as f32 / elapsed;
        telemetry.video_kbps = ((*window_bytes as f32 * 8.0) / (elapsed * 1000.0)) as u32;
        *window_start = Instant::now();
        *window_frames = 0;
        *window_bytes = 0;
    }
}
