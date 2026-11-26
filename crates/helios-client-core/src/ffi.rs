//! Complete C-ABI FFI surface exported by `libhelios_client_core.a` for the native
//! SwiftUI + Metal + VideoToolbox macOS Viewer app (`apps/helios-macos`).

use std::ffi::{CStr, CString, c_char, c_void};
use std::path::Path;

use helios_proto::{
    DeviceId, DeviceIdentity, InputEvent, KeyCode, MouseButton, QualityMode, SessionTelemetry,
    VideoCodec,
};

use crate::session::{ClientCallbacks, ClientSession, ClientSessionConfig, ConnectionState};

/// Live telemetry snapshot passed across C-ABI to SwiftUI's HUD bar.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct HeliosTelemetryC {
    pub rtt_ms: f32,
    pub packet_loss_pct: f32,
    pub video_kbps: u32,
    pub fps: f32,
    pub using_relay: bool,
}

impl From<&SessionTelemetry> for HeliosTelemetryC {
    fn from(s: &SessionTelemetry) -> Self {
        Self {
            rtt_ms: s.rtt_ms,
            packet_loss_pct: s.packet_loss_pct,
            video_kbps: s.video_kbps,
            fps: s.fps,
            using_relay: s.using_relay,
        }
    }
}

/// Callback invoked on decoded RGBA dirty-tile framebuffer updates for Metal rendering.
pub type HeliosRgbaFrameCallback = extern "C" fn(
    user_data: *mut c_void,
    width: u32,
    height: u32,
    rgba_ptr: *const u8,
    rgba_len: usize,
    telemetry: HeliosTelemetryC,
);

/// Callback invoked on encoded Annex-B H.264/H.265/AV1 access units for Apple VideoToolbox decoding.
/// `codec`: 0 = H264, 1 = H265, 2 = AV1.
pub type HeliosVideoFrameCallback = extern "C" fn(
    user_data: *mut c_void,
    width: u32,
    height: u32,
    codec: u8,
    is_keyframe: bool,
    timestamp_us: u64,
    data_ptr: *const u8,
    data_len: usize,
    telemetry: HeliosTelemetryC,
);

/// Callback invoked on connection state changes.
/// `state`: 0 = Disconnected, 1 = Connecting, 2 = ConnectedP2P, 3 = ConnectedRelay, 4 = Error.
pub type HeliosStateCallback = extern "C" fn(
    user_data: *mut c_void,
    state: u8,
    message: *const c_char,
);

/// Callback invoked when the Windows Host sends a new cursor bitmap (raw RGBA bytes).
pub type HeliosCursorShapeCallback = extern "C" fn(
    user_data: *mut c_void,
    width: u16,
    height: u16,
    hotspot_x: u16,
    hotspot_y: u16,
    rgba_ptr: *const u8,
    rgba_len: usize,
);

/// Callback invoked when the Windows Host replies with clipboard text.
pub type HeliosClipboardCallback = extern "C" fn(user_data: *mut c_void, utf8_text: *const c_char);

/// Send-safe wrapper around raw C callback context pointer (Swift ensures lifetime outlives disconnect).
#[derive(Clone, Copy)]
struct SendPtr(*mut c_void);
unsafe impl Send for SendPtr {}
unsafe impl Sync for SendPtr {}
impl SendPtr {
    #[inline]
    fn as_ptr(self) -> *mut c_void {
        self.0
    }
}

/// Opaque session handle owned by Swift and freed via `helios_client_disconnect`.
pub struct HeliosClientHandle {
    runtime: tokio::runtime::Runtime,
    session: ClientSession,
    task: Option<tokio::task::JoinHandle<anyhow::Result<()>>>,
}

pub fn quality_mode_from_c(value: u8) -> QualityMode {
    match value {
        1 => QualityMode::SurvivalText,
        2 => QualityMode::LowBandwidth,
        3 => QualityMode::Balanced,
        4 => QualityMode::Smooth,
        _ => QualityMode::Auto,
    }
}

fn mouse_button_from_c(value: u8) -> MouseButton {
    match value {
        0 => MouseButton::Left,
        1 => MouseButton::Right,
        2 => MouseButton::Middle,
        3 => MouseButton::Back,
        _ => MouseButton::Forward,
    }
}

fn codec_to_c(codec: VideoCodec) -> u8 {
    match codec {
        VideoCodec::H264 => 0,
        VideoCodec::H265 => 1,
        VideoCodec::Av1 => 2,
        VideoCodec::DirtyTilesZstd => 3,
    }
}

unsafe fn cstr_to_string(ptr: *const c_char) -> Option<String> {
    if ptr.is_null() {
        return None;
    }
    unsafe { CStr::from_ptr(ptr) }
        .to_str()
        .ok()
        .map(ToOwned::to_owned)
}

/// Load or create an Ed25519 device identity at `path` and return its `HL-XXXX-XXXX` Device ID.
/// Caller must free the returned string via `helios_string_free`.
///
/// # Safety
/// `path` and `name` must be valid NUL-terminated UTF-8 C strings.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn helios_init_device_id(
    path: *const c_char,
    name: *const c_char,
) -> *mut c_char {
    let (Some(path_str), Some(name_str)) =
        (unsafe { cstr_to_string(path) }, unsafe { cstr_to_string(name) })
    else {
        return std::ptr::null_mut();
    };
    let Ok(id) = DeviceIdentity::load_or_create(Path::new(&path_str), &name_str) else {
        return std::ptr::null_mut();
    };
    CString::new(id.device_id.0)
        .map(CString::into_raw)
        .unwrap_or(std::ptr::null_mut())
}

/// Query online Windows Hosts from the DigitalOcean relay and return a JSON array string
/// (`[{"device_id":"HL-...","name":"...","os":"windows","online":true,"active_sessions":0}]`).
/// Returns null on error. Caller must free with `helios_string_free`.
///
/// # Safety
/// Non-null string arguments must be valid NUL-terminated UTF-8 C strings.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn helios_client_list_hosts(
    relay_url: *const c_char,
    identity_path: *const c_char,
    client_name: *const c_char,
    pairing_token: *const c_char,
) -> *mut c_char {
    let (Some(relay_url), Some(identity_path), Some(client_name)) = (
        unsafe { cstr_to_string(relay_url) },
        unsafe { cstr_to_string(identity_path) },
        unsafe { cstr_to_string(client_name) },
    ) else {
        return std::ptr::null_mut();
    };
    let pairing_token = unsafe { cstr_to_string(pairing_token) }.filter(|s| !s.is_empty());

    let Ok(identity) = DeviceIdentity::load_or_create(Path::new(&identity_path), &client_name)
    else {
        return std::ptr::null_mut();
    };

    let Ok(rt) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return std::ptr::null_mut();
    };

    let cfg = ClientSessionConfig {
        relay_url,
        identity,
        pairing_token,
        initial_mode: QualityMode::Auto,
        force_relay: false,
    };

    let Ok(hosts) = rt.block_on(ClientSession::list_online_hosts(&cfg)) else {
        return std::ptr::null_mut();
    };
    let Ok(json) = serde_json::to_string(&hosts) else {
        return std::ptr::null_mut();
    };
    CString::new(json)
        .map(CString::into_raw)
        .unwrap_or(std::ptr::null_mut())
}

/// Start an asynchronous QUIC remote desktop session to `target_host_id`.
/// Returns an opaque handle on success (must later be closed with `helios_client_disconnect`),
/// or null if parameters/identity initialization failed.
///
/// # Safety
/// String pointers must be valid NUL-terminated UTF-8 C strings (`pairing_token` may be null).
/// `user_data` must remain valid until `helios_client_disconnect` returns.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn helios_client_connect(
    relay_url: *const c_char,
    identity_path: *const c_char,
    client_name: *const c_char,
    pairing_token: *const c_char,
    target_host_id: *const c_char,
    initial_mode: u8,
    user_data: *mut c_void,
    on_rgba_frame: Option<HeliosRgbaFrameCallback>,
    on_video_frame: Option<HeliosVideoFrameCallback>,
    on_state_change: Option<HeliosStateCallback>,
    on_cursor_shape: Option<HeliosCursorShapeCallback>,
    on_clipboard: Option<HeliosClipboardCallback>,
) -> *mut HeliosClientHandle {
    let (Some(relay_url), Some(identity_path), Some(client_name), Some(target_host_id)) = (
        unsafe { cstr_to_string(relay_url) },
        unsafe { cstr_to_string(identity_path) },
        unsafe { cstr_to_string(client_name) },
        unsafe { cstr_to_string(target_host_id) },
    ) else {
        return std::ptr::null_mut();
    };
    let pairing_token = unsafe { cstr_to_string(pairing_token) }.filter(|s| !s.is_empty());

    let Ok(identity) = DeviceIdentity::load_or_create(Path::new(&identity_path), &client_name)
    else {
        return std::ptr::null_mut();
    };

    let Ok(runtime) = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
    else {
        return std::ptr::null_mut();
    };

    let ud_rgba = SendPtr(user_data);
    let ud_video = SendPtr(user_data);
    let ud_state = SendPtr(user_data);
    let ud_cursor = SendPtr(user_data);
    let ud_clip = SendPtr(user_data);

    let callbacks = ClientCallbacks {
        on_rgba_frame: Box::new(move |w, h, rgba, stats| {
            if let Some(cb) = on_rgba_frame {
                cb(
                    ud_rgba.as_ptr(),
                    w,
                    h,
                    rgba.as_ptr(),
                    rgba.len(),
                    HeliosTelemetryC::from(stats),
                );
            }
        }),
        on_video_frame: on_video_frame.map(|cb| {
            Box::new(move |vf: &helios_proto::VideoFramePacket, stats: &SessionTelemetry| {
                cb(
                    ud_video.as_ptr(),
                    vf.width,
                    vf.height,
                    codec_to_c(vf.codec),
                    vf.is_keyframe,
                    vf.timestamp_us,
                    vf.payload.as_ptr(),
                    vf.payload.len(),
                    HeliosTelemetryC::from(stats),
                );
            }) as crate::session::EncodedVideoCallback
        }),
        on_state_change: on_state_change.map(|cb| {
            Box::new(move |state: ConnectionState, msg: &str| {
                if let Ok(cmsg) = CString::new(msg) {
                    cb(ud_state.as_ptr(), state as u8, cmsg.as_ptr());
                }
            }) as crate::session::StateChangeCallback
        }),
        on_cursor_shape: on_cursor_shape.map(|cb| {
            Box::new(move |w: u16, h: u16, hx: u16, hy: u16, rgba: &[u8]| {
                cb(ud_cursor.as_ptr(), w, h, hx, hy, rgba.as_ptr(), rgba.len());
            }) as crate::session::CursorShapeCallback
        }),
        on_clipboard: on_clipboard.map(|cb| {
            Box::new(move |text: &str| {
                if let Ok(ctext) = CString::new(text) {
                    cb(ud_clip.as_ptr(), ctext.as_ptr());
                }
            }) as crate::session::ClipboardCallback
        }),
    };

    let cfg = ClientSessionConfig {
        relay_url,
        identity,
        pairing_token,
        initial_mode: quality_mode_from_c(initial_mode),
        force_relay: false,
    };

    let res = runtime.block_on(ClientSession::connect_with_callbacks(
        cfg,
        DeviceId(target_host_id),
        callbacks,
    ));

    match res {
        Ok((session, task)) => Box::into_raw(Box::new(HeliosClientHandle {
            runtime,
            session,
            task: Some(task),
        })),
        Err(_) => std::ptr::null_mut(),
    }
}

/// # Safety
/// `handle` must be a valid pointer returned by `helios_client_connect`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn helios_client_send_mouse_abs(
    handle: *mut HeliosClientHandle,
    x: f32,
    y: f32,
) -> bool {
    let Some(h) = (unsafe { handle.as_ref() }) else {
        return false;
    };
    h.session
        .send_input(InputEvent::MouseMoveAbs { x, y })
        .is_ok()
}

/// # Safety
/// `handle` must be a valid pointer returned by `helios_client_connect`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn helios_client_send_mouse_button(
    handle: *mut HeliosClientHandle,
    button: u8,
    pressed: bool,
) -> bool {
    let Some(h) = (unsafe { handle.as_ref() }) else {
        return false;
    };
    h.session
        .send_input(InputEvent::MouseButton {
            button: mouse_button_from_c(button),
            pressed,
        })
        .is_ok()
}

/// # Safety
/// `handle` must be a valid pointer returned by `helios_client_connect`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn helios_client_send_mouse_wheel(
    handle: *mut HeliosClientHandle,
    dx: i32,
    dy: i32,
) -> bool {
    let Some(h) = (unsafe { handle.as_ref() }) else {
        return false;
    };
    h.session
        .send_input(InputEvent::MouseWheel { dx, dy })
        .is_ok()
}

/// # Safety
/// `handle` must be a valid pointer returned by `helios_client_connect`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn helios_client_send_key(
    handle: *mut HeliosClientHandle,
    vk_code: u16,
    pressed: bool,
) -> bool {
    let Some(h) = (unsafe { handle.as_ref() }) else {
        return false;
    };
    h.session
        .send_input(InputEvent::Key {
            code: KeyCode(vk_code),
            pressed,
        })
        .is_ok()
}

/// # Safety
/// `handle` must be a valid pointer returned by `helios_client_connect`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn helios_client_send_ctrl_alt_del(handle: *mut HeliosClientHandle) -> bool {
    let Some(h) = (unsafe { handle.as_ref() }) else {
        return false;
    };
    h.session.send_input(InputEvent::SendCtrlAltDel).is_ok()
}

/// # Safety
/// `handle` must be a valid pointer returned by `helios_client_connect`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn helios_client_set_quality_mode(
    handle: *mut HeliosClientHandle,
    mode: u8,
) -> bool {
    let Some(h) = (unsafe { handle.as_ref() }) else {
        return false;
    };
    h.session
        .set_quality_mode(quality_mode_from_c(mode))
        .is_ok()
}

/// # Safety
/// `handle` must be a valid pointer returned by `helios_client_connect`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn helios_client_request_keyframe(handle: *mut HeliosClientHandle) -> bool {
    let Some(h) = (unsafe { handle.as_ref() }) else {
        return false;
    };
    h.session.request_keyframe().is_ok()
}

/// # Safety
/// `handle` must be valid and `utf8_text` must be a valid NUL-terminated UTF-8 string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn helios_client_send_clipboard(
    handle: *mut HeliosClientHandle,
    utf8_text: *const c_char,
) -> bool {
    let Some(h) = (unsafe { handle.as_ref() }) else {
        return false;
    };
    let Some(text) = (unsafe { cstr_to_string(utf8_text) }) else {
        return false;
    };
    h.session.send_clipboard(text).is_ok()
}

/// # Safety
/// `handle` must be a valid pointer returned by `helios_client_connect`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn helios_client_request_clipboard(handle: *mut HeliosClientHandle) -> bool {
    let Some(h) = (unsafe { handle.as_ref() }) else {
        return false;
    };
    h.session.request_clipboard().is_ok()
}

/// # Safety
/// `handle` must be a valid pointer returned by `helios_client_connect`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn helios_client_switch_monitor(
    handle: *mut HeliosClientHandle,
    monitor_index: u32,
) -> bool {
    let Some(h) = (unsafe { handle.as_ref() }) else {
        return false;
    };
    h.session.switch_monitor(monitor_index).is_ok()
}

/// Cleanly close the QUIC session, stop background tasks, and free the handle.
///
/// # Safety
/// `handle` must be null or a valid pointer returned by `helios_client_connect` (called once).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn helios_client_disconnect(handle: *mut HeliosClientHandle) {
    if handle.is_null() {
        return;
    }
    let mut boxed = unsafe { Box::from_raw(handle) };
    boxed.session.disconnect();
    if let Some(task) = boxed.task.take() {
        let _ = boxed.runtime.block_on(async {
            let _ = tokio::time::timeout(std::time::Duration::from_secs(2), task).await;
        });
    }
    boxed
        .runtime
        .shutdown_timeout(std::time::Duration::from_millis(500));
}

/// Free a C string returned by Helios FFI functions.
///
/// # Safety
/// `ptr` must have been returned by a Helios FFI function or be null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn helios_string_free(ptr: *mut c_char) {
    if !ptr.is_null() {
        unsafe {
            let _ = CString::from_raw(ptr);
        }
    }
}
