use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Result, bail};
use helios_proto::{
    ControlMessage, DeviceIdentity, DeviceRole, MediaMessage, QualityMode, SignalingMessage,
};
use helios_transport::{
    AdaptiveController, LatestFrameSlot, NetworkSample, PeerEvent, PeerRole, QuicRendezvous,
    SignalingClient,
};
use tokio::sync::{Mutex, mpsc};
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::capture::create_default_capturer;
use crate::clipboard::{get_host_clipboard_text, set_host_clipboard_text};
use crate::encoder::HybridEncoder;
use crate::input::create_default_injector;

#[derive(Debug, Clone)]
pub struct HostDaemonConfig {
    pub relay_url: String,
    pub identity: DeviceIdentity,
    pub pairing_token: Option<String>,
    pub monitor_index: u32,
    pub force_synthetic_capture: bool,
}

/// Persistent always-on Host daemon loop.
///
/// Maintains a tiny outbound WebSocket heartbeat to the DigitalOcean relay and
/// consumes ~0% CPU/GPU while idle. Automatically reconnects with exponential
/// backoff if the network or droplet restarts. Wakes a capture + QUIC worker
/// only when a verified client sends `IncomingSession`.
pub async fn run_host_daemon(cfg: HostDaemonConfig) -> Result<()> {
    let mut backoff_secs = 2u64;

    loop {
        info!(
            device_id = %cfg.identity.device_id,
            relay = %cfg.relay_url,
            "connecting background host to DigitalOcean relay"
        );

        match SignalingClient::connect(
            &cfg.relay_url,
            &cfg.identity,
            DeviceRole::Host,
            cfg.pairing_token.clone(),
            Duration::from_secs(30),
        )
        .await
        {
            Ok(mut sig) => {
                backoff_secs = 2;
                info!(
                    device_id = %sig.device_id,
                    udp_relay = %sig.udp_relay_addr,
                    "host online and idle — waiting for incoming connection signal"
                );

                let active_sessions: Arc<
                    Mutex<HashMap<Uuid, mpsc::UnboundedSender<SignalingMessage>>>,
                > = Arc::new(Mutex::new(HashMap::new()));

                while let Some(msg) = sig.recv().await {
                    match msg {
                        SignalingMessage::IncomingSession {
                            session_id,
                            client_id,
                            client_name,
                            udp_relay_addr,
                            access_token,
                        } => {
                            if !passcode_matches(
                                &cfg.identity.access_passcode,
                                access_token.as_deref(),
                            ) {
                                warn!(
                                    %session_id,
                                    %client_id,
                                    %client_name,
                                    "rejected incoming session: invalid Host Access Passcode"
                                );
                                let _ = sig.send(SignalingMessage::EndSession {
                                    session_id,
                                    reason: "Invalid Host Access Passcode".into(),
                                });
                                continue;
                            }

                            info!(
                                %session_id,
                                %client_id,
                                %client_name,
                                "access passcode verified — waking capture & QUIC session worker"
                            );
                            let (sess_tx, sess_rx) = mpsc::unbounded_channel();
                            active_sessions.lock().await.insert(session_id, sess_tx);

                            let sig_tx = sig.clone_sender();
                            let sessions_ref = active_sessions.clone();
                            let monitor_index = cfg.monitor_index;
                            let force_synthetic = cfg.force_synthetic_capture;

                            tokio::spawn(async move {
                                if let Err(err) = run_active_session(
                                    session_id,
                                    udp_relay_addr,
                                    sig_tx,
                                    sess_rx,
                                    monitor_index,
                                    force_synthetic,
                                )
                                .await
                                {
                                    warn!(%session_id, "active session ended with error: {err:#}");
                                }
                                sessions_ref.lock().await.remove(&session_id);
                                info!(%session_id, "session worker stopped — returning to zero-CPU idle");
                            });
                        }

                        SignalingMessage::QuicPeerHello { session_id, .. }
                        | SignalingMessage::EndSession { session_id, .. } => {
                            if let Some(tx) = active_sessions.lock().await.get(&session_id) {
                                let _ = tx.send(msg);
                            }
                        }

                        SignalingMessage::Error { code, message } => {
                            warn!(%code, %message, "relay reported error");
                        }

                        _ => {}
                    }
                }

                warn!("signaling connection closed; will reconnect");
            }
            Err(err) => {
                error!("failed to connect to relay: {err:#}");
            }
        }

        info!("reconnecting to relay in {backoff_secs}s...");
        tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
        backoff_secs = (backoff_secs * 2).min(60);
    }
}

async fn run_active_session(
    session_id: Uuid,
    udp_relay_addr: String,
    sig_tx: mpsc::UnboundedSender<SignalingMessage>,
    mut sess_rx: mpsc::UnboundedReceiver<SignalingMessage>,
    monitor_index: u32,
    force_synthetic: bool,
) -> Result<()> {
    #[cfg(windows)]
    prevent_windows_sleep(true);

    let (rdv, hello_msg) =
        QuicRendezvous::prepare(session_id, PeerRole::Host, &udp_relay_addr).await?;
    let _ = sig_tx.send(hello_msg);

    let (remote_cert, remote_endpoints) = match tokio::time::timeout(
        Duration::from_secs(10),
        sess_rx.recv(),
    )
    .await?
    {
        Some(SignalingMessage::QuicPeerHello {
            cert_sha256_hex,
            endpoints,
            ..
        }) => (cert_sha256_hex, endpoints),
        Some(SignalingMessage::EndSession { reason, .. }) => {
            bail!("client canceled session before QUIC handshake: {reason}");
        }
        other => bail!("unexpected signaling message before QUIC handshake: {other:?}"),
    };

    let peer = rdv.establish(remote_cert, remote_endpoints, false).await?;
    let (peer_tx, mut peer_rx) = peer.split();

    let adaptive = Arc::new(Mutex::new(AdaptiveController::new(QualityMode::Auto)));
    let frame_slot = Arc::new(LatestFrameSlot::<MediaMessage>::new());
    let keyframe_flag = Arc::new(AtomicBool::new(true));
    let requested_monitor = Arc::new(std::sync::atomic::AtomicU32::new(monitor_index));
    let mut injector = create_default_injector();

    let (stop_tx, mut stop_rx) = tokio::sync::watch::channel(false);

    let stop_tx_sig = stop_tx.clone();
    let sig_watch = tokio::spawn(async move {
        while let Some(msg) = sess_rx.recv().await {
            if let SignalingMessage::EndSession { reason, .. } = msg {
                info!(%session_id, %reason, "remote ended session via signaling");
                let _ = stop_tx_sig.send(true);
                break;
            }
        }
    });

    // Cursor updates channel (from blocking capture thread -> async QUIC control stream)
    let (cursor_tx, mut cursor_rx) = mpsc::unbounded_channel::<helios_proto::CursorShape>();
    let peer_cursor = peer_tx.clone();
    let mut stop_rx_cursor = stop_rx.clone();
    let cursor_task = tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = stop_rx_cursor.changed() => break,
                Some(shape) = cursor_rx.recv() => {
                    let _ = peer_cursor
                        .send_control(&ControlMessage::Cursor(helios_proto::CursorUpdate::Shape(shape)))
                        .await;
                }
            }
        }
    });

    // Capture & Hybrid Encoding thread (Hardware H.264 MFT + Survival Zstd Dirty-Tiles)
    let slot_cap = frame_slot.clone();
    let adaptive_cap = adaptive.clone();
    let kf_cap = keyframe_flag.clone();
    let mon_cap = requested_monitor.clone();
    let stop_rx_cap = stop_rx.clone();
    let capture_task = tokio::task::spawn_blocking(move || {
        let mut active_monitor = mon_cap.load(Ordering::Acquire);
        let mut capturer = match create_default_capturer(active_monitor, force_synthetic) {
            Ok(c) => c,
            Err(err) => {
                error!("failed to initialize desktop capturer: {err}");
                return;
            }
        };
        let mut encoder = HybridEncoder::new(1920, 1080);

        while !*stop_rx_cap.borrow() {
            let desired_mon = mon_cap.load(Ordering::Acquire);
            if desired_mon != active_monitor {
                if let Err(err) = capturer.switch_monitor(desired_mon) {
                    warn!(desired_mon, "failed to switch monitor: {err}");
                    mon_cap.store(active_monitor, Ordering::Release);
                } else {
                    info!(desired_mon, "switched active capture monitor");
                    active_monitor = desired_mon;
                    encoder.request_keyframe();
                }
            }

            let tier = adaptive_cap.blocking_lock().active_tier();
            let target_fps = tier.target_fps().max(2);
            let frame_interval = Duration::from_millis(1000 / target_fps as u64);
            let started = std::time::Instant::now();

            if kf_cap.swap(false, Ordering::AcqRel) {
                encoder.request_keyframe();
            }

            if let Ok(Some(frame)) = capturer.capture_rgba() {
                if let Some(shape) = frame.cursor_shape {
                    let _ = cursor_tx.send(shape);
                }
                let ts_us = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_micros() as u64;
                if let Ok(Some(media_msg)) =
                    encoder.encode_frame(frame.width, frame.height, &frame.rgba, ts_us, tier)
                {
                    slot_cap.push(media_msg);
                }
            }

            if let Some(remaining) = frame_interval.checked_sub(started.elapsed()) {
                std::thread::sleep(remaining);
            }
        }
    });

    // Media sender task draining LatestFrameSlot onto QUIC datagrams / per-frame uni streams
    let peer_media = peer_tx.clone();
    let slot_send = frame_slot.clone();
    let mut stop_rx_send = stop_rx.clone();
    let sender_task = tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = stop_rx_send.changed() => break,
                msg = slot_send.take() => {
                    if let Err(err) = peer_media.send_media(&msg).await {
                        warn!(%session_id, "failed to send QUIC media frame: {err}");
                        break;
                    }
                }
            }
        }
    });

    // Periodic native QUIC RTT / loss probe feeding AdaptiveController
    let peer_stats = peer_tx.clone();
    let adaptive_stats = adaptive.clone();
    let mut stop_rx_stats = stop_rx.clone();
    let stats_task = tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(1));
        let mut prev_lost = 0u64;
        loop {
            tokio::select! {
                _ = stop_rx_stats.changed() => break,
                _ = ticker.tick() => {
                    let qs = peer_stats.quic_stats();
                    let delta_lost = qs.lost_packets.saturating_sub(prev_lost);
                    prev_lost = qs.lost_packets;
                    let loss_pct = (delta_lost as f32).min(100.0);
                    let mut ctrl = adaptive_stats.lock().await;
                    let target_kbps = ctrl.active_tier().target_bitrate_kbps();
                    ctrl.on_sample(NetworkSample {
                        rtt_ms: qs.rtt_ms,
                        packet_loss_pct: loss_pct,
                        measured_kbps: target_kbps,
                    });
                }
            }
        }
    });

    // Main PeerEvent loop (handles prioritized input, cursor/monitor, clipboard, ping/pong)
    loop {
        tokio::select! {
            _ = stop_rx.changed() => break,
            ev = peer_rx.recv() => {
                match ev {
                    Some(PeerEvent::Ready { using_relay }) => {
                        info!(%session_id, using_relay, "QUIC session ready — streaming active");
                    }
                    Some(PeerEvent::Control(ctrl)) => match ctrl {
                        ControlMessage::Input(input_ev) => {
                            let _ = injector.inject(&input_ev);
                        }
                        ControlMessage::SetQualityMode(mode) => {
                            let tier = adaptive.lock().await.set_mode(mode);
                            keyframe_flag.store(true, Ordering::Release);
                            info!(%session_id, ?mode, ?tier, "updated quality mode");
                        }
                        ControlMessage::RequestKeyframe => {
                            keyframe_flag.store(true, Ordering::Release);
                        }
                        ControlMessage::SwitchMonitor { monitor_index } => {
                            requested_monitor.store(monitor_index, Ordering::Release);
                        }
                        ControlMessage::Clipboard(clip) => {
                            if let Err(err) = set_host_clipboard_text(&clip.text) {
                                warn!(%session_id, "failed to write Windows clipboard: {err}");
                            }
                        }
                        ControlMessage::RequestClipboard => {
                            if let Ok(Some(text)) = get_host_clipboard_text() {
                                let _ = peer_tx
                                    .send_control(&ControlMessage::Clipboard(
                                        helios_proto::ClipboardSync { text },
                                    ))
                                    .await;
                            }
                        }
                        ControlMessage::Ping { seq, client_ts_us } => {
                            let host_ts_us = SystemTime::now()
                                .duration_since(UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_micros() as u64;
                            let _ = peer_tx.send_control(&ControlMessage::Pong {
                                seq,
                                client_ts_us,
                                host_ts_us,
                            }).await;
                        }
                        _ => {}
                    },
                    Some(PeerEvent::Disconnected) | None => break,
                    _ => {}
                }
            }
        }
    }

    let _ = stop_tx.send(true);
    peer_tx.close().await;
    sig_watch.abort();
    cursor_task.abort();
    sender_task.abort();
    stats_task.abort();
    let _ = capture_task.await;

    #[cfg(windows)]
    prevent_windows_sleep(false);

    Ok(())
}

fn passcode_matches(expected: &str, provided: Option<&str>) -> bool {
    let Some(raw) = provided else {
        return false;
    };
    let normalize = |s: &str| -> Vec<u8> {
        s.bytes()
            .filter(|b| !b.is_ascii_whitespace() && *b != b'-')
            .map(|b| b.to_ascii_uppercase())
            .collect()
    };
    let a = normalize(expected);
    let b = normalize(raw);
    if a.len() != b.len() || a.is_empty() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(windows)]
fn prevent_windows_sleep(active: bool) {
    use windows::Win32::System::Power::{
        ES_CONTINUOUS, ES_DISPLAY_REQUIRED, ES_SYSTEM_REQUIRED, SetThreadExecutionState,
    };
    unsafe {
        if active {
            SetThreadExecutionState(ES_CONTINUOUS | ES_SYSTEM_REQUIRED | ES_DISPLAY_REQUIRED);
        } else {
            SetThreadExecutionState(ES_CONTINUOUS);
        }
    }
}
