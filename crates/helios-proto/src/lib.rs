//! Shared wire protocol, identity/cryptography, signaling messages,
//! UDP NAT rendezvous framing, and low-bandwidth dirty-tile codec for Helios.

pub mod crypto;
pub mod media;
pub mod signaling;
pub mod tiles;
pub mod udp;

pub use crypto::{
    DeviceId, DeviceIdentity, IceServer, PairingToken, TurnCredentials, derive_device_id,
    generate_turn_credentials, verify_signature,
};
pub use media::{
    ClipboardSync, ControlMessage, CursorShape, CursorUpdate, InputEvent, KeyCode, MediaMessage,
    MouseButton, QualityMode, QualityTier, SessionTelemetry, VideoCodec, VideoFramePacket,
};
pub use signaling::{DeviceRole, EndpointKind, HostSummary, SignalingMessage, UdpEndpoint};
pub use tiles::{DirtyTile, DirtyTileFrame, TileEncoder, apply_tile_frame};

/// Protocol version checked during signaling handshake.
pub const PROTOCOL_VERSION: u32 = 1;
