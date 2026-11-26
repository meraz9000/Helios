use std::ffi::{CStr, CString, c_char, c_void};
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use helios_client_core::ffi::{
    HeliosTelemetryC, helios_client_connect, helios_client_disconnect, helios_client_list_hosts,
    helios_client_request_clipboard, helios_client_send_key, helios_client_send_mouse_abs,
    helios_client_set_quality_mode, helios_client_switch_monitor, helios_string_free,
};
use helios_proto::{
    ClipboardSync, ControlMessage, CursorShape, CursorUpdate, DeviceIdentity, DeviceRole,
    InputEvent, MediaMessage, SignalingMessage, TileEncoder, VideoCodec, VideoFramePacket,
};
use helios_relay::{DeviceStore, RelayConfig, RelayState, build_router, spawn_udp_relay};
use helios_transport::{PeerEvent, PeerRole, QuicRendezvous, SignalingClient};
use tokio::net::TcpListener;

struct CallbackContext {
    rgba_frames: AtomicUsize,
    video_frames: AtomicUsize,
    cursor_shapes: AtomicUsize,
    clipboard_received: AtomicBool,
    connected_p2p: AtomicBool,
}

extern "C" fn on_rgba_cb(
    user_data: *mut c_void,
    width: u32,
    height: u32,
    rgba_ptr: *const u8,
    rgba_len: usize,
    _telemetry: HeliosTelemetryC,
) {
    let ctx = unsafe { &*(user_data as *const CallbackContext) };
    if width == 128 && height == 64 && !rgba_ptr.is_null() && rgba_len == (128 * 64 * 4) {
        ctx.rgba_frames.fetch_add(1, Ordering::SeqCst);
    }
}

extern "C" fn on_video_cb(
    user_data: *mut c_void,
    width: u32,
    height: u32,
    codec: u8,
    is_keyframe: bool,
    _ts: u64,
    data_ptr: *const u8,
    data_len: usize,
    _telemetry: HeliosTelemetryC,
) {
    let ctx = unsafe { &*(user_data as *const CallbackContext) };
    if width == 1920 && height == 1080 && codec == 0 && is_keyframe && !data_ptr.is_null() && data_len > 0 {
        ctx.video_frames.fetch_add(1, Ordering::SeqCst);
    }
}

extern "C" fn on_state_cb(user_data: *mut c_void, state: u8, _message: *const c_char) {
    let ctx = unsafe { &*(user_data as *const CallbackContext) };
    if state == 2 {
        ctx.connected_p2p.store(true, Ordering::SeqCst);
    }
}

extern "C" fn on_cursor_cb(
    user_data: *mut c_void,
    width: u16,
    height: u16,
    hotspot_x: u16,
    hotspot_y: u16,
    rgba_ptr: *const u8,
    rgba_len: usize,
) {
    let ctx = unsafe { &*(user_data as *const CallbackContext) };
    if width == 16 && height == 16 && hotspot_x == 2 && hotspot_y == 3 && !rgba_ptr.is_null() && rgba_len == 16 * 16 * 4 {
        ctx.cursor_shapes.fetch_add(1, Ordering::SeqCst);
    }
}

extern "C" fn on_clipboard_cb(user_data: *mut c_void, utf8_text: *const c_char) {
    let ctx = unsafe { &*(user_data as *const CallbackContext) };
    if !utf8_text.is_null()
        && let Ok(s) = unsafe { CStr::from_ptr(utf8_text) }.to_str()
        && s == "Hello from Windows Host!"
    {
        ctx.clipboard_received.store(true, Ordering::SeqCst);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn c_abi_ffi_client_receives_rgba_h264_cursor_clipboard_and_sends_input() {
    let (udp_addr, udp_task) = spawn_udp_relay("127.0.0.1:0".parse::<SocketAddr>().unwrap())
        .await
        .unwrap();
    let store = DeviceStore::in_memory(Some("FFI-TOKEN".into()));
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
    let relay_url = format!("ws://{ws_addr}/ws");
    let server_task = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let host_id = DeviceIdentity::generate("Win-Host-FFI");
    let host_device_id = host_id.device_id.0.clone();
    let host_passcode = host_id.access_passcode.clone();
    let host_got_input = Arc::new(AtomicBool::new(false));
    let host_got_monitor = Arc::new(AtomicBool::new(false));
    let host_got_input_clone = host_got_input.clone();
    let host_got_monitor_clone = host_got_monitor.clone();

    let relay_url_host = relay_url.clone();
    let expected_passcode = host_passcode.clone();
    let host_task = tokio::spawn(async move {
        let mut sig = SignalingClient::connect(
            &relay_url_host,
            &host_id,
            DeviceRole::Host,
            Some("FFI-TOKEN".into()),
            Duration::from_secs(30),
        )
        .await
        .unwrap();

        if let Some(SignalingMessage::IncomingSession {
            session_id,
            udp_relay_addr,
            access_token,
            ..
        }) = sig.recv().await
        {
            assert_eq!(access_token.as_deref(), Some(expected_passcode.as_str()));
            let (rdv, hello) =
                QuicRendezvous::prepare(session_id, PeerRole::Host, &udp_relay_addr)
                    .await
                    .unwrap();
            sig.send(hello).unwrap();

            if let Some(SignalingMessage::QuicPeerHello {
                cert_sha256_hex,
                endpoints,
                ..
            }) = sig.recv().await
            {
                let (peer_tx, mut peer_rx) = rdv
                    .establish(cert_sha256_hex, endpoints, false)
                    .await
                    .unwrap()
                    .split();

                // 1. Send dirty-tile RGBA frame
                let mut enc = TileEncoder::new(64, 3, 0);
                let screen = vec![90u8; 128 * 64 * 4];
                let df = enc.encode_rgba(128, 64, &screen, 1000, true).unwrap().unwrap();
                peer_tx.send_media(&MediaMessage::DirtyTiles(df)).await.unwrap();

                // 2. Send Annex-B H.264 IDR frame
                let fake_h264 = VideoFramePacket {
                    frame_id: 2,
                    timestamp_us: 2000,
                    width: 1920,
                    height: 1080,
                    codec: VideoCodec::H264,
                    is_keyframe: true,
                    payload: vec![0, 0, 0, 1, 0x65, 0x88, 0x84],
                };
                peer_tx
                    .send_media(&MediaMessage::VideoFrame(fake_h264))
                    .await
                    .unwrap();

                // 3. Send local cursor bitmap shape (16x16 RGBA Zstd)
                let cursor_rgba = vec![255u8; 16 * 16 * 4];
                let cursor_zstd = zstd::bulk::compress(&cursor_rgba, 3).unwrap();
                peer_tx
                    .send_control(&ControlMessage::Cursor(CursorUpdate::Shape(CursorShape {
                        shape_id: 99,
                        width: 16,
                        height: 16,
                        hotspot_x: 2,
                        hotspot_y: 3,
                        rgba_zstd: cursor_zstd,
                    })))
                    .await
                    .unwrap();

                // 4. Process client input, monitor switch, and on-demand clipboard pull
                while let Some(ev) = peer_rx.recv().await {
                    match ev {
                        PeerEvent::Control(ControlMessage::Input(InputEvent::Key {
                            code,
                            pressed: true,
                        })) if code.0 == 0x41 => {
                            host_got_input_clone.store(true, Ordering::SeqCst);
                        }
                        PeerEvent::Control(ControlMessage::SwitchMonitor { monitor_index: 1 }) => {
                            host_got_monitor_clone.store(true, Ordering::SeqCst);
                        }
                        PeerEvent::Control(ControlMessage::RequestClipboard) => {
                            peer_tx
                                .send_control(&ControlMessage::Clipboard(ClipboardSync {
                                    text: "Hello from Windows Host!".into(),
                                }))
                                .await
                                .unwrap();
                        }
                        PeerEvent::Disconnected => break,
                        _ => {}
                    }
                }
                peer_tx.close().await;
            }
        }
    });

    tokio::time::sleep(Duration::from_millis(150)).await;

    let tmp_id_path =
        std::env::temp_dir().join(format!("helios-ffi-test-{}.json", uuid::Uuid::new_v4()));
    let c_relay = CString::new(relay_url.as_str()).unwrap();
    let c_id_path = CString::new(tmp_id_path.to_str().unwrap()).unwrap();
    let c_name = CString::new("MacBook-FFI").unwrap();
    let c_token = CString::new("FFI-TOKEN").unwrap();
    let c_passcode = CString::new(host_passcode.as_str()).unwrap();
    let c_host_id = CString::new(host_device_id.as_str()).unwrap();

    let list_json = tokio::task::spawn_blocking({
        let c_relay = c_relay.clone();
        let c_id_path = c_id_path.clone();
        let c_name = c_name.clone();
        let c_token = c_token.clone();
        move || unsafe {
            let ptr = helios_client_list_hosts(
                c_relay.as_ptr(),
                c_id_path.as_ptr(),
                c_name.as_ptr(),
                c_token.as_ptr(),
            );
            assert!(!ptr.is_null());
            let s = CStr::from_ptr(ptr).to_str().unwrap().to_string();
            helios_string_free(ptr);
            s
        }
    })
    .await
    .unwrap();
    assert!(list_json.contains(&host_device_id));

    let ctx = Box::new(CallbackContext {
        rgba_frames: AtomicUsize::new(0),
        video_frames: AtomicUsize::new(0),
        cursor_shapes: AtomicUsize::new(0),
        clipboard_received: AtomicBool::new(false),
        connected_p2p: AtomicBool::new(false),
    });
    let ctx_ptr = &*ctx as *const CallbackContext as *mut c_void;

    let handle_addr = tokio::task::spawn_blocking({
        let c_relay = c_relay.clone();
        let c_id_path = c_id_path.clone();
        let c_name = c_name.clone();
        let c_token = c_token.clone();
        let c_passcode = c_passcode.clone();
        let c_host_id = c_host_id.clone();
        let ctx_addr = ctx_ptr as usize;
        move || unsafe {
            let h = helios_client_connect(
                c_relay.as_ptr(),
                c_id_path.as_ptr(),
                c_name.as_ptr(),
                c_token.as_ptr(),
                c_passcode.as_ptr(),
                c_host_id.as_ptr(),
                0,
                ctx_addr as *mut c_void,
                Some(on_rgba_cb),
                Some(on_video_cb),
                Some(on_state_cb),
                Some(on_cursor_cb),
                Some(on_clipboard_cb),
            );
            assert!(!h.is_null());
            assert!(helios_client_set_quality_mode(h, 1));
            assert!(helios_client_send_mouse_abs(h, 0.5, 0.5));
            assert!(helios_client_send_key(h, 0x41, true));
            assert!(helios_client_switch_monitor(h, 1));
            assert!(helios_client_request_clipboard(h));
            h as usize
        }
    })
    .await
    .unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline {
        if ctx.rgba_frames.load(Ordering::SeqCst) >= 1
            && ctx.video_frames.load(Ordering::SeqCst) >= 1
            && ctx.cursor_shapes.load(Ordering::SeqCst) >= 1
            && ctx.clipboard_received.load(Ordering::SeqCst)
            && ctx.connected_p2p.load(Ordering::SeqCst)
            && host_got_input.load(Ordering::SeqCst)
            && host_got_monitor.load(Ordering::SeqCst)
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    assert!(ctx.connected_p2p.load(Ordering::SeqCst), "expected ConnectedP2P");
    assert!(ctx.rgba_frames.load(Ordering::SeqCst) >= 1, "expected RGBA dirty-tile");
    assert!(ctx.video_frames.load(Ordering::SeqCst) >= 1, "expected H.264 video");
    assert!(ctx.cursor_shapes.load(Ordering::SeqCst) >= 1, "expected local cursor shape");
    assert!(ctx.clipboard_received.load(Ordering::SeqCst), "expected clipboard pull reply");
    assert!(host_got_input.load(Ordering::SeqCst), "expected host VK_A key");
    assert!(host_got_monitor.load(Ordering::SeqCst), "expected host monitor switch");

    tokio::task::spawn_blocking(move || unsafe {
        helios_client_disconnect(handle_addr as *mut _);
    })
    .await
    .unwrap();

    let _ = std::fs::remove_file(tmp_id_path);
    host_task.abort();
    server_task.abort();
    udp_task.abort();
}
