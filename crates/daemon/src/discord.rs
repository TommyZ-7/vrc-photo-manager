//! Discord webhook send worker.
//!
//! One step claims an outbox row, materialises the upload image, sends it
//! through [`SendTransport`], and records the result. The module is crate
//! internal because the daemon's worker thread is its only production caller.

use std::fs;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use vrc_photon_core::compressor::encode_for_send;
use vrc_photon_core::db::Db;
use vrc_photon_core::discord::{
    DISCORD_MAX_UPLOAD_BYTES, EmbedLabels, EmbedPhoto, SendImageFormat, SendImageSource,
    choose_send_source, mime_for_format, photo_payload_json, test_payload_json, truncate_chars,
    upload_filename, validate_webhook_url,
};
use vrc_photon_core::model::{
    CompressionProfile, Photo, PhotoStatus, SendCompletion, SendErrorCode, SendKind, SendOrigin,
    SendOutboxEntry, SendSource, SendStatus,
};
use vrc_photon_core::settings::{AppSettings, DiscordAutoMode, load_discord_webhook};
use vrc_photon_i18n::tr;
use vrc_photon_ipc::Event;

use crate::discord_http::{SendImage, SendOutcome, SendRequest, SendTransport};
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
    /// Discord rate-limited the webhook; every send must wait this long.
    RateLimited {
        retry_after: Duration,
    },
}

/// Everything needed to perform one upload, collected under the DB lock.
struct SendPlan {
    entry: SendOutboxEntry,
    webhook: String,
    destination: String,
    kind: SendPlanKind,
}

enum SendPlanKind {
    Test,
    Photo(Box<PhotoPlan>),
    /// The row is already known to be unsendable; no lock is needed to fail it.
    Failure(SendFailure),
}

struct PhotoPlan {
    photo: Photo,
    players: Vec<String>,
    include_players: bool,
    source: SendImageSource,
    profile: Option<CompressionProfile>,
}

struct PreparedRequest {
    request: SendRequest,
    source: Option<SendSource>,
    profile_id: Option<String>,
}

struct SendUpdate {
    outcome: ProcessOutcome,
    status: SendStatus,
    error_code: Option<&'static str>,
}

/// Claims and processes at most one due outbox row.
///
/// The DB mutex is taken in two short phases. File reads, encoding, and the
/// webhook upload all happen while it is released, so a slow upload cannot
/// block the daemon or the UI.
pub(crate) fn process_next_send(
    db: &Arc<Mutex<Db>>,
    transport: &dyn SendTransport,
    events: &EventBus,
    now: i64,
) -> Result<ProcessOutcome> {
    let plan = {
        let db = db.lock().unwrap();
        plan_next_send(&db, now)?
    };
    let Some(SendPlan {
        entry,
        webhook,
        destination,
        kind,
    }) = plan
    else {
        return Ok(ProcessOutcome::Idle);
    };

    publish_send(events, &entry, SendStatus::Processing, None);

    // No DB lock is held while this block reads files, encodes, or uploads.
    let prepared = match prepare_request(kind, webhook) {
        Ok(prepared) => prepared,
        Err(failure) => {
            let update = {
                let db = db.lock().unwrap();
                record_failure(&db, &entry, &destination, failure)?
            };
            publish_send(events, &entry, update.status, update.error_code);
            return Ok(update.outcome);
        }
    };
    let PreparedRequest {
        request,
        source,
        profile_id,
    } = prepared;
    let outcome = transport.send(&request);

    let update = {
        let db = db.lock().unwrap();
        record_outcome(&db, &entry, &destination, now, outcome, source, profile_id)?
    };
    publish_send(events, &entry, update.status, update.error_code);
    Ok(update.outcome)
}

/// Phase one: validate the integration, claim a row, and snapshot all DB data.
fn plan_next_send(db: &Db, now: i64) -> Result<Option<SendPlan>> {
    let settings = AppSettings::load(db)?;
    if ready_webhook(db, &settings)?.is_none() {
        return Ok(None);
    }

    let Some(entry) = db.claim_next_send(now)? else {
        return Ok(None);
    };

    // The integration can be disabled while the row waited in the queue.
    let settings = AppSettings::load(db)?;
    let Some(webhook) = ready_webhook(db, &settings)? else {
        db.release_send(entry.id)?;
        return Ok(None);
    };

    let destination = settings.discord_destination.clone();
    let kind = match entry.kind {
        SendKind::Test => SendPlanKind::Test,
        SendKind::Photo => match build_photo_plan(db, &settings, entry.photo_id)? {
            Some(plan) => SendPlanKind::Photo(Box::new(plan)),
            None => SendPlanKind::Failure(SendFailure::new(
                SendErrorCode::PhotoMissing,
                None,
                None,
                None,
            )),
        },
    };
    Ok(Some(SendPlan {
        entry,
        webhook,
        destination,
        kind,
    }))
}

fn build_photo_plan(
    db: &Db,
    settings: &AppSettings,
    photo_id: Option<i64>,
) -> Result<Option<PhotoPlan>> {
    let Some(photo_id) = photo_id else {
        return Ok(None);
    };
    let Some(photo) = db.photo_by_id(photo_id)? else {
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
    let players = if settings.discord_include_players {
        db.photo_players(photo.id)?
            .into_iter()
            .map(|player| player.display_name)
            .filter(|name| !name.trim().is_empty())
            .collect()
    } else {
        Vec::new()
    };
    Ok(Some(PhotoPlan {
        photo,
        players,
        include_players: settings.discord_include_players,
        source,
        profile,
    }))
}

/// Phase two: materialise the image, build the embed, and prepare the POST.
///
/// This runs without the DB mutex. Every parse/IO/encode error is a normal
/// send failure that phase three records.
fn prepare_request(
    kind: SendPlanKind,
    webhook: String,
) -> std::result::Result<PreparedRequest, SendFailure> {
    match kind {
        SendPlanKind::Failure(failure) => Err(failure),
        SendPlanKind::Test => Ok(PreparedRequest {
            request: SendRequest {
                url: webhook,
                payload_json: test_payload_json(&tr!("discord.test.message")),
                image: None,
            },
            source: None,
            profile_id: None,
        }),
        SendPlanKind::Photo(plan) => {
            let PhotoPlan {
                photo,
                players,
                include_players,
                source,
                profile,
            } = *plan;
            let (image, source, profile_id) = materialise_source(&photo, profile.as_ref(), source)?;
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
                include_players,
                &image.filename,
            );
            Ok(PreparedRequest {
                request: SendRequest {
                    url: webhook,
                    payload_json,
                    image: Some(image),
                },
                source,
                profile_id,
            })
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

/// Queues automatic sends for photos the scan/index path just imported.
///
/// Returns the number of rows actually inserted: photos outside the configured
/// cutoff, photos being deleted, and photos that already have an active send
/// are skipped. An unconfigured or invalid webhook enqueues nothing. The
/// caller notifies the worker after a successful enqueue.
// The scan/index paths call this; it stays public within the crate so the
// enqueue policy can also be exercised in isolation.
pub(crate) fn enqueue_auto_sends(db: &Db, photo_ids: &[i64]) -> Result<usize> {
    let settings = AppSettings::load(db)?;
    if !settings.discord_enabled || settings.discord_auto_mode == DiscordAutoMode::Off {
        return Ok(0);
    }
    let Some(url) = load_discord_webhook(db)? else {
        return Ok(0);
    };
    if validate_webhook_url(&url).is_err() {
        return Ok(0);
    }
    let Some(since) = settings.discord_auto_since else {
        return Ok(0);
    };
    let status = match settings.discord_auto_mode {
        DiscordAutoMode::Immediate => SendStatus::Pending,
        DiscordAutoMode::Waiting => SendStatus::Waiting,
        DiscordAutoMode::Off => return Ok(0),
    };

    let mut enqueued = 0;
    for &photo_id in photo_ids {
        let Some(photo) = db.photo_by_id(photo_id)? else {
            continue;
        };
        if photo.captured_at_utc < since || photo.status == PhotoStatus::Deleting {
            continue;
        }
        if db
            .enqueue_send(photo_id, SendKind::Photo, SendOrigin::Auto, status)?
            .is_some()
        {
            enqueued += 1;
        }
    }
    Ok(enqueued)
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
    let mut paused_until: Option<Instant> = None;
    while running.load(Ordering::Relaxed) {
        // A 429 applies to the webhook, not just the row that received it.
        // Wait here so no other due row can hammer the same endpoint.
        if let Some(until) = paused_until {
            let now = Instant::now();
            if now < until {
                sends.wait(&mut seen_generation, until - now);
                continue;
            }
            paused_until = None;
        }

        let now = chrono::Utc::now().timestamp();
        let outcome = match process_next_send(&db, transport.as_ref(), &events, now) {
            Ok(outcome) => outcome,
            Err(error) => {
                tracing::error!(%error, "Discord send step failed");
                ProcessOutcome::Idle
            }
        };
        match outcome {
            // The signal only saves the poll wait when no row is due.
            ProcessOutcome::Idle => {
                sends.wait(&mut seen_generation, POLL_INTERVAL);
            }
            // Keep at least 400 ms between requests, whatever the result.
            ProcessOutcome::Sent | ProcessOutcome::Rescheduled | ProcessOutcome::Failed => {
                thread::sleep(SUCCESS_INTERVAL);
            }
            ProcessOutcome::RateLimited { retry_after } => {
                paused_until = Some(Instant::now() + retry_after);
                continue;
            }
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

/// Phase three for a row that could not be prepared: record the failure.
fn record_failure(
    db: &Db,
    entry: &SendOutboxEntry,
    destination: &str,
    failure: SendFailure,
) -> Result<SendUpdate> {
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
    tracing::warn!(
        outbox_id = entry.id,
        photo_id = ?entry.photo_id,
        status = "failed",
        error_code = failure.code.as_str(),
        "Discord send failed"
    );
    Ok(SendUpdate {
        outcome: ProcessOutcome::Failed,
        status: SendStatus::Failed,
        error_code: Some(failure.code.as_str()),
    })
}

/// Phase three for a completed upload: persist the outcome under the DB lock.
fn record_outcome(
    db: &Db,
    entry: &SendOutboxEntry,
    destination: &str,
    now: i64,
    outcome: SendOutcome,
    source: Option<SendSource>,
    profile_id: Option<String>,
) -> Result<SendUpdate> {
    match outcome {
        SendOutcome::Sent {
            message_id,
            channel_id,
            guild_id,
        } => {
            let completion = SendCompletion {
                source,
                profile_id,
                destination: destination.to_string(),
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
            tracing::info!(
                outbox_id = entry.id,
                photo_id = ?entry.photo_id,
                status = "sent",
                "Discord send completed"
            );
            Ok(SendUpdate {
                outcome: ProcessOutcome::Sent,
                status: SendStatus::Sent,
                error_code: None,
            })
        }
        SendOutcome::RateLimited { retry_after } => {
            let next_attempt_at = now.saturating_add(retry_after.as_secs() as i64);
            db.reschedule_send(
                entry.id,
                entry.attempts,
                next_attempt_at,
                SendErrorCode::RateLimited,
                Some("HTTP 429"),
            )?;
            tracing::warn!(
                outbox_id = entry.id,
                photo_id = ?entry.photo_id,
                status = "pending",
                retry_after_secs = retry_after.as_secs(),
                "Discord send rate limited"
            );
            Ok(SendUpdate {
                outcome: ProcessOutcome::RateLimited { retry_after },
                status: SendStatus::Pending,
                error_code: Some(SendErrorCode::RateLimited.as_str()),
            })
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
            tracing::warn!(
                outbox_id = entry.id,
                photo_id = ?entry.photo_id,
                status = "pending",
                attempts,
                error_code = code.as_str(),
                "Discord send will be retried"
            );
            Ok(SendUpdate {
                outcome: ProcessOutcome::Rescheduled,
                status: SendStatus::Pending,
                error_code: Some(code.as_str()),
            })
        }
        SendOutcome::Failed { code, detail } => {
            let detail = detail
                .as_deref()
                .map(|detail| truncate_chars(detail, MAX_ERROR_DETAIL_CHARS));
            db.fail_send(
                entry.id,
                code,
                detail.as_deref(),
                source,
                profile_id.as_deref(),
                Some(destination),
            )?;
            tracing::warn!(
                outbox_id = entry.id,
                photo_id = ?entry.photo_id,
                status = "failed",
                error_code = code.as_str(),
                "Discord send failed"
            );
            Ok(SendUpdate {
                outcome: ProcessOutcome::Failed,
                status: SendStatus::Failed,
                error_code: Some(code.as_str()),
            })
        }
    }
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
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::{Duration, Instant};

    use vrc_photon_core::model::{
        NewPhoto, SendErrorCode, SendKind, SendOrigin, SendSource, SendStatus,
    };
    use vrc_photon_core::settings::{
        AppSettings, DiscordAutoMode, clear_discord_webhook, save_discord_webhook,
    };

    use super::*;
    use crate::discord_http::{SendOutcome, SendRequest, SendTransport};
    use crate::ipc_server::EventBus;
    use crate::signals::JobSignal;

    #[derive(Default)]
    struct FakeTransport {
        requests: Mutex<Vec<SendRequest>>,
        outcomes: Mutex<VecDeque<SendOutcome>>,
        /// When set, `send` checks that the daemon DB lock is free.
        db: Option<Arc<Mutex<Db>>>,
        lock_was_free: AtomicBool,
        attempts_at: Mutex<Vec<Instant>>,
    }

    impl SendTransport for FakeTransport {
        fn send(&self, request: &SendRequest) -> SendOutcome {
            if let Some(db) = &self.db
                && db.try_lock().is_ok()
            {
                self.lock_was_free.store(true, Ordering::SeqCst);
            }
            self.attempts_at.lock().unwrap().push(Instant::now());
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

    fn with_db<T>(db: &Arc<Mutex<Db>>, f: impl FnOnce(&Db) -> T) -> T {
        let guard = db.lock().unwrap();
        f(&guard)
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

    fn discord_db(dir: &tempfile::TempDir) -> (Arc<Mutex<Db>>, i64) {
        let db = Db::open(dir.path().join("library.db")).unwrap();
        let photo_dir = dir.path().join("photos");
        std::fs::create_dir_all(&photo_dir).unwrap();
        let photo = seed_photo_file(&db, &photo_dir, "VRChat_2026-11-02_21-34-56.000_16x10.png");
        (Arc::new(Mutex::new(db)), photo)
    }

    fn enable_discord(db: &Arc<Mutex<Db>>, url: &str) {
        with_db(db, |db| {
            let mut settings = AppSettings::load(db).unwrap();
            settings.discord_enabled = true;
            settings.save(db).unwrap();
            save_discord_webhook(db, url).unwrap();
        });
    }

    fn enqueue_photo(db: &Arc<Mutex<Db>>, photo: i64) -> i64 {
        with_db(db, |db| {
            db.enqueue_send(
                photo,
                SendKind::Photo,
                SendOrigin::Manual,
                SendStatus::Pending,
            )
            .unwrap()
            .unwrap()
        })
    }

    fn seed_photo_at(db: &Db, captured_at_utc: i64, name: &str) -> i64 {
        let root = db.add_root("/discord-photos", "reference").unwrap();
        db.upsert_photo(&NewPhoto {
            root_id: root,
            rel_path: name.to_string(),
            abs_path: format!("/discord-photos/{name}"),
            file_name: name.to_string(),
            captured_at_utc,
            captured_at_local: "2026-11-02 21:34:56".to_string(),
            tz_offset_sec: 0,
            width: 16,
            height: 10,
            file_size: 10,
            mtime: captured_at_utc,
            blake3: None,
        })
        .unwrap()
    }

    #[test]
    fn auto_enqueue_respects_mode_webhook_and_auto_since() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path().join("library.db")).unwrap();
        let old = seed_photo_at(&db, 100, "old.png");
        let new = seed_photo_at(&db, 300, "new.png");
        let mut settings = AppSettings::load(&db).unwrap();
        settings.discord_enabled = true;
        settings.discord_auto_mode = DiscordAutoMode::Immediate;
        settings.discord_auto_since = Some(200);
        settings.save(&db).unwrap();
        save_discord_webhook(&db, "https://discord.com/api/webhooks/1/t").unwrap();
        assert_eq!(enqueue_auto_sends(&db, &[old, new]).unwrap(), 1);
        let history = db.send_history(10).unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].entry.photo_id, Some(new));
        assert_eq!(history[0].entry.origin, SendOrigin::Auto);
        assert_eq!(history[0].entry.status, SendStatus::Pending);
    }

    #[test]
    fn auto_enqueue_does_nothing_when_disabled_or_url_missing_or_mode_off() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path().join("library.db")).unwrap();
        let photo = seed_photo_at(&db, 300, "a.png");
        let mut settings = AppSettings::load(&db).unwrap();
        settings.discord_auto_mode = DiscordAutoMode::Immediate;
        settings.discord_auto_since = Some(200);
        settings.save(&db).unwrap();
        save_discord_webhook(&db, "https://discord.com/api/webhooks/1/t").unwrap();
        // enabled=false
        assert_eq!(enqueue_auto_sends(&db, &[photo]).unwrap(), 0);
        // URL なし
        settings.discord_enabled = true;
        settings.save(&db).unwrap();
        clear_discord_webhook(&db).unwrap();
        assert_eq!(enqueue_auto_sends(&db, &[photo]).unwrap(), 0);
        // 無効な URL
        save_discord_webhook(&db, "https://example.com/api/webhooks/1/t").unwrap();
        assert_eq!(enqueue_auto_sends(&db, &[photo]).unwrap(), 0);
        // mode=Off
        save_discord_webhook(&db, "https://discord.com/api/webhooks/1/t").unwrap();
        settings.discord_auto_mode = DiscordAutoMode::Off;
        settings.save(&db).unwrap();
        assert_eq!(enqueue_auto_sends(&db, &[photo]).unwrap(), 0);
    }

    #[test]
    fn waiting_mode_enqueues_waiting_and_release_flips_to_pending() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path().join("library.db")).unwrap();
        let photo = seed_photo_at(&db, 300, "a.png");
        let mut settings = AppSettings::load(&db).unwrap();
        settings.discord_enabled = true;
        settings.discord_auto_mode = DiscordAutoMode::Waiting;
        settings.discord_auto_since = Some(200);
        settings.save(&db).unwrap();
        save_discord_webhook(&db, "https://discord.com/api/webhooks/1/t").unwrap();
        assert_eq!(enqueue_auto_sends(&db, &[photo]).unwrap(), 1);
        let entry = db.send_history(10).unwrap().remove(0).entry;
        assert_eq!(entry.status, SendStatus::Waiting);
        assert_eq!(db.release_waiting_sends().unwrap(), 1);
        assert_eq!(
            db.send_entry_by_id(entry.id).unwrap().unwrap().status,
            SendStatus::Pending
        );
    }

    #[test]
    fn auto_off_cancels_only_waiting_auto_rows() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path().join("library.db")).unwrap();
        let a = seed_photo_at(&db, 300, "a.png");
        let b = seed_photo_at(&db, 300, "b.png");
        let c = seed_photo_at(&db, 300, "c.png");
        let waiting_a = db
            .enqueue_send(a, SendKind::Photo, SendOrigin::Auto, SendStatus::Waiting)
            .unwrap()
            .unwrap();
        let waiting_b = db
            .enqueue_send(b, SendKind::Photo, SendOrigin::Auto, SendStatus::Waiting)
            .unwrap()
            .unwrap();
        let pending_c = db
            .enqueue_send(c, SendKind::Photo, SendOrigin::Auto, SendStatus::Pending)
            .unwrap()
            .unwrap();
        assert_eq!(db.cancel_waiting_auto_sends().unwrap(), 2);
        assert_eq!(
            db.send_entry_by_id(waiting_a).unwrap().unwrap().status,
            SendStatus::Cancelled
        );
        assert_eq!(
            db.send_entry_by_id(waiting_b).unwrap().unwrap().status,
            SendStatus::Cancelled
        );
        assert_eq!(
            db.send_entry_by_id(pending_c).unwrap().unwrap().status,
            SendStatus::Pending
        );
    }

    fn sent_outcome() -> SendOutcome {
        SendOutcome::Sent {
            message_id: Some("42".into()),
            channel_id: Some("7".into()),
            guild_id: Some("9".into()),
        }
    }

    #[test]
    fn disabled_integration_sends_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let (db, photo) = discord_db(&dir);
        with_db(&db, |db| {
            let mut settings = AppSettings::load(db).unwrap();
            settings.discord_enabled = false;
            settings.save(db).unwrap();
            save_discord_webhook(db, "https://discord.com/api/webhooks/1/t").unwrap();
            db.enqueue_send(
                photo,
                SendKind::Photo,
                SendOrigin::Manual,
                SendStatus::Pending,
            )
            .unwrap();
        });
        let transport = FakeTransport::default();
        let events = EventBus::default();
        assert!(matches!(
            process_next_send(&db, &transport, &events, 100).unwrap(),
            ProcessOutcome::Idle
        ));
        assert!(transport.requests.lock().unwrap().is_empty());
    }

    #[test]
    fn success_stores_message_id_and_publishes_sent() {
        let dir = tempfile::tempdir().unwrap();
        let (db, photo) = discord_db(&dir);
        enable_discord(&db, "https://discord.com/api/webhooks/1/t");
        let id = enqueue_photo(&db, photo);
        let transport = FakeTransport::default();
        transport.outcomes.lock().unwrap().push_back(sent_outcome());
        let events = EventBus::default();
        let rx = events.subscribe();
        assert!(matches!(
            process_next_send(&db, &transport, &events, 100).unwrap(),
            ProcessOutcome::Sent
        ));
        let entry = with_db(&db, |db| db.send_entry_by_id(id).unwrap().unwrap());
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
    fn db_lock_is_released_during_upload() {
        let dir = tempfile::tempdir().unwrap();
        let (db, photo) = discord_db(&dir);
        enable_discord(&db, "https://discord.com/api/webhooks/1/t");
        enqueue_photo(&db, photo);
        let transport = FakeTransport {
            db: Some(db.clone()),
            ..FakeTransport::default()
        };
        transport.outcomes.lock().unwrap().push_back(sent_outcome());
        let events = EventBus::default();
        assert!(matches!(
            process_next_send(&db, &transport, &events, 100).unwrap(),
            ProcessOutcome::Sent
        ));
        assert!(
            transport.lock_was_free.load(Ordering::SeqCst),
            "the DB mutex was held during the upload"
        );
    }

    #[test]
    fn test_send_has_no_image() {
        let dir = tempfile::tempdir().unwrap();
        let (db, _photo) = discord_db(&dir);
        enable_discord(&db, "https://discord.com/api/webhooks/1/t");
        let id = with_db(&db, |db| db.enqueue_test_send().unwrap());
        let transport = FakeTransport::default();
        transport.outcomes.lock().unwrap().push_back(sent_outcome());
        let events = EventBus::default();
        assert!(matches!(
            process_next_send(&db, &transport, &events, 100).unwrap(),
            ProcessOutcome::Sent
        ));
        assert_eq!(
            with_db(&db, |db| db.send_entry_by_id(id).unwrap().unwrap().kind),
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
        let id = enqueue_photo(&db, photo);
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
            ProcessOutcome::RateLimited { .. }
        ));
        let entry = with_db(&db, |db| db.send_entry_by_id(id).unwrap().unwrap());
        assert_eq!(entry.status, SendStatus::Pending);
        assert_eq!(entry.attempts, 0);
        assert_eq!(entry.next_attempt_at, Some(102));
        assert_eq!(entry.error_code.as_deref(), Some("rate_limited"));
        assert!(matches!(
            process_next_send(&db, &transport, &events, 101).unwrap(),
            ProcessOutcome::Idle
        ));
        assert_eq!(transport.requests.lock().unwrap().len(), 1);
    }

    #[test]
    fn rate_limit_pauses_every_send_until_retry_after() {
        let dir = tempfile::tempdir().unwrap();
        let (db, photo) = discord_db(&dir);
        enable_discord(&db, "https://discord.com/api/webhooks/1/t");
        enqueue_photo(&db, photo);
        let transport = Arc::new(FakeTransport::default());
        transport
            .outcomes
            .lock()
            .unwrap()
            .push_back(SendOutcome::RateLimited {
                retry_after: Duration::from_millis(300),
            });
        transport.outcomes.lock().unwrap().push_back(sent_outcome());

        let running = Arc::new(AtomicBool::new(true));
        let sends = Arc::new(JobSignal::default());
        let events = EventBus::default();
        let worker_transport: Arc<dyn SendTransport> = transport.clone();
        let handle = thread::spawn({
            let db = db.clone();
            let running = running.clone();
            let sends = sends.clone();
            let events = events.clone();
            move || worker_loop(db, running, sends, events, worker_transport)
        });

        let deadline = Instant::now() + Duration::from_secs(3);
        while transport.requests.lock().unwrap().len() < 2 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        running.store(false, Ordering::Relaxed);
        sends.notify();
        handle.join().unwrap();

        let times = transport.attempts_at.lock().unwrap();
        assert_eq!(times.len(), 2, "worker did not issue the retry");
        let gap = times[1] - times[0];
        assert!(
            gap >= Duration::from_millis(150),
            "second send started after only {gap:?}; the 300 ms webhook pause was ignored"
        );
    }

    #[test]
    fn network_error_reschedules_with_attempts_and_backoff() {
        let dir = tempfile::tempdir().unwrap();
        let (db, photo) = discord_db(&dir);
        enable_discord(&db, "https://discord.com/api/webhooks/1/t");
        let id = enqueue_photo(&db, photo);
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
        let entry = with_db(&db, |db| db.send_entry_by_id(id).unwrap().unwrap());
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
        let id = enqueue_photo(&db, photo);
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
        let entry = with_db(&db, |db| db.send_entry_by_id(id).unwrap().unwrap());
        assert_eq!(entry.status, SendStatus::Failed);
        assert_eq!(entry.error_code.as_deref(), Some("http_401"));
        assert_eq!(entry.error_detail.as_deref(), Some("unauthorized"));
    }

    #[test]
    fn missing_photo_without_derivative_marks_failed() {
        let dir = tempfile::tempdir().unwrap();
        let (db, photo) = discord_db(&dir);
        enable_discord(&db, "https://discord.com/api/webhooks/1/t");
        let path = with_db(&db, |db| db.photo_by_id(photo).unwrap().unwrap().abs_path);
        std::fs::remove_file(path).unwrap();
        let id = enqueue_photo(&db, photo);
        let transport = FakeTransport::default();
        let events = EventBus::default();
        assert!(matches!(
            process_next_send(&db, &transport, &events, 100).unwrap(),
            ProcessOutcome::Failed
        ));
        let entry = with_db(&db, |db| db.send_entry_by_id(id).unwrap().unwrap());
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
        let derivative = with_db(&db, |db| {
            db.add_derivative(
                photo,
                display.to_str().unwrap(),
                "webp",
                16,
                10,
                std::fs::metadata(&display).unwrap().len(),
                "other",
                "replace",
            )
            .unwrap()
        });
        with_db(&db, |db| db.set_replaced(photo, derivative).unwrap());
        let id = enqueue_photo(&db, photo);
        let transport = FakeTransport::default();
        transport.outcomes.lock().unwrap().push_back(sent_outcome());
        let events = EventBus::default();
        assert!(matches!(
            process_next_send(&db, &transport, &events, 100).unwrap(),
            ProcessOutcome::Sent
        ));
        let entry = with_db(&db, |db| db.send_entry_by_id(id).unwrap().unwrap());
        assert_eq!(entry.source, Some(SendSource::Derivative));
        let requests = transport.requests.lock().unwrap();
        assert_eq!(requests[0].image.as_ref().unwrap().bytes, b"WEBPDATA");
    }

    #[test]
    fn recover_interrupted_sends_marks_processing_failed() {
        let dir = tempfile::tempdir().unwrap();
        let (db, photo) = discord_db(&dir);
        let id = enqueue_photo(&db, photo);
        with_db(&db, |db| db.claim_next_send(100).unwrap());
        assert_eq!(with_db(&db, |db| recover_interrupted_sends(db).unwrap()), 1);
        let entry = with_db(&db, |db| db.send_entry_by_id(id).unwrap().unwrap());
        assert_eq!(entry.status, SendStatus::Failed);
        assert_eq!(entry.error_code.as_deref(), Some("interrupted"));
    }

    #[test]
    fn missing_or_invalid_webhook_never_claims_a_send() {
        let dir = tempfile::tempdir().unwrap();
        let (db, photo) = discord_db(&dir);
        let id = with_db(&db, |db| {
            let mut settings = AppSettings::load(db).unwrap();
            settings.discord_enabled = true;
            settings.save(db).unwrap();
            db.enqueue_send(
                photo,
                SendKind::Photo,
                SendOrigin::Manual,
                SendStatus::Pending,
            )
            .unwrap()
            .unwrap()
        });
        let transport = FakeTransport::default();
        let events = EventBus::default();
        assert!(matches!(
            process_next_send(&db, &transport, &events, 100).unwrap(),
            ProcessOutcome::Idle
        ));
        with_db(&db, |db| {
            save_discord_webhook(db, "http://discord.com/api/webhooks/1/t").unwrap()
        });
        assert!(matches!(
            process_next_send(&db, &transport, &events, 101).unwrap(),
            ProcessOutcome::Idle
        ));
        assert!(transport.requests.lock().unwrap().is_empty());
        let entry = with_db(&db, |db| db.send_entry_by_id(id).unwrap().unwrap());
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
