use std::net::SocketAddr;
use std::time::Duration;

use helios_proto::{
    ControlMessage, DeviceIdentity, DeviceRole, InputEvent, MediaMessage, MouseButton,
    SignalingMessage, TileEncoder, apply_tile_frame,
};
use helios_relay::{DeviceStore, RelayConfig, RelayState, build_router, spawn_udp_relay};
use helios_transport::{PeerEvent, PeerRole, QuicRendezvous, SignalingClient};
use tokio::net::TcpListener;
use tokio::time::timeout;

async fn run_quic_e2e(force_relay: bool) {
    // 1. Start integrated UDP rendezvous/relay socket + HTTP/WS signaling server
    let (udp_addr, udp_task) = spawn_udp_relay("127.0.0.1:0".parse::<SocketAddr>().unwrap())
        .await
        .unwrap();

    let store = DeviceStore::in_memory(Some("TEST-PAIR-SECRET".into()));
    let state = RelayState::new(
        RelayConfig {
            udp_relay_addr: udp_addr.to_string(),
            allow_open_enrollment: false,
        },
        store,
    );
    let app = build_router(state);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let ws_addr = listener.local_addr().unwrap();
    let relay_ws_url = format!("ws://{ws_addr}/ws");

    let server_task = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    // 2. Unauthorized device without pairing token must be rejected
    let rogue_id = DeviceIdentity::generate("rogue-pc");
    let rogue_res = SignalingClient::connect(
        &relay_ws_url,
        &rogue_id,
        DeviceRole::Client,
        None,
        Duration::from_secs(30),
    )
    .await;
    assert!(rogue_res.is_err(), "unpaired device must be rejected");

    // 3. Connect Windows Host and macOS Client using pairing token
    let host_id = DeviceIdentity::generate("win-workstation");
    let client_id = DeviceIdentity::generate("macbook-pro");

    let mut host_sig = SignalingClient::connect(
        &relay_ws_url,
        &host_id,
        DeviceRole::Host,
        Some("TEST-PAIR-SECRET".into()),
        Duration::from_secs(30),
    )
    .await
    .expect("host connects");

    let mut client_sig = SignalingClient::connect(
        &relay_ws_url,
        &client_id,
        DeviceRole::Client,
        Some("TEST-PAIR-SECRET".into()),
        Duration::from_secs(30),
    )
    .await
    .expect("client connects");

    // 4. Client discovers online Host and initiates session
    client_sig.send(SignalingMessage::ListHosts).unwrap();
    let hosts = match timeout(Duration::from_secs(3), client_sig.recv())
        .await
        .unwrap()
        .unwrap()
    {
        SignalingMessage::HostList { hosts } => hosts,
        other => panic!("expected HostList, got {other:?}"),
    };
    assert_eq!(hosts.len(), 1);
    assert_eq!(hosts[0].device_id, host_id.device_id);

    client_sig
        .send(SignalingMessage::RequestSession {
            target_host_id: host_id.device_id.clone(),
            access_token: None,
        })
        .unwrap();

    // 5. Receive SessionCreated (client) and IncomingSession (host)
    let (session_id, client_udp_relay) = match timeout(Duration::from_secs(3), client_sig.recv())
        .await
        .unwrap()
        .unwrap()
    {
        SignalingMessage::SessionCreated {
            session_id,
            udp_relay_addr,
            ..
        } => (session_id, udp_relay_addr),
        other => panic!("expected SessionCreated, got {other:?}"),
    };

    let (host_session_id, host_udp_relay) = match timeout(Duration::from_secs(3), host_sig.recv())
        .await
        .unwrap()
        .unwrap()
    {
        SignalingMessage::IncomingSession {
            session_id,
            udp_relay_addr,
            ..
        } => (session_id, udp_relay_addr),
        other => panic!("expected IncomingSession, got {other:?}"),
    };
    assert_eq!(session_id, host_session_id);

    // 6. Both peers prepare QUIC rendezvous (discover observed NAT addr + generate pinned TLS 1.3 cert)
    let (host_rdv, host_hello) =
        QuicRendezvous::prepare(session_id, PeerRole::Host, &host_udp_relay)
            .await
            .unwrap();
    let (client_rdv, client_hello) =
        QuicRendezvous::prepare(session_id, PeerRole::Client, &client_udp_relay)
            .await
            .unwrap();

    host_sig.send(host_hello).unwrap();
    client_sig.send(client_hello).unwrap();

    let (host_remote_cert, host_remote_eps) = match timeout(Duration::from_secs(3), host_sig.recv())
        .await
        .unwrap()
        .unwrap()
    {
        SignalingMessage::QuicPeerHello {
            cert_sha256_hex,
            endpoints,
            ..
        } => (cert_sha256_hex, endpoints),
        other => panic!("host expected QuicPeerHello, got {other:?}"),
    };

    let (client_remote_cert, client_remote_eps) =
        match timeout(Duration::from_secs(3), client_sig.recv())
            .await
            .unwrap()
            .unwrap()
        {
            SignalingMessage::QuicPeerHello {
                cert_sha256_hex,
                endpoints,
                ..
            } => (cert_sha256_hex, endpoints),
            other => panic!("client expected QuicPeerHello, got {other:?}"),
        };

    // 7. Establish encrypted QUIC TLS 1.3 connection concurrently (direct P2P or droplet UDP relay)
    let (host_res, client_res) = tokio::join!(
        host_rdv.establish(host_remote_cert, host_remote_eps, force_relay),
        client_rdv.establish(client_remote_cert, client_remote_eps, force_relay)
    );
    let mut host_peer = host_res.expect("host QUIC establish");
    let mut client_peer = client_res.expect("client QUIC establish");

    assert_eq!(host_peer.using_relay, force_relay);
    assert_eq!(client_peer.using_relay, force_relay);

    // 8. Stream a Survival/Text Mode dirty-tile frame Host -> Client over QUIC
    let width = 128u32;
    let height = 64u32;
    let mut screen = vec![42u8; (width * height * 4) as usize];
    screen[0] = 250;
    screen[1] = 100;
    screen[2] = 50;
    screen[3] = 255;

    let mut tile_enc = TileEncoder::new(64, 3, 0);
    let tile_frame = tile_enc
        .encode_rgba(width, height, &screen, 12345, true)
        .unwrap()
        .unwrap();
    host_peer
        .send_media(&MediaMessage::DirtyTiles(tile_frame))
        .await
        .unwrap();

    // 9. Send high-priority mouse click Client -> Host on dedicated QUIC control stream
    client_peer
        .send_control(&ControlMessage::Input(InputEvent::MouseButton {
            button: MouseButton::Left,
            pressed: true,
        }))
        .await
        .unwrap();

    // Verify Client receives and reconstructs exact framebuffer
    let mut got_frame = false;
    timeout(Duration::from_secs(5), async {
        while let Some(ev) = client_peer.recv_event().await {
            if let PeerEvent::Media(MediaMessage::DirtyTiles(df)) = ev {
                let mut cw = 0;
                let mut ch = 0;
                let mut fb = Vec::new();
                apply_tile_frame(&df, &mut cw, &mut ch, &mut fb).unwrap();
                assert_eq!(cw, width);
                assert_eq!(ch, height);
                assert_eq!(fb, screen);
                got_frame = true;
                break;
            }
        }
    })
    .await
    .expect("client must receive dirty tile frame over QUIC");
    assert!(got_frame);

    // Verify Host receives prioritized mouse click
    let mut got_input = false;
    timeout(Duration::from_secs(5), async {
        while let Some(ev) = host_peer.recv_event().await {
            if let PeerEvent::Control(ControlMessage::Input(InputEvent::MouseButton {
                button: MouseButton::Left,
                pressed: true,
            })) = ev
            {
                got_input = true;
                break;
            }
        }
    })
    .await
    .expect("host must receive mouse click over QUIC control stream");
    assert!(got_input);

    // Clean shutdown
    host_peer.close().await;
    client_peer.close().await;
    server_task.abort();
    udp_task.abort();
}

#[tokio::test]
async fn quic_direct_p2p_e2e() {
    run_quic_e2e(false).await;
}

#[tokio::test]
async fn quic_droplet_udp_relay_fallback_e2e() {
    run_quic_e2e(true).await;
}
