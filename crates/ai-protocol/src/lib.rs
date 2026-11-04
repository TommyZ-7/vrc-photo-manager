//! Versioned sidecar protocol shared by the daemon and the AI plugin.

pub mod catalog;
pub mod error;
pub mod framing;
pub mod protocol;
pub mod status;

pub use catalog::{Catalog, PlaceBranch, TagBranch, TagDefaults, TagDefinition};
pub use error::{ProtocolError, SidecarError, SidecarErrorCode};
pub use framing::{MAX_LINE_BYTES, read_message, write_message};
pub use protocol::{
    BackendInfo, Hello, ImageRef, ModelInfo, Op, PlaceProbabilities, Probe, SidecarMessage,
    SidecarRequest, SidecarResponse, SidecarResult, TagProbability,
};
pub use status::{
    ModelState, PluginInstallStatus, TaggingErrorCode, TaggingJobStatus, TaggingStage,
};

/// Version of the sidecar JSONL contract implemented by this crate.
pub const PROTOCOL_VERSION: u32 = 1;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_version_is_one() {
        assert_eq!(PROTOCOL_VERSION, 1);
        assert_eq!(MAX_LINE_BYTES, 1024 * 1024);
    }
}
