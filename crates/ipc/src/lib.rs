//! Versioned local-socket protocol shared by the UI and the daemon.

pub mod framing;
pub mod protocol;

pub const PROTOCOL_VERSION: u32 = 1;
pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION");
