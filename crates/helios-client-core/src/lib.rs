//! Shared Rust client engine for Helios Viewer (used by the native macOS
//! SwiftUI/Metal/VideoToolbox app via C-ABI FFI and by `helios-cli`).

pub mod ffi;
pub mod session;

pub use session::{
    ClientCallbacks, ClientSession, ClientSessionConfig, ConnectionState, DecodedFrameCallback,
};
