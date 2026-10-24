//! Versioned local-socket protocol shared by the UI and the daemon.

pub mod client;
pub mod endpoint;
pub mod framing;
pub mod protocol;
pub mod server;

pub use client::IpcError;
pub use endpoint::Endpoint;
pub use protocol::*;

pub const PROTOCOL_VERSION: u32 = 1;
pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION");
