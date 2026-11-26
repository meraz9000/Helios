use serde::{Deserialize, Serialize};

use crate::tiles::DirtyTileFrame;

/// User-selectable quality mode exposed in the macOS viewer HUD.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QualityMode {
    /// Dynamically switch tiers based on RTT inflation, packet loss, and throughput.
    Auto,
    /// Full resolution, 30–60 FPS video (>3 Mbps).
    Smooth,
    /// Balanced 1080p/900p, 15–30 FPS (500 kbps – 3 Mbps).
    Balanced,
    /// 720p, 5–15 FPS, aggressive static-frame skip (150 – 500 kbps).
    LowBandwidth,
    /// Razor-sharp Zstd dirty-tile mode at 2–5 FPS for awful links (<150 kbps).
    SurvivalText,
}

/// Active quality tier decided by the adaptive congestion controller.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QualityTier {
    /// <150 kbps or severe packet loss: dirty 64x64 Zstd tiles, crisp text.
    SurvivalText = 0,
    /// 150–500 kbps: 720p, 5–15 FPS, low bitrate.
    SlowLink = 1,
    /// 500 kbps – 3 Mbps: 900p/1080p, 15–30 FPS.
    Constrained = 2,
    /// >3 Mbps: native/1080p, 30–60 FPS.
    Normal = 3,
}

impl QualityTier {
    pub fn target_fps(self) -> u32 {
        match self {
            Self::SurvivalText => 4,
            Self::SlowLink => 10,
            Self::Constrained => 24,
            Self::Normal => 60,
        }
    }

    pub fn target_bitrate_kbps(self) -> u32 {
        match self {
            Self::SurvivalText => 120,
            Self::SlowLink => 350,
            Self::Constrained => 1_500,
            Self::Normal => 6_000,
        }
    }

    pub fn max_scale_width(self) -> u32 {
        match self {
            Self::SurvivalText => 1920, // Keep native resolution for crisp text tiles!
            Self::SlowLink => 1280,
            Self::Constrained => 1600,
            Self::Normal => 3840,
        }
    }

    pub fn uses_dirty_tiles(self) -> bool {
        matches!(self, Self::SurvivalText)
    }
}

/// Codec used for an encoded video frame packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum VideoCodec {
    H264,
    H265,
    Av1,
    DirtyTilesZstd,
}

/// Encoded H.264/H.265/AV1 video access unit transmitted on the low-latency media channel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VideoFramePacket {
    pub frame_id: u64,
    pub timestamp_us: u64,
    pub width: u32,
    pub height: u32,
    pub codec: VideoCodec,
    pub is_keyframe: bool,
    pub payload: Vec<u8>,
}

/// Mouse button identifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
    Back,
    Forward,
}

/// Platform-independent USB HID Usage Page 0x07 keycode (or virtual key fallback).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyCode(pub u16);

/// High-priority interactive input events sent Client -> Host on the reliable
/// control channel so video traffic never delays clicks or keystrokes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum InputEvent {
    /// Absolute normalized pointer coordinates in `[0.0, 1.0]` across the active monitor.
    MouseMoveAbs { x: f32, y: f32 },
    /// Relative pointer delta (useful when pointer lock is active).
    MouseMoveRel { dx: i32, dy: i32 },
    MouseButton { button: MouseButton, pressed: bool },
    MouseWheel { dx: i32, dy: i32 },
    Key { code: KeyCode, pressed: bool },
    /// Trigger Windows Secure Attention Sequence (Ctrl+Alt+Del) via `SendSAS`.
    SendCtrlAltDel,
}

/// Cursor bitmap sent only when the Windows cursor shape changes so macOS can
/// render the pointer locally with zero latency.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CursorShape {
    pub shape_id: u64,
    pub width: u16,
    pub height: u16,
    pub hotspot_x: u16,
    pub hotspot_y: u16,
    /// Zstd-compressed RGBA cursor bitmap.
    pub rgba_zstd: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum CursorUpdate {
    Hidden,
    Position { x: f32, y: f32 },
    Shape(CursorShape),
}

/// On-demand text clipboard sync (capped to avoid saturating slow links).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClipboardSync {
    pub text: String,
}

/// Live telemetry shown in the macOS viewer HUD and consumed by the adaptive controller.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SessionTelemetry {
    pub rtt_ms: f32,
    pub packet_loss_pct: f32,
    pub video_kbps: u32,
    pub fps: f32,
    pub dropped_frames: u64,
    pub active_tier: Option<QualityTier>,
    pub using_relay: bool,
}

/// Messages sent on the **high-priority reliable Control & Input DataChannel** (`helios-ctrl`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ControlMessage {
    Input(InputEvent),
    Cursor(CursorUpdate),
    SetQualityMode(QualityMode),
    RequestKeyframe,
    SwitchMonitor { monitor_index: u32 },
    Clipboard(ClipboardSync),
    /// On-demand pull of Host's current text clipboard (avoids wasting slow bandwidth).
    RequestClipboard,
    Ping { seq: u64, client_ts_us: u64 },
    Pong { seq: u64, client_ts_us: u64, host_ts_us: u64 },
    Telemetry(SessionTelemetry),
}

/// Messages sent on the **unordered, drop-stale-frame Media DataChannel** (`helios-media`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum MediaMessage {
    VideoFrame(VideoFramePacket),
    DirtyTiles(DirtyTileFrame),
}

impl ControlMessage {
    pub fn encode(&self) -> Result<Vec<u8>, bincode::Error> {
        bincode::serialize(self)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, bincode::Error> {
        bincode::deserialize(bytes)
    }
}

impl MediaMessage {
    pub fn encode(&self) -> Result<Vec<u8>, bincode::Error> {
        bincode::serialize(self)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, bincode::Error> {
        bincode::deserialize(bytes)
    }
}
