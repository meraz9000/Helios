use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use futures_util::{SinkExt, StreamExt};
use helios_proto::{DeviceId, DeviceIdentity, DeviceRole, SignalingMessage};
use tokio::sync::mpsc;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, info, warn};

/// Authenticated signaling connection handle to the DigitalOcean `helios-relay`.
pub struct SignalingHandle {
    pub device_id: DeviceId,
    pub udp_relay_addr: String,
    tx: mpsc::UnboundedSender<SignalingMessage>,
    rx: mpsc::UnboundedReceiver<SignalingMessage>,
}

impl SignalingHandle {
    pub fn send(&self, msg: SignalingMessage) -> Result<()> {
        self.tx
            .send(msg)
            .map_err(|_| anyhow::anyhow!("signaling connection closed"))
    }

    pub async fn recv(&mut self) -> Option<SignalingMessage> {
        self.rx.recv().await
    }

    pub fn clone_sender(&self) -> mpsc::UnboundedSender<SignalingMessage> {
        self.tx.clone()
    }
}

pub struct SignalingClient;

impl SignalingClient {
    /// Connect to `ws://...` or `wss://...` on the DigitalOcean relay, complete
    /// the Ed25519 challenge-response handshake, and start background reader/writer
    /// and heartbeat tasks.
    pub async fn connect(
        relay_ws_url: &str,
        identity: &DeviceIdentity,
        role: DeviceRole,
        pairing_token: Option<String>,
        heartbeat_interval: Duration,
    ) -> Result<SignalingHandle> {
        let (ws_stream, _resp) = connect_async(relay_ws_url)
            .await
            .with_context(|| format!("failed to connect to relay at {relay_ws_url}"))?;

        let (mut ws_tx, mut ws_rx) = ws_stream.split();

        // 1. Expect Challenge from relay
        let nonce = match ws_rx.next().await {
            Some(Ok(Message::Text(txt))) => match serde_json::from_str::<SignalingMessage>(&txt)? {
                SignalingMessage::Challenge { nonce, .. } => nonce,
                other => bail!("expected Challenge from relay, got {other:?}"),
            },
            other => bail!("unexpected stream response while awaiting Challenge: {other:?}"),
        };

        // 2. Sign nonce with Ed25519 private key and send Authenticate
        let signature_b64 = identity.sign_b64(nonce.as_bytes());
        let auth = SignalingMessage::Authenticate {
            name: identity.name.clone(),
            os: std::env::consts::OS.to_string(),
            role,
            public_key_b64: identity.public_key_b64(),
            signature_b64,
            pairing_token,
        };
        let auth_json = serde_json::to_string(&auth)?;
        ws_tx.send(Message::Text(auth_json.into())).await?;

        // 3. Expect AuthOk
        let (device_id, udp_relay_addr) = match ws_rx.next().await {
            Some(Ok(Message::Text(txt))) => match serde_json::from_str::<SignalingMessage>(&txt)? {
                SignalingMessage::AuthOk {
                    device_id,
                    udp_relay_addr,
                } => (device_id, udp_relay_addr),
                SignalingMessage::Error { code, message } => {
                    bail!("relay rejected authentication ({code}): {message}");
                }
                other => bail!("expected AuthOk from relay, got {other:?}"),
            },
            other => bail!("unexpected stream response while awaiting AuthOk: {other:?}"),
        };

        info!(%device_id, ?role, relay = %relay_ws_url, %udp_relay_addr, "authenticated with helios-relay");

        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<SignalingMessage>();
        let (in_tx, in_rx) = mpsc::unbounded_channel::<SignalingMessage>();

        let hb_tx = out_tx.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(heartbeat_interval);
            ticker.tick().await;
            loop {
                ticker.tick().await;
                let ts = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64;
                if hb_tx
                    .send(SignalingMessage::Heartbeat { timestamp_ms: ts })
                    .is_err()
                {
                    break;
                }
            }
        });

        tokio::spawn(async move {
            while let Some(msg) = out_rx.recv().await {
                match serde_json::to_string(&msg) {
                    Ok(txt) => {
                        if ws_tx.send(Message::Text(txt.into())).await.is_err() {
                            break;
                        }
                    }
                    Err(err) => warn!("failed to serialize signaling message: {err}"),
                }
            }
        });

        tokio::spawn(async move {
            while let Some(item) = ws_rx.next().await {
                match item {
                    Ok(Message::Text(txt)) => match serde_json::from_str::<SignalingMessage>(&txt) {
                        Ok(SignalingMessage::HeartbeatAck { .. }) => {
                            debug!("received relay heartbeat ack");
                        }
                        Ok(msg) => {
                            if in_tx.send(msg).is_err() {
                                break;
                            }
                        }
                        Err(err) => warn!("invalid signaling JSON from relay: {err}"),
                    },
                    Ok(Message::Close(_)) => break,
                    Ok(_) => {}
                    Err(err) => {
                        warn!("signaling websocket read error: {err}");
                        break;
                    }
                }
            }
        });

        Ok(SignalingHandle {
            device_id,
            udp_relay_addr,
            tx: out_tx,
            rx: in_rx,
        })
    }
}
