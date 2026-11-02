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
    /// Nudges the daemon's Discord send worker.
    WakeSends,
    SetPaused {
        paused: bool,
    },
    /// Batched notice that the UI generated thumbnails on its own side.
    ThumbnailsGenerated {
        files: u64,
        bytes: u64,
    },
    /// Delete the whole thumbnail cache and reset the daemon's byte ledger.
    ClearThumbnailCache,
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
    ThumbnailCacheCleared {
        removed_files: u64,
        removed_bytes: u64,
        remaining_files: u64,
        remaining_bytes: u64,
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
    /// Discord sends still queued or in flight. Defaults to 0 so a snapshot
    /// from a daemon without the send worker still decodes.
    #[serde(default)]
    pub pending_sends: i64,
    /// Monotonic version stamped by the daemon when the snapshot is taken
    /// under its status lock. A snapshot published late therefore still
    /// carries the older version, and receivers drop it once a newer one
    /// has been applied. Zero means the sender does not version snapshots.
    #[serde(default)]
    pub version: u64,
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

/// Lifecycle of one Discord webhook send. Mirrors the core outbox status so
/// the daemon can report every row without translation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SendStatus {
    Waiting,
    Pending,
    Processing,
    Sent,
    Failed,
    Cancelled,
    /// A status written by a newer version. Never treated as terminal.
    Unknown,
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
    /// One Discord send changed state. `error_code` is set when the daemon
    /// reports a classified failure.
    SendProgress {
        outbox_id: i64,
        photo_id: Option<i64>,
        status: SendStatus,
        error_code: Option<String>,
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
    fn status_snapshot_without_a_version_decodes_as_zero() {
        // Snapshots written before versioning existed carry no version
        // field; they decode as 0, which receivers always accept.
        let json = r#"{"paused":false,"watching":true,"scanning":true,"scan_phase":"walking","scan_processed":3,"scan_total":null,"scan_indexed":2,"scan_root":0,"scan_roots":1,"photo_count":10,"pending_jobs":1}"#;
        let snapshot: StatusSnapshot = serde_json::from_str(json).unwrap();
        assert_eq!(snapshot.version, 0);
        assert!(snapshot.scanning);
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
    fn clear_thumbnail_cache_round_trips() {
        let value = RequestEnvelope {
            id: 9,
            request: Request::ClearThumbnailCache,
        };
        let json = serde_json::to_string(&value).unwrap();
        assert!(json.contains("\"clear_thumbnail_cache\""));
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
    fn wake_sends_round_trips() {
        let value = RequestEnvelope {
            id: 10,
            request: Request::WakeSends,
        };
        let json = serde_json::to_string(&value).unwrap();
        assert!(json.contains("\"wake_sends\""));
        assert_eq!(
            serde_json::from_str::<RequestEnvelope>(&json).unwrap(),
            value
        );
    }

    #[test]
    fn send_progress_round_trips_and_defaults_pending_sends() {
        let event = Event::SendProgress {
            outbox_id: 4,
            photo_id: Some(9),
            status: SendStatus::Processing,
            error_code: None,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert_eq!(serde_json::from_str::<Event>(&json).unwrap(), event);
        let snapshot: StatusSnapshot = serde_json::from_str(r#"{"paused":false,"watching":true,"scanning":false,"scan_phase":null,"scan_processed":0,"scan_total":null,"scan_indexed":0,"scan_root":0,"scan_roots":0,"photo_count":1,"pending_jobs":0}"#).unwrap();
        assert_eq!(snapshot.pending_sends, 0);
    }

    #[test]
    fn send_status_serializes_as_snake_case() {
        let cases = [
            (SendStatus::Waiting, "\"waiting\""),
            (SendStatus::Pending, "\"pending\""),
            (SendStatus::Processing, "\"processing\""),
            (SendStatus::Sent, "\"sent\""),
            (SendStatus::Failed, "\"failed\""),
            (SendStatus::Cancelled, "\"cancelled\""),
            (SendStatus::Unknown, "\"unknown\""),
        ];
        for (status, json) in cases {
            assert_eq!(serde_json::to_string(&status).unwrap(), json);
            assert_eq!(serde_json::from_str::<SendStatus>(json).unwrap(), status);
        }
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
