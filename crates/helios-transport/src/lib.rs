//! Shared QUIC (`quinn`) P2P/Relay transport, signaling client, adaptive low-bandwidth
//! congestion controller, and anti-bufferbloat frame queue for Helios.

pub mod adaptive;
pub mod quic;
pub mod signaling;

pub use adaptive::{AdaptiveController, LatestFrameSlot, NetworkSample};
pub use quic::{
    HeliosPeer, HeliosPeerSender, PeerEvent, PeerRole, QuicRendezvous, QuicTransportStats,
};
pub use signaling::{SignalingClient, SignalingHandle};
