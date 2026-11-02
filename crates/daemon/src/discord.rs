//! Discord webhook send worker.
//!
//! One step claims an outbox row, materialises the upload image, sends it
//! through [`SendTransport`], and records the result. The module is crate
//! internal because the daemon's worker thread is its only production caller.

use std::fs;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::Result;
use vrc_photon_core::compressor::encode_for_send;
use vrc_photon_core::db::Db;
use vrc_photon_core::discord::{
    DISCORD_MAX_UPLOAD_BYTES, EmbedLabels, EmbedPhoto, SendImageFormat, SendImageSource,
    choose_send_source, mime_for_format, photo_payload_json, test_payload_json, truncate_chars,
    upload_filename, validate_webhook_url,
};
use vrc_photon_core::model::{
    CompressionProfile, Photo, SendCompletion, SendErrorCode, SendKind, SendOutboxEntry,
    SendSource, SendStatus,
};
use vrc_photon_core::settings::{AppSettings, load_discord_webhook};
use vrc_photon_i18n::tr;
use vrc_photon_ipc::Event;

use crate::discord_http::{SendImage, SendOutcome, SendRequest, SendTransport, UreqTransport};
use crate::ipc_server::EventBus;
use crate::signals::JobSignal;

/// Error details are capped before they reach the outbox or an event.
const MAX_ERROR_DETAIL_CHARS: usize = 200;
/// Terminal send rows kept after a successful upload.
const SEND_HISTORY_LIMIT: i64 = 500;
/// Worker poll interval while the outbox has no due row.
const POLL_INTERVAL: Duration = Duration::from_millis(500);
/// Minimum spacing between two successful webhook requests.
const SUCCESS_INTERVAL: Duration = Duration::from_millis(400);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProcessOutcome {
    Idle,
    Sent,
    Rescheduled,
    Failed,
}

/// Claims at most one due outbox row and processes it.
///
/// The integration settings are read before the claim so a disabled feature
/// never takes work out of the queue, then re-read after the claim because the
/// user can disable Discord while the row is between those two reads.
pub(crate) fn process_next_send(
    db: &Db,
    transport: &dyn SendTransport,
    events: &EventBus,
    now: i64,
) -> Result<ProcessOutcome> {
    let settings = AppSettings::load(db)?;
    if ready_webhook(db, &settings)?.is_none() {
        return Ok(ProcessOutcome::Idle);
    }

    let Some(entry) = db.claim_next_send(now)? else {
        return Ok(ProcessOutcome::Idle);
    };

    let settings = AppSettings::load(db)?;
    let Some(webhook) = ready_webhook(db, &settings)? else {
        db.release_send(entry.id)?;
        return Ok(ProcessOutcome::Idle);
    };

    publish_send(events, &entry, SendStatus::Processing, None);

    let prepared = match entry.kind {
        SendKind::Test => PreparedSend {
            payload_json: test_payload_json(&tr!("discord.test.message")),
            image: None,
            source: None,
            profile_id: None,
        },
        SendKind::Photo => {
            match prepare_photo_send(db, &settings, events, &entry, &settings.discord_destination)?
            {
                Some(prepared) => prepared,
                None => return Ok(ProcessOutcome::Failed),
            }
        }
    };

    tracing::debug!(
        outbox_id = entry.id,
        photo_id = ?entry.photo_id,
        kind = entry.kind.as_str(),
        "sending Discord webhook"
    );

    let PreparedSend {
        payload_json,
        image,
        source,
        profile_id,
    } = prepared;
    let request = SendRequest {
        url: webhook,
        payload_json,
        image,
    };
    let outcome = transport.send(&request);
    match outcome {
        SendOutcome::Sent {
            message_id,
            channel_id,
            guild_id,
        } => {
            let completion = SendCompletion {
                source,
                profile_id,
                destination: settings.discord_destination.clone(),
                message_id,
                channel_id,
                guild_id,
            };
            db.complete_send(entry.id, &completion)?;
            if let Err(error) = db.prune_send_history(SEND_HISTORY_LIMIT) {
                tracing::warn!(
                    %error,
                    outbox_id = entry.id,
                    "cannot prune Discord send history"
                );
            }
            publish_send(events, &entry, SendStatus::Sent, None);
            tracing::info!(
                outbox_id = entry.id,
                photo_id = ?entry.photo_id,
                status = "sent",
                "Discord send completed"
            );
            Ok(ProcessOutcome::Sent)
        }
        SendOutcome::RateLimited { retry_after } => {
            let next_attempt_at = now.saturating_add(retry_after.as_secs() as i64);
            db.reschedule_send(
                entry.id,
                entry.attempts,
                next_attempt_at,
                SendErrorCode::Network,
                Some("HTTP 429"),
            )?;
            publish_send(
                events,
                &entry,
                SendStatus::Pending,
                Some(SendErrorCode::Network.as_str()),
            );
            tracing::warn!(
                outbox_id = entry.id,
                photo_id = ?entry.photo_id,
                status = "pending",
                retry_after_secs = retry_after.as_secs(),
                "Discord send rate limited"
            );
            Ok(ProcessOutcome::Rescheduled)
        }
        SendOutcome::Retryable { detail } => {
            let attempts = entry.attempts.saturating_add(1);
            let next_attempt_at = now.saturating_add(retry_delay_seconds(attempts));
            let code = if detail.starts_with("HTTP ") {
                SendErrorCode::Http5xx
            } else {
                SendErrorCode::Network
            };
            let detail = truncate_chars(&detail, MAX_ERROR_DETAIL_CHARS);
            db.reschedule_send(entry.id, attempts, next_attempt_at, code, Some(&detail))?;
            publish_send(events, &entry, SendStatus::Pending, Some(code.as_str()));
            tracing::warn!(
                outbox_id = entry.id,
                photo_id = ?entry.photo_id,
                status = "pending",
                attempts,
                error_code = code.as_str(),
                "Discord send will be retried"
            );
            Ok(ProcessOutcome::Rescheduled)
        }
        SendOutcome::Failed { code, detail } => {
            record_failure(
                db,
                events,
                &entry,
                SendFailure {
                    code,
                    detail,
                    source,
                    profile_id,
                },
                &settings.discord_destination,
            )?;
            Ok(ProcessOutcome::Failed)
        }
    }
}

/// Marks stale `processing` rows as failed once when the worker starts.
///
/// Discord has no idempotency key, so a row that was already uploading when
/// the daemon stopped must not be sent again automatically.
pub(crate) fn recover_interrupted_sends(db: &Db) -> Result<usize> {
    let count = db.reset_processing_sends()?;
    if count > 0 {
        tracing::warn!(count, "marked interrupted Discord sends as failed");
    }
    Ok(count)
}

pub(crate) fn ipc_send_status(status: SendStatus) -> vrc_photon_ipc::SendStatus {
    match status {
        SendStatus::Waiting => vrc_photon_ipc::SendStatus::Waiting,
        SendStatus::Pending => vrc_photon_ipc::SendStatus::Pending,
        SendStatus::Processing => vrc_photon_ipc::SendStatus::Processing,
        SendStatus::Sent => vrc_photon_ipc::SendStatus::Sent,
        SendStatus::Failed => vrc_photon_ipc::SendStatus::Failed,
        SendStatus::Cancelled => vrc_photon_ipc::SendStatus::Cancelled,
        SendStatus::Unknown => vrc_photon_ipc::SendStatus::Unknown,
    }
}

/// The production transport used by the daemon's send worker thread.
pub(crate) fn default_transport() -> Arc<dyn SendTransport> {
    Arc::new(UreqTransport::new())
}

pub(crate) fn worker_loop(
    db: Arc<Mutex<Db>>,
    running: Arc<AtomicBool>,
    sends: Arc<JobSignal>,
    events: EventBus,
    transport: Arc<dyn SendTransport>,
) {
    {
        let db = db.lock().unwrap();
        if let Err(error) = recover_interrupted_sends(&db) {
            tracing::warn!(%error, "cannot recover interrupted Discord sends");
        }
    }

    let mut seen_generation = 0;
    while running.load(Ordering::Relaxed) {
        let now = chrono::Utc::now().timestamp();
        let outcome = {
            let db = db.lock().unwrap();
            match process_next_send(&db, transport.as_ref(), &events, now) {
                Ok(outcome) => outcome,
                Err(error) => {
                    tracing::error!(%error, "Discord send step failed");
                    ProcessOutcome::Idle
                }
            }
        };
        match outcome {
            // The signal only saves the poll wait when no row is due.
            ProcessOutcome::Idle => {
                sends.wait(&mut seen_generation, POLL_INTERVAL);
            }
            ProcessOutcome::Sent => thread::sleep(SUCCESS_INTERVAL),
            // Another due row can follow immediately; both outcomes updated it.
            ProcessOutcome::Rescheduled | ProcessOutcome::Failed => {}
        }
    }
}

/// The webhook to use, or `None` when Discord is off/unconfigured/invalid.
fn ready_webhook(db: &Db, settings: &AppSettings) -> Result<Option<String>> {
    if !settings.discord_enabled {
        return Ok(None);
    }
    let Some(url) = load_discord_webhook(db)? else {
        return Ok(None);
    };
    if validate_webhook_url(&url).is_err() {
        return Ok(None);
    }
    Ok(Some(url))
}

struct PreparedSend {
    payload_json: String,
    image: Option<SendImage>,
    source: Option<SendSource>,
    profile_id: Option<String>,
}

struct SendFailure {
    code: SendErrorCode,
    detail: Option<String>,
    source: Option<SendSource>,
    profile_id: Option<String>,
}

impl SendFailure {
    fn new(
        code: SendErrorCode,
        detail: Option<String>,
        source: Option<SendSource>,
        profile_id: Option<String>,
    ) -> Self {
        Self {
            code,
            detail,
            source,
            profile_id,
        }
    }
}

fn prepare_photo_send(
    db: &Db,
    settings: &AppSettings,
    events: &EventBus,
    entry: &SendOutboxEntry,
    destination: &str,
) -> Result<Option<PreparedSend>> {
    let Some(photo_id) = entry.photo_id else {
        record_failure(
            db,
            events,
            entry,
            SendFailure::new(SendErrorCode::PhotoMissing, None, None, None),
            destination,
        )?;
        return Ok(None);
    };
    let Some(photo) = db.photo_by_id(photo_id)? else {
        record_failure(
            db,
            events,
            entry,
            SendFailure::new(SendErrorCode::PhotoMissing, None, None, None),
            destination,
        )?;
        return Ok(None);
    };
    let profile = db
        .list_profiles()?
        .into_iter()
        .find(|profile| profile.discord && profile.enabled);
    let derivatives = db.derivatives_for_photo(photo.id)?;
    let source = choose_send_source(
        &photo,
        settings.discord_send_original,
        profile.as_ref(),
        &derivatives,
    );
    let (image, source, profile_id) = match materialise_source(&photo, profile.as_ref(), source) {
        Ok(value) => value,
        Err(failure) => {
            record_failure(db, events, entry, failure, destination)?;
            return Ok(None);
        }
    };
    let players = if settings.discord_include_players {
        db.photo_players(photo.id)?
            .into_iter()
            .map(|player| player.display_name)
            .filter(|name| !name.trim().is_empty())
            .collect()
    } else {
        Vec::new()
    };
    let captured_at = tr!("discord.embed.captured_at");
    let players_label = tr!("discord.embed.players");
    let unlinked = tr!("discord.embed.unlinked");
    let footer_app = format!(
        "{} {}",
        vrc_photon_core::APP_NAME,
        env!("CARGO_PKG_VERSION")
    );
    let labels = EmbedLabels {
        captured_at: &captured_at,
        players: &players_label,
        unlinked: &unlinked,
        footer_app: &footer_app,
    };
    let payload_json = photo_payload_json(
        &EmbedPhoto {
            world_name: photo.world_name.as_deref(),
            captured_at_utc: photo.captured_at_utc,
            captured_at_local: &photo.captured_at_local,
            file_name: &photo.file_name,
            players: &players,
        },
        &labels,
        settings.discord_include_players,
        &image.filename,
    );
    Ok(Some(PreparedSend {
        payload_json,
        image: Some(image),
        source,
        profile_id,
    }))
}

fn materialise_source(
    photo: &Photo,
    profile: Option<&CompressionProfile>,
    source: SendImageSource,
) -> std::result::Result<(SendImage, Option<SendSource>, Option<String>), SendFailure> {
    match source {
        SendImageSource::File {
            path,
            source,
            format,
        } => {
            let size = fs::metadata(&path)
                .map_err(|error| {
                    SendFailure::new(
                        SendErrorCode::PhotoMissing,
                        Some(error.to_string()),
                        Some(source),
                        None,
                    )
                })?
                .len();
            if size > DISCORD_MAX_UPLOAD_BYTES {
                return Err(SendFailure::new(
                    SendErrorCode::TooLarge,
                    Some(format!("{size} bytes")),
                    Some(source),
                    None,
                ));
            }
            let bytes = fs::read(&path).map_err(|error| {
                SendFailure::new(
                    SendErrorCode::PhotoMissing,
                    Some(error.to_string()),
                    Some(source),
                    None,
                )
            })?;
            // The file can grow between the metadata check and the read.
            let size = bytes.len() as u64;
            if size > DISCORD_MAX_UPLOAD_BYTES {
                return Err(SendFailure::new(
                    SendErrorCode::TooLarge,
                    Some(format!("{size} bytes")),
                    Some(source),
                    None,
                ));
            }
            let profile_id = if source == SendSource::Profile {
                profile.map(|profile| profile.id.clone())
            } else {
                None
            };
            Ok((
                SendImage {
                    bytes,
                    filename: upload_filename(photo.id, photo.captured_at_utc, format),
                    mime: mime_for_format(format).to_string(),
                },
                Some(source),
                profile_id,
            ))
        }
        SendImageSource::Encode {
            source_path,
            profile_id,
            ..
        } => {
            let Some(profile) = profile else {
                return Err(SendFailure::new(
                    SendErrorCode::ProfileMissing,
                    None,
                    Some(SendSource::Profile),
                    Some(profile_id),
                ));
            };
            let encoded = encode_for_send(&source_path, profile).map_err(|error| {
                SendFailure::new(
                    SendErrorCode::EncodeFailed,
                    Some(error.to_string()),
                    Some(SendSource::Profile),
                    Some(profile.id.clone()),
                )
            })?;
            let size = encoded.bytes.len() as u64;
            if size > DISCORD_MAX_UPLOAD_BYTES {
                return Err(SendFailure::new(
                    SendErrorCode::TooLarge,
                    Some(format!("{size} bytes")),
                    Some(SendSource::Profile),
                    Some(profile.id.clone()),
                ));
            }
            let format = SendImageFormat::from(&encoded.format);
            Ok((
                SendImage {
                    bytes: encoded.bytes,
                    filename: upload_filename(photo.id, photo.captured_at_utc, format),
                    mime: mime_for_format(format).to_string(),
                },
                Some(SendSource::Profile),
                Some(profile.id.clone()),
            ))
        }
        SendImageSource::MissingProfile => Err(SendFailure::new(
            SendErrorCode::ProfileMissing,
            None,
            None,
            None,
        )),
        SendImageSource::MissingPhoto => Err(SendFailure::new(
            SendErrorCode::PhotoMissing,
            None,
            None,
            None,
        )),
        SendImageSource::TooLarge { size } => Err(SendFailure::new(
            SendErrorCode::TooLarge,
            Some(format!("{size} bytes")),
            None,
            None,
        )),
    }
}

fn record_failure(
    db: &Db,
    events: &EventBus,
    entry: &SendOutboxEntry,
    failure: SendFailure,
    destination: &str,
) -> Result<()> {
    let detail = failure
        .detail
        .as_deref()
        .map(|detail| truncate_chars(detail, MAX_ERROR_DETAIL_CHARS));
    db.fail_send(
        entry.id,
        failure.code,
        detail.as_deref(),
        failure.source,
        failure.profile_id.as_deref(),
        Some(destination),
    )?;
    publish_send(
        events,
        entry,
        SendStatus::Failed,
        Some(failure.code.as_str()),
    );
    tracing::warn!(
        outbox_id = entry.id,
        photo_id = ?entry.photo_id,
        status = "failed",
        error_code = failure.code.as_str(),
        "Discord send failed"
    );
    Ok(())
}

fn publish_send(
    events: &EventBus,
    entry: &SendOutboxEntry,
    status: SendStatus,
    error_code: Option<&str>,
) {
    events.publish(Event::SendProgress {
        outbox_id: entry.id,
        photo_id: entry.photo_id,
        status: ipc_send_status(status),
        error_code: error_code.map(str::to_string),
    });
}

/// `min(2^(attempts - 1) * 2, 300)` seconds: 2, 4, 8, ... capped at 300.
fn retry_delay_seconds(attempts: i64) -> i64 {
    let exponent = attempts.saturating_sub(1).clamp(0, 8) as u32;
    (2i64.saturating_pow(exponent)).saturating_mul(2).min(300)
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::path::Path;
    use std::sync::Mutex;
    use std::time::Duration;

    use vrc_photon_core::model::{
        NewPhoto, SendErrorCode, SendKind, SendOrigin, SendSource, SendStatus,
    };
    use vrc_photon_core::settings::{AppSettings, save_discord_webhook};

    use super::*;
    use crate::discord_http::{SendOutcome, SendRequest, SendTransport};
    use crate::ipc_server::EventBus;

    #[derive(Default)]
    struct FakeTransport {
        requests: Mutex<Vec<SendRequest>>,
        outcomes: Mutex<VecDeque<SendOutcome>>,
    }

    impl SendTransport for FakeTransport {
        fn send(&self, request: &SendRequest) -> SendOutcome {
            self.requests.lock().unwrap().push(request.clone());
            self.outcomes
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(SendOutcome::Failed {
                    code: SendErrorCode::Unknown,
                    detail: None,
                })
        }
    }

    fn seed_photo_file(db: &Db, photo_dir: &Path, name: &str) -> i64 {
        let path = photo_dir.join(name);
        image::RgbaImage::from_pixel(16, 10, image::Rgba([10, 20, 30, 255]))
            .save(&path)
            .unwrap();
        let root = db
            .add_root(photo_dir.to_str().unwrap(), "reference")
            .unwrap();
        let meta = std::fs::metadata(&path).unwrap();
        db.upsert_photo(&NewPhoto {
            root_id: root,
            rel_path: name.to_string(),
            abs_path: path.to_string_lossy().into_owned(),
            file_name: name.to_string(),
            captured_at_utc: 1_700_000_000,
            captured_at_local: "2026-11-02 21:34:56".to_string(),
            tz_offset_sec: 0,
            width: 16,
            height: 10,
            file_size: meta.len(),
            mtime: 1_700_000_000,
            blake3: None,
        })
        .unwrap()
    }

    fn discord_db(dir: &tempfile::TempDir) -> (Db, i64) {
        let db = Db::open(dir.path().join("library.db")).unwrap();
        let photo_dir = dir.path().join("photos");
        std::fs::create_dir_all(&photo_dir).unwrap();
        let photo = seed_photo_file(&db, &photo_dir, "VRChat_2026-11-02_21-34-56.000_16x10.png");
        (db, photo)
    }

    #[test]
    fn disabled_integration_sends_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let (db, photo) = discord_db(&dir);
        let mut settings = AppSettings::load(&db).unwrap();
        settings.discord_enabled = false;
        settings.save(&db).unwrap();
        save_discord_webhook(&db, "https://discord.com/api/webhooks/1/t").unwrap();
        db.enqueue_send(
            photo,
            SendKind::Photo,
            SendOrigin::Manual,
            SendStatus::Pending,
        )
        .unwrap();
        let transport = FakeTransport::default();
        let events = EventBus::default();
        assert!(matches!(
            process_next_send(&db, &transport, &events, 100).unwrap(),
            ProcessOutcome::Idle
        ));
        assert!(transport.requests.lock().unwrap().is_empty());
    }

    fn enable_discord(db: &Db, url: &str) {
        let mut settings = AppSettings::load(db).unwrap();
        settings.discord_enabled = true;
        settings.save(db).unwrap();
        save_discord_webhook(db, url).unwrap();
    }

    fn sent_outcome() -> SendOutcome {
        SendOutcome::Sent {
            message_id: Some("42".into()),
            channel_id: Some("7".into()),
            guild_id: Some("9".into()),
        }
    }

    #[test]
    fn success_stores_message_id_and_publishes_sent() {
        let dir = tempfile::tempdir().unwrap();
        let (db, photo) = discord_db(&dir);
        enable_discord(&db, "https://discord.com/api/webhooks/1/t");
        let id = db
            .enqueue_send(
                photo,
                SendKind::Photo,
                SendOrigin::Manual,
                SendStatus::Pending,
            )
            .unwrap()
            .unwrap();
        let transport = FakeTransport::default();
        transport.outcomes.lock().unwrap().push_back(sent_outcome());
        let events = EventBus::default();
        let rx = events.subscribe();
        assert!(matches!(
            process_next_send(&db, &transport, &events, 100).unwrap(),
            ProcessOutcome::Sent
        ));
        let entry = db.send_entry_by_id(id).unwrap().unwrap();
        assert_eq!(entry.status, SendStatus::Sent);
        assert_eq!(entry.discord_message_id.as_deref(), Some("42"));
        assert_eq!(entry.discord_channel_id.as_deref(), Some("7"));
        assert_eq!(entry.source, Some(SendSource::Original));
        let requests = transport.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].image.is_some());
        drop(requests);
        let mut seen = Vec::new();
        while let Ok(event) = rx.try_recv() {
            seen.push(event);
        }
        assert!(seen.iter().any(|event| matches!(
            event,
            Event::SendProgress {
                status: vrc_photon_ipc::SendStatus::Sent,
                ..
            }
        )));
    }

    #[test]
    fn test_send_has_no_image() {
        let dir = tempfile::tempdir().unwrap();
        let (db, _photo) = discord_db(&dir);
        enable_discord(&db, "https://discord.com/api/webhooks/1/t");
        let id = db.enqueue_test_send().unwrap();
        let transport = FakeTransport::default();
        transport.outcomes.lock().unwrap().push_back(sent_outcome());
        let events = EventBus::default();
        assert!(matches!(
            process_next_send(&db, &transport, &events, 100).unwrap(),
            ProcessOutcome::Sent
        ));
        assert_eq!(
            db.send_entry_by_id(id).unwrap().unwrap().kind,
            SendKind::Test
        );
        let requests = transport.requests.lock().unwrap();
        assert!(requests[0].image.is_none());
        assert!(requests[0].payload_json.contains("content"));
    }

    #[test]
    fn rate_limit_reschedules_without_incrementing_attempts() {
        let dir = tempfile::tempdir().unwrap();
        let (db, photo) = discord_db(&dir);
        enable_discord(&db, "https://discord.com/api/webhooks/1/t");
        let id = db
            .enqueue_send(
                photo,
                SendKind::Photo,
                SendOrigin::Manual,
                SendStatus::Pending,
            )
            .unwrap()
            .unwrap();
        let transport = FakeTransport::default();
        transport
            .outcomes
            .lock()
            .unwrap()
            .push_back(SendOutcome::RateLimited {
                retry_after: Duration::from_secs(2),
            });
        let events = EventBus::default();
        assert!(matches!(
            process_next_send(&db, &transport, &events, 100).unwrap(),
            ProcessOutcome::Rescheduled
        ));
        let entry = db.send_entry_by_id(id).unwrap().unwrap();
        assert_eq!(entry.status, SendStatus::Pending);
        assert_eq!(entry.attempts, 0);
        assert_eq!(entry.next_attempt_at, Some(102));
        assert!(matches!(
            process_next_send(&db, &transport, &events, 101).unwrap(),
            ProcessOutcome::Idle
        ));
        assert_eq!(transport.requests.lock().unwrap().len(), 1);
    }

    #[test]
    fn network_error_reschedules_with_attempts_and_backoff() {
        let dir = tempfile::tempdir().unwrap();
        let (db, photo) = discord_db(&dir);
        enable_discord(&db, "https://discord.com/api/webhooks/1/t");
        let id = db
            .enqueue_send(
                photo,
                SendKind::Photo,
                SendOrigin::Manual,
                SendStatus::Pending,
            )
            .unwrap()
            .unwrap();
        let transport = FakeTransport::default();
        transport
            .outcomes
            .lock()
            .unwrap()
            .push_back(SendOutcome::Retryable {
                detail: "network error: ConnectionRefused".into(),
            });
        let events = EventBus::default();
        assert!(matches!(
            process_next_send(&db, &transport, &events, 100).unwrap(),
            ProcessOutcome::Rescheduled
        ));
        let entry = db.send_entry_by_id(id).unwrap().unwrap();
        assert_eq!(entry.status, SendStatus::Pending);
        assert_eq!(entry.attempts, 1);
        assert_eq!(entry.next_attempt_at, Some(102));
        assert_eq!(entry.error_code.as_deref(), Some("network"));
        assert!(matches!(
            process_next_send(&db, &transport, &events, 101).unwrap(),
            ProcessOutcome::Idle
        ));
    }

    #[test]
    fn unauthorized_marks_failed() {
        let dir = tempfile::tempdir().unwrap();
        let (db, photo) = discord_db(&dir);
        enable_discord(&db, "https://discord.com/api/webhooks/1/t");
        let id = db
            .enqueue_send(
                photo,
                SendKind::Photo,
                SendOrigin::Manual,
                SendStatus::Pending,
            )
            .unwrap()
            .unwrap();
        let transport = FakeTransport::default();
        transport
            .outcomes
            .lock()
            .unwrap()
            .push_back(SendOutcome::Failed {
                code: SendErrorCode::Http401,
                detail: Some("unauthorized".into()),
            });
        let events = EventBus::default();
        assert!(matches!(
            process_next_send(&db, &transport, &events, 100).unwrap(),
            ProcessOutcome::Failed
        ));
        let entry = db.send_entry_by_id(id).unwrap().unwrap();
        assert_eq!(entry.status, SendStatus::Failed);
        assert_eq!(entry.error_code.as_deref(), Some("http_401"));
        assert_eq!(entry.error_detail.as_deref(), Some("unauthorized"));
    }

    #[test]
    fn missing_photo_without_derivative_marks_failed() {
        let dir = tempfile::tempdir().unwrap();
        let (db, photo) = discord_db(&dir);
        enable_discord(&db, "https://discord.com/api/webhooks/1/t");
        let path = db.photo_by_id(photo).unwrap().unwrap().abs_path;
        std::fs::remove_file(path).unwrap();
        let id = db
            .enqueue_send(
                photo,
                SendKind::Photo,
                SendOrigin::Manual,
                SendStatus::Pending,
            )
            .unwrap()
            .unwrap();
        let transport = FakeTransport::default();
        let events = EventBus::default();
        assert!(matches!(
            process_next_send(&db, &transport, &events, 100).unwrap(),
            ProcessOutcome::Failed
        ));
        let entry = db.send_entry_by_id(id).unwrap().unwrap();
        assert_eq!(entry.status, SendStatus::Failed);
        assert_eq!(entry.error_code.as_deref(), Some("photo_missing"));
        assert!(transport.requests.lock().unwrap().is_empty());
    }

    #[test]
    fn replaced_photo_sends_display_derivative() {
        let dir = tempfile::tempdir().unwrap();
        let (db, photo) = discord_db(&dir);
        enable_discord(&db, "https://discord.com/api/webhooks/1/t");
        let display = dir.path().join("display.webp");
        std::fs::write(&display, b"WEBPDATA").unwrap();
        let derivative = db
            .add_derivative(
                photo,
                display.to_str().unwrap(),
                "webp",
                16,
                10,
                std::fs::metadata(&display).unwrap().len(),
                "other",
                "replace",
            )
            .unwrap();
        db.set_replaced(photo, derivative).unwrap();
        let id = db
            .enqueue_send(
                photo,
                SendKind::Photo,
                SendOrigin::Manual,
                SendStatus::Pending,
            )
            .unwrap()
            .unwrap();
        let transport = FakeTransport::default();
        transport.outcomes.lock().unwrap().push_back(sent_outcome());
        let events = EventBus::default();
        assert!(matches!(
            process_next_send(&db, &transport, &events, 100).unwrap(),
            ProcessOutcome::Sent
        ));
        let entry = db.send_entry_by_id(id).unwrap().unwrap();
        assert_eq!(entry.source, Some(SendSource::Derivative));
        let requests = transport.requests.lock().unwrap();
        assert_eq!(requests[0].image.as_ref().unwrap().bytes, b"WEBPDATA");
    }

    #[test]
    fn recover_interrupted_sends_marks_processing_failed() {
        let dir = tempfile::tempdir().unwrap();
        let (db, photo) = discord_db(&dir);
        let id = db
            .enqueue_send(
                photo,
                SendKind::Photo,
                SendOrigin::Manual,
                SendStatus::Pending,
            )
            .unwrap()
            .unwrap();
        db.claim_next_send(100).unwrap();
        assert_eq!(recover_interrupted_sends(&db).unwrap(), 1);
        let entry = db.send_entry_by_id(id).unwrap().unwrap();
        assert_eq!(entry.status, SendStatus::Failed);
        assert_eq!(entry.error_code.as_deref(), Some("interrupted"));
    }

    #[test]
    fn missing_or_invalid_webhook_never_claims_a_send() {
        let dir = tempfile::tempdir().unwrap();
        let (db, photo) = discord_db(&dir);
        let mut settings = AppSettings::load(&db).unwrap();
        settings.discord_enabled = true;
        settings.save(&db).unwrap();
        let id = db
            .enqueue_send(
                photo,
                SendKind::Photo,
                SendOrigin::Manual,
                SendStatus::Pending,
            )
            .unwrap()
            .unwrap();
        let transport = FakeTransport::default();
        let events = EventBus::default();
        assert!(matches!(
            process_next_send(&db, &transport, &events, 100).unwrap(),
            ProcessOutcome::Idle
        ));
        save_discord_webhook(&db, "http://discord.com/api/webhooks/1/t").unwrap();
        assert!(matches!(
            process_next_send(&db, &transport, &events, 101).unwrap(),
            ProcessOutcome::Idle
        ));
        assert!(transport.requests.lock().unwrap().is_empty());
        let entry = db.send_entry_by_id(id).unwrap().unwrap();
        assert_eq!(entry.status, SendStatus::Pending);
        assert_eq!(entry.attempts, 0);
    }

    #[test]
    fn retry_backoff_doubles_and_caps_at_five_minutes() {
        assert_eq!(retry_delay_seconds(1), 2);
        assert_eq!(retry_delay_seconds(2), 4);
        assert_eq!(retry_delay_seconds(3), 8);
        assert_eq!(retry_delay_seconds(8), 256);
        assert_eq!(retry_delay_seconds(9), 300);
        assert_eq!(retry_delay_seconds(100), 300);
    }

    #[test]
    fn ipc_status_matches_every_core_status() {
        for (core, ipc) in [
            (SendStatus::Waiting, vrc_photon_ipc::SendStatus::Waiting),
            (SendStatus::Pending, vrc_photon_ipc::SendStatus::Pending),
            (
                SendStatus::Processing,
                vrc_photon_ipc::SendStatus::Processing,
            ),
            (SendStatus::Sent, vrc_photon_ipc::SendStatus::Sent),
            (SendStatus::Failed, vrc_photon_ipc::SendStatus::Failed),
            (SendStatus::Cancelled, vrc_photon_ipc::SendStatus::Cancelled),
            (SendStatus::Unknown, vrc_photon_ipc::SendStatus::Unknown),
        ] {
            assert_eq!(ipc_send_status(core), ipc);
        }
    }
}
