use serde::{Deserialize, Serialize};

/// Client identity sent during the handshake.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientInfo {
    pub name: String,
    pub version: String,
}

/// Requests accepted by the daemon.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    Hello {
        protocol_version: u32,
        client: ClientInfo,
    },
    Status,
    WakeCommands,
    WakeJobs,
    SetPaused {
        paused: bool,
    },
    /// Batched notice that the UI generated thumbnails on its own side.
    ThumbnailsGenerated {
        files: u64,
        bytes: u64,
    },
    Shutdown,
    Subscribe,
}

/// Responses returned by the daemon.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    Hello {
        protocol_version: u32,
        daemon_version: String,
        pid: u32,
        status: StatusSnapshot,
    },
    Status {
        status: StatusSnapshot,
    },
    Subscribed {
        status: StatusSnapshot,
    },
    Ok,
    Error {
        code: ErrorCode,
        message: String,
    },
}

/// Error codes currently emitted by the daemon. A version mismatch is
/// reported through the `hello` response, not an error response.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    InvalidRequest,
}

/// Live daemon state shared with the UI.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusSnapshot {
    pub paused: bool,
    pub watching: bool,
    pub scanning: bool,
    pub scan_phase: Option<ScanPhase>,
    pub scan_processed: usize,
    pub scan_total: Option<usize>,
    pub scan_indexed: usize,
    pub scan_root: usize,
    pub scan_roots: usize,
    pub photo_count: i64,
    pub pending_jobs: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScanPhase {
    Walking,
    Indexing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    Pending,
    Processing,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Operation {
    Scan,
    Log,
    Link,
}

/// Events streamed to subscribed clients.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    ScanStarted {
        roots: usize,
        /// Set when the scan targets one import root; `None` for full scans.
        #[serde(default)]
        target: Option<String>,
    },
    ScanProgress {
        phase: ScanPhase,
        processed: usize,
        total: Option<usize>,
        indexed: usize,
        root: usize,
        roots: usize,
    },
    ScanFinished {
        found: usize,
        indexed: usize,
        skipped: usize,
        failures: usize,
        failed: bool,
        /// Automatic compression jobs created by this scan. Defaults to 0 so
        /// an older daemon stays compatible with a newer UI.
        #[serde(default)]
        auto_enqueued: usize,
        /// Set when the scan targets one import root; `None` for full scans.
        #[serde(default)]
        target: Option<String>,
    },
    CompressionProgress {
        job_id: String,
        photo_id: i64,
        status: JobStatus,
        progress: f64,
        error: Option<String>,
    },
    DaemonErrorChanged {
        operation: Operation,
    },
    StatusChanged {
        status: StatusSnapshot,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RequestEnvelope {
    pub id: u64,
    pub request: Request,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    Response { id: u64, response: Response },
    Event { event: Event },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_envelope_round_trips_as_tagged_json() {
        let value = RequestEnvelope {
            id: 7,
            request: Request::SetPaused { paused: true },
        };
        let json = serde_json::to_string(&value).unwrap();
        assert!(json.contains("\"set_paused\""));
        assert_eq!(
            serde_json::from_str::<RequestEnvelope>(&json).unwrap(),
            value
        );
    }

    #[test]
    fn thumbnails_generated_round_trips() {
        let value = RequestEnvelope {
            id: 8,
            request: Request::ThumbnailsGenerated {
                files: 3,
                bytes: 4096,
            },
        };
        let json = serde_json::to_string(&value).unwrap();
        assert!(json.contains("\"thumbnails_generated\""));
        assert_eq!(
            serde_json::from_str::<RequestEnvelope>(&json).unwrap(),
            value
        );
    }

    #[test]
    fn server_event_round_trips() {
        let value = ServerMessage::Event {
            event: Event::ScanProgress {
                phase: ScanPhase::Indexing,
                processed: 50,
                total: Some(100),
                indexed: 49,
                root: 1,
                roots: 2,
            },
        };
        let json = serde_json::to_string(&value).unwrap();
        assert_eq!(serde_json::from_str::<ServerMessage>(&json).unwrap(), value);
    }

    #[test]
    fn scan_finished_defaults_new_fields_for_older_daemons() {
        let event: Event = serde_json::from_str(
            r#"{"type":"scan_finished","found":1,"indexed":1,"skipped":0,"failures":0,"failed":false}"#,
        )
        .unwrap();
        assert_eq!(
            event,
            Event::ScanFinished {
                found: 1,
                indexed: 1,
                skipped: 0,
                failures: 0,
                failed: false,
                auto_enqueued: 0,
                target: None,
            }
        );
    }
}
