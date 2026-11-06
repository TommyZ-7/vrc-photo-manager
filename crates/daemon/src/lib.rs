use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use image::DynamicImage;
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use vrc_photon_ai_protocol::TaggingJobStatus;
use vrc_photon_core::compressor;
use vrc_photon_core::db::Db;
use vrc_photon_core::hashing;
use vrc_photon_core::linker;
use vrc_photon_core::log_parser::LogIngestor;
use vrc_photon_core::model::{
    CompressionJob, CompressionProfile, DaemonCommand, DaemonOperation, DeletionReason,
    DeletionRequest, Derivative, Photo, PhotoStatus, QueueStatus,
};
use vrc_photon_core::photo_filename::is_source_photo;
use vrc_photon_core::scanner::{
    ScanFailure, ScanPhase as CoreScanPhase, ScanProgress, ScanReport, scan_and_index_shared,
    scan_one,
};
use vrc_photon_core::settings::{AppSettings, detect_vrchat_paths};
use vrc_photon_core::thumbnail;
use vrc_photon_ipc::{Endpoint, Event, JobStatus, Operation, ScanPhase as IpcScanPhase};

#[cfg(test)]
mod ai_e2e;
mod discord;
mod discord_http;
mod ipc_server;
pub(crate) mod plugin;
mod signals;
pub(crate) mod tagging;
mod tray;
mod vrchat_process;
mod world_name_http;
mod world_names;
use discord_http::UreqTransport;
use ipc_server::{EventBus, IpcControl, IpcServer, status_snapshot, tagging_status};
use plugin::{AI_PLUGIN_ID, PluginService};
use signals::{JobSignal, WakeSignal};
use tagging::{TaggingControl, TaggingWorker, TaggingWorkerConfig};
use tray::{Tray, TrayAction};
use vrc_photon_ai_host::{ModelHttpClient, UreqModelClient};
use world_name_http::UreqWorldNameTransport;

const PARSER_VERSION: &str = "1";
/// Number of skipped-line sentences kept in one log error detail.
const MAX_SKIPPED_DETAILS: usize = 5;
const SCAN_INTERVAL: Duration = Duration::from_millis(500);
const LOG_INTERVAL: Duration = Duration::from_millis(750);
/// Complete log lines one poll may return. The next poll continues from the
/// last committed offset.
const LOG_READ_MAX_LINES: usize = 5_000;
/// Bytes of log data one poll may consume. A line that crosses this budget is
/// left partial for the next poll instead of growing memory without bound.
const LOG_READ_MAX_BYTES: u64 = 8 * 1024 * 1024;
/// Complete lines applied per database transaction. This bounds how long log
/// ingestion holds the shared database mutex.
const LOG_INGEST_CHUNK_LINES: usize = 500;
/// Consecutive log-poll misses required before a missing VRChat process closes
/// the active session. This tolerates one-off process-table read failures and
/// short restart gaps.
const VRCHAT_MISSING_POLLS_BEFORE_CLOSE: u8 = 3;
/// How often the daemon re-reads the compression settings and thumbnail
/// cache limit the UI saved. Runs on every loop iteration, paused or not,
/// so a change is applied within roughly one interval plus one loop wait.
const SETTINGS_INTERVAL: Duration = Duration::from_millis(500);
const THUMBNAIL_PRUNE_BATCH: usize = 128;
const THUMBNAIL_PRUNE_IDLE: Duration = Duration::from_millis(500);

#[derive(Debug, Clone, Default)]
pub struct DaemonStatus {
    pub paused: bool,
    pub watching: bool,
    pub world_name: Option<String>,
    pub players: usize,
    pub last_log: Option<String>,
    pub photo_count: i64,
    pub pending_jobs: i64,
    pub pending_sends: i64,
    /// `waiting` + `pending` + `processing` AI tagging jobs.
    pub pending_tagging_jobs: i64,
    /// Whether an install, ready model, and saved probe are all present.
    pub tagging_ready: bool,
    pub scanning: bool,
    pub scan_phase: Option<vrc_photon_ipc::ScanPhase>,
    pub scan_processed: usize,
    pub scan_total: Option<usize>,
    pub scan_indexed: usize,
    pub scan_root: usize,
    pub scan_roots: usize,
    /// Number of snapshots taken so far. Each snapshot stamps and bumps
    /// this under the status lock, so a snapshot's version orders it
    /// against every other snapshot of this status.
    pub version: u64,
}

/// A user-facing daemon error summary that can be rendered in any locale.
#[derive(Debug, Clone)]
struct LocalizedError {
    key: &'static str,
    args: Vec<(&'static str, String)>,
}

impl LocalizedError {
    fn new(key: &'static str) -> Self {
        Self {
            key,
            args: Vec::new(),
        }
    }

    fn arg(mut self, name: &'static str, value: impl ToString) -> Self {
        self.args.push((name, value.to_string()));
        self
    }

    fn message(&self) -> String {
        self.message_in(&vrc_photon_i18n::current_locale())
    }

    fn message_in(&self, locale: &str) -> String {
        vrc_photon_i18n::tr_args_in(locale, self.key, &self.args)
    }

    fn args_json(&self) -> String {
        let args: std::collections::BTreeMap<&str, &str> = self
            .args
            .iter()
            .map(|(name, value)| (*name, value.as_str()))
            .collect();
        serde_json::to_string(&args).unwrap_or_else(|_| "{}".to_string())
    }

    fn signature(&self, detail: Option<&str>) -> String {
        error_signature(&format!("{}|{}", self.key, self.args_json()), detail)
    }
}

#[derive(Debug)]
struct LogTail {
    path: PathBuf,
    ingestor: LogIngestor,
    offset: i64,
    failed_offset: Option<i64>,
    failed_attempts: u8,
}

/// Remembers a failed line at `offset`. The caller retries the line until it
/// has failed three times, then this returns `true` so the poison line can be
/// skipped.
fn record_line_failure(
    failed_offset: &mut Option<i64>,
    failed_attempts: &mut u8,
    offset: i64,
) -> bool {
    if *failed_offset != Some(offset) {
        *failed_offset = Some(offset);
        *failed_attempts = 1;
        return false;
    }
    *failed_attempts = failed_attempts.saturating_add(1);
    if *failed_attempts >= 3 {
        *failed_offset = None;
        *failed_attempts = 0;
        return true;
    }
    false
}

#[derive(Debug, Default)]
struct LogPass {
    processed: usize,
    skipped: usize,
    failure: Option<String>,
    /// One sentence per skipped poison line, capped at [`MAX_SKIPPED_DETAILS`].
    skipped_details: Vec<String>,
}

impl LogPass {
    /// Records one poison line that was skipped, including its path and byte
    /// range so the reported error changes even when the parse error repeats.
    fn record_skip(&mut self, path: &Path, start: i64, end: i64) {
        self.skipped += 1;
        if self.skipped_details.len() < MAX_SKIPPED_DETAILS {
            self.skipped_details.push(format!(
                "{}: オフセット {}〜{} の行は3回失敗したためスキップしました",
                path.display(),
                start,
                end
            ));
        }
    }

    /// The original error plus the skipped-line sentences. Skips beyond the
    /// cap are summarized so one pass cannot grow the detail without bound.
    fn failure_detail(&self) -> Option<String> {
        let mut parts = Vec::new();
        if let Some(failure) = &self.failure {
            parts.push(failure.clone());
        }
        parts.extend(self.skipped_details.iter().cloned());
        let omitted = self.skipped.saturating_sub(self.skipped_details.len());
        if omitted > 0 {
            parts.push(format!("ほか {omitted} 行を同じ理由でスキップしました"));
        }
        if parts.is_empty() {
            None
        } else {
            Some(parts.join("\n"))
        }
    }

    /// A pass can only clear a previous error when it processed real log data
    /// without skipping or failing on any line.
    fn is_clean(&self) -> bool {
        self.processed > 0 && self.skipped == 0 && self.failure.is_none()
    }
}

pub struct Daemon {
    db: Arc<Mutex<Db>>,
    settings: AppSettings,
    status: Arc<Mutex<DaemonStatus>>,
    running: Arc<AtomicBool>,
    wake: Arc<WakeSignal>,
    jobs: Arc<JobSignal>,
    sends: Arc<JobSignal>,
    /// Wakes the world name worker when lookups are queued.
    world_names: Arc<JobSignal>,
    /// Live copy of `settings.world_name_resolution_enabled` for the worker.
    world_name_resolution_enabled: Arc<AtomicBool>,
    events: EventBus,
    ipc_endpoint: Option<Endpoint>,
    ipc: Option<IpcServer>,
    watchers: Vec<RecommendedWatcher>,
    watcher_rx: Option<Receiver<notify::Result<notify::Event>>>,
    /// Roots the current watchers were set up for, to skip pointless
    /// rebuilds that would drop buffered events.
    watcher_roots: Vec<PathBuf>,
    /// The watcher-start failure currently kept visible, if any. It is
    /// recorded under the `Scan` operation but must survive clean scans:
    /// scanning works while watching does not.
    watcher_error: Option<(LocalizedError, Option<String>)>,
    /// New photo ids scanned while no thumbnail worker exists (notably
    /// `--once`); the caller generates their thumbnails directly.
    deferred_thumbnail_ids: Vec<i64>,
    log_tail: Option<LogTail>,
    vrchat_missing_polls: u8,
    vrchat_session_id: Option<i64>,
    workers: Vec<JoinHandle<()>>,
    dirty_paths: Vec<PathBuf>,
    last_log_poll: std::time::Instant,
    tray: Option<Tray>,
    tray_enabled: bool,
    thumbnail_tx: Option<mpsc::Sender<i64>>,
    thumbnail_handle: Option<JoinHandle<()>>,
    hash_handle: Option<JoinHandle<()>>,
    send_handle: Option<JoinHandle<()>>,
    world_name_handle: Option<JoinHandle<()>>,
    owns_deletion_executor: bool,
    deletion_handle: Option<JoinHandle<()>>,
    last_autostart_check: std::time::Instant,
    last_settings_check: std::time::Instant,
    last_heartbeat: std::time::Instant,
    last_asset_check: Instant,
    thumbnail_dir: PathBuf,
    thumbnail_cache_limit: Arc<AtomicU64>,
    /// Packed quality and long edge for the thumbnail worker.
    thumbnail_spec: Arc<AtomicU32>,
    /// Bumped when the thumbnail cache is cleared so the worker resets its
    /// byte accounting and rescans on the next pass.
    thumbnail_cache_epoch: Arc<AtomicU64>,
    /// Bytes of thumbnails the UI generated itself, as reported over IPC.
    thumbnail_external_bytes: Arc<AtomicU64>,
    last_errors: HashMap<DaemonOperation, String>,
    /// AI plugin installs, model files, probe cache, and the lazy sidecar.
    plugin: PluginService,
    /// Wakes the AI tagging worker when jobs arrive or settings change.
    tagging: Arc<JobSignal>,
    /// In-flight tagging cancel tokens shared with IPC.
    tagging_control: Arc<TaggingControl>,
    tagging_handle: Option<JoinHandle<()>>,
    /// Root for AI plugin installs, model files, snapshots, and logs.
    ai_data_dir: PathBuf,
    #[cfg(test)]
    ai_plugin_keys: Vec<String>,
    #[cfg(test)]
    ai_http: Arc<dyn ModelHttpClient>,
}

impl Daemon {
    /// Opens the daemon as the deletion executor. The caller must own the
    /// single-instance lock so no other process can still be executing a
    /// `running` deletion.
    pub fn open(
        db_path: impl AsRef<Path>,
        photo_dir: Option<PathBuf>,
        log_dir: Option<PathBuf>,
    ) -> Result<Self> {
        Self::open_inner(db_path, photo_dir, log_dir, true)
    }

    /// Opens the daemon without deletion-executor ownership: `running`
    /// deletions are not reset and [`Self::process_deletions`] is a no-op.
    /// Maintenance commands use this when they do not own the single-instance
    /// lock, because the live daemon may still be executing those requests.
    pub fn open_preserving_running_deletions(
        db_path: impl AsRef<Path>,
        photo_dir: Option<PathBuf>,
        log_dir: Option<PathBuf>,
    ) -> Result<Self> {
        Self::open_inner(db_path, photo_dir, log_dir, false)
    }

    fn open_inner(
        db_path: impl AsRef<Path>,
        photo_dir: Option<PathBuf>,
        log_dir: Option<PathBuf>,
        owns_deletion_executor: bool,
    ) -> Result<Self> {
        let db_path = db_path.as_ref().to_path_buf();
        let db = Db::open(&db_path).context("cannot open library database")?;
        if owns_deletion_executor {
            db.reset_running_deletions()
                .context("cannot recover interrupted deletions")?;
            let resumed = db
                .resume_interrupted_tagging_jobs()
                .context("cannot recover interrupted tagging jobs")?;
            if resumed > 0 {
                tracing::info!(jobs = resumed, "resumed interrupted tagging jobs");
            }
            let resumed_compression = db
                .resume_interrupted_compression_jobs()
                .context("cannot recover interrupted compression jobs")?;
            if resumed_compression > 0 {
                tracing::info!(
                    jobs = resumed_compression,
                    "resumed interrupted compression jobs"
                );
            }
        } else {
            tracing::debug!(
                "preserving running deletions and tagging jobs because this process does not own the executor lock"
            );
        }
        let mut settings = AppSettings::load(&db).context("cannot load settings")?;
        settings.language = vrc_photon_i18n::set_locale(&settings.language);
        if let Some(path) = photo_dir {
            settings.photo_roots = vec![path.to_string_lossy().to_string()];
        }
        if settings.photo_roots.is_empty() {
            let detected = detect_vrchat_paths();
            settings.photo_roots = detected
                .photo_roots
                .iter()
                .map(|path| path.to_string_lossy().to_string())
                .collect();
            if settings.log_dir.is_none() {
                settings.log_dir = detected
                    .log_dir
                    .map(|path| path.to_string_lossy().to_string());
            }
        }
        if let Some(path) = log_dir {
            settings.log_dir = Some(path.to_string_lossy().to_string());
        }
        settings.save(&db).context("cannot save settings")?;
        let tray_enabled = settings.tray;
        let world_name_resolution_enabled =
            Arc::new(AtomicBool::new(settings.world_name_resolution_enabled));
        let thumbnail_dir = vrc_photon_core::thumbnail::thumbnails_dir()
            .unwrap_or_else(|| PathBuf::from("thumbnails"));
        let thumbnail_cache_limit = Arc::new(AtomicU64::new(thumbnail_cache_limit_bytes(
            settings.thumbnail_cache_mb,
        )));
        let thumbnail_spec = Arc::new(AtomicU32::new(pack_thumbnail_spec(
            settings.thumbnail_spec(),
        )));
        let errors = db.daemon_errors().context("cannot load daemon errors")?;
        let watcher_error = errors
            .iter()
            .find(|error| {
                error.operation == DaemonOperation::Scan
                    && error.message_key.as_deref() == Some("daemon.watcher_failed")
            })
            .map(|error| {
                (
                    LocalizedError::new("daemon.watcher_failed"),
                    error.detail.clone(),
                )
            });
        let last_errors: std::collections::HashMap<DaemonOperation, String> = errors
            .into_iter()
            .map(|error| {
                let signature_source = match error.message_key.as_deref() {
                    Some(key) => {
                        format!("{}|{}", key, error.message_args.as_deref().unwrap_or("{}"))
                    }
                    None => error.message.clone(),
                };
                (
                    error.operation,
                    error_signature(&signature_source, error.detail.as_deref()),
                )
            })
            .collect();
        let ipc_endpoint =
            vrc_photon_core::data_dir().and_then(|dir| match Endpoint::for_data_dir(&dir) {
                Ok(endpoint) => Some(endpoint),
                Err(error) => {
                    tracing::warn!(%error, "cannot resolve IPC endpoint");
                    None
                }
            });

        let db = Arc::new(Mutex::new(db));
        let events = EventBus::default();
        let ai_data_dir = vrc_photon_core::data_dir().unwrap_or_else(|| PathBuf::from("."));
        let ai_plugin_keys = vrc_photon_ai_host::effective_plugin_keys();
        let ai_http: Arc<dyn ModelHttpClient> = Arc::new(UreqModelClient::new());
        let plugin = PluginService::new(
            db.clone(),
            ai_data_dir.clone(),
            events.clone(),
            ai_plugin_keys.clone(),
            ai_http.clone(),
        );
        if owns_deletion_executor && let Err(error) = plugin.recover_interrupted_model_state() {
            tracing::warn!(%error, "cannot recover interrupted AI model state");
        }
        let tagging = Arc::new(JobSignal::default());
        let tagging_control = Arc::new(TaggingControl::new(tagging.clone()));

        Ok(Self {
            db,
            settings,
            status: Arc::new(Mutex::new(DaemonStatus::default())),
            running: Arc::new(AtomicBool::new(true)),
            wake: Arc::new(WakeSignal::default()),
            jobs: Arc::new(JobSignal::default()),
            sends: Arc::new(JobSignal::default()),
            world_names: Arc::new(JobSignal::default()),
            world_name_resolution_enabled,
            events,
            ipc_endpoint,
            ipc: None,
            watchers: Vec::new(),
            watcher_rx: None,
            watcher_roots: Vec::new(),
            watcher_error,
            deferred_thumbnail_ids: Vec::new(),
            log_tail: None,
            vrchat_missing_polls: 0,
            vrchat_session_id: None,
            workers: Vec::new(),
            dirty_paths: Vec::new(),
            last_log_poll: std::time::Instant::now() - LOG_INTERVAL,
            tray: None,
            tray_enabled,
            thumbnail_tx: None,
            thumbnail_handle: None,
            hash_handle: None,
            send_handle: None,
            world_name_handle: None,
            owns_deletion_executor,
            deletion_handle: None,
            last_autostart_check: std::time::Instant::now(),
            last_settings_check: Instant::now() - Duration::from_secs(2),
            last_heartbeat: std::time::Instant::now() - Duration::from_secs(10),
            last_asset_check: Instant::now() - Duration::from_secs(30),
            thumbnail_dir,
            thumbnail_cache_limit,
            thumbnail_spec,
            thumbnail_cache_epoch: Arc::new(AtomicU64::new(0)),
            thumbnail_external_bytes: Arc::new(AtomicU64::new(0)),
            last_errors,
            plugin,
            tagging,
            tagging_control,
            tagging_handle: None,
            ai_data_dir,
            #[cfg(test)]
            ai_plugin_keys,
            #[cfg(test)]
            ai_http,
        })
    }

    pub fn status(&self) -> Arc<Mutex<DaemonStatus>> {
        self.status.clone()
    }

    pub fn set_thumbnail_dir(&mut self, path: PathBuf) {
        self.thumbnail_dir = path;
    }

    pub fn set_tray_enabled(&mut self, enabled: bool) {
        self.tray_enabled = enabled;
        if !enabled {
            self.tray = None;
        }
    }

    #[cfg(test)]
    pub(crate) fn set_ai_data_dir(&mut self, path: PathBuf) {
        self.ai_data_dir = path;
        self.rebuild_plugin_service();
    }

    #[cfg(test)]
    pub(crate) fn set_ai_plugin_keys(&mut self, keys: Vec<String>) {
        self.ai_plugin_keys = keys;
        self.rebuild_plugin_service();
    }

    #[cfg(test)]
    pub(crate) fn set_ai_http_client(&mut self, client: Arc<dyn ModelHttpClient>) {
        self.ai_http = client;
        self.rebuild_plugin_service();
    }

    #[cfg(test)]
    fn rebuild_plugin_service(&mut self) {
        self.plugin = PluginService::new(
            self.db.clone(),
            self.ai_data_dir.clone(),
            self.events.clone(),
            self.ai_plugin_keys.clone(),
            self.ai_http.clone(),
        );
    }

    #[cfg(test)]
    pub(crate) fn plugin_service(&self) -> &PluginService {
        &self.plugin
    }

    /// Overrides the resolved IPC endpoint. Tests disable it with `None`.
    pub fn set_ipc_endpoint(&mut self, endpoint: Option<Endpoint>) {
        self.ipc_endpoint = endpoint;
    }

    pub fn set_paused(&self, paused: bool) {
        let changed = match self.status.lock() {
            Ok(mut status) => {
                if status.paused == paused {
                    false
                } else {
                    status.paused = paused;
                    true
                }
            }
            Err(_) => false,
        };
        if changed {
            self.wake.notify();
            self.publish_status();
        }
    }

    fn publish_status(&self) {
        let status = status_snapshot(&mut self.status.lock().unwrap());
        self.events.publish(Event::StatusChanged { status });
    }

    pub fn settings(&self) -> &AppSettings {
        &self.settings
    }

    /// Persists a technical daemon failure without a catalog key. Kept for
    /// errors whose detail text has no user-facing translation.
    #[allow(dead_code)]
    fn report_daemon_error(
        &mut self,
        operation: DaemonOperation,
        message: &str,
        detail: Option<&str>,
    ) -> Result<()> {
        self.report_daemon_error_fields(operation, message, None, None, detail, message)
    }

    /// Records a user-facing failure with a catalog key and JSON args.
    fn report_localized_error(
        &mut self,
        operation: DaemonOperation,
        error: LocalizedError,
        detail: Option<&str>,
    ) -> Result<()> {
        let args_json = error.args_json();
        let signature = format!("{}|{}", error.key, args_json);
        self.report_daemon_error_fields(
            operation,
            &error.message(),
            Some(error.key),
            Some(&args_json),
            detail,
            &signature,
        )
    }

    fn report_daemon_error_fields(
        &mut self,
        operation: DaemonOperation,
        message: &str,
        message_key: Option<&str>,
        message_args: Option<&str>,
        detail: Option<&str>,
        signature_source: &str,
    ) -> Result<()> {
        let signature = error_signature(signature_source, detail);
        if self.last_errors.get(&operation) == Some(&signature) {
            return Ok(());
        }
        {
            let db = self.db.lock().unwrap();
            db.record_daemon_error_i18n(operation, message, message_key, message_args, detail)?;
        }
        self.last_errors.insert(operation, signature);
        self.events.publish(Event::DaemonErrorChanged {
            operation: ipc_operation(operation),
        });
        Ok(())
    }

    /// Removes a previously reported failure once the operation recovers.
    fn clear_daemon_error(&mut self, operation: DaemonOperation) -> Result<()> {
        if !self.last_errors.contains_key(&operation) {
            return Ok(());
        }
        {
            let db = self.db.lock().unwrap();
            db.clear_daemon_error(operation)?;
        }
        self.last_errors.remove(&operation);
        self.events.publish(Event::DaemonErrorChanged {
            operation: ipc_operation(operation),
        });
        Ok(())
    }

    /// Consumes queued UI requests once. See
    /// [`Self::poll_commands_with_initial`].
    fn poll_commands(&mut self) -> Result<()> {
        self.poll_commands_with_initial(None)
    }

    /// Consumes queued UI requests once: reloads settings, rebuilds the photo
    /// watchers for the new roots, runs each command in request order, then
    /// refreshes the status view. Commands are consumed even when they fail so
    /// a broken request cannot loop forever; the failure is recorded under its
    /// operation instead.
    ///
    /// When `initial_scan` is `Some(true)`, targeted `ScanRoot` commands run
    /// before full rescans. An import into an empty library must be indexed as
    /// new work before the startup scan can classify it as backlog.
    fn poll_commands_with_initial(&mut self, initial_scan: Option<bool>) -> Result<()> {
        let mut commands = {
            let db = self.db.lock().unwrap();
            db.take_daemon_commands()?
        };
        if commands.is_empty() {
            return Ok(());
        }
        self.reload_settings()?;
        self.restart_watchers();
        if initial_scan == Some(true) {
            commands.sort_by_key(|command| !matches!(command, DaemonCommand::ScanRoot { .. }));
        }
        for command in commands {
            let kind = command.as_str();
            if let Err(error) = self.run_command_with_initial(&command, initial_scan) {
                tracing::error!(command = kind, %error, "daemon command failed");
                let (operation, message) = match &command {
                    DaemonCommand::Rescan => (
                        DaemonOperation::Scan,
                        LocalizedError::new("daemon.rescan_failed"),
                    ),
                    DaemonCommand::Relog => (
                        DaemonOperation::Log,
                        LocalizedError::new("daemon.log_reimport_failed"),
                    ),
                    DaemonCommand::Relink => (
                        DaemonOperation::Link,
                        LocalizedError::new("daemon.relink_failed"),
                    ),
                    DaemonCommand::ScanRoot { .. } => (
                        DaemonOperation::Scan,
                        LocalizedError::new("daemon.rescan_failed"),
                    ),
                };
                if let Err(report_error) =
                    self.report_localized_error(operation, message, Some(&error.to_string()))
                {
                    tracing::error!(%report_error, "cannot record daemon command error");
                }
            }
        }
        self.refresh_status()
    }

    /// Replaces the in-memory settings with the current database values so a
    /// corrected path or switch takes effect without restarting the daemon.
    fn reload_settings(&mut self) -> Result<()> {
        let mut settings = {
            let db = self.db.lock().unwrap();
            AppSettings::load(&db)?
        };
        settings.language = vrc_photon_i18n::normalize(&settings.language);
        let language_changed = settings.language != self.settings.language;
        if settings.autostart != self.settings.autostart {
            self.apply_autostart(settings.autostart);
        }
        self.thumbnail_cache_limit.store(
            thumbnail_cache_limit_bytes(settings.thumbnail_cache_mb),
            Ordering::Relaxed,
        );
        self.thumbnail_spec.store(
            pack_thumbnail_spec(settings.thumbnail_spec()),
            Ordering::Relaxed,
        );
        let world_name_enabled = settings.world_name_resolution_enabled;
        self.settings = settings;
        let was_world_name_enabled = self
            .world_name_resolution_enabled
            .swap(world_name_enabled, Ordering::Relaxed);
        if world_name_enabled && !was_world_name_enabled {
            self.enqueue_world_name_backfill();
            self.world_names.notify();
        }
        if language_changed {
            vrc_photon_i18n::set_locale(&self.settings.language);
            if let Some(tray) = self.tray.as_mut() {
                tray.set_language();
            }
        }
        Ok(())
    }

    /// Stops the current watchers and sets up new ones for the reloaded roots.
    /// A failure leaves them stopped so the next command or daemon start can
    /// retry the setup.
    ///
    /// Buffered events are drained first, and when the existing roots are
    /// already watched the channel (and its buffer) is kept as is: a
    /// command batch must not lose photo events that arrived while it ran.
    fn rebuild_watchers(&mut self) -> Result<()> {
        self.poll_watchers();
        let wanted: Vec<PathBuf> = self
            .settings
            .photo_roots()
            .into_iter()
            .filter(|root| root.is_dir())
            .collect();
        if self.watcher_rx.is_some() && self.watcher_roots == wanted {
            return Ok(());
        }
        self.watchers.clear();
        self.watcher_rx = None;
        self.watcher_roots.clear();
        let result = self.setup_watchers();
        if result.is_err() {
            self.watchers.clear();
            self.watcher_roots.clear();
        }
        result
    }

    /// Rebuilds the watchers, recording a `Scan` failure instead of aborting
    /// the daemon when the operating system refuses a watch. The failure is
    /// tracked separately from scan failures so a clean scan cannot erase
    /// it; it clears when the watchers start successfully.
    fn restart_watchers(&mut self) {
        match self.rebuild_watchers() {
            Ok(()) => {
                if let Some((localized, detail)) = self.watcher_error.take() {
                    // Clear only when the Scan slot still shows the watcher
                    // failure; a scan failure recorded since supersedes it.
                    if self.last_errors.get(&DaemonOperation::Scan)
                        == Some(&localized.signature(detail.as_deref()))
                        && let Err(error) = self.clear_daemon_error(DaemonOperation::Scan)
                    {
                        tracing::error!(%error, "cannot clear watcher error");
                    }
                }
            }
            Err(error) => {
                tracing::error!(%error, "cannot start photo watchers");
                let detail = error.to_string();
                let localized = LocalizedError::new("daemon.watcher_failed");
                self.watcher_error = Some((localized.clone(), Some(detail.clone())));
                if let Err(report_error) =
                    self.report_localized_error(DaemonOperation::Scan, localized, Some(&detail))
                {
                    tracing::error!(%report_error, "cannot record watcher error");
                }
            }
        }
    }

    /// Runs one queued command with an optional startup initial-scan flag.
    /// `ScanRoot` always indexes a targeted root and therefore never treats
    /// its photos as backlog.
    fn run_command_with_initial(
        &mut self,
        command: &DaemonCommand,
        initial_scan: Option<bool>,
    ) -> Result<()> {
        match command {
            DaemonCommand::Rescan => {
                self.make_unlinked_photos_due()?;
                self.scan_photos_with_initial(initial_scan)?;
                self.link_unlinked()?;
            }
            DaemonCommand::Relog => {
                self.make_unlinked_photos_due()?;
                self.ingest_logs()?;
                self.link_unlinked()?;
            }
            DaemonCommand::Relink => self.link_unlinked()?,
            DaemonCommand::ScanRoot { path } => {
                self.scan_root_photos(PathBuf::from(path))?;
                self.link_unlinked()?;
            }
        }
        Ok(())
    }

    pub fn run_once(&mut self) -> Result<()> {
        self.repair_open_sessions()?;
        let initial_scan = {
            let db = self.db.lock().unwrap();
            db.count_photos()? == 0
        };
        self.poll_commands_with_initial(Some(initial_scan))?;
        self.write_heartbeat();
        let report = self.scan_photos_with_initial(Some(initial_scan))?;
        // Photos indexed by the queued commands above were scanned while
        // no thumbnail worker exists in `--once` mode, so their ids were
        // deferred; generate for the union of both batches.
        let mut thumbnail_ids = std::mem::take(&mut self.deferred_thumbnail_ids);
        for id in &report.new_ids {
            if !thumbnail_ids.contains(id) {
                thumbnail_ids.push(*id);
            }
        }
        for id in &thumbnail_ids {
            let path = {
                let db = self.db.lock().unwrap();
                match db.photo_by_id(*id) {
                    Ok(photo) => photo.map(|photo| photo.effective_path().to_string()),
                    Err(error) => {
                        tracing::warn!(
                            %error,
                            photo_id = *id,
                            "cannot load photo for thumbnail"
                        );
                        None
                    }
                }
            };
            if let Some(path) = path {
                match thumbnail::ensure_thumbnail_in(
                    *id,
                    Path::new(&path),
                    &self.thumbnail_dir,
                    self.settings.thumbnail_spec(),
                ) {
                    Ok(Some(_)) => {}
                    Ok(None) => tracing::debug!(
                        photo_id = *id,
                        path = %path,
                        "thumbnail source is missing"
                    ),
                    Err(error) => tracing::warn!(
                        %error,
                        photo_id = *id,
                        path = %path,
                        "cannot generate thumbnail"
                    ),
                }
            }
        }
        let _ = prune_thumbnail_cache(
            &self.thumbnail_dir,
            self.thumbnail_cache_limit.load(Ordering::Relaxed),
        );
        // The scan above ran before this first log pass; clear the backoff it
        // recorded so the sessions just ingested can link those photos.
        self.make_unlinked_photos_due()?;
        self.ingest_logs()?;
        self.poll_vrchat_presence()?;
        self.link_unlinked()?;
        self.refresh_status()?;
        self.process_deletions()?;
        Ok(())
    }

    pub fn compress_pending(&mut self) -> Result<()> {
        loop {
            let jobs = {
                let db = self.db.lock().unwrap();
                db.next_pending_photo_jobs()?
            };
            if jobs.is_empty() {
                break;
            }
            process_photo_batch(&self.db, &jobs, Some(&self.thumbnail_dir));
        }
        self.process_deletions()?;
        Ok(())
    }

    /// Moves the AI backlog from `waiting` to `pending` and wakes the worker.
    /// The settings UI performs the same database update directly; this
    /// method exists for in-process callers and tests.
    pub fn release_ai_backlog(&mut self) -> Result<usize> {
        let released = {
            let db = self.db.lock().unwrap();
            db.release_waiting_tagging_jobs(AI_PLUGIN_ID)?
        };
        self.tagging.notify();
        Ok(released)
    }

    /// Executes every deletion that has no remaining job blocker. The
    /// background thread calls the same routine on a timer. A process that
    /// does not own the instance lock must leave deletion execution to the
    /// owner so an interrupted maintenance run cannot strand a `running` row.
    pub fn process_deletions(&mut self) -> Result<()> {
        if !self.owns_deletion_executor {
            return Ok(());
        }
        run_deletions_once(&self.db)?;
        Ok(())
    }

    /// Prepares the initial scan: load roots, run queued import scans before
    /// the first full scan, then scan the library. The empty-library decision
    /// is taken before commands run so imported photos are new work while
    /// pre-existing photos stay backlog.
    fn startup_scan(&mut self) -> Result<()> {
        self.ensure_roots()?;
        if self.stopping() {
            return Ok(());
        }
        let initial_scan = {
            let db = self.db.lock().unwrap();
            db.count_photos()? == 0
        };
        self.poll_commands_with_initial(Some(initial_scan))?;
        if self.stopping() {
            return Ok(());
        }
        self.scan_photos_with_initial(Some(initial_scan))?;
        Ok(())
    }

    pub fn run(&mut self) -> Result<()> {
        let _ = prune_thumbnail_cache(
            &self.thumbnail_dir,
            self.thumbnail_cache_limit.load(Ordering::Relaxed),
        );
        self.repair_open_sessions()?;
        self.start_ipc();
        if self.stopping() {
            self.shutdown();
            return Ok(());
        }
        self.record_supervisor();
        // Start workers before the initial scan so a long scan does not delay
        // already queued compression, tagging, or deletion work. The scan now
        // takes the database mutex only for short per-photo operations.
        self.start_workers();
        self.startup_scan()?;
        if self.stopping() {
            self.shutdown();
            return Ok(());
        }
        // The startup scan runs before logs are ingested, so its link pass can
        // record a retry before the sessions needed to link are available.
        // Clear that schedule once at startup; the periodic loop keeps the
        // backoff and only retries due photos.
        self.make_unlinked_photos_due()?;
        self.ingest_logs()?;
        self.poll_vrchat_presence()?;
        self.link_unlinked()?;
        if self.stopping() {
            self.shutdown();
            return Ok(());
        }
        self.write_heartbeat();
        self.restart_watchers();
        self.apply_autostart(self.settings.autostart);
        self.ensure_tray();

        tracing::info!(
            photos = self.settings.photo_roots.len(),
            log_dir = ?self.settings.log_dir,
            ai_data_dir = %self.ai_data_dir.display(),
            "daemon running"
        );

        while self.running.load(Ordering::Relaxed) {
            if let Err(error) = self.poll_commands() {
                tracing::error!(%error, "cannot handle daemon commands");
            }
            self.refresh_periodic_settings();
            if self
                .status
                .lock()
                .map(|status| status.paused)
                .unwrap_or(false)
            {
                self.wake.wait(SCAN_INTERVAL);
                continue;
            }
            self.poll_watchers();
            if !self.dirty_paths.is_empty()
                && let Err(error) = self.index_dirty_paths()
            {
                tracing::error!(%error, "incremental photo scan failed");
                if let Err(report_error) = self.report_localized_error(
                    DaemonOperation::Scan,
                    LocalizedError::new("daemon.diff_scan_failed"),
                    Some(&error.to_string()),
                ) {
                    tracing::error!(%report_error, "cannot record scan error");
                }
            }
            if self.last_log_poll.elapsed() >= LOG_INTERVAL {
                if let Err(error) = self.ingest_logs() {
                    tracing::error!(%error, "log ingestion failed");
                }
                if let Err(error) = self.poll_vrchat_presence() {
                    tracing::error!(%error, "VRChat process check failed");
                }
                self.last_log_poll = std::time::Instant::now();
            }
            if let Err(error) = self.refresh_status() {
                tracing::warn!(%error, "cannot refresh daemon status");
            }
            if self.last_asset_check.elapsed() >= Duration::from_secs(30) {
                if let Err(error) = self.verify_replaced_assets() {
                    tracing::warn!(%error, "cannot verify replaced photo assets");
                }
                self.last_asset_check = Instant::now();
            }
            if self.last_heartbeat.elapsed() >= Duration::from_secs(10) {
                self.write_heartbeat();
            }
            self.poll_tray();
            self.wake.wait(SCAN_INTERVAL);
        }
        self.shutdown();
        Ok(())
    }

    fn stopping(&self) -> bool {
        !self.running.load(Ordering::Relaxed)
    }

    fn start_ipc(&mut self) {
        if self.ipc.is_some() {
            return;
        }
        let Some(endpoint) = self.ipc_endpoint.clone() else {
            return;
        };
        let control = IpcControl {
            running: self.running.clone(),
            status: self.status.clone(),
            wake: self.wake.clone(),
            jobs: self.jobs.clone(),
            sends: self.sends.clone(),
            events: self.events.clone(),
            thumbnail_dir: self.thumbnail_dir.clone(),
            thumbnail_cache_epoch: self.thumbnail_cache_epoch.clone(),
            thumbnail_external_bytes: self.thumbnail_external_bytes.clone(),
            plugin: self.plugin.clone(),
            tagging: self.tagging.clone(),
            tagging_control: self.tagging_control.clone(),
            db: self.db.clone(),
        };
        match IpcServer::start(endpoint, control) {
            Ok(server) => self.ipc = Some(server),
            Err(error) => {
                tracing::warn!(%error, "cannot start IPC server; continuing without IPC")
            }
        }
    }

    fn ensure_roots(&mut self) -> Result<()> {
        if self.settings.photo_roots.is_empty() {
            let detected = detect_vrchat_paths();
            self.settings.photo_roots = detected
                .photo_roots
                .iter()
                .map(|path| path.to_string_lossy().to_string())
                .collect();
            if self.settings.log_dir.is_none() {
                self.settings.log_dir = detected
                    .log_dir
                    .map(|path| path.to_string_lossy().to_string());
            }
            if self.settings.photo_roots.is_empty() {
                tracing::warn!(
                    "no VRChat photo folder detected; set VRC_PHOTON_DIR or configure settings"
                );
            } else {
                let db = self.db.lock().unwrap();
                self.settings.save(&db)?;
            }
        }
        Ok(())
    }

    fn index_dirty_paths(&mut self) -> Result<()> {
        let mut paths = std::mem::take(&mut self.dirty_paths);
        if paths.is_empty() {
            return Ok(());
        }
        self.refresh_compression_settings();
        let tagging_ready = self.plugin.is_ready(AI_PLUGIN_ID);
        paths.sort_by_cached_key(|path| !path.is_file());
        paths.dedup();
        let roots = self.settings.photo_roots();
        let mut new_ids = Vec::new();
        let mut thumbnail_ids = Vec::new();
        let mut missing_ids = Vec::new();
        let mut changed = false;
        for path in paths {
            let Some(root) = roots.iter().find(|root| path.starts_with(root)) else {
                continue;
            };
            let root_id = {
                let db = self.db.lock().unwrap();
                db.add_root(&root.to_string_lossy(), "reference")?
            };
            if path.is_file() {
                let Some(scanned) = scan_one(root, &path) else {
                    continue;
                };
                let mut new_photo = scanned.into_new_photo(root_id);
                let abs_path = new_photo.abs_path.clone();
                let existing = {
                    let db = self.db.lock().unwrap();
                    db.photo_by_path(&abs_path)?
                };
                let existed = existing.is_some();
                new_photo.blake3 = match existing.as_ref() {
                    Some(existing)
                        if existing.blake3.is_some()
                            && existing.file_size == new_photo.file_size
                            && existing.mtime == new_photo.mtime =>
                    {
                        existing.blake3.clone()
                    }
                    _ => hashing::file_blake3(Path::new(&new_photo.abs_path))
                        .ok()
                        .flatten(),
                };

                if !existed {
                    let candidate = {
                        let db = self.db.lock().unwrap();
                        db.find_rebind_candidate(&new_photo)?
                    };
                    if let Some(candidate) = candidate {
                        let old_path_exists = Path::new(&candidate.abs_path).exists();
                        let rebound = if old_path_exists {
                            false
                        } else {
                            let db = self.db.lock().unwrap();
                            db.rebind_photo(candidate.id, &new_photo)?
                        };
                        if rebound {
                            thumbnail_ids.push(candidate.id);
                            changed = true;
                            continue;
                        }
                    }
                }

                let id = {
                    let db = self.db.lock().unwrap();
                    db.upsert_photo(&new_photo)?
                };
                changed = true;
                if !existed {
                    new_ids.push(id);
                    thumbnail_ids.push(id);
                }
            } else {
                let photo = {
                    let db = self.db.lock().unwrap();
                    db.photo_by_path(&path.to_string_lossy())?
                };
                let Some(photo) = photo else {
                    continue;
                };
                if photo.status == PhotoStatus::Present {
                    let marked = {
                        let db = self.db.lock().unwrap();
                        db.mark_missing_if_path(photo.id, &photo.abs_path)?
                    };
                    if marked {
                        missing_ids.push(photo.id);
                        changed = true;
                    }
                }
            }
        }
        // A remove event and its matching create event can arrive in one
        // batch. Only after every addition has had a chance to rebind do we
        // promote a derivative for rows that are still missing.
        {
            let db = self.db.lock().unwrap();
            for photo_id in missing_ids {
                if let Some(photo) = db.photo_by_id(photo_id)?
                    && photo.status == PhotoStatus::Missing
                    && promote_display_derivative(&db, &photo)?
                {
                    changed = true;
                }
            }
            if changed {
                db.bump_library_revision()?;
            }
        }
        if self.settings.auto_compress && !new_ids.is_empty() {
            {
                let db = self.db.lock().unwrap();
                enqueue_auto_jobs(
                    &db,
                    &new_ids,
                    self.settings.auto_compress_keep_original,
                    self.settings.auto_compress_immediate,
                )?;
            }
            self.jobs.notify();
        }
        if !new_ids.is_empty() {
            let tagging_enqueued = {
                let db = self.db.lock().unwrap();
                enqueue_auto_tagging(&db, &new_ids, &self.settings, tagging_ready)?
            };
            if tagging_enqueued > 0 {
                self.tagging.notify();
            }
        }
        if let Some(tx) = &self.thumbnail_tx {
            for id in &thumbnail_ids {
                if let Err(error) = tx.send(*id) {
                    tracing::debug!(
                        %error,
                        photo_id = *id,
                        "thumbnail worker is unavailable; dropping request"
                    );
                }
            }
        }
        if !new_ids.is_empty() {
            self.link_unlinked()?;
            let enqueued = {
                let db = self.db.lock().unwrap();
                match discord::enqueue_auto_sends(&db, &new_ids) {
                    Ok(count) => count,
                    Err(error) => {
                        // Automatic sending is best effort: an outbox error
                        // must not fail or roll back the photo scan.
                        tracing::warn!(%error, "cannot enqueue automatic Discord sends");
                        0
                    }
                }
            };
            if enqueued > 0 {
                self.sends.notify();
            }
        }
        Ok(())
    }

    #[cfg(test)]
    fn scan_photos(&mut self) -> Result<ScanReport> {
        self.scan_photos_with_initial(None)
    }

    fn scan_photos_with_initial(&mut self, initial_scan: Option<bool>) -> Result<ScanReport> {
        let roots = self.settings.photo_roots();
        self.scan_roots_with_initial(roots, true, initial_scan)
    }

    fn scan_root_photos(&mut self, root: PathBuf) -> Result<ScanReport> {
        self.scan_roots_with_initial(vec![root], false, None)
    }

    fn scan_roots_with_initial(
        &mut self,
        roots: Vec<PathBuf>,
        full_scan: bool,
        initial_scan: Option<bool>,
    ) -> Result<ScanReport> {
        let target =
            (!full_scan && roots.len() == 1).then(|| roots[0].to_string_lossy().to_string());
        self.begin_scan(roots.len(), target.clone());
        let result = self.scan_roots_inner(roots, full_scan, initial_scan);
        let (report_result, auto_enqueued) = match result {
            Ok((report, auto_enqueued)) => (Ok(report), auto_enqueued),
            Err(error) => (Err(error), 0),
        };
        self.finish_scan(&report_result, auto_enqueued, target);
        report_result
    }

    fn begin_scan(&self, roots: usize, target: Option<String>) {
        {
            let mut status = self.status.lock().unwrap();
            status.scanning = true;
            status.scan_phase = Some(IpcScanPhase::Walking);
            status.scan_processed = 0;
            status.scan_total = None;
            status.scan_indexed = 0;
            status.scan_root = usize::from(roots > 0);
            status.scan_roots = roots;
        }
        self.events.publish(Event::ScanStarted { roots, target });
    }

    fn finish_scan(
        &self,
        result: &Result<ScanReport>,
        auto_enqueued: usize,
        target: Option<String>,
    ) {
        let (found, indexed, skipped, failures, failed) = match result {
            Ok(report) => (
                report.found,
                report.indexed,
                report.skipped,
                report.failures.len(),
                false,
            ),
            Err(_) => (0, 0, 0, 0, true),
        };
        {
            let mut status = self.status.lock().unwrap();
            status.scanning = false;
            status.scan_phase = None;
            status.scan_processed = found;
            status.scan_total = Some(found);
            status.scan_indexed = indexed;
        }
        self.events.publish(Event::ScanFinished {
            found,
            indexed,
            skipped,
            failures,
            failed,
            auto_enqueued,
            target,
        });
    }

    fn scan_roots_inner(
        &mut self,
        roots: Vec<PathBuf>,
        full_scan: bool,
        initial_scan_override: Option<bool>,
    ) -> Result<(ScanReport, usize)> {
        self.refresh_compression_settings();
        let tagging_ready = self.plugin.is_ready(AI_PLUGIN_ID);
        let mut report = ScanReport::default();
        let mut auto_enqueued = 0;
        let root_count = roots.len();
        if !roots.is_empty() {
            let initial_scan = {
                let db = self.db.lock().unwrap();
                initial_scan_override.unwrap_or(full_scan && db.count_photos()? == 0)
            };
            let mut changed = false;
            let mut missing_ids = Vec::new();
            for (index, root) in roots.into_iter().enumerate() {
                if self.stopping() {
                    report.cancelled = true;
                    break;
                }
                let root_number = index + 1;
                {
                    let mut status = self.status.lock().unwrap();
                    status.scan_root = root_number;
                    status.scan_roots = root_count;
                    status.scan_phase = Some(IpcScanPhase::Walking);
                    status.scan_processed = 0;
                    status.scan_total = None;
                }
                if !root.is_dir() {
                    tracing::warn!(path = %root.display(), "photo root does not exist");
                    report.failures.push(ScanFailure {
                        path: root,
                        message: vrc_photon_i18n::tr!("daemon.photo_root_missing"),
                    });
                    continue;
                }
                let root_id = {
                    let db = self.db.lock().unwrap();
                    db.add_root(&root.to_string_lossy(), "reference")?
                };
                let existing = {
                    let db = self.db.lock().unwrap();
                    db.photos_for_root(root_id)?
                };
                let indexed_before = report.indexed;
                let mut observer = ScanObserver::new(
                    self.events.clone(),
                    self.status.clone(),
                    indexed_before,
                    root_number,
                    root_count,
                );
                let root_report = match scan_and_index_shared(
                    &self.db,
                    root_id,
                    &root,
                    Some(self.running.as_ref()),
                    |progress| observer.observe(progress),
                ) {
                    Ok(root_report) => root_report,
                    Err(error) => {
                        tracing::error!(path = %root.display(), %error, "photo scan failed");
                        report.failures.push(ScanFailure {
                            path: root,
                            message: error.to_string(),
                        });
                        continue;
                    }
                };
                if root_report.failures.is_empty() && !root_report.cancelled {
                    let seen: std::collections::HashSet<&str> =
                        root_report.seen_paths.iter().map(String::as_str).collect();
                    let rebound: std::collections::HashSet<i64> =
                        root_report.rebound_ids.iter().copied().collect();
                    // Keep the write lock bounded even when a large library
                    // contains many deleted photos.
                    for chunk in existing.chunks(500) {
                        let db = self.db.lock().unwrap();
                        for (id, path) in chunk {
                            if !seen.contains(path.as_str()) && !rebound.contains(id) {
                                match db.mark_missing_if_path(*id, path) {
                                    Ok(true) => {
                                        changed = true;
                                        missing_ids.push(*id);
                                    }
                                    Ok(false) => {}
                                    Err(error) => {
                                        tracing::warn!(
                                            %error,
                                            photo_id = id,
                                            "cannot mark missing photo"
                                        )
                                    }
                                }
                            }
                        }
                    }
                }
                changed |= root_report.modified > 0;
                report.found += root_report.found;
                report.indexed += root_report.indexed;
                report.modified += root_report.modified;
                report.skipped += root_report.skipped;
                report.new_ids.extend(root_report.new_ids);
                report.rebound_ids.extend(root_report.rebound_ids);
                report.failures.extend(root_report.failures);
                if root_report.cancelled {
                    report.cancelled = true;
                    break;
                }
            }
            // Give every root a chance to rebind a moved file before falling
            // back to a compressed derivative for a missing original.
            for chunk in missing_ids.chunks(500) {
                let db = self.db.lock().unwrap();
                for photo_id in chunk {
                    if let Some(photo) = db.photo_by_id(*photo_id)?
                        && photo.status == PhotoStatus::Missing
                        && promote_display_derivative(&db, &photo)?
                    {
                        changed = true;
                    }
                }
            }
            if changed {
                let db = self.db.lock().unwrap();
                db.bump_library_revision()?;
            }
            if self.settings.auto_compress
                && !report.new_ids.is_empty()
                && (!initial_scan || self.settings.auto_compress_backlog)
            {
                let enqueued = {
                    let db = self.db.lock().unwrap();
                    enqueue_auto_jobs(
                        &db,
                        &report.new_ids,
                        self.settings.auto_compress_keep_original,
                        self.settings.auto_compress_immediate,
                    )?
                };
                auto_enqueued = enqueued;
                self.jobs.notify();
            }
            if !report.new_ids.is_empty() {
                let tagging_enqueued = {
                    let db = self.db.lock().unwrap();
                    enqueue_auto_tagging(&db, &report.new_ids, &self.settings, tagging_ready)?
                };
                if tagging_enqueued > 0 {
                    self.tagging.notify();
                }
            }
        }
        self.verify_replaced_assets()?;
        self.report_scan_outcome(&report, full_scan)?;
        if report.modified > 0 {
            self.link_unlinked()?;
        }
        if !report.new_ids.is_empty() {
            let enqueued = {
                let db = self.db.lock().unwrap();
                match discord::enqueue_auto_sends(&db, &report.new_ids) {
                    Ok(count) => count,
                    Err(error) => {
                        // Automatic sending is best effort: an outbox error
                        // must not fail or roll back the photo scan.
                        tracing::warn!(%error, "cannot enqueue automatic Discord sends");
                        0
                    }
                }
            };
            if enqueued > 0 {
                self.sends.notify();
            }
        }
        if let Some(tx) = &self.thumbnail_tx {
            for id in &report.new_ids {
                if let Err(error) = tx.send(*id) {
                    tracing::debug!(
                        %error,
                        photo_id = *id,
                        "thumbnail worker is unavailable; dropping request"
                    );
                }
            }
        } else {
            // No thumbnail worker (notably `--once`): keep the ids so the
            // caller can generate directly instead of dropping them.
            self.deferred_thumbnail_ids
                .extend_from_slice(&report.new_ids);
        }
        Ok((report, auto_enqueued))
    }

    /// Records one `Scan` error summarizing the current failures, or clears a
    /// stale error when a full scan had none. A targeted scan (`ScanRoot`)
    /// without failures leaves any recorded error alone: it says nothing
    /// about the roots it did not scan.
    ///
    /// A watcher-start failure shares the `Scan` slot but is not a scan
    /// failure: a clean scan keeps it visible (or restores it after clearing
    /// a scan failure) until the watchers start successfully.
    fn report_scan_outcome(&mut self, report: &ScanReport, full_scan: bool) -> Result<()> {
        if report.cancelled {
            return Ok(());
        }
        if report.failures.is_empty() {
            if full_scan {
                if let Some((localized, detail)) = self.watcher_error.clone() {
                    if self.last_errors.get(&DaemonOperation::Scan)
                        == Some(&localized.signature(detail.as_deref()))
                    {
                        return Ok(());
                    }
                    self.clear_daemon_error(DaemonOperation::Scan)?;
                    return self.report_localized_error(
                        DaemonOperation::Scan,
                        localized,
                        detail.as_deref(),
                    );
                }
                return self.clear_daemon_error(DaemonOperation::Scan);
            }
            return Ok(());
        }
        let detail = report
            .failures
            .iter()
            .take(5)
            .map(|failure| format!("{}: {}", failure.path.display(), failure.message))
            .collect::<Vec<_>>()
            .join("\n");
        self.report_localized_error(
            DaemonOperation::Scan,
            LocalizedError::new("daemon.scan_failed").arg("count", report.failures.len()),
            Some(&detail),
        )
    }

    /// Closes sessions left open by a previous daemon, except the session that
    /// still belongs to the latest log while VRChat is running: that one is
    /// resumed by the log ingestor instead. Only the owner of the
    /// single-instance lock may repair sessions: a maintenance run must not
    /// close the live daemon's active session.
    fn repair_open_sessions(&mut self) -> Result<usize> {
        self.repair_open_sessions_with_running(vrchat_process::is_running())
    }

    fn repair_open_sessions_with_running(&mut self, vrchat_running: Option<bool>) -> Result<usize> {
        if !self.owns_deletion_executor {
            return Ok(0);
        }
        let latest_log = self
            .settings
            .log_dir()
            .and_then(|dir| match latest_log_file(&dir) {
                Ok(path) => path,
                Err(error) => {
                    tracing::warn!(%error, "cannot resolve the latest log for session repair");
                    None
                }
            });
        let db = self.db.lock().unwrap();
        let sessions = db.open_sessions()?;
        let now = chrono::Utc::now().timestamp();
        let mut repaired = 0;
        for session in sessions {
            if should_resume_open_session(
                Path::new(&session.log_file),
                latest_log.as_deref(),
                vrchat_running,
            ) {
                tracing::info!(
                    session_id = session.id,
                    log_file = %session.log_file,
                    "resuming open session at startup"
                );
                continue;
            }
            let ended_at =
                session_end_time(&db, Path::new(&session.log_file), session.started_at, now);
            db.close_session(session.id, ended_at)?;
            tracing::info!(
                session_id = session.id,
                log_file = %session.log_file,
                ended_at,
                "closed stale session at startup"
            );
            repaired += 1;
        }
        Ok(repaired)
    }

    fn poll_vrchat_presence(&mut self) -> Result<()> {
        let has_session = self
            .log_tail
            .as_ref()
            .and_then(|tail| tail.ingestor.session_id())
            .is_some();
        if !has_session {
            self.vrchat_missing_polls = 0;
            self.vrchat_session_id = None;
            return Ok(());
        }
        self.observe_vrchat_presence(vrchat_process::is_running())
    }

    fn observe_vrchat_presence(&mut self, running: Option<bool>) -> Result<()> {
        let Some(session_id) = self
            .log_tail
            .as_ref()
            .and_then(|tail| tail.ingestor.session_id())
        else {
            self.vrchat_missing_polls = 0;
            self.vrchat_session_id = None;
            return Ok(());
        };
        if self.vrchat_session_id != Some(session_id) {
            self.vrchat_session_id = Some(session_id);
            self.vrchat_missing_polls = 0;
        }
        let Some(running) = running else {
            self.vrchat_missing_polls = 0;
            return Ok(());
        };
        if running {
            self.vrchat_missing_polls = 0;
            return Ok(());
        }
        self.vrchat_missing_polls = self.vrchat_missing_polls.saturating_add(1);
        if self.vrchat_missing_polls < VRCHAT_MISSING_POLLS_BEFORE_CLOSE {
            return Ok(());
        }

        let now = chrono::Utc::now().timestamp();
        let (ended_at, closed) = {
            let db = self.db.lock().unwrap();
            let ended_at = self
                .log_tail
                .as_ref()
                .map(|tail| {
                    let started_at = tail.ingestor.session_started_at().unwrap_or(now);
                    session_end_time(&db, &tail.path, started_at, now)
                })
                .unwrap_or(now);
            let closed = self
                .log_tail
                .as_mut()
                .map(|tail| tail.ingestor.close_current_session(&db, ended_at))
                .transpose()?
                .unwrap_or(false);
            (ended_at, closed)
        };
        self.vrchat_missing_polls = 0;
        self.vrchat_session_id = None;
        if closed {
            tracing::info!(ended_at, "closed session after VRChat exited");
            self.refresh_status()?;
        }
        Ok(())
    }

    fn ingest_logs(&mut self) -> Result<()> {
        let Some(log_dir) = self.settings.log_dir() else {
            self.clear_daemon_error(DaemonOperation::Log)?;
            return Ok(());
        };
        if !log_dir.is_dir() {
            self.report_localized_error(
                DaemonOperation::Log,
                LocalizedError::new("daemon.log_dir_missing").arg("path", log_dir.display()),
                None,
            )?;
            return Ok(());
        }
        let path = match latest_log_file(&log_dir) {
            Ok(Some(path)) => path,
            Ok(None) => return Ok(()),
            Err(error) => {
                self.report_localized_error(
                    DaemonOperation::Log,
                    LocalizedError::new("daemon.log_unreadable"),
                    Some(&error.to_string()),
                )?;
                return Ok(());
            }
        };
        let pass = self.ingest_log_pass(&path)?;
        if let Some(failure) = pass.failure_detail() {
            self.report_localized_error(
                DaemonOperation::Log,
                LocalizedError::new("daemon.log_ingest_failed"),
                Some(&failure),
            )?;
        } else if pass.is_clean() {
            self.clear_daemon_error(DaemonOperation::Log)?;
        }
        Ok(())
    }

    fn ingest_log_pass(&mut self, path: &Path) -> Result<LogPass> {
        let mut pass = LogPass::default();
        let path_string = path.to_string_lossy().to_string();
        let metadata = match std::fs::metadata(path) {
            Ok(metadata) => metadata,
            Err(error) => {
                pass.failure = Some(format!("{}: {error}", path.display()));
                return Ok(pass);
            }
        };
        let size = metadata.len() as i64;
        let mtime = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|duration| duration.as_secs() as i64)
            .unwrap_or(0);

        let new_tail = self
            .log_tail
            .as_ref()
            .map(|tail| tail.path != path)
            .unwrap_or(true);
        if new_tail {
            if let Some(mut previous) = self.log_tail.take() {
                let db = self.db.lock().unwrap();
                let now = chrono::Utc::now().timestamp();
                let started_at = previous.ingestor.session_started_at().unwrap_or(now);
                let finish_at = session_end_time(&db, &previous.path, started_at, now);
                previous.ingestor.finish(&db, finish_at)?;
            }
            let (offset, ingestor) = {
                let db = self.db.lock().unwrap();
                let offset = db.log_offset(&path_string)?.unwrap_or(0);
                let open_session = db
                    .open_sessions()?
                    .into_iter()
                    .find(|session| session.log_file == path_string);
                let ingestor = match open_session {
                    Some(session) => LogIngestor::resume_from_db(&db, &path_string, session.id)?
                        .unwrap_or_else(|| LogIngestor::new(&path_string)),
                    None => LogIngestor::new(&path_string),
                };
                (offset, ingestor)
            };
            self.log_tail = Some(LogTail {
                path: path.to_path_buf(),
                ingestor,
                offset,
                failed_offset: None,
                failed_attempts: 0,
            });
        }

        let tail = self.log_tail.as_mut().unwrap();
        if size < tail.offset {
            tail.offset = 0;
            tail.ingestor = LogIngestor::new(&path_string);
            tail.failed_offset = None;
            tail.failed_attempts = 0;
        }
        let (lines, new_offset) = match read_new_lines(path, tail.offset) {
            Ok(result) => result,
            Err(error) => {
                pass.failure = Some(format!("{}: {error}", path.display()));
                return Ok(pass);
            }
        };
        let mut next = 0;
        while next < lines.len() {
            let chunk_end = (next + LOG_INGEST_CHUNK_LINES).min(lines.len());
            let chunk = &lines[next..chunk_end];
            let chunk_start = tail.offset;
            // Work on a copy so a failed transaction leaves both the database
            // and the in-memory session state at the last committed offset.
            let mut ingestor = tail.ingestor.clone();
            let mut line_start = chunk_start;
            let mut committed_offset = chunk_start;
            let mut failed_offset = tail.failed_offset;
            let mut failed_attempts = tail.failed_attempts;
            let mut processed = 0;
            let mut skipped = Vec::new();
            let mut failure = None;
            let mut stop = false;
            let result = {
                let db = self.db.lock().unwrap();
                db.in_transaction(|db| {
                    for (line, line_end) in chunk {
                        match ingestor.ingest_line(db, line) {
                            Ok(_) => {
                                committed_offset = *line_end;
                                line_start = *line_end;
                                processed += 1;
                            }
                            Err(error) => {
                                tracing::debug!(%error, "cannot parse log line");
                                failure
                                    .get_or_insert_with(|| format!("{}: {error}", path.display()));
                                if !record_line_failure(
                                    &mut failed_offset,
                                    &mut failed_attempts,
                                    line_start,
                                ) {
                                    stop = true;
                                    break;
                                }
                                skipped.push((line_start, *line_end));
                                committed_offset = *line_end;
                                line_start = *line_end;
                            }
                        }
                    }
                    // The ingest and its offset commit atomically. A crash
                    // between chunks re-reads only lines that were rolled back.
                    if committed_offset != chunk_start {
                        db.set_log_offset(
                            &path_string,
                            size,
                            mtime,
                            committed_offset,
                            PARSER_VERSION,
                        )?;
                    }
                    Ok(())
                })
            };
            if let Err(error) = result {
                return Err(error.into());
            }
            tail.ingestor = ingestor;
            tail.offset = committed_offset;
            tail.failed_offset = failed_offset;
            tail.failed_attempts = failed_attempts;
            pass.processed += processed;
            if let Some(failure) = failure {
                pass.failure.get_or_insert(failure);
            }
            for (start, end) in skipped {
                pass.record_skip(path, start, end);
            }
            if stop {
                break;
            }
            next = chunk_end;
        }
        if pass.failure.is_none() && tail.offset != new_offset {
            let db = self.db.lock().unwrap();
            db.set_log_offset(&path_string, size, mtime, new_offset, PARSER_VERSION)?;
            drop(db);
            tail.offset = new_offset;
        }
        self.refresh_status()?;
        self.link_unlinked()?;
        Ok(pass)
    }

    /// Clears the link retry schedule for still-unlinked photos. Explicit
    /// Relog/Rescan commands retry everything immediately; the periodic log
    /// ingestion and watcher paths keep honoring the backoff.
    fn make_unlinked_photos_due(&self) -> Result<usize> {
        let changed = {
            let db = self.db.lock().unwrap();
            db.make_unlinked_photos_due()?
        };
        if changed > 0 {
            tracing::debug!(
                photos = changed,
                "cleared link retry backoff for an explicit retry"
            );
        }
        Ok(changed)
    }

    fn link_unlinked(&mut self) -> Result<()> {
        let result = {
            let db = self.db.lock().unwrap();
            linker::relink_unlinked(&db, 2000)
        };
        match result {
            Ok(linked) => {
                if linked > 0 {
                    tracing::info!(linked, "linked photos to log context");
                    if self.world_name_resolution_enabled.load(Ordering::Relaxed) {
                        self.enqueue_world_name_backfill();
                        self.world_names.notify();
                    }
                }
                self.clear_daemon_error(DaemonOperation::Link)
            }
            Err(error) => {
                tracing::error!(%error, "cannot link photos to log context");
                self.report_localized_error(
                    DaemonOperation::Link,
                    LocalizedError::new("daemon.link_failed"),
                    Some(&error.to_string()),
                )?;
                Err(error.into())
            }
        }
    }

    /// Marks replaced photos missing when their compressed display file was
    /// deleted or moved away. The notify watcher intentionally ignores the
    /// `compressed` directory, so the daemon checks these paths periodically.
    fn verify_replaced_assets(&mut self) -> Result<()> {
        let rows = {
            let db = self.db.lock().unwrap();
            db.display_derivative_paths(100_000)?
        };
        // Stat every asset without the database lock so a slow or disconnected
        // volume cannot block scans, IPC, or the heartbeat.
        let missing =
            missing_display_derivatives(rows, |path| path.is_dir(), |path| path.is_file());
        if missing.is_empty() {
            return Ok(());
        }
        let db = self.db.lock().unwrap();
        db.in_transaction(|db| {
            for photo_id in missing {
                db.mark_missing(photo_id)?;
                db.clear_display_derivative(photo_id)?;
            }
            db.bump_library_revision()?;
            Ok(())
        })?;
        Ok(())
    }

    fn refresh_status(&self) -> Result<()> {
        let (world_name, players, photo_count, pending_jobs, pending_sends) = {
            let db = self.db.lock().unwrap();
            let now = chrono::Utc::now().timestamp();
            let session = db.session_at(now)?;
            let (world_name, players) = if let Some(session) = &session {
                let world = db.world_by_id(session.world_id)?;
                let players = db.players_at(session.id, now)?.len();
                (world.and_then(|world| world.alias.or(world.name)), players)
            } else {
                (None, 0)
            };
            (
                world_name,
                players,
                db.count_photos()?,
                db.count_jobs(QueueStatus::Pending)?,
                db.count_pending_sends()?,
            )
        };
        // The readiness check reads the database, so it must run after the
        // block above released the lock. The cached values keep IPC snapshots
        // cheap and lock-order safe.
        let (pending_tagging_jobs, tagging_ready) = tagging_status(&self.db, &self.plugin);
        let mut status = self.status.lock().unwrap();
        status.watching = self.settings.log_dir.is_some();
        status.world_name = world_name;
        status.players = players;
        status.photo_count = photo_count;
        status.pending_jobs = pending_jobs;
        status.pending_sends = pending_sends;
        status.pending_tagging_jobs = pending_tagging_jobs;
        status.tagging_ready = tagging_ready;
        status.last_log = self
            .log_tail
            .as_ref()
            .map(|tail| tail.path.to_string_lossy().to_string());
        Ok(())
    }

    fn write_heartbeat(&mut self) {
        match self.db.lock() {
            Ok(db) => {
                if let Err(error) = db.set_setting(
                    "daemon_heartbeat",
                    &chrono::Utc::now().timestamp().to_string(),
                ) {
                    tracing::warn!(%error, "cannot write daemon heartbeat");
                }
            }
            Err(error) => tracing::warn!(%error, "cannot lock database for heartbeat"),
        }
        self.last_heartbeat = std::time::Instant::now();
    }

    fn record_supervisor(&self) {
        let cgroup = std::fs::read_to_string("/proc/self/cgroup").ok();
        let supervisor = if cgroup
            .as_deref()
            .is_some_and(|cgroup| cgroup_has_unit(cgroup, "vrc-photon-daemon.service"))
        {
            "systemd"
        } else {
            supervisor_label(
                std::env::var_os("INVOCATION_ID").as_deref(),
                std::env::var_os("SYSTEMD_EXEC_PID").as_deref(),
                std::process::id(),
            )
        };
        match self.db.lock() {
            Ok(db) => {
                if let Err(error) = db.set_setting("daemon_supervisor", supervisor) {
                    tracing::warn!(%error, "cannot record daemon supervisor");
                }
            }
            Err(error) => tracing::warn!(%error, "cannot lock database for supervisor"),
        }
    }

    /// Runs the timed settings refreshes. Called on every main-loop
    /// iteration, including while paused, so a settings change the UI saved
    /// is applied promptly instead of waiting until the daemon resumes.
    fn refresh_periodic_settings(&mut self) {
        if self.last_settings_check.elapsed() >= SETTINGS_INTERVAL {
            self.refresh_compression_settings();
            self.last_settings_check = Instant::now();
        }
        if self.last_autostart_check.elapsed() >= Duration::from_secs(10) {
            self.refresh_autostart_setting();
            self.last_autostart_check = std::time::Instant::now();
        }
    }

    fn refresh_autostart_setting(&mut self) {
        let enabled = {
            let db = self.db.lock().unwrap();
            match AppSettings::load(&db) {
                Ok(settings) => settings.autostart,
                Err(error) => {
                    tracing::warn!(%error, "cannot reload settings for autostart");
                    self.settings.autostart
                }
            }
        };
        if enabled != self.settings.autostart {
            self.settings.autostart = enabled;
            self.apply_autostart(enabled);
        }
    }

    /// Applies the compression switches and thumbnail cache limit without
    /// touching paths or restarting watchers. The UI saves these immediately;
    /// a running daemon must not keep enqueuing with the previous policy or
    /// prune with a stale cache limit. This also mirrors
    /// `world_name_resolution_enabled` into the worker flag and runs the
    /// backfill when the switch turns on.
    fn refresh_compression_settings(&mut self) {
        let loaded = {
            let db = self.db.lock().unwrap();
            match AppSettings::load(&db) {
                Ok(settings) => Some(settings),
                Err(error) => {
                    tracing::warn!(%error, "cannot reload compression settings");
                    None
                }
            }
        };
        let Some(settings) = loaded else {
            return;
        };
        self.settings.auto_compress = settings.auto_compress;
        self.settings.auto_compress_backlog = settings.auto_compress_backlog;
        self.settings.auto_compress_immediate = settings.auto_compress_immediate;
        self.settings.auto_compress_keep_original = settings.auto_compress_keep_original;
        self.settings.ai_tagging_enabled = settings.ai_tagging_enabled;
        self.settings.ai_tagging_auto = settings.ai_tagging_auto;
        self.settings.ai_tagging_backlog = settings.ai_tagging_backlog;
        self.settings.ai_tagging_idle_timeout_sec = settings.ai_tagging_idle_timeout_sec;
        self.settings.thumbnail_quality = settings.thumbnail_quality;
        self.settings.thumbnail_size = settings.thumbnail_size;
        self.thumbnail_cache_limit.store(
            thumbnail_cache_limit_bytes(settings.thumbnail_cache_mb),
            Ordering::Relaxed,
        );
        self.thumbnail_spec.store(
            pack_thumbnail_spec(settings.thumbnail_spec()),
            Ordering::Relaxed,
        );
        let world_name_enabled = settings.world_name_resolution_enabled;
        let was_enabled = self
            .world_name_resolution_enabled
            .swap(world_name_enabled, Ordering::Relaxed);
        self.settings.world_name_resolution_enabled = world_name_enabled;
        if world_name_enabled && !was_enabled {
            self.enqueue_world_name_backfill();
            self.world_names.notify();
        } else if !world_name_enabled && was_enabled {
            self.world_names.notify();
        }
    }

    /// Fills photo contexts from stored world names, then queues every world
    /// that still has no name. Lookup failures stay out of `daemon_errors`:
    /// the feature is best-effort and must not surface as a user-facing fault.
    fn enqueue_world_name_backfill(&self) {
        let db = self.db.lock().unwrap();
        if let Err(error) = db.backfill_photo_context_world_names() {
            tracing::warn!(%error, "cannot backfill photo context world names");
        }
        if let Err(error) = db.enqueue_missing_world_name_lookups() {
            tracing::warn!(%error, "cannot enqueue missing world name lookups");
        }
    }

    fn apply_autostart(&self, enabled: bool) {
        let Ok(exe) = std::env::current_exe() else {
            return;
        };
        let exe = exe.to_string_lossy();
        let mut builder = auto_launch::AutoLaunchBuilder::new();
        builder.set_app_name("VRC PhotoN").set_app_path(&exe);
        match builder.build() {
            Ok(auto) => {
                let result = if enabled {
                    auto.enable()
                } else {
                    auto.disable()
                };
                if let Err(error) = result {
                    tracing::warn!(%error, enabled, "cannot update autostart");
                }
            }
            Err(error) => tracing::warn!(%error, "cannot build autostart entry"),
        }
    }

    fn ensure_tray(&mut self) {
        if !self.tray_enabled || self.tray.is_some() {
            return;
        }
        match Tray::new() {
            Ok(tray) => self.tray = Some(tray),
            Err(error) => {
                tracing::warn!(%error, "tray unavailable; continuing headless");
                self.tray_enabled = false;
            }
        }
    }

    fn poll_tray(&mut self) {
        self.ensure_tray();
        let status = self
            .status
            .lock()
            .map(|status| status.clone())
            .unwrap_or_default();
        let Some(tray) = self.tray.as_mut() else {
            return;
        };
        match tray.poll(&status) {
            TrayAction::Noop => {}
            TrayAction::TogglePause(paused) => self.set_paused(paused),
            TrayAction::Quit => {
                self.running.store(false, Ordering::Relaxed);
                self.wake.notify();
            }
            TrayAction::OpenUi => tray::open_ui(),
        }
    }

    fn setup_watchers(&mut self) -> Result<()> {
        if self.watcher_rx.is_some() {
            return Ok(());
        }
        let (tx, rx) = mpsc::channel();
        let mut watched = Vec::new();
        for root in self.settings.photo_roots() {
            if !root.is_dir() {
                continue;
            }
            let tx = tx.clone();
            let mut watcher = notify::recommended_watcher(move |event| {
                if let Err(error) = tx.send(event) {
                    tracing::debug!(%error, "photo watcher receiver closed; dropping event");
                }
            })
            .context("cannot create photo watcher")?;
            watcher
                .watch(&root, RecursiveMode::Recursive)
                .with_context(|| format!("cannot watch {}", root.display()))?;
            self.watchers.push(watcher);
            watched.push(root);
        }
        self.watcher_rx = Some(rx);
        self.watcher_roots = watched;
        Ok(())
    }

    fn poll_watchers(&mut self) {
        let Some(rx) = &self.watcher_rx else {
            return;
        };
        while let Ok(result) = rx.try_recv() {
            match result {
                Ok(event) => {
                    if matches!(
                        event.kind,
                        notify::EventKind::Create(_)
                            | notify::EventKind::Modify(_)
                            | notify::EventKind::Remove(_)
                    ) {
                        for path in event.paths {
                            if path
                                .components()
                                .any(|part| part.as_os_str() == "compressed")
                            {
                                continue;
                            }
                            if is_source_photo(&path) {
                                self.dirty_paths.push(path);
                            }
                        }
                    }
                }
                Err(error) => tracing::warn!(%error, "photo watcher error"),
            }
        }
    }

    fn start_tagging_worker(&mut self) {
        if self.tagging_handle.is_some() {
            return;
        }
        let config = TaggingWorkerConfig {
            idle_timeout: Duration::from_secs(self.settings.ai_tagging_idle_timeout_sec),
            ..TaggingWorkerConfig::default()
        };
        let worker = TaggingWorker::new(
            self.db.clone(),
            self.ai_data_dir.clone(),
            self.plugin.clone(),
            self.events.clone(),
            self.running.clone(),
            self.tagging.clone(),
            self.tagging_control.clone(),
            config,
        );
        self.tagging_handle = Some(tagging::start_tagging_worker(worker));
    }

    fn start_workers(&mut self) {
        if self.thumbnail_tx.is_none() {
            let (tx, rx) = mpsc::channel();
            let db = self.db.clone();
            let running = self.running.clone();
            let cache_dir = self.thumbnail_dir.clone();
            let cache_limit = self.thumbnail_cache_limit.clone();
            let spec = self.thumbnail_spec.clone();
            let cache_epoch = self.thumbnail_cache_epoch.clone();
            let external_bytes = self.thumbnail_external_bytes.clone();
            let handle = thread::Builder::new()
                .name("thumbnails".to_string())
                .spawn(move || {
                    thumbnail_loop(
                        db,
                        running,
                        rx,
                        cache_dir,
                        cache_limit,
                        spec,
                        cache_epoch,
                        external_bytes,
                    )
                })
                .expect("cannot spawn thumbnail worker");
            self.thumbnail_tx = Some(tx.clone());
            self.thumbnail_handle = Some(handle);
            if let Ok(db) = self.db.lock()
                && let Ok(photos) =
                    db.list_photos(5000, 0, vrc_photon_core::model::PhotoSort::Newest)
            {
                for photo in photos {
                    if let Err(error) = tx.send(photo.id) {
                        tracing::debug!(
                            %error,
                            photo_id = photo.id,
                            "thumbnail worker is unavailable; dropping request"
                        );
                    }
                }
            }
            for id in std::mem::take(&mut self.deferred_thumbnail_ids) {
                if let Err(error) = tx.send(id) {
                    tracing::debug!(
                        %error,
                        photo_id = id,
                        "thumbnail worker is unavailable; dropping deferred request"
                    );
                }
            }
        }
        if self.hash_handle.is_none() {
            let db = self.db.clone();
            let running = self.running.clone();
            let handle = thread::Builder::new()
                .name("photo-hashes".to_string())
                .spawn(move || hash_loop(db, running))
                .expect("cannot spawn photo hash worker");
            self.hash_handle = Some(handle);
        }
        if self.owns_deletion_executor && self.deletion_handle.is_none() {
            let db = self.db.clone();
            let running = self.running.clone();
            let jobs = self.jobs.clone();
            let handle = thread::Builder::new()
                .name("deletion-executor".to_string())
                .spawn(move || deletion_loop(db, running, jobs))
                .expect("cannot spawn deletion worker");
            self.deletion_handle = Some(handle);
        }
        if self.send_handle.is_none() {
            let db = self.db.clone();
            let running = self.running.clone();
            let sends = self.sends.clone();
            let events = self.events.clone();
            let handle = thread::Builder::new()
                .name("discord-send".to_string())
                .spawn(move || {
                    discord::worker_loop(db, running, sends, events, Arc::new(UreqTransport::new()))
                })
                .expect("cannot spawn Discord send worker");
            self.send_handle = Some(handle);
        }
        if self.world_name_handle.is_none() {
            if self.world_name_resolution_enabled.load(Ordering::Relaxed) {
                self.enqueue_world_name_backfill();
            }
            let db = self.db.clone();
            let running = self.running.clone();
            let enabled = self.world_name_resolution_enabled.clone();
            let wake = self.world_names.clone();
            let transport = Arc::new(UreqWorldNameTransport::new());
            let handle = thread::Builder::new()
                .name("world-name".to_string())
                .spawn(move || world_names::worker_loop(db, running, enabled, wake, transport))
                .expect("cannot spawn world name worker");
            self.world_name_handle = Some(handle);
        }
        self.start_tagging_worker();
        if !self.workers.is_empty() {
            return;
        }
        let count = self.settings.compression_workers.clamp(1, 4);
        for index in 0..count {
            let db = self.db.clone();
            let running = self.running.clone();
            let jobs = self.jobs.clone();
            let events = self.events.clone();
            let thumbnail_dir = self.thumbnail_dir.clone();
            let handle = thread::Builder::new()
                .name(format!("compress-{index}"))
                .spawn(move || worker_loop(db, running, jobs, events, thumbnail_dir))
                .expect("cannot spawn compression worker");
            self.workers.push(handle);
        }
    }

    fn stop_workers(&mut self) {
        self.running.store(false, Ordering::Relaxed);
        // Interrupt an in-flight sidecar request before joining the tagging
        // worker; otherwise shutdown waits for the whole inference.
        self.tagging_control.cancel_all();
        self.jobs.notify();
        self.sends.notify();
        self.world_names.notify();
        self.tagging.notify();
        self.thumbnail_tx = None;
        if let Some(handle) = self.thumbnail_handle.take()
            && handle.join().is_err()
        {
            tracing::error!("thumbnail worker panicked");
        }
        if let Some(handle) = self.hash_handle.take()
            && handle.join().is_err()
        {
            tracing::error!("photo hash worker panicked");
        }
        if let Some(handle) = self.deletion_handle.take()
            && handle.join().is_err()
        {
            tracing::error!("deletion worker panicked");
        }
        if let Some(handle) = self.send_handle.take()
            && handle.join().is_err()
        {
            tracing::error!("Discord send worker panicked");
        }
        if let Some(handle) = self.world_name_handle.take()
            && handle.join().is_err()
        {
            tracing::error!("world name worker panicked");
        }
        if let Some(handle) = self.tagging_handle.take()
            && handle.join().is_err()
        {
            tracing::error!("tagging worker panicked");
        }
        for (index, handle) in self.workers.drain(..).enumerate() {
            if handle.join().is_err() {
                tracing::error!(worker = index, "compression worker panicked");
            }
        }
    }

    fn shutdown(&mut self) {
        self.ipc = None;
        self.running.store(false, Ordering::Relaxed);
        self.wake.notify();
        self.sends.notify();
        self.world_names.notify();
        if let Some(mut tail) = self.log_tail.take() {
            let db = self.db.lock().unwrap();
            let now = chrono::Utc::now().timestamp();
            let started_at = tail.ingestor.session_started_at().unwrap_or(now);
            let ended_at = session_end_time(&db, &tail.path, started_at, now);
            if let Err(error) = tail.ingestor.finish(&db, ended_at) {
                tracing::warn!(%error, "cannot finish log ingestion on shutdown");
            }
        }
        self.stop_workers();
        self.plugin.stop_sidecar();
        self.tray = None;
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        self.ipc = None;
        self.running.store(false, Ordering::Relaxed);
        self.wake.notify();
    }
}

struct ScanObserver {
    events: EventBus,
    status: Arc<Mutex<DaemonStatus>>,
    indexed_before: usize,
    root: usize,
    roots: usize,
    last_emit: Instant,
    last_phase: Option<IpcScanPhase>,
}

impl ScanObserver {
    fn new(
        events: EventBus,
        status: Arc<Mutex<DaemonStatus>>,
        indexed_before: usize,
        root: usize,
        roots: usize,
    ) -> Self {
        Self {
            events,
            status,
            indexed_before,
            root,
            roots,
            last_emit: Instant::now(),
            last_phase: None,
        }
    }

    fn observe(&mut self, progress: ScanProgress) {
        let phase = match progress.phase {
            CoreScanPhase::Walking => IpcScanPhase::Walking,
            CoreScanPhase::Indexing => IpcScanPhase::Indexing,
        };
        let final_indexing =
            progress.phase == CoreScanPhase::Indexing && progress.total == Some(progress.processed);
        let phase_changed = self.last_phase != Some(phase);
        if !phase_changed
            && !final_indexing
            && self.last_emit.elapsed() < Duration::from_millis(200)
        {
            return;
        }
        self.last_phase = Some(phase);
        self.last_emit = Instant::now();
        let indexed = self.indexed_before + progress.indexed;
        {
            let mut status = self.status.lock().unwrap();
            status.scan_phase = Some(phase);
            status.scan_processed = progress.processed;
            status.scan_total = progress.total;
            status.scan_indexed = indexed;
            status.scan_root = self.root;
            status.scan_roots = self.roots;
        }
        self.events.publish(Event::ScanProgress {
            phase,
            processed: progress.processed,
            total: progress.total,
            indexed,
            root: self.root,
            roots: self.roots,
        });
    }
}

fn ipc_operation(operation: DaemonOperation) -> Operation {
    match operation {
        DaemonOperation::Scan => Operation::Scan,
        DaemonOperation::Log => Operation::Log,
        DaemonOperation::Link => Operation::Link,
    }
}

fn error_signature(message: &str, detail: Option<&str>) -> String {
    match detail {
        Some(detail) => format!("{message}\n{detail}"),
        None => message.to_string(),
    }
}

fn supervisor_label(
    invocation_id: Option<&std::ffi::OsStr>,
    exec_pid: Option<&std::ffi::OsStr>,
    current_pid: u32,
) -> &'static str {
    let exec_pid = exec_pid
        .and_then(|pid| pid.to_str())
        .and_then(|pid| pid.parse::<u32>().ok());
    if invocation_id.is_some() && exec_pid == Some(current_pid) {
        "systemd"
    } else {
        "app"
    }
}

fn cgroup_has_unit(cgroup: &str, unit: &str) -> bool {
    cgroup
        .lines()
        .flat_map(|line| line.split('/'))
        .any(|part| part == unit)
}

fn enqueue_auto_jobs(
    db: &Db,
    new_ids: &[i64],
    keep_original: bool,
    immediate: bool,
) -> Result<usize> {
    let profiles = db.list_profiles()?;
    let auto_profiles: Vec<&CompressionProfile> = profiles
        .iter()
        .filter(|profile| profile.enabled && profile.auto_enabled)
        .collect();
    if auto_profiles.is_empty() {
        return Ok(0);
    }
    let policy = if keep_original { "keep" } else { "replace" };
    let status = if immediate {
        QueueStatus::Pending
    } else {
        QueueStatus::Waiting
    };
    let mut enqueued = 0;
    for photo_id in new_ids {
        for profile in &auto_profiles {
            if db.enqueue_auto_job_for_existing_profile_with_status(
                *photo_id,
                &profile.id,
                policy,
                status,
            )? {
                enqueued += 1;
            }
        }
    }
    tracing::info!(
        photos = new_ids.len(),
        profiles = auto_profiles.len(),
        jobs = enqueued,
        "auto compression jobs enqueued"
    );
    Ok(enqueued)
}

/// Enqueues pending AI tagging jobs for newly indexed photos. Automatic
/// tagging is opt-in twice (enabled and auto) and requires a ready plugin;
/// everything else is left for the manual and backlog paths.
fn enqueue_auto_tagging(
    db: &Db,
    new_ids: &[i64],
    settings: &AppSettings,
    plugin_ready: bool,
) -> Result<usize> {
    if new_ids.is_empty()
        || !settings.ai_tagging_enabled
        || !settings.ai_tagging_auto
        || !plugin_ready
    {
        return Ok(0);
    }
    let enqueued = db.enqueue_tagging_jobs(new_ids, AI_PLUGIN_ID, TaggingJobStatus::Pending)?;
    tracing::info!(
        photos = new_ids.len(),
        jobs = enqueued,
        "automatic AI tagging jobs enqueued"
    );
    Ok(enqueued)
}

/// Keeps a photo visible when its original file was deleted manually but a
/// completed compression output remains. Uses the same deterministic profile
/// preference as replacement jobs.
fn promote_display_derivative(db: &Db, photo: &Photo) -> Result<bool> {
    let derivatives = db.derivatives_for_photo(photo.id)?;
    let profiles = db.list_profiles()?;
    let Some(preferred) = preferred_display_derivative(&derivatives, &profiles) else {
        return Ok(false);
    };
    Ok(db.set_replaced(photo.id, preferred.id)?)
}

/// Picks the derivative to show for a photo. Profiles are ordered by
/// `sort_order`, so the first profile in `list_profiles()` wins. Unknown
/// profiles rank below known ones and fall back to the larger derivative id.
/// Only files that still exist are candidates.
fn preferred_display_derivative<'a>(
    derivatives: &'a [Derivative],
    profiles: &[CompressionProfile],
) -> Option<&'a Derivative> {
    derivatives
        .iter()
        .filter(|derivative| Path::new(&derivative.path).is_file())
        .max_by_key(|derivative| {
            (
                profiles
                    .iter()
                    .position(|profile| profile.id == derivative.profile_id)
                    .map(std::cmp::Reverse),
                derivative.id,
            )
        })
}

/// Tracks the cache size between scans. `known_bytes` is `None` while the
/// cache has not been measured yet, which forces the first prune decision to
/// scan even when nothing was generated.
struct ThumbnailCacheBudget {
    limit_bytes: u64,
    known_bytes: Option<u64>,
}

impl ThumbnailCacheBudget {
    fn new(limit: u64) -> Self {
        Self {
            limit_bytes: limit,
            known_bytes: None,
        }
    }

    fn set_limit(&mut self, limit: u64) {
        if limit == self.limit_bytes {
            return;
        }
        self.limit_bytes = limit;
        self.known_bytes = None;
    }

    fn note_generated(&mut self, bytes: u64) {
        self.add(bytes);
    }

    fn note_external(&mut self, bytes: u64) {
        self.add(bytes);
    }

    fn note_pruned(&mut self, report: &thumbnail::ThumbnailPruneReport) {
        self.known_bytes = (self.limit_bytes > 0).then_some(report.remaining_bytes);
    }

    /// The cache was cleared behind the worker's back; rescan before the next
    /// prune decision instead of trusting the previous byte count.
    fn note_cleared(&mut self) {
        self.known_bytes = None;
    }

    fn needs_prune(&self) -> bool {
        self.limit_bytes > 0
            && self
                .known_bytes
                .is_none_or(|known| known > self.limit_bytes)
    }

    fn add(&mut self, bytes: u64) {
        if let Some(known) = self.known_bytes {
            self.known_bytes = Some(known.saturating_add(bytes));
        }
    }
}

struct ThumbnailPruneSchedule {
    batch: usize,
    generated: usize,
}

impl ThumbnailPruneSchedule {
    fn new(batch: usize) -> Self {
        Self {
            batch,
            generated: 0,
        }
    }

    fn note_generated(&mut self) -> bool {
        self.generated += 1;
        if self.generated >= self.batch {
            self.note_pruned();
            true
        } else {
            false
        }
    }

    fn has_pending(&self) -> bool {
        self.generated > 0
    }

    fn note_pruned(&mut self) {
        self.generated = 0;
    }
}

fn thumbnail_cache_limit_bytes(megabytes: u64) -> u64 {
    megabytes.saturating_mul(1024 * 1024)
}

fn pack_thumbnail_spec(spec: thumbnail::ThumbnailSpec) -> u32 {
    (u32::from(spec.quality) << 16) | (spec.size & 0xffff)
}

fn unpack_thumbnail_spec(packed: u32) -> thumbnail::ThumbnailSpec {
    thumbnail::ThumbnailSpec::new((packed >> 16) as u8, packed & 0xffff)
}

/// Prunes the cache when a limit is set. Returns the measurement from the
/// scan so the worker can update its budget; `None` means the cache was not
/// scanned (unlimited) or the scan failed.
fn prune_thumbnail_cache(
    cache_dir: &Path,
    limit_bytes: u64,
) -> Option<thumbnail::ThumbnailPruneReport> {
    if limit_bytes == 0 {
        return None;
    }
    match thumbnail::prune_thumbnails_in(cache_dir, limit_bytes) {
        Ok(report) => {
            if report.removed_files > 0 {
                tracing::info!(
                    removed_files = report.removed_files,
                    removed_bytes = report.removed_bytes,
                    remaining_bytes = report.remaining_bytes,
                    limit_bytes,
                    dir = %cache_dir.display(),
                    "pruned thumbnail cache"
                );
            }
            Some(report)
        }
        Err(error) => {
            tracing::warn!(
                %error,
                dir = %cache_dir.display(),
                "cannot prune thumbnail cache"
            );
            None
        }
    }
}

/// Reloads the live cache limit immediately before a prune decision. A limit
/// of zero always wins, so a pending prune cannot delete files after the user
/// selected unlimited while the worker was blocked or generating.
fn refresh_thumbnail_budget_and_prune(
    cache_dir: &Path,
    cache_limit: &AtomicU64,
    budget: &mut ThumbnailCacheBudget,
) {
    let limit = cache_limit.load(Ordering::Relaxed);
    budget.set_limit(limit);
    if limit == 0 || !budget.needs_prune() {
        return;
    }
    if let Some(report) = prune_thumbnail_cache(cache_dir, limit) {
        budget.note_pruned(&report);
    }
}

#[allow(clippy::too_many_arguments)]
fn thumbnail_loop(
    db: Arc<Mutex<Db>>,
    running: Arc<AtomicBool>,
    rx: Receiver<i64>,
    cache_dir: PathBuf,
    cache_limit: Arc<AtomicU64>,
    spec: Arc<AtomicU32>,
    cache_epoch: Arc<AtomicU64>,
    external_bytes: Arc<AtomicU64>,
) {
    let mut budget = ThumbnailCacheBudget::new(cache_limit.load(Ordering::Relaxed));
    let mut schedule = ThumbnailPruneSchedule::new(THUMBNAIL_PRUNE_BATCH);
    let mut seen_epoch = cache_epoch.load(Ordering::Relaxed);
    // Notifications that arrived before this worker started describe a cache
    // that the startup scan is about to measure anyway.
    external_bytes.swap(0, Ordering::Relaxed);
    refresh_thumbnail_budget_and_prune(&cache_dir, &cache_limit, &mut budget);

    while running.load(Ordering::Relaxed) {
        let epoch = cache_epoch.load(Ordering::Relaxed);
        if epoch != seen_epoch {
            seen_epoch = epoch;
            external_bytes.swap(0, Ordering::Relaxed);
            budget.note_cleared();
            schedule.note_pruned();
            refresh_thumbnail_budget_and_prune(&cache_dir, &cache_limit, &mut budget);
        }
        let limit = cache_limit.load(Ordering::Relaxed);
        if limit != budget.limit_bytes {
            schedule.note_pruned();
            refresh_thumbnail_budget_and_prune(&cache_dir, &cache_limit, &mut budget);
        }
        match rx.recv_timeout(THUMBNAIL_PRUNE_IDLE) {
            Ok(id) => {
                let external = external_bytes.swap(0, Ordering::Relaxed);
                budget.note_external(external);
                let path = {
                    let db = db.lock().unwrap();
                    match db.photo_by_id(id) {
                        Ok(photo) => photo.map(|photo| photo.effective_path().to_string()),
                        Err(error) => {
                            tracing::warn!(
                                %error,
                                photo_id = id,
                                "cannot load photo for thumbnail"
                            );
                            None
                        }
                    }
                };
                let mut batch_due = false;
                if let Some(path) = path {
                    let thumbnail_spec = unpack_thumbnail_spec(spec.load(Ordering::Relaxed));
                    match thumbnail::ensure_thumbnail_in_with_status(
                        id,
                        Path::new(&path),
                        &cache_dir,
                        thumbnail_spec,
                    ) {
                        Ok(Some(ensured)) => {
                            if ensured.created {
                                budget.note_generated(ensured.bytes);
                                batch_due = schedule.note_generated();
                            }
                        }
                        Ok(None) => tracing::debug!(
                            photo_id = id,
                            path = %path,
                            "thumbnail source is missing"
                        ),
                        Err(error) => tracing::warn!(
                            %error,
                            photo_id = id,
                            path = %path,
                            "cannot generate thumbnail"
                        ),
                    }
                }
                // The notice may describe a thumbnail the UI already wrote, so
                // it must prune even when this id generated nothing (or was
                // unknown). The helper rechecks the live limit first.
                if batch_due || external > 0 {
                    refresh_thumbnail_budget_and_prune(&cache_dir, &cache_limit, &mut budget);
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let external = external_bytes.swap(0, Ordering::Relaxed);
                budget.note_external(external);
                if schedule.has_pending() || external > 0 {
                    schedule.note_pruned();
                    refresh_thumbnail_budget_and_prune(&cache_dir, &cache_limit, &mut budget);
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
}

/// Fills `photos.blake3` for rows indexed before hashing was introduced.
/// The worker is deliberately low-frequency so a large first run does not
/// starve compression or watcher work.
fn hash_loop(db: Arc<Mutex<Db>>, running: Arc<AtomicBool>) {
    let mut hashed_since_idle = false;
    while running.load(Ordering::Relaxed) {
        let rows = {
            let Ok(db) = db.lock() else { break };
            match db.photos_without_hash(16) {
                Ok(rows) => rows,
                Err(error) => {
                    tracing::warn!(%error, "cannot load photos for hashing");
                    Vec::new()
                }
            }
        };
        if rows.is_empty() {
            if hashed_since_idle {
                if let Ok(db) = db.lock() {
                    let _ = db.bump_library_revision();
                }
                hashed_since_idle = false;
            }
            thread::sleep(Duration::from_secs(2));
            continue;
        }
        let mut progressed = false;
        for (photo_id, path) in rows {
            if !running.load(Ordering::Relaxed) {
                break;
            }
            match hashing::file_blake3(Path::new(&path)) {
                Ok(Some(hash)) => {
                    if let Ok(db) = db.lock()
                        && let Err(error) = db.set_photo_blake3(photo_id, &hash)
                    {
                        tracing::warn!(%error, photo_id, "cannot store photo hash");
                    } else {
                        hashed_since_idle = true;
                        progressed = true;
                    }
                }
                Ok(None) => tracing::debug!(photo_id, path, "photo disappeared before hashing"),
                Err(error) => tracing::warn!(%error, photo_id, path, "cannot hash photo"),
            }
        }
        thread::sleep(if progressed {
            Duration::from_millis(200)
        } else {
            Duration::from_secs(2)
        });
    }
}

fn deletion_loop(db: Arc<Mutex<Db>>, running: Arc<AtomicBool>, jobs: Arc<JobSignal>) {
    let mut seen_generation = 0;
    while running.load(Ordering::Relaxed) {
        if let Err(error) = run_deletions_once(&db) {
            tracing::error!(%error, "deletion executor failed");
        }
        jobs.wait(&mut seen_generation, Duration::from_millis(500));
    }
}

/// A forced request may be recorded directly with `force = 1` instead of
/// through `force_deletion`, which cancels tagging jobs itself. Requests with
/// active tagging jobs are hidden from `pending_deletion_requests`, so cancel
/// those jobs first in one bulk statement; the late sidecar result is rejected
/// by the cancelled job state. `claim_deletion` then only waits for runs that
/// were not forced.
fn cancel_tagging_for_forced_deletions(db: &Arc<Mutex<Db>>) -> Result<()> {
    let cancelled = {
        let db = db.lock().unwrap();
        db.cancel_tagging_for_forced_deletions()?
    };
    if cancelled > 0 {
        tracing::info!(cancelled, "cancelled tagging jobs for forced deletions");
    }
    Ok(())
}

/// Claims and executes every pending deletion whose jobs are terminal. A
/// dependency failure blocks the request instead of busy-polling it; a file
/// failure blocks it until the user retries or forces.
fn run_deletions_once(db: &Arc<Mutex<Db>>) -> Result<usize> {
    cancel_tagging_for_forced_deletions(db)?;
    let requests = {
        let db = db.lock().unwrap();
        db.pending_deletion_requests(100)?
    };
    let mut executed = 0;
    for pending in requests {
        if pending.reason == DeletionReason::Unknown {
            // The snapshot may be stale: the UI can upgrade the request to
            // manual after collection. Only block when the stored reason is
            // still unknown.
            let blocked = {
                let db = db.lock().unwrap();
                db.block_deletion_if_unknown(pending.id)?
            };
            if blocked {
                tracing::warn!(
                    request = pending.id,
                    photo_id = pending.photo_id,
                    "unknown deletion reason; request blocked"
                );
            }
            continue;
        }
        let blocked = {
            let db = db.lock().unwrap();
            db.block_deletion_if_jobs_failed(pending.id)?
        };
        if blocked {
            continue;
        }
        let claimed = {
            let db = db.lock().unwrap();
            db.claim_deletion(pending.id)?
        };
        if let Some(request) = claimed {
            match execute_deletion(db, &request) {
                Ok(()) => executed += 1,
                Err(error) => tracing::error!(
                    request = request.id,
                    photo_id = request.photo_id,
                    %error,
                    "deletion failed"
                ),
            }
        }
    }
    Ok(executed)
}

fn execute_deletion(db: &Arc<Mutex<Db>>, request: &DeletionRequest) -> Result<()> {
    if request.reason == DeletionReason::Unknown {
        anyhow::bail!("unknown deletion reason");
    }
    let (photo, targets) = {
        let db = db.lock().unwrap();
        let Some(photo) = db.photo_by_id(request.photo_id)? else {
            db.complete_deletion(request.id)?;
            db.bump_library_revision()?;
            return Ok(());
        };
        let derivatives = db.derivatives_for_photo(photo.id)?;
        if request.reason == DeletionReason::AutoReplace && !request.force {
            let profiles = db.list_profiles()?;
            let Some(valid) = preferred_display_derivative(&derivatives, &profiles) else {
                db.block_deletion(request.id, "missing_asset", None)?;
                return Err(anyhow::anyhow!(
                    "replacement derivative is missing; refusing to delete the original"
                ));
            };
            if photo.status != PhotoStatus::Replaced
                || photo.display_derivative_id != Some(valid.id)
            {
                db.set_replaced(photo.id, valid.id)?;
            }
        }
        let mut targets = Vec::new();
        let mut seen = HashSet::new();
        if request.reason == DeletionReason::Manual {
            for derivative in derivatives {
                if seen.insert(derivative.path.clone()) {
                    targets.push(PathBuf::from(derivative.path));
                }
            }
        }
        if seen.insert(photo.abs_path.clone()) {
            targets.push(PathBuf::from(&photo.abs_path));
        }
        (photo, targets)
    };

    for path in targets {
        if !path.is_file() {
            continue;
        }
        if let Err(error) = trash::delete(&path)
            && path.exists()
        {
            let db = db.lock().unwrap();
            db.block_deletion(request.id, "file_error", Some(&error.to_string()))?;
            return Err(anyhow::anyhow!(
                "cannot move {} to trash: {error}",
                path.display()
            ));
        }
    }

    {
        let db = db.lock().unwrap();
        if request.reason == DeletionReason::Manual {
            db.complete_manual_deletion(photo.id)?;
        }
        db.complete_deletion(request.id)?;
        db.bump_library_revision()?;
    }
    tracing::info!(
        request = request.id,
        photo_id = photo.id,
        reason = request.reason.as_str(),
        "deletion completed"
    );
    Ok(())
}

fn worker_loop(
    db: Arc<Mutex<Db>>,
    running: Arc<AtomicBool>,
    jobs: Arc<JobSignal>,
    events: EventBus,
    thumbnail_dir: PathBuf,
) {
    let mut seen_generation = 0;
    let mut last_poll_error: Option<String> = None;
    while running.load(Ordering::Relaxed) {
        let batch = {
            let db = db.lock().unwrap();
            match db.next_pending_photo_jobs() {
                Ok(batch) => {
                    last_poll_error = None;
                    batch
                }
                Err(error) => {
                    // The loop polls every 400 ms; warn once per distinct
                    // failure instead of flooding the log while it persists.
                    let message = error.to_string();
                    if last_poll_error.as_deref() != Some(&message) {
                        tracing::warn!(%error, "cannot load pending compression jobs");
                        last_poll_error = Some(message);
                    }
                    Vec::new()
                }
            }
        };
        if batch.is_empty() {
            // Poll first so a batch keeps all workers busy; the signal only
            // saves the 400 ms wait when the queue was empty.
            jobs.wait(&mut seen_generation, Duration::from_millis(400));
            continue;
        }
        for job in &batch {
            publish_compression(&events, job, JobStatus::Processing, 0.05, None);
        }
        for outcome in process_photo_batch(&db, &batch, Some(&thumbnail_dir)) {
            match outcome.status {
                QueueStatus::Completed => {
                    publish_compression(&events, &outcome.job, JobStatus::Completed, 1.0, None)
                }
                QueueStatus::Cancelled => {
                    publish_compression(&events, &outcome.job, JobStatus::Cancelled, 0.0, None)
                }
                _ => publish_compression(
                    &events,
                    &outcome.job,
                    JobStatus::Failed,
                    0.0,
                    outcome.error,
                ),
            }
        }
    }
}

fn publish_compression(
    events: &EventBus,
    job: &CompressionJob,
    status: JobStatus,
    progress: f64,
    error: Option<String>,
) {
    events.publish(Event::CompressionProgress {
        job_id: job.id.clone(),
        photo_id: job.photo_id,
        status,
        progress,
        error,
    });
}

struct JobOutcome {
    job: CompressionJob,
    status: QueueStatus,
    error: Option<String>,
}

/// Processes every job for one photo after decoding the source once.
///
/// The source path is frozen before the first job, so a `replace` job cannot
/// feed its derivative into a later job in the same batch. Individual job
/// failures are recorded and do not stop the remaining jobs. A batch-level
/// failure only marks jobs that are still pending/processing as failed, so
/// completed and cancelled rows are never rewritten.
fn process_photo_batch(
    db: &Arc<Mutex<Db>>,
    jobs: &[CompressionJob],
    thumbnail_dir: Option<&Path>,
) -> Vec<JobOutcome> {
    match process_photo_batch_inner(db, jobs, thumbnail_dir) {
        Ok(outcomes) => outcomes,
        Err(error) => {
            let message = error.to_string();
            tracing::error!(%error, "compression batch failed");
            fail_claimed_jobs(db, jobs, &message)
        }
    }
}

fn fail_claimed_jobs(
    db: &Arc<Mutex<Db>>,
    jobs: &[CompressionJob],
    message: &str,
) -> Vec<JobOutcome> {
    let mut outcomes = Vec::with_capacity(jobs.len());
    for job in jobs {
        let status = match db.lock().unwrap().fail_job_if_active(&job.id, message) {
            Ok(status) => status.unwrap_or(QueueStatus::Failed),
            Err(error) => {
                tracing::error!(
                    job = %job.id,
                    %error,
                    "cannot record failed compression job"
                );
                QueueStatus::Failed
            }
        };
        outcomes.push(JobOutcome {
            job: job.clone(),
            error: (status == QueueStatus::Failed).then(|| message.to_string()),
            status,
        });
    }
    outcomes
}

fn process_photo_batch_inner(
    db: &Arc<Mutex<Db>>,
    jobs: &[CompressionJob],
    thumbnail_dir: Option<&Path>,
) -> Result<Vec<JobOutcome>> {
    let Some(first) = jobs.first() else {
        return Ok(Vec::new());
    };
    debug_assert!(jobs.iter().all(|job| job.photo_id == first.photo_id));

    let photo = {
        let db = db.lock().unwrap();
        db.photo_by_id(first.photo_id)?
    };
    let Some(photo) = photo else {
        return fail_batch(db, jobs, "photo no longer exists");
    };
    let source_path = if Path::new(&photo.abs_path).is_file() {
        PathBuf::from(&photo.abs_path)
    } else {
        PathBuf::from(photo.effective_path())
    };
    if photo.blake3.is_none()
        && source_path == Path::new(&photo.abs_path)
        && let Ok(Some(hash)) = hashing::file_blake3(&source_path)
    {
        db.lock().unwrap().set_photo_blake3(photo.id, &hash)?;
    }

    let Some(image) = vrc_photon_core::image_decode::decode_image(&source_path) else {
        return fail_batch(
            db,
            jobs,
            &format!("cannot decode {}", source_path.display()),
        );
    };

    let mut outcomes = Vec::with_capacity(jobs.len());
    for (index, job) in jobs.iter().enumerate() {
        if index > 0 && db.lock().unwrap().photo_has_forced_deletion(photo.id)? {
            for remaining in &jobs[index..] {
                db.lock().unwrap().update_job(
                    &remaining.id,
                    QueueStatus::Cancelled,
                    0.0,
                    None,
                    None,
                )?;
                outcomes.push(JobOutcome {
                    job: remaining.clone(),
                    status: QueueStatus::Cancelled,
                    error: None,
                });
            }
            break;
        }
        db.lock()
            .unwrap()
            .update_job(&job.id, QueueStatus::Processing, 0.05, None, None)?;
        match process_decoded_job(db, &photo, &source_path, &image, job, thumbnail_dir) {
            Ok(()) => outcomes.push(JobOutcome {
                job: job.clone(),
                status: QueueStatus::Completed,
                error: None,
            }),
            Err(error) => {
                let message = error.to_string();
                tracing::error!(job = %job.id, %error, "compression failed");
                if let Err(update_error) = db.lock().unwrap().update_job(
                    &job.id,
                    QueueStatus::Failed,
                    0.0,
                    Some(&message),
                    None,
                ) {
                    tracing::error!(
                        job = %job.id,
                        %update_error,
                        "cannot record failed compression job"
                    );
                }
                outcomes.push(JobOutcome {
                    job: job.clone(),
                    status: QueueStatus::Failed,
                    error: Some(message),
                });
            }
        }
    }
    Ok(outcomes)
}

fn fail_batch(
    db: &Arc<Mutex<Db>>,
    jobs: &[CompressionJob],
    error: &str,
) -> Result<Vec<JobOutcome>> {
    let mut outcomes = Vec::with_capacity(jobs.len());
    for job in jobs {
        tracing::error!(job = %job.id, %error, "compression failed");
        db.lock()
            .unwrap()
            .update_job(&job.id, QueueStatus::Failed, 0.0, Some(error), None)?;
        outcomes.push(JobOutcome {
            job: job.clone(),
            status: QueueStatus::Failed,
            error: Some(error.to_string()),
        });
    }
    Ok(outcomes)
}

fn process_decoded_job(
    db: &Arc<Mutex<Db>>,
    photo: &Photo,
    source_path: &Path,
    image: &DynamicImage,
    job: &CompressionJob,
    thumbnail_dir: Option<&Path>,
) -> Result<()> {
    let mut profile = {
        let db = db.lock().unwrap();
        db.list_profiles()?
            .into_iter()
            .find(|profile| profile.id == job.profile_id)
            .context("compression profile no longer exists")?
    };
    if let Some(output_dir) = job
        .output_dir
        .as_deref()
        .filter(|dir| !dir.trim().is_empty())
    {
        profile.output_dir = Some(output_dir.to_string());
    }

    let result = compressor::compress_decoded(image, source_path, &profile)?;
    if !result.output_path.is_file()
        || std::fs::metadata(&result.output_path)
            .map(|meta| meta.len() == 0)
            .unwrap_or(true)
    {
        anyhow::bail!("compressed output is missing or empty");
    }
    let db = db.lock().unwrap();
    db.add_derivative(
        photo.id,
        &result.output_path.to_string_lossy(),
        result.format.as_str(),
        result.width,
        result.height,
        result.compressed_size,
        &profile.id,
        &job.policy,
    )?;
    let current_status = db.photo_by_id(photo.id)?.map(|row| row.status);
    if job.policy == "replace" && current_status != Some(PhotoStatus::Deleting) {
        let derivatives = db.derivatives_for_photo(photo.id)?;
        let profiles = db.list_profiles()?;
        let preferred = preferred_display_derivative(&derivatives, &profiles)
            .context("no displayable derivative after compression")?;
        // The displayed file is about to change. Drop every cached thumbnail
        // and preview for this photo before publishing `replaced`, so the next
        // display request regenerates from the derivative instead of showing
        // the old original. Failures are non-fatal: the source stamp still
        // invalidates a stale entry on the next read.
        if let Some(cache_dir) = thumbnail_dir
            && let Err(error) = thumbnail::remove_thumbnail_files_in(cache_dir, photo.id)
        {
            tracing::warn!(photo_id = photo.id, %error, "cannot invalidate thumbnail cache");
        }
        if let Some(cache_dir) = vrc_photon_core::preview::previews_dir()
            && let Err(error) =
                vrc_photon_core::preview::remove_preview_files_in(&cache_dir, photo.id)
        {
            tracing::warn!(photo_id = photo.id, %error, "cannot invalidate preview cache");
        }
        if !db.set_replaced(photo.id, preferred.id)? {
            anyhow::bail!("derivative was not attached to the photo");
        }
        db.request_deletion(&[photo.id], DeletionReason::AutoReplace, false)?;
    }
    db.update_job(
        &job.id,
        QueueStatus::Completed,
        1.0,
        None,
        Some(&result.output_path.to_string_lossy()),
    )?;
    if let Err(error) = db.resume_blocked_deletions_for_photo(photo.id) {
        tracing::warn!(photo_id = photo.id, %error, "cannot resume blocked deletion");
    }
    db.bump_library_revision()?;
    tracing::info!(
        photo = %source_path.display(),
        output = %result.output_path.display(),
        saved = result.compressed_size,
        "compressed"
    );
    Ok(())
}

#[cfg(test)]
fn process_job(db: &Arc<Mutex<Db>>, job: &CompressionJob) -> Result<()> {
    process_job_with_thumbnail_dir(db, job, None)
}

#[cfg(test)]
fn process_job_with_thumbnail_dir(
    db: &Arc<Mutex<Db>>,
    job: &CompressionJob,
    thumbnail_dir: Option<&Path>,
) -> Result<()> {
    let mut outcomes = process_photo_batch(db, std::slice::from_ref(job), thumbnail_dir);
    let outcome = outcomes.pop().context("compression produced no outcome")?;
    match outcome.status {
        QueueStatus::Completed => Ok(()),
        _ => Err(anyhow::anyhow!(
            outcome
                .error
                .unwrap_or_else(|| "compression failed".to_string())
        )),
    }
}

fn modified_utc(path: &Path) -> Option<i64> {
    path.metadata()
        .ok()?
        .modified()
        .ok()
        .map(vrc_photon_core::time::from_system_time)
        .map(|stamp| stamp.utc)
}

/// Picks the end timestamp for a session: log file mtime, then the mtime
/// recorded by the last log ingestion, then `now`. The result is clamped to
/// `[started_at, now]` so a future or pre-start mtime cannot keep the session
/// visible (and so a clock skew cannot create an invalid interval).
fn session_end_time(db: &Db, log_file: &Path, started_at: i64, now: i64) -> i64 {
    let candidate = modified_utc(log_file)
        .or_else(|| db.log_mtime(&log_file.to_string_lossy()).ok().flatten())
        .unwrap_or(now);
    candidate.clamp(started_at.min(now), now)
}

/// An open session may only survive startup when it belongs to the newest log
/// and VRChat is still running. A missing or unknown process state is treated
/// as "not running" so repair keeps its conservative close behavior.
fn should_resume_open_session(
    open_log: &Path,
    latest_log: Option<&Path>,
    vrchat_running: Option<bool>,
) -> bool {
    vrchat_running == Some(true) && latest_log.is_some_and(|latest| latest == open_log)
}

fn latest_log_file(dir: &Path) -> Result<Option<PathBuf>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("cannot read log folder {}", dir.display()));
        }
    };
    let mut files: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .map(|name| {
                    (name.starts_with("output_log_") || name == "output_log.txt")
                        && name.ends_with(".txt")
                })
                .unwrap_or(false)
        })
        .collect();
    files.sort();
    Ok(files.pop())
}

/// Returns the ids of replaced photos whose display derivative is missing.
/// Roots that are not currently directories are skipped entirely so a
/// disconnected drive never marks its photos missing. Root checks are cached
/// per root, and both predicates run without the database lock held.
fn missing_display_derivatives(
    rows: Vec<(i64, Option<String>, String)>,
    mut root_is_dir: impl FnMut(&Path) -> bool,
    mut file_is_file: impl FnMut(&Path) -> bool,
) -> Vec<i64> {
    let mut roots: HashMap<String, bool> = HashMap::new();
    let mut missing = Vec::new();
    for (photo_id, path, root) in rows {
        let available = *roots
            .entry(root.clone())
            .or_insert_with(|| root_is_dir(Path::new(&root)));
        if !available {
            continue;
        }
        let valid = path
            .as_deref()
            .is_some_and(|path| file_is_file(Path::new(path)));
        if !valid {
            missing.push(photo_id);
        }
    }
    missing
}

/// Reads complete lines from `offset`, leaving a trailing partial line for the
/// next poll. Each line is returned with its end byte offset.
fn read_new_lines(path: &Path, offset: i64) -> Result<(Vec<(String, i64)>, i64)> {
    read_new_lines_bounded(path, offset, LOG_READ_MAX_LINES, LOG_READ_MAX_BYTES)
}

/// Bounded variant used by tests and [`read_new_lines`]. It never reads past
/// the byte budget, so a partial line at the budget boundary is retried on the
/// next poll instead of being treated as complete.
fn read_new_lines_bounded(
    path: &Path,
    offset: i64,
    max_lines: usize,
    max_bytes: u64,
) -> Result<(Vec<(String, i64)>, i64)> {
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(offset.max(0) as u64))?;
    let mut reader = BufReader::new(file).take(max_bytes);
    let mut buffer = Vec::new();
    let mut lines = Vec::new();
    let mut complete_lines = 0usize;
    let mut consumed = 0u64;
    while complete_lines < max_lines {
        buffer.clear();
        let read = reader.read_until(b'\n', &mut buffer)?;
        if read == 0 || buffer.last() != Some(&b'\n') {
            break;
        }
        complete_lines += 1;
        consumed += read as u64;
        let text = String::from_utf8_lossy(&buffer[..buffer.len() - 1]);
        let line = text.trim_end_matches('\r');
        if !line.is_empty() {
            lines.push((line.to_string(), offset + consumed as i64));
        }
    }
    Ok((lines, offset + consumed as i64))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::time::SystemTime;
    use vrc_photon_ai_host::test_support::sample_manifest;
    use vrc_photon_ai_protocol::Probe;
    use vrc_photon_core::ai::PluginInstallation;
    use vrc_photon_core::model::{
        DeletionStatus, JobStatusCounts, NewPhoto, PhotoContext, SendOrigin, SendStatus,
    };
    use vrc_photon_core::settings::{DiscordAutoMode, save_discord_webhook};

    #[test]
    fn localized_error_builds_text_and_json_args() {
        let error = LocalizedError::new("daemon.scan_failed").arg("count", 3);
        assert!(error.message_in("ja").contains('3'));
        assert!(error.message_in("en").contains('3'));
        assert_eq!(error.args_json(), r#"{"count":"3"}"#);
    }

    #[test]
    fn reads_only_complete_lines_with_offsets() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("output_log_test.txt");
        let mut file = File::create(&path).unwrap();
        write!(file, "line1\nline2\npart").unwrap();
        drop(file);

        let (lines, offset) = read_new_lines(&path, 0).unwrap();
        assert_eq!(
            lines,
            vec![("line1".to_string(), 6), ("line2".to_string(), 12)]
        );
        assert_eq!(offset, 12);
    }

    #[test]
    fn read_new_lines_stops_at_the_line_budget() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("output_log_test.txt");
        std::fs::write(&path, "one\ntwo\nthree\nfour").unwrap();

        let (lines, offset) = read_new_lines_bounded(&path, 0, 2, 1024).unwrap();
        assert_eq!(lines, vec![("one".to_string(), 4), ("two".to_string(), 8)]);
        assert_eq!(offset, 8);

        let (lines, offset) = read_new_lines_bounded(&path, offset, 2, 1024).unwrap();
        assert_eq!(lines, vec![("three".to_string(), 14)]);
        assert_eq!(offset, 14, "the partial last line is left for next time");
    }

    #[test]
    fn read_new_lines_stops_at_the_byte_budget_without_consuming_a_partial_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("output_log_test.txt");
        std::fs::write(&path, "line1\nline2\n").unwrap();

        let (lines, offset) = read_new_lines_bounded(&path, 0, 100, 8).unwrap();
        assert_eq!(lines, vec![("line1".to_string(), 6)]);
        assert_eq!(offset, 6);

        let (lines, offset) = read_new_lines_bounded(&path, offset, 100, 8).unwrap();
        assert_eq!(lines, vec![("line2".to_string(), 12)]);
        assert_eq!(offset, 12);
    }

    #[test]
    fn poison_line_is_skipped_only_after_three_attempts() {
        let mut failed_offset = None;
        let mut failed_attempts = 0;
        assert!(!record_line_failure(
            &mut failed_offset,
            &mut failed_attempts,
            0
        ));
        assert!(!record_line_failure(
            &mut failed_offset,
            &mut failed_attempts,
            0
        ));
        assert!(record_line_failure(
            &mut failed_offset,
            &mut failed_attempts,
            0
        ));
        assert_eq!(failed_attempts, 0);
        assert!(!record_line_failure(
            &mut failed_offset,
            &mut failed_attempts,
            10
        ));
    }

    #[test]
    fn skipped_line_detail_records_path_offset_and_attempts() {
        let original = "/logs/output_log_x.txt: parse error";
        let mut pass = LogPass {
            failure: Some(original.to_string()),
            ..LogPass::default()
        };
        pass.record_skip(Path::new("/logs/output_log_x.txt"), 120, 180);

        let detail = pass.failure_detail().expect("failure detail");
        assert!(detail.contains("/logs/output_log_x.txt"), "{detail}");
        assert!(detail.contains("120"), "{detail}");
        assert!(detail.contains("180"), "{detail}");
        assert!(detail.contains("3回失敗したためスキップ"), "{detail}");
        assert!(
            detail.contains("parse error"),
            "the original error is kept: {detail}"
        );
        assert_eq!(pass.skipped, 1);
        assert!(!pass.is_clean());
        assert_ne!(
            error_signature("ログの取り込みに失敗しました", Some(original)),
            error_signature("ログの取り込みに失敗しました", Some(&detail)),
            "the skipped line must change the reported signature"
        );
    }

    #[test]
    fn skipped_line_details_are_capped_and_summarized() {
        let mut pass = LogPass {
            failure: Some("parse error".to_string()),
            ..LogPass::default()
        };
        for index in 0..MAX_SKIPPED_DETAILS + 2 {
            let start = index as i64 * 10;
            pass.record_skip(Path::new("/logs/a.txt"), start, start + 5);
        }

        let detail = pass.failure_detail().expect("failure detail");
        assert_eq!(
            detail.matches("3回失敗したためスキップ").count(),
            MAX_SKIPPED_DETAILS
        );
        assert!(detail.contains("ほか 2 行"), "{detail}");
        assert_eq!(pass.skipped, MAX_SKIPPED_DETAILS + 2);
    }

    #[test]
    fn stale_daemon_errors_clear_after_a_successful_run() {
        let dir = tempfile::tempdir().unwrap();
        let photo_dir = dir.path().join("photos");
        let log_dir = dir.path().join("logs");
        std::fs::create_dir_all(&photo_dir).unwrap();
        std::fs::create_dir_all(&log_dir).unwrap();
        let db_path = dir.path().join("library.db");

        {
            let db = Db::open(&db_path).unwrap();
            db.record_daemon_error(DaemonOperation::Scan, "old scan", None)
                .unwrap();
            db.record_daemon_error(DaemonOperation::Log, "old log", None)
                .unwrap();
            db.record_daemon_error(DaemonOperation::Link, "old link", None)
                .unwrap();
        }
        std::fs::write(
            log_dir.join("output_log_2026-10-21_21-00-00.txt"),
            "2026.10.21 21:23:12 Debug      -  [Behaviour] Unloading scenes\n",
        )
        .unwrap();

        let mut daemon = Daemon::open(&db_path, Some(photo_dir), Some(log_dir)).unwrap();
        daemon.run_once().unwrap();

        let db = Db::open(&db_path).unwrap();
        assert!(db.daemon_errors().unwrap().is_empty());
    }

    #[test]
    fn missing_photo_root_records_a_scan_error() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing");
        let log_dir = dir.path().join("logs");
        std::fs::create_dir_all(&log_dir).unwrap();
        let db_path = dir.path().join("library.db");

        let mut daemon = Daemon::open(&db_path, Some(missing), Some(log_dir)).unwrap();
        daemon.run_once().unwrap();

        let db = Db::open(&db_path).unwrap();
        let errors = db.daemon_errors().unwrap();
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].operation, DaemonOperation::Scan);
        assert!(errors[0].message.contains("写真フォルダ"));
    }

    #[test]
    fn scan_root_success_keeps_another_roots_scan_error() {
        let dir = tempfile::tempdir().unwrap();
        let good = dir.path().join("photos");
        let missing = dir.path().join("missing");
        let log_dir = dir.path().join("logs");
        std::fs::create_dir_all(&good).unwrap();
        std::fs::create_dir_all(&log_dir).unwrap();
        write_photo(&good.join("VRChat_2026-10-21_21-24-00.000_4x3.png"));
        let db_path = dir.path().join("library.db");

        let mut daemon = Daemon::open(&db_path, Some(good.clone()), Some(log_dir)).unwrap();
        daemon.set_ipc_endpoint(None);
        daemon.settings.photo_roots = vec![
            good.to_string_lossy().to_string(),
            missing.to_string_lossy().to_string(),
        ];

        // The full scan covers the missing root and records a Scan error.
        daemon.scan_photos().unwrap();
        {
            let db = daemon.db.lock().unwrap();
            let errors = db.daemon_errors().unwrap();
            assert_eq!(errors.len(), 1);
            assert_eq!(errors[0].operation, DaemonOperation::Scan);
        }

        // A successful targeted scan of the healthy root must not clear the
        // error recorded for the missing one.
        let report = daemon.scan_root_photos(good.clone()).unwrap();
        assert!(report.failures.is_empty());
        {
            let db = daemon.db.lock().unwrap();
            let errors = db.daemon_errors().unwrap();
            assert_eq!(errors.len(), 1);
            assert_eq!(errors[0].operation, DaemonOperation::Scan);
        }

        // A full scan with every root healthy clears the stale error.
        std::fs::create_dir_all(&missing).unwrap();
        daemon.scan_photos().unwrap();
        let db = daemon.db.lock().unwrap();
        assert!(db.daemon_errors().unwrap().is_empty());
    }

    #[test]
    fn missing_log_folder_records_a_log_error() {
        let dir = tempfile::tempdir().unwrap();
        let photo_dir = dir.path().join("photos");
        std::fs::create_dir_all(&photo_dir).unwrap();
        let log_dir = dir.path().join("missing-logs");
        let db_path = dir.path().join("library.db");

        let mut daemon = Daemon::open(&db_path, Some(photo_dir), Some(log_dir)).unwrap();
        daemon.run_once().unwrap();

        let db = Db::open(&db_path).unwrap();
        let errors = db.daemon_errors().unwrap();
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].operation, DaemonOperation::Log);
    }

    #[test]
    fn rescan_command_clears_missing_root_error_and_indexes_the_folder() {
        let dir = tempfile::tempdir().unwrap();
        let photo_dir = dir.path().join("photos");
        let log_dir = dir.path().join("logs");
        std::fs::create_dir_all(&log_dir).unwrap();
        let db_path = dir.path().join("library.db");
        let mut daemon = Daemon::open(&db_path, Some(photo_dir.clone()), Some(log_dir)).unwrap();
        daemon.run_once().unwrap();

        {
            let db = Db::open(&db_path).unwrap();
            assert_eq!(
                db.daemon_errors().unwrap()[0].operation,
                DaemonOperation::Scan
            );
        }

        std::fs::create_dir_all(&photo_dir).unwrap();
        write_photo(&photo_dir.join("VRChat_2026-10-21_21-24-00.000_4x3.png"));
        {
            let db = Db::open(&db_path).unwrap();
            assert!(db.request_daemon_command(DaemonCommand::Rescan).unwrap());
        }
        daemon.run_once().unwrap();

        let db = Db::open(&db_path).unwrap();
        assert!(db.daemon_errors().unwrap().is_empty());
        assert_eq!(db.count_photos().unwrap(), 1);
    }

    #[test]
    fn large_appended_log_is_ingested_in_bounded_passes() {
        let dir = tempfile::tempdir().unwrap();
        let mut daemon = test_daemon(&dir);
        let log_path = dir.path().join("logs/output_log_2026-10-21_21-00-00.txt");
        let join =
            "2026.10.21 21:23:12 Debug      -  [Behaviour] Joining wrld_test:12345~region(jp)";
        let name =
            "2026.10.21 21:23:13 Debug      -  [Behaviour] Joining or Creating Room: Test World";
        let player_one = "2026.10.21 21:23:20 Debug      -  [Behaviour] OnPlayerJoined Mio (usr_a)";
        let player_two =
            "2026.10.21 21:24:20 Debug      -  [Behaviour] OnPlayerJoined Kenta (usr_b)";
        let filler = "not a vrchat log line";
        let mut head_lines = vec![join.to_string(), name.to_string()];
        for _ in 2..510 {
            head_lines.push(filler.to_string());
        }
        // This player is past the 500-line transaction boundary but still in
        // the first read budget, so session state must carry between chunks.
        head_lines.push(player_one.to_string());
        while head_lines.len() < LOG_READ_MAX_LINES {
            head_lines.push(filler.to_string());
        }
        let head = format!("{}\n", head_lines.join("\n"));
        let tail = format!("{player_two}\n");
        std::fs::write(&log_path, format!("{head}{tail}")).unwrap();

        let log_file = log_path.to_str().unwrap();
        daemon.ingest_logs().unwrap();
        let first_offset = {
            let db = daemon.db.lock().unwrap();
            db.log_offset(log_file).unwrap().unwrap()
        };
        assert_eq!(
            first_offset,
            head.len() as i64,
            "one poll stops at the line budget"
        );
        let joined_at = vrc_photon_core::log_parser::parse_line(join)
            .unwrap()
            .timestamp_utc();
        let player_one_at = vrc_photon_core::log_parser::parse_line(player_one)
            .unwrap()
            .timestamp_utc();
        let player_two_at = vrc_photon_core::log_parser::parse_line(player_two)
            .unwrap()
            .timestamp_utc();
        {
            let db = daemon.db.lock().unwrap();
            let session = db.session_at(joined_at + 1).unwrap().unwrap();
            let players = db.players_at(session.id, player_one_at + 1).unwrap();
            assert_eq!(players.len(), 1);
            assert_eq!(players[0].display_name, "Mio");
            assert_eq!(
                db.players_at(session.id, player_two_at + 1).unwrap().len(),
                1
            );
        }

        daemon.ingest_logs().unwrap();
        {
            let db = daemon.db.lock().unwrap();
            assert_eq!(
                db.log_offset(log_file).unwrap(),
                Some((head.len() + tail.len()) as i64)
            );
            let session = db.session_at(joined_at + 1).unwrap().unwrap();
            let players = db.players_at(session.id, player_two_at + 1).unwrap();
            assert_eq!(players.len(), 2);
            assert_eq!(players[1].display_name, "Kenta");
            assert_eq!(db.open_sessions().unwrap().len(), 1);
        }

        // The next poll has nothing new to ingest and must not create a
        // second session for the same world join.
        daemon.ingest_logs().unwrap();
        let db = daemon.db.lock().unwrap();
        assert_eq!(db.open_sessions().unwrap().len(), 1);
        assert_eq!(
            db.log_offset(log_file).unwrap(),
            Some((head.len() + tail.len()) as i64)
        );
    }

    #[test]
    fn trailing_partial_log_line_is_ingested_after_it_completes() {
        let dir = tempfile::tempdir().unwrap();
        let mut daemon = test_daemon(&dir);
        let log_path = dir.path().join("logs/output_log_2026-10-21_21-00-00.txt");
        let join =
            "2026.10.21 21:23:12 Debug      -  [Behaviour] Joining wrld_test:12345~region(jp)";
        let player = "2026.10.21 21:23:20 Debug      -  [Behaviour] OnPlayerJoined Mio (usr_a)";
        std::fs::write(&log_path, format!("{join}\n{player}")).unwrap();
        let log_file = log_path.to_str().unwrap();
        daemon.ingest_logs().unwrap();

        let join_offset = format!("{join}\n").len() as i64;
        let joined_at = vrc_photon_core::log_parser::parse_line(join)
            .unwrap()
            .timestamp_utc();
        let player_at = vrc_photon_core::log_parser::parse_line(player)
            .unwrap()
            .timestamp_utc();
        {
            let db = daemon.db.lock().unwrap();
            assert_eq!(db.log_offset(log_file).unwrap(), Some(join_offset));
            let session = db.session_at(joined_at + 1).unwrap().unwrap();
            assert!(db.players_at(session.id, player_at).unwrap().is_empty());
        }

        let mut file = File::options().append(true).open(&log_path).unwrap();
        writeln!(file).unwrap();
        drop(file);
        daemon.ingest_logs().unwrap();

        let db = daemon.db.lock().unwrap();
        assert_eq!(
            db.log_offset(log_file).unwrap(),
            Some(join_offset + player.len() as i64 + 1)
        );
        let session = db.session_at(joined_at + 1).unwrap().unwrap();
        let players = db.players_at(session.id, player_at + 1).unwrap();
        assert_eq!(players.len(), 1);
        assert_eq!(players[0].display_name, "Mio");
    }

    #[test]
    fn relog_command_clears_a_log_error_and_reads_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let photo_dir = dir.path().join("photos");
        std::fs::create_dir_all(&photo_dir).unwrap();
        let log_dir = dir.path().join("logs");
        let db_path = dir.path().join("library.db");
        let mut daemon = Daemon::open(&db_path, Some(photo_dir), Some(log_dir.clone())).unwrap();
        daemon.run_once().unwrap();

        std::fs::create_dir_all(&log_dir).unwrap();
        let log_path = log_dir.join("output_log_2026-10-21_21-00-00.txt");
        std::fs::write(
            &log_path,
            concat!(
                "2026.10.21 21:23:12 Debug      -  [Behaviour] Joining wrld_test:12345~region(jp)\n",
                "2026.10.21 21:23:13 Debug      -  [Behaviour] Joining or Creating Room: Test World\n",
            ),
        )
        .unwrap();
        {
            let db = Db::open(&db_path).unwrap();
            assert!(db.request_daemon_command(DaemonCommand::Relog).unwrap());
        }
        daemon.run_once().unwrap();

        let db = Db::open(&db_path).unwrap();
        assert!(db.daemon_errors().unwrap().is_empty());
        assert!(db.log_offset(log_path.to_str().unwrap()).unwrap().is_some());
    }

    #[test]
    fn command_reloads_settings_and_rebuilds_watchers() {
        let dir = tempfile::tempdir().unwrap();
        let old_root = dir.path().join("old");
        let new_root = dir.path().join("new");
        std::fs::create_dir_all(&old_root).unwrap();
        std::fs::create_dir_all(&new_root).unwrap();
        write_photo(&new_root.join("VRChat_2026-10-21_21-24-00.000_4x3.png"));
        let log_dir = dir.path().join("logs");
        std::fs::create_dir_all(&log_dir).unwrap();
        let db_path = dir.path().join("library.db");

        let mut daemon = Daemon::open(&db_path, Some(old_root), Some(log_dir)).unwrap();
        {
            let db = Db::open(&db_path).unwrap();
            let mut settings = AppSettings::load(&db).unwrap();
            settings.photo_roots = vec![new_root.to_string_lossy().to_string()];
            settings.save(&db).unwrap();
            assert!(db.request_daemon_command(DaemonCommand::Rescan).unwrap());
        }
        daemon.run_once().unwrap();

        let db = Db::open(&db_path).unwrap();
        assert_eq!(db.count_photos().unwrap(), 1);
        assert_eq!(daemon.watchers.len(), 1);
        assert!(db.daemon_errors().unwrap().is_empty());
    }

    #[test]
    fn poll_commands_preserves_buffered_watcher_events() {
        let dir = tempfile::tempdir().unwrap();
        let mut daemon = test_daemon(&dir);
        let photo_dir = dir.path().join("photos");
        // Establish the real watchers so the root set is recorded, then
        // stand in for the channel with an event already buffered.
        daemon.restart_watchers();
        assert_eq!(daemon.watcher_roots, vec![photo_dir.clone()]);
        let (tx, rx) = std::sync::mpsc::channel();
        daemon.watcher_rx = Some(rx);
        let create = |path: PathBuf| -> notify::Result<notify::Event> {
            Ok(
                notify::Event::new(notify::EventKind::Create(notify::event::CreateKind::File))
                    .add_path(path),
            )
        };
        let first = photo_dir.join("VRChat_2026-10-21_21-24-00.000_4x3.png");
        tx.send(create(first.clone())).unwrap();
        {
            let db = Db::open(dir.path().join("library.db")).unwrap();
            assert!(db.request_daemon_command(DaemonCommand::Relog).unwrap());
        }
        daemon.poll_commands().unwrap();

        // The buffered event was drained, not dropped with a rebuilt
        // channel, and the unchanged roots keep the same channel alive.
        assert_eq!(daemon.dirty_paths, vec![first]);
        let second = photo_dir.join("VRChat_2026-10-21_21-25-00.000_4x3.png");
        tx.send(create(second.clone())).unwrap();
        daemon.poll_watchers();
        assert_eq!(daemon.dirty_paths.len(), 2);
        assert_eq!(daemon.dirty_paths[1], second);
    }

    #[test]
    fn poll_commands_drains_watcher_events_before_a_rebuild() {
        let dir = tempfile::tempdir().unwrap();
        let mut daemon = test_daemon(&dir);
        let photo_dir = dir.path().join("photos");
        daemon.restart_watchers();
        let (tx, rx) = std::sync::mpsc::channel();
        daemon.watcher_rx = Some(rx);
        let pending = photo_dir.join("VRChat_2026-10-21_21-24-00.000_4x3.png");
        tx.send(Ok(notify::Event::new(notify::EventKind::Create(
            notify::event::CreateKind::File,
        ))
        .add_path(pending.clone())))
            .unwrap();
        // Point the settings at a different root so the batch rebuilds.
        let new_root = dir.path().join("new");
        std::fs::create_dir_all(&new_root).unwrap();
        {
            let db = Db::open(dir.path().join("library.db")).unwrap();
            let mut settings = AppSettings::load(&db).unwrap();
            settings.photo_roots = vec![new_root.to_string_lossy().to_string()];
            settings.save(&db).unwrap();
            assert!(db.request_daemon_command(DaemonCommand::Relog).unwrap());
        }
        daemon.poll_commands().unwrap();

        assert_eq!(daemon.dirty_paths, vec![pending]);
        assert_eq!(daemon.watcher_roots, vec![new_root]);
    }

    #[test]
    fn watcher_failure_survives_clean_scans_until_watchers_recover() {
        let dir = tempfile::tempdir().unwrap();
        let mut daemon = test_daemon(&dir);
        write_photo(&original_path(&dir));
        let db_path = dir.path().join("library.db");
        // Seed the state a failed watcher start leaves behind.
        let watcher_error = LocalizedError::new("daemon.watcher_failed");
        daemon.watcher_error = Some((watcher_error.clone(), Some("boom".to_string())));
        daemon
            .report_localized_error(DaemonOperation::Scan, watcher_error, Some("boom"))
            .unwrap();

        // A clean full scan must not erase the watcher failure.
        daemon.scan_photos().unwrap();
        {
            let db = Db::open(&db_path).unwrap();
            let errors = db.daemon_errors().unwrap();
            assert_eq!(errors.len(), 1);
            assert_eq!(
                errors[0].message_key.as_deref(),
                Some("daemon.watcher_failed")
            );
        }

        // A scan failure takes the slot; the next clean scan clears it and
        // surfaces the still-unrecovered watcher failure again.
        daemon
            .report_localized_error(
                DaemonOperation::Scan,
                LocalizedError::new("daemon.scan_failed").arg("count", 1),
                Some("detail"),
            )
            .unwrap();
        daemon.scan_photos().unwrap();
        {
            let db = Db::open(&db_path).unwrap();
            let errors = db.daemon_errors().unwrap();
            assert_eq!(errors.len(), 1);
            assert_eq!(
                errors[0].message_key.as_deref(),
                Some("daemon.watcher_failed")
            );
        }

        // Once the watchers start, the failure clears.
        daemon.restart_watchers();
        let db = Db::open(&db_path).unwrap();
        assert!(db.daemon_errors().unwrap().is_empty());
    }

    #[test]
    fn run_once_generates_thumbnails_for_command_scans() {
        let dir = tempfile::tempdir().unwrap();
        let mut daemon = test_daemon(&dir);
        let cache = dir.path().join("thumbnails");
        std::fs::create_dir_all(&cache).unwrap();
        daemon.set_thumbnail_dir(cache.clone());
        write_photo(&original_path(&dir));
        {
            let db = Db::open(dir.path().join("library.db")).unwrap();
            assert!(db.request_daemon_command(DaemonCommand::Rescan).unwrap());
        }

        daemon.run_once().unwrap();

        // The Rescan command indexed the photo, so run_once's own scan had
        // nothing new; the thumbnail still exists.
        let db = Db::open(dir.path().join("library.db")).unwrap();
        assert_eq!(db.count_photos().unwrap(), 1);
        assert!(
            thumbnail::thumbnail_path_in(&cache, 1, daemon.settings().thumbnail_spec()).exists()
        );
    }

    #[test]
    fn scan_marks_missing_only_for_roots_that_scanned_cleanly() {
        let dir = tempfile::tempdir().unwrap();
        let mut daemon = test_daemon(&dir);
        let healthy = dir.path().join("photos");
        let deleted = healthy.join("VRChat_2026-10-21_21-24-00.000_4x3.png");
        write_photo(&deleted);
        // A "root" that is a plain file fails as a root; the photo indexed
        // under it earlier must not be marked missing by the healthy
        // root's clean diff.
        let broken_root = dir.path().join("broken");
        std::fs::write(&broken_root, b"not a directory").unwrap();
        let broken_photo_id = {
            let db = daemon.db.lock().unwrap();
            let root_id = db
                .add_root(&broken_root.to_string_lossy(), "reference")
                .unwrap();
            db.upsert_photo(&vrc_photon_core::model::NewPhoto {
                root_id,
                rel_path: "lost.png".to_string(),
                abs_path: broken_root.join("lost.png").to_string_lossy().to_string(),
                file_name: "lost.png".to_string(),
                captured_at_utc: 1_700_000_000,
                captured_at_local: "2026-10-21 21:24:00".to_string(),
                tz_offset_sec: 0,
                width: 4,
                height: 3,
                file_size: 10,
                mtime: 1,
                blake3: None,
            })
            .unwrap()
        };
        daemon.settings.photo_roots = vec![
            broken_root.to_string_lossy().to_string(),
            healthy.to_string_lossy().to_string(),
        ];

        let first = daemon.scan_photos().unwrap();
        assert_eq!(first.indexed, 1);
        std::fs::remove_file(&deleted).unwrap();
        let report = daemon.scan_photos().unwrap();

        assert_eq!(report.failures.len(), 1);
        assert_eq!(report.failures[0].path, broken_root);
        let db = daemon.db.lock().unwrap();
        let healthy_photo = db
            .photo_by_path(deleted.to_str().unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(healthy_photo.status, PhotoStatus::Missing);
        let broken_photo = db.photo_by_id(broken_photo_id).unwrap().unwrap();
        assert_eq!(broken_photo.status, PhotoStatus::Present);
    }

    #[test]
    fn poll_commands_rescan_dispatches_scan_without_run_once() {
        let dir = tempfile::tempdir().unwrap();
        let photo_dir = dir.path().join("photos");
        let log_dir = dir.path().join("logs");
        std::fs::create_dir_all(&log_dir).unwrap();
        let db_path = dir.path().join("library.db");
        {
            let db = Db::open(&db_path).unwrap();
            db.record_daemon_error(DaemonOperation::Scan, "old scan", None)
                .unwrap();
            assert!(db.request_daemon_command(DaemonCommand::Rescan).unwrap());
        }

        let mut daemon = Daemon::open(&db_path, Some(photo_dir.clone()), Some(log_dir)).unwrap();
        std::fs::create_dir_all(&photo_dir).unwrap();
        write_photo(&photo_dir.join("VRChat_2026-10-21_21-24-00.000_4x3.png"));
        daemon.poll_commands().unwrap();

        let db = Db::open(&db_path).unwrap();
        assert_eq!(db.count_photos().unwrap(), 1);
        assert!(db.daemon_errors().unwrap().is_empty());
    }

    #[test]
    fn poll_commands_relog_dispatches_log_ingestion_without_run_once() {
        let dir = tempfile::tempdir().unwrap();
        let photo_dir = dir.path().join("photos");
        std::fs::create_dir_all(&photo_dir).unwrap();
        let log_dir = dir.path().join("logs");
        let db_path = dir.path().join("library.db");
        {
            let db = Db::open(&db_path).unwrap();
            db.record_daemon_error(DaemonOperation::Log, "old log", None)
                .unwrap();
            assert!(db.request_daemon_command(DaemonCommand::Relog).unwrap());
        }

        let mut daemon = Daemon::open(&db_path, Some(photo_dir), Some(log_dir.clone())).unwrap();
        std::fs::create_dir_all(&log_dir).unwrap();
        let log_path = log_dir.join("output_log_2026-10-21_21-00-00.txt");
        std::fs::write(
            &log_path,
            concat!(
                "2026.10.21 21:23:12 Debug      -  [Behaviour] Joining wrld_test:12345~region(jp)\n",
                "2026.10.21 21:23:13 Debug      -  [Behaviour] Joining or Creating Room: Test World\n",
                "2026.10.21 21:23:20 Debug      -  [Behaviour] OnPlayerJoined Mio (usr_a)\n",
            ),
        )
        .unwrap();
        daemon.poll_commands().unwrap();

        let db = Db::open(&db_path).unwrap();
        assert!(db.daemon_errors().unwrap().is_empty());
        assert!(db.log_offset(log_path.to_str().unwrap()).unwrap().is_some());
        let joined_at = vrc_photon_core::log_parser::parse_line(
            "2026.10.21 21:23:20 Debug      -  [Behaviour] OnPlayerJoined Mio (usr_a)",
        )
        .unwrap()
        .timestamp_utc();
        let session = db.session_at(joined_at).unwrap().expect("log ingested");
        let world = db.world_by_id(session.world_id).unwrap().unwrap();
        assert_eq!(world.name.as_deref(), Some("Test World"));
        assert_eq!(db.players_at(session.id, joined_at).unwrap().len(), 1);
    }

    #[test]
    fn poll_commands_relink_dispatches_linking_without_run_once() {
        let dir = tempfile::tempdir().unwrap();
        let photo_dir = dir.path().join("photos");
        std::fs::create_dir_all(&photo_dir).unwrap();
        let log_dir = dir.path().join("logs");
        std::fs::create_dir_all(&log_dir).unwrap();
        let db_path = dir.path().join("library.db");
        let photo_path = photo_dir.join("VRChat_2026-10-21_21-24-00.000_4x3.png");
        write_photo(&photo_path);

        let photo_id = {
            let db = Db::open(&db_path).unwrap();
            let mut ingestor = LogIngestor::new("output_log_2026-10-21_21-00-00.txt");
            for line in [
                "2026.10.21 21:23:12 Debug      -  [Behaviour] Joining wrld_test:12345~region(jp)",
                "2026.10.21 21:23:13 Debug      -  [Behaviour] Joining or Creating Room: Test World",
                "2026.10.21 21:23:20 Debug      -  [Behaviour] OnPlayerJoined Mio (usr_a)",
            ] {
                ingestor.ingest_line(&db, line).unwrap();
            }
            let captured_at = vrc_photon_core::log_parser::parse_line(
                "2026.10.21 21:24:00 Debug      -  [Behaviour] Unloading scenes",
            )
            .unwrap()
            .timestamp_utc();
            let root = db
                .add_root(photo_dir.to_str().unwrap(), "reference")
                .unwrap();
            let file_name = photo_path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .to_string();
            let photo_id = db
                .upsert_photo(&vrc_photon_core::model::NewPhoto {
                    root_id: root,
                    rel_path: file_name.clone(),
                    abs_path: photo_path.to_string_lossy().to_string(),
                    file_name,
                    captured_at_utc: captured_at,
                    captured_at_local: "2026-10-21 21:24:00.000".to_string(),
                    tz_offset_sec: 9 * 3600,
                    width: 16,
                    height: 10,
                    file_size: 1,
                    mtime: captured_at,
                    blake3: None,
                })
                .unwrap();
            assert!(db.photo_context(photo_id).unwrap().is_none());
            assert!(db.request_daemon_command(DaemonCommand::Relink).unwrap());
            photo_id
        };

        let mut daemon = Daemon::open(&db_path, Some(photo_dir), Some(log_dir)).unwrap();
        daemon.poll_commands().unwrap();

        let db = Db::open(&db_path).unwrap();
        let context = db
            .photo_context(photo_id)
            .unwrap()
            .expect("relink linked the photo");
        assert_eq!(context.world_name.as_deref(), Some("Test World"));
        let players = db.photo_players(photo_id).unwrap();
        assert_eq!(players.len(), 1);
        assert_eq!(players[0].display_name, "Mio");
    }

    #[test]
    fn rescan_command_retries_a_photo_in_link_backoff() {
        let dir = tempfile::tempdir().unwrap();
        let photo_dir = dir.path().join("photos");
        let log_dir = dir.path().join("logs");
        std::fs::create_dir_all(&photo_dir).unwrap();
        std::fs::create_dir_all(&log_dir).unwrap();
        let db_path = dir.path().join("library.db");
        let captured_at = vrc_photon_core::log_parser::parse_line(
            "2026.10.21 21:24:00 Debug      -  [Behaviour] Unloading scenes",
        )
        .unwrap()
        .timestamp_utc();

        let photo_id = {
            let db = Db::open(&db_path).unwrap();
            let mut ingestor = LogIngestor::new("output_log_2026-10-21_21-00-00.txt");
            for line in [
                "2026.10.21 21:23:12 Debug      -  [Behaviour] Joining wrld_test:12345~region(jp)",
                "2026.10.21 21:23:13 Debug      -  [Behaviour] Joining or Creating Room: Test World",
            ] {
                ingestor.ingest_line(&db, line).unwrap();
            }
            let photo_id = seed_photo_with_link_backoff(&db, &photo_dir, captured_at);
            assert!(db.photo_context(photo_id).unwrap().is_none());
            assert!(db.request_daemon_command(DaemonCommand::Rescan).unwrap());
            photo_id
        };

        let mut daemon = Daemon::open(&db_path, Some(photo_dir), Some(log_dir)).unwrap();
        daemon.poll_commands().unwrap();

        let db = Db::open(&db_path).unwrap();
        assert!(db.photo_context(photo_id).unwrap().is_some());
    }

    #[test]
    fn relog_command_retries_a_photo_in_link_backoff() {
        let dir = tempfile::tempdir().unwrap();
        let photo_dir = dir.path().join("photos");
        let log_dir = dir.path().join("logs");
        std::fs::create_dir_all(&photo_dir).unwrap();
        std::fs::create_dir_all(&log_dir).unwrap();
        let db_path = dir.path().join("library.db");
        let captured_at = vrc_photon_core::log_parser::parse_line(
            "2026.10.21 21:24:00 Debug      -  [Behaviour] Unloading scenes",
        )
        .unwrap()
        .timestamp_utc();

        let photo_id = {
            let db = Db::open(&db_path).unwrap();
            let photo_id = seed_photo_with_link_backoff(&db, &photo_dir, captured_at);
            assert!(db.photo_context(photo_id).unwrap().is_none());
            assert!(db.request_daemon_command(DaemonCommand::Relog).unwrap());
            photo_id
        };
        std::fs::write(
            log_dir.join("output_log_2026-10-21_21-00-00.txt"),
            concat!(
                "2026.10.21 21:23:12 Debug      -  [Behaviour] Joining wrld_test:12345~region(jp)\n",
                "2026.10.21 21:23:13 Debug      -  [Behaviour] Joining or Creating Room: Test World\n",
            ),
        )
        .unwrap();

        let mut daemon = Daemon::open(&db_path, Some(photo_dir), Some(log_dir)).unwrap();
        daemon.poll_commands().unwrap();

        let db = Db::open(&db_path).unwrap();
        assert!(db.photo_context(photo_id).unwrap().is_some());
    }

    #[test]
    fn run_once_consumes_queued_commands() {
        let dir = tempfile::tempdir().unwrap();
        let photo_dir = dir.path().join("photos");
        let log_dir = dir.path().join("logs");
        std::fs::create_dir_all(&photo_dir).unwrap();
        std::fs::create_dir_all(&log_dir).unwrap();
        let db_path = dir.path().join("library.db");
        let mut daemon = Daemon::open(&db_path, Some(photo_dir), Some(log_dir)).unwrap();
        {
            let db = Db::open(&db_path).unwrap();
            assert!(db.request_daemon_command(DaemonCommand::Rescan).unwrap());
        }

        daemon.run_once().unwrap();

        let db = Db::open(&db_path).unwrap();
        assert!(db.take_daemon_commands().unwrap().is_empty());
    }

    #[test]
    fn failed_job_can_be_retried_and_completed() {
        let dir = tempfile::tempdir().unwrap();
        let photo_dir = dir.path().join("photos");
        let log_dir = dir.path().join("logs");
        std::fs::create_dir_all(&photo_dir).unwrap();
        std::fs::create_dir_all(&log_dir).unwrap();
        let photo_path = photo_dir.join("VRChat_2026-10-21_21-24-00.000_4x3.png");
        write_photo(&photo_path);
        let db_path = dir.path().join("library.db");
        {
            let db = Db::open(&db_path).unwrap();
            db.save_profile(&CompressionProfile {
                id: "p".to_string(),
                name: "retry".to_string(),
                format: vrc_photon_core::model::CompressionFormat::Webp,
                suffix: "_retry".to_string(),
                ..CompressionProfile::default()
            })
            .unwrap();
        }

        let mut daemon = Daemon::open(&db_path, Some(photo_dir), Some(log_dir)).unwrap();
        daemon.set_thumbnail_dir(dir.path().join("thumbnails"));
        daemon.run_once().unwrap();

        let job = {
            let db = Db::open(&db_path).unwrap();
            let photo = db
                .photo_by_path(photo_path.to_str().unwrap())
                .unwrap()
                .unwrap();
            let job = db.enqueue_job(photo.id, "p", "keep").unwrap();
            db.update_job(&job, QueueStatus::Failed, 0.0, Some("boom"), None)
                .unwrap();
            assert!(db.retry_job(&job).unwrap());
            job
        };
        daemon.compress_pending().unwrap();

        let db = Db::open(&db_path).unwrap();
        let job = db
            .list_jobs(10)
            .unwrap()
            .into_iter()
            .find(|row| row.id == job)
            .unwrap();
        assert_eq!(job.status, QueueStatus::Completed);
        assert!(db.derivatives_for_photo(job.photo_id).unwrap().len() == 1);
    }

    #[cfg(unix)]
    #[test]
    fn scan_failure_does_not_mark_unseen_photos_missing() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let photo_dir = dir.path().join("photos");
        let log_dir = dir.path().join("logs");
        std::fs::create_dir_all(&photo_dir).unwrap();
        std::fs::create_dir_all(&log_dir).unwrap();
        let photo_path = photo_dir.join("VRChat_2026-10-21_21-24-00.000_4x3.png");
        write_photo(&photo_path);
        let db_path = dir.path().join("library.db");
        let mut daemon = Daemon::open(&db_path, Some(photo_dir.clone()), Some(log_dir)).unwrap();
        daemon.set_thumbnail_dir(dir.path().join("thumbnails"));
        daemon.run_once().unwrap();

        let db = Db::open(&db_path).unwrap();
        let photo = db
            .photo_by_path(photo_path.to_str().unwrap())
            .unwrap()
            .unwrap();
        drop(db);

        let locked = photo_dir.join("locked");
        std::fs::create_dir(&locked).unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::read_dir(&locked).is_ok() {
            // Privileged environments bypass directory permissions, so the
            // unreadable-subtree failure cannot be simulated here.
            std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
            std::fs::remove_dir(&locked).unwrap();
            eprintln!("skipping permission lock: current user can read mode 000");
            return;
        }
        std::fs::remove_file(&photo_path).unwrap();
        daemon.run_once().unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();

        let db = Db::open(&db_path).unwrap();
        let photo = db.photo_by_id(photo.id).unwrap().unwrap();
        assert_eq!(photo.status, vrc_photon_core::model::PhotoStatus::Present);
    }

    #[test]
    fn cancelled_scan_does_not_mark_unseen_photos_missing() {
        let dir = tempfile::tempdir().unwrap();
        let photo_dir = dir.path().join("photos");
        let log_dir = dir.path().join("logs");
        std::fs::create_dir_all(&photo_dir).unwrap();
        std::fs::create_dir_all(&log_dir).unwrap();
        let photo_path = photo_dir.join("VRChat_2026-10-21_21-24-00.000_4x3.png");
        write_photo(&photo_path);
        let db_path = dir.path().join("library.db");
        let mut daemon = Daemon::open(&db_path, Some(photo_dir), Some(log_dir)).unwrap();
        daemon.set_thumbnail_dir(dir.path().join("thumbnails"));
        daemon.run_once().unwrap();

        let db = Db::open(&db_path).unwrap();
        let photo = db
            .photo_by_path(photo_path.to_str().unwrap())
            .unwrap()
            .unwrap();
        drop(db);

        std::fs::remove_file(&photo_path).unwrap();
        daemon.running.store(false, Ordering::Relaxed);
        daemon.run_once().unwrap();

        let db = Db::open(&db_path).unwrap();
        let photo = db.photo_by_id(photo.id).unwrap().unwrap();
        assert_eq!(photo.status, vrc_photon_core::model::PhotoStatus::Present);
    }

    #[cfg(unix)]
    #[test]
    fn unstatable_photo_keeps_the_existing_row_present() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let photo_dir = dir.path().join("photos");
        let locked = photo_dir.join("locked");
        std::fs::create_dir_all(&locked).unwrap();
        let log_dir = dir.path().join("logs");
        std::fs::create_dir_all(&log_dir).unwrap();
        let photo_path = locked.join("VRChat_2026-10-21_21-24-00.000_4x3.png");
        write_photo(&photo_path);
        let db_path = dir.path().join("library.db");
        let mut daemon = Daemon::open(&db_path, Some(photo_dir), Some(log_dir)).unwrap();
        daemon.set_thumbnail_dir(dir.path().join("thumbnails"));
        daemon.run_once().unwrap();

        let photo = {
            let db = Db::open(&db_path).unwrap();
            db.photo_by_path(photo_path.to_str().unwrap())
                .unwrap()
                .expect("photo indexed")
        };

        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o444)).unwrap();
        let child_type = std::fs::read_dir(&locked)
            .ok()
            .and_then(|mut entries| entries.next())
            .and_then(Result::ok)
            .and_then(|entry| entry.file_type().ok());
        if std::fs::metadata(&photo_path).is_ok() || !child_type.is_some_and(|kind| kind.is_file())
        {
            // Root and some filesystems ignore the missing execute bit, so the
            // stat failure cannot be simulated here.
            std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
            eprintln!("skipping stat lock: cannot simulate a listable but unstatable directory");
            return;
        }

        daemon.run_once().unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();

        let db = Db::open(&db_path).unwrap();
        let photo = db.photo_by_id(photo.id).unwrap().unwrap();
        assert_eq!(photo.status, vrc_photon_core::model::PhotoStatus::Present);
        let errors = db.daemon_errors().unwrap();
        assert_eq!(errors.len(), 1, "errors: {errors:?}");
        assert_eq!(errors[0].operation, DaemonOperation::Scan);
        assert!(
            errors[0]
                .detail
                .as_deref()
                .unwrap_or_default()
                .contains("VRChat_2026-10-21_21-24-00.000_4x3.png"),
            "detail should name the unreadable photo: {:?}",
            errors[0].detail
        );
    }

    #[test]
    fn end_to_end_scan_log_and_link() {
        let dir = tempfile::tempdir().unwrap();
        let photo_dir = dir.path().join("photos");
        let log_dir = dir.path().join("logs");
        std::fs::create_dir_all(&photo_dir).unwrap();
        std::fs::create_dir_all(&log_dir).unwrap();

        let photo_path = photo_dir.join("VRChat_2026-10-21_21-24-00.000_4x3.png");
        image::RgbaImage::from_pixel(4, 3, image::Rgba([10, 20, 30, 255]))
            .save(&photo_path)
            .unwrap();

        let log_path = log_dir.join("output_log_2026-10-21_21-00-00.txt");
        let log = concat!(
            "2026.10.21 21:23:12 Debug      -  [Behaviour] Joining wrld_test:12345~region(jp)\n",
            "2026.10.21 21:23:13 Debug      -  [Behaviour] Joining or Creating Room: Test World\n",
            "2026.10.21 21:23:20 Debug      -  [Behaviour] OnPlayerJoined Mio (usr_a)\n",
        );
        std::fs::write(&log_path, log).unwrap();

        let db_path = dir.path().join("library.db");
        let mut daemon = Daemon::open(&db_path, Some(photo_dir), Some(log_dir)).unwrap();
        daemon.set_thumbnail_dir(dir.path().join("thumbnails"));
        daemon.run_once().unwrap();

        let db = Db::open(&db_path).unwrap();
        let photo = db
            .photo_by_path(photo_path.to_str().unwrap())
            .unwrap()
            .expect("photo indexed");
        let context = db.photo_context(photo.id).unwrap().expect("photo linked");
        assert_eq!(context.world_name.as_deref(), Some("Test World"));
        let players = db.photo_players(photo.id).unwrap();
        assert_eq!(players.len(), 1);
        assert_eq!(players[0].display_name, "Mio");

        std::fs::remove_file(&photo_path).unwrap();
        daemon.run_once().unwrap();
        let photo = db.photo_by_id(photo.id).unwrap().unwrap();
        assert_eq!(photo.status, vrc_photon_core::model::PhotoStatus::Missing);
    }

    #[test]
    fn startup_repairs_open_sessions_at_the_log_mtime() {
        let dir = tempfile::tempdir().unwrap();
        let photo_dir = dir.path().join("photos");
        let log_dir = dir.path().join("logs");
        std::fs::create_dir_all(&photo_dir).unwrap();
        std::fs::create_dir_all(&log_dir).unwrap();
        let log_path = log_dir.join("output_log_2026-10-21_21-00-00.txt");
        std::fs::write(&log_path, "").unwrap();
        let mtime: i64 = 1_700_000_500;
        let file = File::options().write(true).open(&log_path).unwrap();
        file.set_modified(std::time::UNIX_EPOCH + Duration::from_secs(mtime as u64))
            .unwrap();

        let db_path = dir.path().join("library.db");
        {
            let db = Db::open(&db_path).unwrap();
            let world = db
                .upsert_world("wrld_test", Some("Test World"), Some("log"))
                .unwrap();
            let session = db
                .create_session(log_path.to_str().unwrap(), mtime - 120, world, None)
                .unwrap();
            db.upsert_session_player(session, "usr_a", "Mio", mtime - 60)
                .unwrap();
        }

        let mut daemon = Daemon::open(&db_path, Some(photo_dir), Some(log_dir)).unwrap();
        // Inject the process state so this test is deterministic on a machine
        // that happens to have VRChat running.
        assert_eq!(
            daemon
                .repair_open_sessions_with_running(Some(false))
                .unwrap(),
            1
        );

        let db = Db::open(&db_path).unwrap();
        assert!(db.open_sessions().unwrap().is_empty());
        let session = db.session_at(mtime - 60).unwrap().unwrap();
        assert_eq!(session.ended_at, Some(mtime));
    }

    #[test]
    fn resume_decision_requires_the_latest_log_and_a_live_vrchat() {
        let latest = Path::new("/logs/output_log_2026-10-21_21-00-00.txt");

        assert!(should_resume_open_session(latest, Some(latest), Some(true)));
        assert!(!should_resume_open_session(
            Path::new("/logs/output_log_2026-10-20_21-00-00.txt"),
            Some(latest),
            Some(true)
        ));
        assert!(!should_resume_open_session(
            latest,
            Some(latest),
            Some(false)
        ));
        assert!(!should_resume_open_session(latest, Some(latest), None));
        assert!(!should_resume_open_session(latest, None, Some(true)));
    }

    #[test]
    fn restart_mid_session_resumes_and_applies_a_later_player_leave() {
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("logs/output_log_2026-10-21_21-00-00.txt");
        let join =
            "2026.10.21 21:23:12 Debug      -  [Behaviour] Joining wrld_test:12345~region(jp)";
        let name =
            "2026.10.21 21:23:13 Debug      -  [Behaviour] Joining or Creating Room: Test World";
        let player = "2026.10.21 21:23:20 Debug      -  [Behaviour] OnPlayerJoined Mio (usr_a)";
        let leave =
            "2026.10.21 21:25:00 Debug      -  [Behaviour] OnPlayerLeft Mio Renamed (usr_a)";

        let mut first = test_daemon(&dir);
        std::fs::write(&log_path, format!("{join}\n{name}\n{player}\n")).unwrap();
        let log_file = log_path.to_str().unwrap();
        first.ingest_logs().unwrap();
        let session_id = {
            let db = first.db.lock().unwrap();
            let session = db
                .session_at(
                    vrc_photon_core::log_parser::parse_line(join)
                        .unwrap()
                        .timestamp_utc()
                        + 1,
                )
                .unwrap()
                .unwrap();
            assert_eq!(db.open_sessions().unwrap().len(), 1);
            session.id
        };
        let offset_before_restart = {
            let db = first.db.lock().unwrap();
            db.log_offset(log_file).unwrap().unwrap()
        };
        // Simulate a crash: dropping the daemon must not close the session.
        drop(first);

        let mut file = File::options().append(true).open(&log_path).unwrap();
        writeln!(file, "{leave}").unwrap();
        drop(file);

        let mut second = test_daemon(&dir);
        assert_eq!(
            second
                .repair_open_sessions_with_running(Some(true))
                .unwrap(),
            0,
            "the live session must be kept open"
        );
        assert_eq!(
            second.db.lock().unwrap().open_sessions().unwrap()[0].id,
            session_id
        );
        assert!(second.log_tail.is_none(), "repair must not replay the log");

        second.ingest_logs().unwrap();

        let left_at = vrc_photon_core::log_parser::parse_line(leave)
            .unwrap()
            .timestamp_utc();
        let db = second.db.lock().unwrap();
        let players = db.players_at(session_id, left_at - 1).unwrap();
        assert_eq!(players.len(), 1);
        assert_eq!(players[0].display_name, "Mio Renamed");
        assert_eq!(players[0].left_at, Some(left_at));
        assert_eq!(db.open_sessions().unwrap()[0].id, session_id);
        assert_eq!(
            db.log_offset(log_file).unwrap(),
            Some(offset_before_restart + leave.len() as i64 + 1),
            "ingestion resumes after the persisted offset"
        );
        assert_eq!(
            second.log_tail.as_ref().unwrap().ingestor.session_id(),
            Some(session_id)
        );
    }

    #[test]
    fn startup_repair_falls_back_to_the_ingested_log_mtime() {
        let dir = tempfile::tempdir().unwrap();
        let photo_dir = dir.path().join("photos");
        let log_dir = dir.path().join("logs");
        std::fs::create_dir_all(&photo_dir).unwrap();
        std::fs::create_dir_all(&log_dir).unwrap();
        let missing_log = log_dir.join("output_log_deleted.txt");
        let db_path = dir.path().join("library.db");
        {
            let db = Db::open(&db_path).unwrap();
            let world = db.upsert_world("wrld_test", None, None).unwrap();
            db.create_session(missing_log.to_str().unwrap(), 100, world, None)
                .unwrap();
            db.set_log_offset(missing_log.to_str().unwrap(), 10, 500, 10, "1")
                .unwrap();
        }

        let mut daemon = Daemon::open(&db_path, Some(photo_dir), Some(log_dir)).unwrap();
        assert_eq!(daemon.repair_open_sessions().unwrap(), 1);

        let db = Db::open(&db_path).unwrap();
        let session = db.session_at(200).unwrap().unwrap();
        assert_eq!(session.ended_at, Some(500));
    }

    #[test]
    fn maintenance_run_does_not_repair_open_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let photo_dir = dir.path().join("photos");
        let log_dir = dir.path().join("logs");
        std::fs::create_dir_all(&photo_dir).unwrap();
        std::fs::create_dir_all(&log_dir).unwrap();
        let db_path = dir.path().join("library.db");
        {
            let db = Db::open(&db_path).unwrap();
            let world = db.upsert_world("wrld_test", None, None).unwrap();
            db.create_session("output_log_live.txt", 100, world, None)
                .unwrap();
        }

        let mut daemon =
            Daemon::open_preserving_running_deletions(&db_path, Some(photo_dir), Some(log_dir))
                .unwrap();
        daemon.run_once().unwrap();

        let db = Db::open(&db_path).unwrap();
        assert_eq!(db.open_sessions().unwrap().len(), 1);
    }

    #[test]
    fn vrchat_exit_closes_the_session_at_the_log_mtime_after_consecutive_misses() {
        let dir = tempfile::tempdir().unwrap();
        let mut daemon = test_daemon(&dir);
        let log_path = dir.path().join("logs/output_log_2026-10-21_21-00-00.txt");
        let join =
            "2026.10.21 21:23:12 Debug      -  [Behaviour] Joining wrld_test:12345~region(jp)";
        let name =
            "2026.10.21 21:23:13 Debug      -  [Behaviour] Joining or Creating Room: Test World";
        std::fs::write(&log_path, format!("{join}\n{name}\n")).unwrap();
        daemon.ingest_logs().unwrap();
        let started = vrc_photon_core::log_parser::parse_line(join)
            .unwrap()
            .timestamp_utc();
        let mtime = started + 120;
        let file = File::options().write(true).open(&log_path).unwrap();
        file.set_modified(std::time::UNIX_EPOCH + Duration::from_secs(mtime as u64))
            .unwrap();
        assert!(
            daemon
                .log_tail
                .as_ref()
                .unwrap()
                .ingestor
                .session_id()
                .is_some()
        );

        daemon.observe_vrchat_presence(Some(false)).unwrap();
        daemon.observe_vrchat_presence(Some(false)).unwrap();
        daemon.observe_vrchat_presence(Some(true)).unwrap();
        daemon.observe_vrchat_presence(Some(false)).unwrap();
        daemon.observe_vrchat_presence(Some(false)).unwrap();
        {
            let db = daemon.db.lock().unwrap();
            assert!(db.session_at(started + 1).unwrap().is_some());
        }
        daemon.observe_vrchat_presence(Some(false)).unwrap();

        assert!(
            daemon
                .log_tail
                .as_ref()
                .unwrap()
                .ingestor
                .session_id()
                .is_none()
        );
        let db = daemon.db.lock().unwrap();
        let session = db.session_at(started + 1).unwrap().unwrap();
        assert_eq!(session.ended_at, Some(mtime));
        drop(db);
        let status = daemon.status.lock().unwrap();
        assert_eq!(status.world_name, None);
    }

    #[test]
    fn photos_imported_after_vrchat_exit_do_not_link_to_the_closed_session() {
        let dir = tempfile::tempdir().unwrap();
        let mut daemon = test_daemon(&dir);
        let log_path = dir.path().join("logs/output_log_2026-10-21_21-00-00.txt");
        let join =
            "2026.10.21 21:23:12 Debug      -  [Behaviour] Joining wrld_test:12345~region(jp)";
        let name =
            "2026.10.21 21:23:13 Debug      -  [Behaviour] Joining or Creating Room: Test World";
        std::fs::write(&log_path, format!("{join}\n{name}\n")).unwrap();
        daemon.ingest_logs().unwrap();
        let started = vrc_photon_core::log_parser::parse_line(join)
            .unwrap()
            .timestamp_utc();
        let mtime = started + 120;
        let file = File::options().write(true).open(&log_path).unwrap();
        file.set_modified(std::time::UNIX_EPOCH + Duration::from_secs(mtime as u64))
            .unwrap();
        for _ in 0..VRCHAT_MISSING_POLLS_BEFORE_CLOSE {
            daemon.observe_vrchat_presence(Some(false)).unwrap();
        }

        let photo_path = dir
            .path()
            .join("photos/VRChat_2026-10-21_21-26-00.000_4x3.png");
        write_photo(&photo_path);
        daemon.run_once().unwrap();

        let db = daemon.db.lock().unwrap();
        let photo = db
            .photo_by_path(photo_path.to_str().unwrap())
            .unwrap()
            .expect("photo indexed");
        assert!(db.photo_context(photo.id).unwrap().is_none());
    }

    #[test]
    fn runtime_close_uses_the_ingested_mtime_when_the_log_file_is_gone() {
        let dir = tempfile::tempdir().unwrap();
        let mut daemon = test_daemon(&dir);
        let log_path = dir.path().join("logs/output_log_2026-10-21_21-00-00.txt");
        let join =
            "2026.10.21 21:23:12 Debug      -  [Behaviour] Joining wrld_test:12345~region(jp)";
        std::fs::write(&log_path, format!("{join}\n")).unwrap();
        let started = vrc_photon_core::log_parser::parse_line(join)
            .unwrap()
            .timestamp_utc();
        let mtime = started + 60;
        let file = File::options().write(true).open(&log_path).unwrap();
        file.set_modified(std::time::UNIX_EPOCH + Duration::from_secs(mtime as u64))
            .unwrap();
        daemon.ingest_logs().unwrap();
        std::fs::remove_file(&log_path).unwrap();

        for _ in 0..VRCHAT_MISSING_POLLS_BEFORE_CLOSE {
            daemon.observe_vrchat_presence(Some(false)).unwrap();
        }

        let db = daemon.db.lock().unwrap();
        let session = db.session_at(started + 1).unwrap().unwrap();
        assert_eq!(session.ended_at, Some(mtime));
    }

    #[test]
    fn shutdown_uses_the_ingested_mtime_when_the_log_file_is_gone() {
        let dir = tempfile::tempdir().unwrap();
        let mut daemon = test_daemon(&dir);
        let log_path = dir.path().join("logs/output_log_2026-10-21_21-00-00.txt");
        let join =
            "2026.10.21 21:23:12 Debug      -  [Behaviour] Joining wrld_test:12345~region(jp)";
        std::fs::write(&log_path, format!("{join}\n")).unwrap();
        let started = vrc_photon_core::log_parser::parse_line(join)
            .unwrap()
            .timestamp_utc();
        let mtime = started + 60;
        let file = File::options().write(true).open(&log_path).unwrap();
        file.set_modified(std::time::UNIX_EPOCH + Duration::from_secs(mtime as u64))
            .unwrap();
        daemon.ingest_logs().unwrap();
        std::fs::remove_file(&log_path).unwrap();

        daemon.shutdown();

        let db = daemon.db.lock().unwrap();
        let session = db.session_at(started + 1).unwrap().unwrap();
        assert_eq!(session.ended_at, Some(mtime));
    }

    #[test]
    fn future_log_mtime_is_clamped_to_now() {
        let dir = tempfile::tempdir().unwrap();
        let mut daemon = test_daemon(&dir);
        let log_path = dir.path().join("logs/output_log_2026-10-21_21-00-00.txt");
        let join =
            "2026.10.21 21:23:12 Debug      -  [Behaviour] Joining wrld_test:12345~region(jp)";
        std::fs::write(&log_path, format!("{join}\n")).unwrap();
        daemon.ingest_logs().unwrap();
        let started = vrc_photon_core::log_parser::parse_line(join)
            .unwrap()
            .timestamp_utc();
        let now = chrono::Utc::now().timestamp();
        let future = now + 3600;
        let file = File::options().write(true).open(&log_path).unwrap();
        file.set_modified(std::time::UNIX_EPOCH + Duration::from_secs(future as u64))
            .unwrap();

        for _ in 0..VRCHAT_MISSING_POLLS_BEFORE_CLOSE {
            daemon.observe_vrchat_presence(Some(false)).unwrap();
        }

        let db = daemon.db.lock().unwrap();
        let session = db.session_at(started + 1).unwrap().unwrap();
        assert!(session.ended_at.unwrap() <= chrono::Utc::now().timestamp());
        assert!(db.session_at(now + 1).unwrap().is_none());
    }

    #[test]
    fn pending_misses_do_not_close_a_new_session() {
        let dir = tempfile::tempdir().unwrap();
        let mut daemon = test_daemon(&dir);
        let log_path = dir.path().join("logs/output_log_2026-10-21_21-00-00.txt");
        let first_join =
            "2026.10.21 21:23:12 Debug      -  [Behaviour] Joining wrld_first:12345~region(jp)";
        std::fs::write(&log_path, format!("{first_join}\n")).unwrap();
        daemon.ingest_logs().unwrap();
        daemon.observe_vrchat_presence(Some(false)).unwrap();
        daemon.observe_vrchat_presence(Some(false)).unwrap();

        let second_join =
            "2026.10.21 21:24:12 Debug      -  [Behaviour] Joining wrld_second:12345~region(jp)";
        let mut file = File::options().append(true).open(&log_path).unwrap();
        writeln!(file, "{second_join}").unwrap();
        daemon.ingest_logs().unwrap();
        daemon.observe_vrchat_presence(Some(false)).unwrap();

        let second_started = vrc_photon_core::log_parser::parse_line(second_join)
            .unwrap()
            .timestamp_utc();
        let db = daemon.db.lock().unwrap();
        assert!(
            db.session_at(second_started + 1)
                .unwrap()
                .unwrap()
                .ended_at
                .is_none()
        );
    }

    #[test]
    fn unknown_vrchat_presence_does_not_count_as_a_miss() {
        let dir = tempfile::tempdir().unwrap();
        let mut daemon = test_daemon(&dir);
        let log_path = dir.path().join("logs/output_log_2026-10-21_21-00-00.txt");
        let join =
            "2026.10.21 21:23:12 Debug      -  [Behaviour] Joining wrld_test:12345~region(jp)";
        std::fs::write(&log_path, format!("{join}\n")).unwrap();
        daemon.ingest_logs().unwrap();

        for _ in 0..10 {
            daemon.observe_vrchat_presence(None).unwrap();
        }

        daemon.observe_vrchat_presence(Some(false)).unwrap();
        daemon.observe_vrchat_presence(Some(false)).unwrap();
        daemon.observe_vrchat_presence(None).unwrap();
        daemon.observe_vrchat_presence(Some(false)).unwrap();

        let db = daemon.db.lock().unwrap();
        let started = vrc_photon_core::log_parser::parse_line(join)
            .unwrap()
            .timestamp_utc();
        assert!(
            db.session_at(started + 1)
                .unwrap()
                .unwrap()
                .ended_at
                .is_none()
        );
    }

    #[test]
    fn shutdown_closes_the_session_at_the_log_mtime() {
        let dir = tempfile::tempdir().unwrap();
        let mut daemon = test_daemon(&dir);
        let log_path = dir.path().join("logs/output_log_2026-10-21_21-00-00.txt");
        let join =
            "2026.10.21 21:23:12 Debug      -  [Behaviour] Joining wrld_test:12345~region(jp)";
        std::fs::write(&log_path, format!("{join}\n")).unwrap();
        daemon.ingest_logs().unwrap();
        let started = vrc_photon_core::log_parser::parse_line(join)
            .unwrap()
            .timestamp_utc();
        let mtime = started + 120;
        let file = File::options().write(true).open(&log_path).unwrap();
        file.set_modified(std::time::UNIX_EPOCH + Duration::from_secs(mtime as u64))
            .unwrap();

        daemon.shutdown();

        let db = daemon.db.lock().unwrap();
        let session = db.session_at(started + 1).unwrap().unwrap();
        assert_eq!(session.ended_at, Some(mtime));
    }

    #[test]
    fn session_end_time_handles_a_start_time_in_the_future() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path().join("library.db")).unwrap();
        let ended_at = session_end_time(&db, Path::new("/missing/log.txt"), 2_000, 1_000);
        assert_eq!(ended_at, 1_000);
    }

    fn save_daemon_settings(daemon: &Daemon, update: impl FnOnce(&mut AppSettings)) {
        let db = daemon.db.lock().unwrap();
        let mut settings = AppSettings::load(&db).unwrap_or_default();
        update(&mut settings);
        settings.save(&db).unwrap();
    }

    fn add_auto_profile(db_path: &Path) {
        let db = Db::open(db_path).unwrap();
        db.save_profile(&CompressionProfile {
            name: "test webp".to_string(),
            format: vrc_photon_core::model::CompressionFormat::Webp,
            auto_enabled: true,
            quality: 80,
            suffix: "_test".to_string(),
            ..CompressionProfile::default()
        })
        .unwrap();
    }

    fn write_photo(path: &Path) {
        image::RgbaImage::from_pixel(16, 10, image::Rgba([10, 20, 30, 255]))
            .save(path)
            .unwrap();
    }

    /// Stores a photo that is on disk but has a future link retry time, as if
    /// a previous batch failed to find its session.
    fn seed_photo_with_link_backoff(db: &Db, photo_dir: &Path, captured_at: i64) -> i64 {
        let photo_path = photo_dir.join("VRChat_2026-10-21_21-24-00.000_4x3.png");
        write_photo(&photo_path);
        let root = db
            .add_root(photo_dir.to_str().unwrap(), "reference")
            .unwrap();
        let file_name = photo_path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();
        let photo_id = db
            .upsert_photo(&NewPhoto {
                root_id: root,
                rel_path: file_name.clone(),
                abs_path: photo_path.to_string_lossy().to_string(),
                file_name,
                captured_at_utc: captured_at,
                captured_at_local: "2026-10-21 21:24:00.000".to_string(),
                tz_offset_sec: 9 * 3600,
                width: 16,
                height: 10,
                file_size: 1,
                mtime: captured_at,
                blake3: None,
            })
            .unwrap();
        db.record_link_attempt(photo_id, chrono::Utc::now().timestamp())
            .unwrap();
        photo_id
    }

    fn test_daemon(dir: &tempfile::TempDir) -> Daemon {
        let photo_dir = dir.path().join("photos");
        let log_dir = dir.path().join("logs");
        std::fs::create_dir_all(&photo_dir).unwrap();
        std::fs::create_dir_all(&log_dir).unwrap();
        let db_path = dir.path().join("library.db");
        let mut daemon = Daemon::open(&db_path, Some(photo_dir), Some(log_dir)).unwrap();
        daemon.set_ipc_endpoint(None);
        daemon
    }

    /// Adds a replaced photo whose display asset lives at `derivative`. The
    /// original and derivative files do not have to exist; callers decide what
    /// the verification pass should see on disk.
    fn add_replaced_photo(daemon: &Daemon, photo: &Path, derivative: &Path) -> i64 {
        let db = daemon.db.lock().unwrap();
        let root = db
            .add_root(
                photo.parent().unwrap().to_string_lossy().as_ref(),
                "reference",
            )
            .unwrap();
        let file_name = photo.file_name().unwrap().to_string_lossy().to_string();
        let id = db
            .upsert_photo(&NewPhoto {
                root_id: root,
                rel_path: file_name.clone(),
                abs_path: photo.to_string_lossy().to_string(),
                file_name,
                captured_at_utc: 100,
                captured_at_local: "2026-10-21 21:24:00.000".to_string(),
                tz_offset_sec: 9 * 3600,
                width: 16,
                height: 10,
                file_size: 1,
                mtime: 100,
                blake3: None,
            })
            .unwrap();
        let derivative = derivative.to_string_lossy().to_string();
        let derivative_id = db
            .add_derivative(id, &derivative, "webp", 16, 10, 1, "test", "replace")
            .unwrap();
        assert!(db.set_replaced(id, derivative_id).unwrap());
        id
    }

    #[test]
    fn daemon_ai_plugin_service_uses_injected_configuration() {
        let dir = tempfile::tempdir().unwrap();
        let mut daemon = test_daemon(&dir);
        let ai_dir = dir.path().join("ai");

        daemon.set_ai_data_dir(ai_dir);
        daemon.set_ai_plugin_keys(vec!["test-key".to_string()]);
        daemon.set_ai_http_client(Arc::new(UreqModelClient::new()));

        assert!(!daemon.plugin_service().is_ready("vrc-photon-autotag"));
    }

    const PLUGIN: &str = AI_PLUGIN_ID;

    /// A daemon with an isolated library and a test plugin that can be marked
    /// ready without spawning a sidecar.
    struct DaemonAiTest {
        _dir: tempfile::TempDir,
        daemon: Daemon,
        db: Arc<Mutex<Db>>,
        photo_dir: PathBuf,
        photo_seq: usize,
    }

    impl DaemonAiTest {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let photo_dir = dir.path().join("photos");
            let log_dir = dir.path().join("logs");
            std::fs::create_dir_all(&photo_dir).unwrap();
            std::fs::create_dir_all(&log_dir).unwrap();
            let db_path = dir.path().join("library.db");
            let mut daemon =
                Daemon::open(&db_path, Some(photo_dir.clone()), Some(log_dir)).unwrap();
            daemon.set_ipc_endpoint(None);
            daemon.set_thumbnail_dir(dir.path().join("thumbnails"));
            daemon.set_ai_data_dir(dir.path().join("ai"));
            let db = daemon.db.clone();
            Self {
                _dir: dir,
                daemon,
                db,
                photo_dir,
                photo_seq: 0,
            }
        }

        fn enable_ai(&mut self, enabled: bool, auto: bool) {
            save_daemon_settings(&self.daemon, |settings| {
                settings.ai_tagging_enabled = enabled;
                settings.ai_tagging_auto = auto;
            });
            self.daemon.settings.ai_tagging_enabled = enabled;
            self.daemon.settings.ai_tagging_auto = auto;
        }

        /// Stores the install, model state, and probe rows `is_ready` checks.
        /// The model files and sidecar are intentionally not created: the
        /// auto-enqueue paths only consult readiness.
        fn mark_plugin_ready(&self) {
            let manifest = sample_manifest();
            let now = chrono::Utc::now().to_rfc3339();
            let install = PluginInstallation {
                plugin_id: manifest.plugin_id.clone(),
                variant: manifest.variant.clone(),
                version: manifest.plugin_version.clone(),
                protocol_version: manifest.protocol_version,
                path: self
                    ._dir
                    .path()
                    .join("plugin")
                    .to_string_lossy()
                    .into_owned(),
                manifest_json: serde_json::to_string(&manifest).unwrap(),
                catalog_version: Some(manifest.catalog_version.clone()),
                active: true,
                installed_at: now.clone(),
                updated_at: now,
            };
            let probe = Probe {
                protocol_version: manifest.protocol_version,
                sidecar_version: "1.0.0".to_string(),
                plugin_id: manifest.plugin_id.clone(),
                variant: manifest.variant.clone(),
                catalog_version: manifest.catalog_version.clone(),
                backends: Vec::new(),
            };
            {
                let db = self.db.lock().unwrap();
                db.upsert_plugin_installation(&install).unwrap();
                db.set_plugin_model_state(
                    PLUGIN,
                    vrc_photon_core::ai::ModelState::Ready,
                    Some(&manifest.model.revision),
                    0,
                    None,
                    None,
                )
                .unwrap();
                db.set_plugin_probe_json(PLUGIN, Some(&serde_json::to_string(&probe).unwrap()))
                    .unwrap();
            }
            assert!(self.daemon.plugin_service().is_ready(PLUGIN));
        }

        fn add_photo(&mut self) -> PathBuf {
            self.photo_seq += 1;
            let path = self.photo_dir.join(format!(
                "VRChat_2026-10-21_21-{:02}-00.000_4x3.png",
                self.photo_seq
            ));
            write_photo(&path);
            path
        }

        fn scan(&mut self) -> ScanReport {
            self.daemon
                .scan_root_photos(self.photo_dir.clone())
                .unwrap()
        }

        fn add_photo_and_scan(&mut self) -> (i64, PathBuf) {
            let path = self.add_photo();
            let report = self.scan();
            assert_eq!(report.new_ids.len(), 1, "one new photo per call");
            (report.new_ids[0], path)
        }

        fn photo_id(&self, path: &Path) -> i64 {
            self.db
                .lock()
                .unwrap()
                .photo_by_path(path.to_str().unwrap())
                .unwrap()
                .unwrap()
                .id
        }

        fn tagging_counts(&self) -> JobStatusCounts {
            self.db.lock().unwrap().tagging_job_counts(PLUGIN).unwrap()
        }
    }

    #[test]
    fn auto_tagging_enqueues_only_new_photos_when_ready() {
        let mut ctx = DaemonAiTest::new();
        ctx.enable_ai(true, true);
        ctx.mark_plugin_ready();

        let (new_id, _) = ctx.add_photo_and_scan();
        assert_eq!(ctx.tagging_counts().pending, 1);
        assert_eq!(
            ctx.db
                .lock()
                .unwrap()
                .tagging_jobs_for_photo(new_id)
                .unwrap()
                .len(),
            1
        );

        ctx.add_photo_and_scan();
        assert_eq!(ctx.tagging_counts().pending, 2);

        // Rescanning the same photos must not duplicate their active jobs.
        ctx.scan();
        assert_eq!(ctx.tagging_counts().pending, 2);
        assert_eq!(
            ctx.db
                .lock()
                .unwrap()
                .tagging_jobs_for_photo(new_id)
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn auto_tagging_does_nothing_when_not_ready() {
        let mut ctx = DaemonAiTest::new();
        ctx.enable_ai(true, true);

        ctx.add_photo_and_scan();
        assert_eq!(ctx.tagging_counts().unfinished(), 0);

        ctx.daemon.refresh_status().unwrap();
        assert!(!ctx.daemon.status.lock().unwrap().tagging_ready);
    }

    #[test]
    fn auto_tagging_requires_enabled_and_auto_switches() {
        let mut ctx = DaemonAiTest::new();
        ctx.mark_plugin_ready();

        ctx.enable_ai(false, true);
        ctx.add_photo_and_scan();
        assert_eq!(ctx.tagging_counts().unfinished(), 0);

        ctx.enable_ai(true, false);
        ctx.add_photo_and_scan();
        assert_eq!(ctx.tagging_counts().unfinished(), 0);

        ctx.enable_ai(true, true);
        ctx.add_photo_and_scan();
        assert_eq!(ctx.tagging_counts().pending, 1);
    }

    #[test]
    fn watcher_new_photo_enqueues_tagging() {
        let mut ctx = DaemonAiTest::new();
        ctx.enable_ai(true, true);
        ctx.mark_plugin_ready();
        let path = ctx.add_photo();
        ctx.daemon.dirty_paths.push(path.clone());

        ctx.daemon.index_dirty_paths().unwrap();

        let photo_id = ctx.photo_id(&path);
        assert_eq!(ctx.tagging_counts().pending, 1);
        assert_eq!(
            ctx.db
                .lock()
                .unwrap()
                .tagging_jobs_for_photo(photo_id)
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn backlog_enqueue_is_waiting_until_released() {
        let mut ctx = DaemonAiTest::new();
        ctx.add_photo_and_scan();
        ctx.add_photo_and_scan();
        assert_eq!(
            ctx.db
                .lock()
                .unwrap()
                .enqueue_tagging_backlog(PLUGIN)
                .unwrap(),
            2
        );
        assert_eq!(ctx.tagging_counts().waiting, 2);
        assert_eq!(ctx.tagging_counts().pending, 0);

        assert_eq!(ctx.daemon.release_ai_backlog().unwrap(), 2);
        let counts = ctx.tagging_counts();
        assert_eq!(counts.waiting, 0);
        assert_eq!(counts.pending, 2);
    }

    #[test]
    fn release_backlog_wakes_the_worker() {
        let mut ctx = DaemonAiTest::new();
        let (photo_id, _) = ctx.add_photo_and_scan();
        ctx.db
            .lock()
            .unwrap()
            .enqueue_tagging_jobs(&[photo_id], PLUGIN, TaggingJobStatus::Waiting)
            .unwrap();
        let before = ctx.daemon.tagging.generation();

        assert_eq!(ctx.daemon.release_ai_backlog().unwrap(), 1);
        assert!(
            ctx.daemon.tagging.generation() > before,
            "release must notify the tagging worker"
        );
    }

    #[test]
    fn status_counts_include_tagging() {
        let mut ctx = DaemonAiTest::new();
        ctx.enable_ai(true, true);
        ctx.mark_plugin_ready();
        ctx.add_photo_and_scan();
        ctx.add_photo_and_scan();
        assert_eq!(ctx.tagging_counts().pending, 2);

        // A backlog job is unfinished too, even though the worker never claims
        // it until release.
        ctx.enable_ai(false, false);
        let (backlog_photo, _) = ctx.add_photo_and_scan();
        ctx.db
            .lock()
            .unwrap()
            .enqueue_tagging_jobs(&[backlog_photo], PLUGIN, TaggingJobStatus::Waiting)
            .unwrap();
        ctx.enable_ai(true, true);
        assert_eq!(ctx.tagging_counts().waiting, 1);
        assert_eq!(ctx.tagging_counts().unfinished(), 3);

        ctx.daemon.refresh_status().unwrap();
        {
            let mut status = ctx.daemon.status.lock().unwrap();
            assert_eq!(status.pending_tagging_jobs, 3);
            assert!(status.tagging_ready);
            let snapshot = status_snapshot(&mut status);
            assert_eq!(snapshot.pending_tagging_jobs, 3);
            assert!(snapshot.tagging_ready);
        }

        // A processing job still counts as unfinished.
        let claimed = {
            let db = ctx.db.lock().unwrap();
            db.next_pending_tagging_job().unwrap().unwrap()
        };
        assert_eq!(claimed.status, TaggingJobStatus::Processing);
        ctx.daemon.refresh_status().unwrap();
        assert_eq!(ctx.daemon.status.lock().unwrap().pending_tagging_jobs, 3);
    }

    #[test]
    fn run_once_does_not_start_the_tagging_worker() {
        let mut ctx = DaemonAiTest::new();
        ctx.enable_ai(true, true);
        ctx.mark_plugin_ready();
        ctx.add_photo();

        ctx.daemon.run_once().unwrap();

        assert!(ctx.daemon.tagging_handle.is_none());
        assert_eq!(ctx.tagging_counts().pending, 1);
    }

    #[test]
    fn start_workers_spawns_and_stops_the_tagging_worker() {
        let mut ctx = DaemonAiTest::new();
        ctx.daemon.start_workers();
        assert!(ctx.daemon.tagging_handle.is_some());
        ctx.daemon.stop_workers();
        assert!(ctx.daemon.tagging_handle.is_none());
    }

    #[test]
    fn startup_resumes_interrupted_tagging_jobs() {
        let dir = tempfile::tempdir().unwrap();
        let photo_dir = dir.path().join("photos");
        let log_dir = dir.path().join("logs");
        std::fs::create_dir_all(&photo_dir).unwrap();
        std::fs::create_dir_all(&log_dir).unwrap();
        let photo_path = photo_dir.join("VRChat_2026-10-21_21-24-00.000_4x3.png");
        write_photo(&photo_path);
        let db_path = dir.path().join("library.db");
        let job_id = {
            let db = Db::open(&db_path).unwrap();
            let root = db
                .add_root(photo_dir.to_str().unwrap(), "reference")
                .unwrap();
            let photo_id = db
                .upsert_photo(&NewPhoto {
                    root_id: root,
                    rel_path: photo_path
                        .file_name()
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                    abs_path: photo_path.to_string_lossy().into_owned(),
                    file_name: photo_path
                        .file_name()
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                    captured_at_utc: 1_700_000_000,
                    captured_at_local: "2026-10-21 21:24:00".to_string(),
                    tz_offset_sec: 9 * 3600,
                    width: 16,
                    height: 10,
                    file_size: 10,
                    mtime: 1_700_000_000,
                    blake3: None,
                })
                .unwrap();
            db.enqueue_tagging_jobs(&[photo_id], PLUGIN, TaggingJobStatus::Pending)
                .unwrap();
            db.next_pending_tagging_job().unwrap().unwrap().id
        };

        // A maintenance process must not steal the live daemon's jobs.
        let preserving = Daemon::open_preserving_running_deletions(
            &db_path,
            Some(photo_dir.clone()),
            Some(log_dir.clone()),
        )
        .unwrap();
        assert_eq!(
            preserving
                .db
                .lock()
                .unwrap()
                .tagging_job(&job_id)
                .unwrap()
                .unwrap()
                .status,
            TaggingJobStatus::Processing
        );
        drop(preserving);

        let owning = Daemon::open(&db_path, Some(photo_dir), Some(log_dir)).unwrap();
        assert_eq!(
            owning
                .db
                .lock()
                .unwrap()
                .tagging_job(&job_id)
                .unwrap()
                .unwrap()
                .status,
            TaggingJobStatus::Pending
        );
    }

    #[test]
    fn force_delete_cancels_tagging_before_claim() {
        let mut ctx = DaemonAiTest::new();
        let (photo_id, path) = ctx.add_photo_and_scan();
        let job_id = {
            let db = ctx.db.lock().unwrap();
            db.enqueue_tagging_jobs(&[photo_id], PLUGIN, TaggingJobStatus::Pending)
                .unwrap();
            db.next_pending_tagging_job().unwrap().unwrap().id
        };
        let request = {
            let db = ctx.db.lock().unwrap();
            db.request_deletion(&[photo_id], DeletionReason::Manual, true)
                .unwrap()[0]
        };

        ctx.daemon.process_deletions().unwrap();

        let db = ctx.db.lock().unwrap();
        assert_eq!(
            db.tagging_job(&job_id).unwrap().unwrap().status,
            TaggingJobStatus::Cancelled
        );
        assert_eq!(
            db.deletion_request_by_id(request).unwrap().unwrap().status,
            DeletionStatus::Completed
        );
        assert!(!path.exists(), "the forced deletion must remove the file");
    }

    #[test]
    fn forced_deletion_batch_cancels_all_tagging_in_one_pass() {
        let mut ctx = DaemonAiTest::new();
        let (photos, requests) = {
            let db = ctx.db.lock().unwrap();
            let root = db
                .add_root(ctx.photo_dir.to_str().unwrap(), "reference")
                .unwrap();
            let mut photos = Vec::new();
            let mut requests = Vec::new();
            for index in 0..150i64 {
                let name = format!("bulk{index:03}.png");
                let abs_path = ctx.photo_dir.join(&name);
                let photo_id = db
                    .upsert_photo(&NewPhoto {
                        root_id: root,
                        rel_path: name.clone(),
                        abs_path: abs_path.to_string_lossy().into_owned(),
                        file_name: name,
                        captured_at_utc: 1_700_000_000 + index,
                        captured_at_local: "2026-10-21 21:24:00".to_string(),
                        tz_offset_sec: 9 * 3600,
                        width: 16,
                        height: 10,
                        file_size: 10,
                        mtime: 1_700_000_000 + index,
                        blake3: None,
                    })
                    .unwrap();
                db.enqueue_tagging_jobs(&[photo_id], PLUGIN, TaggingJobStatus::Pending)
                    .unwrap();
                let request = db
                    .request_deletion(&[photo_id], DeletionReason::Manual, true)
                    .unwrap()[0];
                photos.push(photo_id);
                requests.push(request);
            }
            (photos, requests)
        };
        assert_eq!(ctx.tagging_counts().unfinished(), 150);

        ctx.daemon.process_deletions().unwrap();

        // One bulk statement cancels every job, even though the executor only
        // claims 100 requests per pass.
        assert_eq!(ctx.tagging_counts().unfinished(), 0);
        {
            let db = ctx.db.lock().unwrap();
            let completed = requests
                .iter()
                .filter(|request| {
                    db.deletion_request_by_id(**request)
                        .unwrap()
                        .unwrap()
                        .status
                        == DeletionStatus::Completed
                })
                .count();
            assert_eq!(completed, 100);
            for photo_id in &photos {
                let jobs = db.tagging_jobs_for_photo(*photo_id).unwrap();
                assert!(
                    jobs.iter()
                        .all(|job| job.status == TaggingJobStatus::Cancelled)
                );
            }
        }

        ctx.daemon.process_deletions().unwrap();
        let db = ctx.db.lock().unwrap();
        for request in requests {
            assert_eq!(
                db.deletion_request_by_id(request).unwrap().unwrap().status,
                DeletionStatus::Completed
            );
        }
    }

    #[test]
    fn deletion_waits_for_processing_tagging() {
        let mut ctx = DaemonAiTest::new();
        let (photo_id, path) = ctx.add_photo_and_scan();
        let (job_id, request) = {
            let db = ctx.db.lock().unwrap();
            db.enqueue_tagging_jobs(&[photo_id], PLUGIN, TaggingJobStatus::Pending)
                .unwrap();
            let job_id = db.next_pending_tagging_job().unwrap().unwrap().id;
            let request = db
                .request_deletion(&[photo_id], DeletionReason::Manual, false)
                .unwrap()[0];
            (job_id, request)
        };

        ctx.daemon.process_deletions().unwrap();
        {
            let db = ctx.db.lock().unwrap();
            assert_eq!(
                db.tagging_job(&job_id).unwrap().unwrap().status,
                TaggingJobStatus::Processing,
                "a non-forced deletion must not cancel the job"
            );
            assert_eq!(
                db.deletion_request_by_id(request).unwrap().unwrap().status,
                DeletionStatus::Pending
            );
        }
        assert!(path.exists(), "the photo must survive while tagging runs");

        ctx.db.lock().unwrap().cancel_tagging_job(&job_id).unwrap();
        ctx.daemon.process_deletions().unwrap();

        let db = ctx.db.lock().unwrap();
        assert_eq!(
            db.deletion_request_by_id(request).unwrap().unwrap().status,
            DeletionStatus::Completed
        );
        assert!(!path.exists());
    }

    fn original_path(dir: &tempfile::TempDir) -> PathBuf {
        dir.path()
            .join("photos/VRChat_2026-10-21_21-24-00.000_4x3.png")
    }

    fn replace_test_daemon(dir: &tempfile::TempDir) -> Daemon {
        let photo_dir = dir.path().join("photos");
        let log_dir = dir.path().join("logs");
        std::fs::create_dir_all(&photo_dir).unwrap();
        std::fs::create_dir_all(&log_dir).unwrap();
        write_photo(&original_path(dir));
        let db_path = dir.path().join("library.db");
        add_auto_profile(&db_path);
        let mut daemon = Daemon::open(&db_path, Some(photo_dir), Some(log_dir)).unwrap();
        daemon.set_ipc_endpoint(None);
        daemon.set_thumbnail_dir(dir.path().join("thumbnails"));
        save_daemon_settings(&daemon, |settings| {
            settings.auto_compress_backlog = true;
            settings.auto_compress_keep_original = false;
            settings.auto_compress_immediate = true;
        });
        daemon
    }

    fn manual_test_daemon(dir: &tempfile::TempDir) -> Daemon {
        let photo_dir = dir.path().join("photos");
        let log_dir = dir.path().join("logs");
        std::fs::create_dir_all(&photo_dir).unwrap();
        std::fs::create_dir_all(&log_dir).unwrap();
        write_photo(&original_path(dir));
        let db_path = dir.path().join("library.db");
        let mut daemon = Daemon::open(&db_path, Some(photo_dir), Some(log_dir)).unwrap();
        daemon.set_ipc_endpoint(None);
        daemon.set_thumbnail_dir(dir.path().join("thumbnails"));
        daemon
    }

    fn multi_profile_test_daemon(dir: &tempfile::TempDir) -> Daemon {
        let photo_dir = dir.path().join("photos");
        let log_dir = dir.path().join("logs");
        std::fs::create_dir_all(&photo_dir).unwrap();
        std::fs::create_dir_all(&log_dir).unwrap();
        let db_path = dir.path().join("library.db");
        let mut daemon = Daemon::open(&db_path, Some(photo_dir), Some(log_dir)).unwrap();
        daemon.set_ipc_endpoint(None);
        daemon.set_thumbnail_dir(dir.path().join("thumbnails"));
        daemon
    }

    fn write_gradient_photo(path: &Path) {
        let image = image::RgbaImage::from_fn(64, 64, |x, y| {
            image::Rgba([(x * 4) as u8, (y * 4) as u8, ((x + y) * 2) as u8, 255])
        });
        image.save(path).unwrap();
    }

    fn save_batch_profile(
        db: &Db,
        id: &str,
        format: vrc_photon_core::model::CompressionFormat,
        suffix: &str,
        resize: Option<(u32, u32)>,
    ) {
        db.save_profile(&CompressionProfile {
            id: id.to_string(),
            name: id.to_string(),
            format,
            suffix: suffix.to_string(),
            resize_enabled: resize.is_some(),
            width: resize.map(|(w, _)| w).unwrap_or(3840),
            height: resize.map(|(_, h)| h).unwrap_or(2160),
            ..CompressionProfile::default()
        })
        .unwrap();
    }

    fn add_replace_profiles(db: &Db) {
        db.save_profile(&CompressionProfile {
            id: "lossy".to_string(),
            name: "lossy".to_string(),
            auto_enabled: true,
            format: vrc_photon_core::model::CompressionFormat::Webp,
            quality: 10,
            lossless: false,
            resize_enabled: false,
            output_dir: Some("lossy".to_string()),
            suffix: "_lossy".to_string(),
            ..CompressionProfile::default()
        })
        .unwrap();
        db.save_profile(&CompressionProfile {
            id: "lossless".to_string(),
            name: "lossless".to_string(),
            auto_enabled: true,
            format: vrc_photon_core::model::CompressionFormat::Webp,
            lossless: true,
            resize_enabled: false,
            output_dir: Some("lossless".to_string()),
            suffix: "_lossless".to_string(),
            ..CompressionProfile::default()
        })
        .unwrap();
    }

    fn wait_until_idle(db: &Arc<Mutex<Db>>, timeout: Duration) {
        let started = Instant::now();
        loop {
            let (active, waiting) = {
                let db = db.lock().unwrap();
                (
                    db.count_active_jobs().unwrap(),
                    db.count_jobs(QueueStatus::Waiting).unwrap(),
                )
            };
            if active == 0 && waiting == 0 {
                return;
            }
            assert!(
                started.elapsed() < timeout,
                "jobs did not finish within {timeout:?}"
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn wait_for(mut condition: impl FnMut() -> bool, timeout: Duration) {
        let started = Instant::now();
        while !condition() {
            assert!(
                started.elapsed() < timeout,
                "condition was not met within {timeout:?}"
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn write_thumbnail_cache_entry(path: &Path, bytes: usize, modified: SystemTime) {
        std::fs::write(path, vec![0u8; bytes]).unwrap();
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(modified)
            .unwrap();
    }

    #[test]
    fn startup_prunes_thumbnail_cache_to_limit() {
        let dir = tempfile::tempdir().unwrap();
        let mut daemon = test_daemon(&dir);
        let cache = dir.path().join("thumbnails");
        std::fs::create_dir_all(&cache).unwrap();
        let now = SystemTime::now();
        for (id, age) in [(1, 30), (2, 20), (3, 10)] {
            write_thumbnail_cache_entry(
                &cache.join(format!("{id}.webp")),
                600_000,
                now - Duration::from_secs(age),
            );
        }
        save_daemon_settings(&daemon, |settings| settings.thumbnail_cache_mb = 1);
        daemon.refresh_compression_settings();
        daemon.set_thumbnail_dir(cache.clone());

        daemon.start_workers();
        wait_for(
            || !cache.join("1.webp").exists() && !cache.join("2.webp").exists(),
            Duration::from_secs(5),
        );
        assert!(cache.join("3.webp").exists());
        daemon.stop_workers();
    }

    #[test]
    fn run_once_prunes_thumbnail_cache_after_direct_generation() {
        let dir = tempfile::tempdir().unwrap();
        let mut daemon = test_daemon(&dir);
        let cache = dir.path().join("thumbnails");
        std::fs::create_dir_all(&cache).unwrap();
        let now = SystemTime::now();
        for (id, age) in [(1, 30), (2, 20), (3, 10)] {
            write_thumbnail_cache_entry(
                &cache.join(format!("{id}.webp")),
                600_000,
                now - Duration::from_secs(age),
            );
        }
        save_daemon_settings(&daemon, |settings| settings.thumbnail_cache_mb = 1);
        daemon.refresh_compression_settings();
        daemon.set_thumbnail_dir(cache.clone());
        write_photo(
            &dir.path()
                .join("photos/VRChat_2026-10-21_21-24-00.000_4x3.png"),
        );

        daemon.run_once().unwrap();

        assert!(!cache.join("1.webp").exists());
        assert!(!cache.join("2.webp").exists());
        assert!(cache.join("3.webp").exists());
    }

    fn report_with_remaining(bytes: u64) -> thumbnail::ThumbnailPruneReport {
        thumbnail::ThumbnailPruneReport {
            remaining_bytes: bytes,
            ..thumbnail::ThumbnailPruneReport::default()
        }
    }

    #[test]
    fn thumbnail_budget_prunes_only_above_the_limit() {
        let mut budget = ThumbnailCacheBudget::new(1000);
        assert!(budget.needs_prune()); // 未計測
        budget.note_pruned(&report_with_remaining(1000));
        assert!(!budget.needs_prune());
        budget.note_generated(1);
        assert!(budget.needs_prune());
    }

    #[test]
    fn thumbnail_budget_tracks_generation_external_growth_and_reset() {
        let mut budget = ThumbnailCacheBudget::new(1000);
        budget.note_pruned(&report_with_remaining(500));
        budget.note_generated(200);
        budget.note_external(100);
        assert!(!budget.needs_prune());
        budget.note_generated(300);
        assert!(budget.needs_prune());
        budget.note_pruned(&report_with_remaining(400));
        assert!(!budget.needs_prune());
    }

    #[test]
    fn thumbnail_budget_never_prunes_when_unlimited() {
        let mut budget = ThumbnailCacheBudget::new(0);
        budget.note_generated(10_000_000);
        budget.note_external(10_000_000);
        assert!(!budget.needs_prune());
        budget.note_pruned(&report_with_remaining(0));
        assert!(!budget.needs_prune());
    }

    #[test]
    fn external_thumbnail_growth_prunes_over_the_limit() {
        let dir = tempfile::tempdir().unwrap();
        let mut daemon = test_daemon(&dir);
        let cache = dir.path().join("thumbnails");
        std::fs::create_dir_all(&cache).unwrap();
        let now = SystemTime::now();
        for (id, age) in [(1, 30), (2, 20), (3, 10)] {
            write_thumbnail_cache_entry(
                &cache.join(format!("{id}.webp")),
                600_000,
                now - Duration::from_secs(age),
            );
        }
        save_daemon_settings(&daemon, |settings| settings.thumbnail_cache_mb = 2);
        daemon.refresh_compression_settings();
        daemon.set_thumbnail_dir(cache.clone());

        daemon.start_workers();
        // The startup pass discards notifications, so consume two pulses: the
        // second one can only be handled by the idle branch after startup.
        for _ in 0..2 {
            daemon
                .thumbnail_external_bytes
                .fetch_add(1, Ordering::Relaxed);
            wait_for(
                || daemon.thumbnail_external_bytes.load(Ordering::Relaxed) == 0,
                Duration::from_secs(5),
            );
        }
        assert!(cache.join("1.webp").exists());

        // The UI writes its own thumbnails; the notice reports bytes the
        // daemon's generation counter never saw.
        write_thumbnail_cache_entry(&cache.join("4.webp"), 1_000_000, now);
        daemon
            .thumbnail_external_bytes
            .fetch_add(1_000_000, Ordering::Relaxed);
        wait_for(|| !cache.join("1.webp").exists(), Duration::from_secs(5));
        assert!(cache.join("3.webp").exists());
        daemon.stop_workers();
    }

    #[test]
    fn external_notice_prunes_through_a_channel_message() {
        let dir = tempfile::tempdir().unwrap();
        let mut daemon = test_daemon(&dir);
        let cache = dir.path().join("thumbnails");
        std::fs::create_dir_all(&cache).unwrap();
        let now = SystemTime::now();
        for (id, age) in [(1, 30), (2, 20), (3, 10)] {
            write_thumbnail_cache_entry(
                &cache.join(format!("{id}.webp")),
                600_000,
                now - Duration::from_secs(age),
            );
        }
        save_daemon_settings(&daemon, |settings| settings.thumbnail_cache_mb = 2);
        daemon.refresh_compression_settings();
        daemon.set_thumbnail_dir(cache.clone());

        daemon.start_workers();
        for _ in 0..2 {
            daemon
                .thumbnail_external_bytes
                .fetch_add(1, Ordering::Relaxed);
            wait_for(
                || daemon.thumbnail_external_bytes.load(Ordering::Relaxed) == 0,
                Duration::from_secs(5),
            );
        }
        assert!(cache.join("1.webp").exists());

        write_thumbnail_cache_entry(&cache.join("4.webp"), 1_000_000, now);
        // Hold the DB lock so the worker must consume the notice in the Ok
        // branch and take the post-lookup prune path after we release it.
        let db = daemon.db.lock().unwrap();
        daemon
            .thumbnail_external_bytes
            .fetch_add(1_000_000, Ordering::Relaxed);
        daemon.thumbnail_tx.as_ref().unwrap().send(999_999).unwrap();
        wait_for(
            || daemon.thumbnail_external_bytes.load(Ordering::Relaxed) == 0,
            Duration::from_secs(5),
        );
        drop(db);

        wait_for(|| !cache.join("1.webp").exists(), Duration::from_secs(5));
        assert!(cache.join("3.webp").exists());
        daemon.stop_workers();
    }

    #[test]
    fn thumbnail_budget_skips_a_prune_after_the_limit_turns_unlimited() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("thumbnails");
        std::fs::create_dir_all(&cache).unwrap();
        let path = cache.join("1.webp");
        write_thumbnail_cache_entry(&path, 2_000, SystemTime::now());
        let cache_limit = AtomicU64::new(1000);
        let mut budget = ThumbnailCacheBudget::new(1000);
        budget.note_pruned(&report_with_remaining(2_000));
        assert!(budget.needs_prune());

        cache_limit.store(0, Ordering::Relaxed);
        refresh_thumbnail_budget_and_prune(&cache, &cache_limit, &mut budget);

        assert_eq!(budget.limit_bytes, 0);
        assert!(budget.known_bytes.is_none());
        assert!(!budget.needs_prune());
        assert!(path.exists());
    }

    #[test]
    fn prune_thumbnail_cache_with_zero_limit_leaves_files_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("1.webp");
        write_thumbnail_cache_entry(&path, 2_000, SystemTime::now());

        assert!(prune_thumbnail_cache(dir.path(), 0).is_none());

        assert_eq!(std::fs::metadata(&path).unwrap().len(), 2_000);
    }

    #[test]
    fn thumbnail_prune_schedule_fires_after_a_batch() {
        let mut schedule = ThumbnailPruneSchedule::new(2);

        assert!(!schedule.note_generated());
        assert!(schedule.note_generated());
        assert!(!schedule.has_pending());
    }

    #[test]
    fn thumbnail_prune_schedule_resets_pending_after_an_idle_prune() {
        let mut schedule = ThumbnailPruneSchedule::new(128);

        assert!(!schedule.note_generated());
        assert!(schedule.has_pending());
        schedule.note_pruned();
        assert!(!schedule.has_pending());
    }

    #[test]
    fn live_thumbnail_limit_change_prunes_cache() {
        let dir = tempfile::tempdir().unwrap();
        let mut daemon = test_daemon(&dir);
        let cache = dir.path().join("thumbnails");
        std::fs::create_dir_all(&cache).unwrap();
        let now = SystemTime::now();
        for (id, age) in [(1, 30), (2, 20), (3, 10)] {
            write_thumbnail_cache_entry(
                &cache.join(format!("{id}.webp")),
                600_000,
                now - Duration::from_secs(age),
            );
        }
        daemon.set_thumbnail_dir(cache.clone());
        daemon.start_workers();
        assert!(cache.join("1.webp").exists());

        save_daemon_settings(&daemon, |settings| settings.thumbnail_cache_mb = 1);
        daemon.refresh_compression_settings();
        wait_for(
            || !cache.join("1.webp").exists() && !cache.join("2.webp").exists(),
            Duration::from_secs(5),
        );
        assert!(cache.join("3.webp").exists());
        daemon.stop_workers();
    }

    #[test]
    fn replace_job_queues_deletion_instead_of_trashing_immediately() {
        let dir = tempfile::tempdir().unwrap();
        let mut daemon = replace_test_daemon(&dir);
        daemon.run_once().unwrap();

        // Drive one job directly so the assertion sees the state between job
        // completion and deletion execution.
        let job = {
            let db = daemon.db.lock().unwrap();
            db.next_pending_job().unwrap().unwrap()
        };
        process_job(&daemon.db, &job).unwrap();

        let db = Db::open(dir.path().join("library.db")).unwrap();
        let photo = db
            .photo_by_path(original_path(&dir).to_str().unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(photo.status, PhotoStatus::Replaced);
        assert!(
            original_path(&dir).exists(),
            "the original survives until the request runs"
        );
        let request = db.active_deletion_request(photo.id).unwrap().unwrap();
        assert_eq!(request.reason, DeletionReason::AutoReplace);
        assert_eq!(request.status, DeletionStatus::Pending);

        daemon.process_deletions().unwrap();
        assert!(!original_path(&dir).exists());
        assert_eq!(db.derivatives_for_photo(photo.id).unwrap().len(), 1);
        assert!(db.active_deletion_request(photo.id).unwrap().is_none());
    }

    #[test]
    fn deletion_waits_for_every_job_of_the_photo() {
        let dir = tempfile::tempdir().unwrap();
        let mut daemon = replace_test_daemon(&dir);
        daemon.run_once().unwrap();
        let db = Db::open(dir.path().join("library.db")).unwrap();
        let photo = db
            .photo_by_path(original_path(&dir).to_str().unwrap())
            .unwrap()
            .unwrap();
        db.save_profile(&CompressionProfile {
            id: "second".to_string(),
            name: "Second".to_string(),
            ..CompressionProfile::default()
        })
        .unwrap();
        db.enqueue_job(photo.id, "second", "keep").unwrap();
        db.request_deletion(&[photo.id], DeletionReason::Manual, false)
            .unwrap();

        daemon.process_deletions().unwrap();
        assert!(
            original_path(&dir).exists(),
            "pending job must block deletion"
        );
        daemon.compress_pending().unwrap();
        daemon.process_deletions().unwrap();
        assert!(!original_path(&dir).exists());
        assert_eq!(
            db.photo_by_id(photo.id).unwrap().unwrap().status,
            PhotoStatus::Missing
        );
    }

    #[test]
    fn manual_deletion_removes_derivatives_and_marks_photo_missing() {
        let dir = tempfile::tempdir().unwrap();
        let mut daemon = replace_test_daemon(&dir);
        daemon.run_once().unwrap();
        daemon.compress_pending().unwrap();
        daemon.process_deletions().unwrap();

        let db = Db::open(dir.path().join("library.db")).unwrap();
        let photo = db
            .photo_by_path(original_path(&dir).to_str().unwrap())
            .unwrap()
            .unwrap();
        let derivative = db.derivatives_for_photo(photo.id).unwrap()[0].path.clone();
        db.request_deletion(&[photo.id], DeletionReason::Manual, false)
            .unwrap();
        daemon.process_deletions().unwrap();

        assert!(!original_path(&dir).exists());
        assert!(!Path::new(&derivative).exists());
        let photo = db.photo_by_id(photo.id).unwrap().unwrap();
        assert_eq!(photo.status, PhotoStatus::Missing);
        assert!(db.derivatives_for_photo(photo.id).unwrap().is_empty());
        assert_eq!(db.count_photos().unwrap(), 0);
    }

    #[test]
    fn daemon_open_resumes_interrupted_compression_jobs() {
        let dir = tempfile::tempdir().unwrap();
        let mut daemon = manual_test_daemon(&dir);
        daemon.run_once().unwrap();
        drop(daemon);

        let db_path = dir.path().join("library.db");
        let job_id = {
            let db = Db::open(&db_path).unwrap();
            let photo = db
                .photo_by_path(original_path(&dir).to_str().unwrap())
                .unwrap()
                .unwrap();
            let job = db.enqueue_job(photo.id, "p", "keep").unwrap();
            let claimed = db.next_pending_job().unwrap().unwrap();
            assert_eq!(claimed.id, job);
            job
        };

        let reopened = Daemon::open(
            &db_path,
            Some(dir.path().join("photos")),
            Some(dir.path().join("logs")),
        )
        .unwrap();
        drop(reopened);

        let db = Db::open(&db_path).unwrap();
        let resumed = db
            .list_jobs(10)
            .unwrap()
            .into_iter()
            .find(|job| job.id == job_id)
            .expect("compression job");
        assert_eq!(resumed.status, QueueStatus::Pending);
        assert_eq!(resumed.retry_count, 0);
        assert!(resumed.started_at.is_none());
    }

    #[test]
    fn maintenance_open_preserves_a_live_daemons_running_deletion() {
        let dir = tempfile::tempdir().unwrap();
        let mut daemon = manual_test_daemon(&dir);
        daemon.run_once().unwrap();
        let db_path = dir.path().join("library.db");
        let photo = {
            let db = Db::open(&db_path).unwrap();
            let photo = db
                .photo_by_path(original_path(&dir).to_str().unwrap())
                .unwrap()
                .unwrap();
            let request = db
                .request_deletion(&[photo.id], DeletionReason::Manual, false)
                .unwrap()[0];
            assert!(db.claim_deletion(request).unwrap().is_some());
            photo
        };

        // A maintenance command opens the same database while the first
        // daemon still owns the running request.
        let mut maintenance = Daemon::open_preserving_running_deletions(
            &db_path,
            Some(dir.path().join("photos")),
            Some(dir.path().join("logs")),
        )
        .unwrap();
        maintenance.run_once().unwrap();
        {
            let db = Db::open(&db_path).unwrap();
            let request = db.active_deletion_request(photo.id).unwrap().unwrap();
            assert_eq!(request.status, DeletionStatus::Running);
        }
        drop(maintenance);
        drop(daemon);

        // Once no executor remains, the next normal startup recovers it.
        let _recovered = Daemon::open(
            &db_path,
            Some(dir.path().join("photos")),
            Some(dir.path().join("logs")),
        )
        .unwrap();
        let db = Db::open(&db_path).unwrap();
        let request = db.active_deletion_request(photo.id).unwrap().unwrap();
        assert_eq!(request.status, DeletionStatus::Pending);
    }

    #[test]
    fn preserving_open_leaves_pending_deletions_to_the_executor() {
        let dir = tempfile::tempdir().unwrap();
        let mut daemon = manual_test_daemon(&dir);
        daemon.run_once().unwrap();
        let db_path = dir.path().join("library.db");
        let photo = {
            let db = Db::open(&db_path).unwrap();
            let photo = db
                .photo_by_path(original_path(&dir).to_str().unwrap())
                .unwrap()
                .unwrap();
            db.request_deletion(&[photo.id], DeletionReason::Manual, false)
                .unwrap();
            photo
        };

        let mut maintenance = Daemon::open_preserving_running_deletions(
            &db_path,
            Some(dir.path().join("photos")),
            Some(dir.path().join("logs")),
        )
        .unwrap();
        maintenance.process_deletions().unwrap();
        {
            let db = Db::open(&db_path).unwrap();
            let request = db.active_deletion_request(photo.id).unwrap().unwrap();
            assert_eq!(request.status, DeletionStatus::Pending);
        }
        assert!(original_path(&dir).exists());

        drop(maintenance);
        drop(daemon);

        let mut owner = Daemon::open(
            &db_path,
            Some(dir.path().join("photos")),
            Some(dir.path().join("logs")),
        )
        .unwrap();
        owner.process_deletions().unwrap();
        assert!(!original_path(&dir).exists());
        let db = Db::open(&db_path).unwrap();
        assert!(db.active_deletion_request(photo.id).unwrap().is_none());
    }

    #[test]
    fn deletion_recovers_after_partial_file_removal() {
        let dir = tempfile::tempdir().unwrap();
        let mut daemon = manual_test_daemon(&dir);
        daemon.run_once().unwrap();
        let db_path = dir.path().join("library.db");
        let photo = {
            let db = Db::open(&db_path).unwrap();
            let photo = db
                .photo_by_path(original_path(&dir).to_str().unwrap())
                .unwrap()
                .unwrap();
            let request = db
                .request_deletion(&[photo.id], DeletionReason::Manual, false)
                .unwrap()[0];
            assert!(db.claim_deletion(request).unwrap().is_some());
            photo
        };
        std::fs::remove_file(original_path(&dir)).unwrap();
        drop(daemon);

        // Reopen through the production path: the running request must return
        // to pending while keeping the partial-execution marker.
        let mut daemon = Daemon::open(
            &db_path,
            Some(dir.path().join("photos")),
            Some(dir.path().join("logs")),
        )
        .unwrap();
        daemon.set_ipc_endpoint(None);
        daemon.set_thumbnail_dir(dir.path().join("thumbnails"));
        {
            let db = Db::open(&db_path).unwrap();
            let request = db.active_deletion_request(photo.id).unwrap().unwrap();
            assert_eq!(request.status, DeletionStatus::Pending);
            assert!(request.started_at.is_some());
            assert!(!db.cancel_deletion(request.id).unwrap());
        }

        daemon.process_deletions().unwrap();
        let db = Db::open(&db_path).unwrap();
        assert!(db.active_deletion_request(photo.id).unwrap().is_none());
        assert_eq!(
            db.photo_by_id(photo.id).unwrap().unwrap().status,
            PhotoStatus::Missing
        );
    }

    #[test]
    fn cancel_blocked_job_failed_manual_deletion_keeps_files() {
        let dir = tempfile::tempdir().unwrap();
        let mut daemon = manual_test_daemon(&dir);
        daemon.run_once().unwrap();
        let db_path = dir.path().join("library.db");
        let db = Db::open(&db_path).unwrap();
        let photo = db
            .photo_by_path(original_path(&dir).to_str().unwrap())
            .unwrap()
            .unwrap();
        let derivative_path = dir.path().join("photos/kept.webp");
        std::fs::copy(original_path(&dir), &derivative_path).unwrap();
        db.add_derivative(
            photo.id,
            derivative_path.to_str().unwrap(),
            "webp",
            16,
            10,
            std::fs::metadata(&derivative_path).unwrap().len(),
            "p",
            "keep",
        )
        .unwrap();
        db.save_profile(&CompressionProfile {
            id: "p".to_string(),
            name: "P".to_string(),
            ..CompressionProfile::default()
        })
        .unwrap();
        let failed = db.enqueue_job(photo.id, "p", "keep").unwrap();
        db.update_job(&failed, QueueStatus::Failed, 0.0, Some("boom"), None)
            .unwrap();
        let request = db
            .request_deletion(&[photo.id], DeletionReason::Manual, false)
            .unwrap()[0];
        daemon.process_deletions().unwrap();
        assert_eq!(
            db.active_deletion_request(photo.id)
                .unwrap()
                .unwrap()
                .status,
            DeletionStatus::Blocked
        );

        assert!(db.cancel_deletion(request).unwrap());
        assert!(original_path(&dir).exists());
        assert!(derivative_path.exists());
        assert_eq!(
            db.photo_by_id(photo.id).unwrap().unwrap().status,
            PhotoStatus::Present
        );
        assert!(db.active_deletion_request(photo.id).unwrap().is_none());
    }

    #[test]
    fn auto_replace_without_a_derivative_keeps_the_original() {
        let dir = tempfile::tempdir().unwrap();
        let mut daemon = replace_test_daemon(&dir);
        daemon.run_once().unwrap();
        let job = {
            let db = daemon.db.lock().unwrap();
            db.next_pending_job().unwrap().unwrap()
        };
        process_job(&daemon.db, &job).unwrap();
        let db = Db::open(dir.path().join("library.db")).unwrap();
        let photo = db
            .photo_by_path(original_path(&dir).to_str().unwrap())
            .unwrap()
            .unwrap();
        let derivative = db.derivatives_for_photo(photo.id).unwrap()[0].path.clone();
        std::fs::remove_file(&derivative).unwrap();

        daemon.process_deletions().unwrap();
        assert!(original_path(&dir).exists(), "the only copy must survive");
        let request = db.active_deletion_request(photo.id).unwrap().unwrap();
        assert_eq!(request.status, DeletionStatus::Blocked);
        assert_eq!(request.blocked_reason.as_deref(), Some("missing_asset"));
    }

    #[test]
    fn preferred_display_derivative_uses_profile_order_then_id() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first.webp");
        let second = dir.path().join("second.webp");
        std::fs::write(&first, b"a").unwrap();
        std::fs::write(&second, b"b").unwrap();
        let derivative = |id: i64, profile: &str, path: &Path| vrc_photon_core::model::Derivative {
            id,
            photo_id: 1,
            path: path.to_string_lossy().to_string(),
            format: "webp".to_string(),
            width: 16,
            height: 10,
            file_size: 1,
            profile_id: profile.to_string(),
            policy: "replace".to_string(),
            created_at: String::new(),
        };
        let derivatives = vec![
            derivative(2, "second", &second),
            derivative(1, "first", &first),
            // Unknown profiles rank below known ones even with a larger id.
            derivative(3, "gone", &first),
        ];
        let profiles = vec![
            CompressionProfile {
                id: "first".to_string(),
                ..CompressionProfile::default()
            },
            CompressionProfile {
                id: "second".to_string(),
                ..CompressionProfile::default()
            },
        ];

        let picked = preferred_display_derivative(&derivatives, &profiles).unwrap();
        assert_eq!(picked.id, 1);

        // Unknown profiles rank below known ones; among themselves the larger id wins.
        let unknown = vec![
            derivative(5, "gone", &first),
            derivative(7, "gone", &second),
            derivative(9, "gone", &first),
        ];
        let picked = preferred_display_derivative(&unknown, &profiles).unwrap();
        assert_eq!(picked.id, 9);
    }

    #[test]
    fn ready_deletion_after_more_than_hundred_blocked_requests_runs() {
        let dir = tempfile::tempdir().unwrap();
        let mut daemon = test_daemon(&dir);
        let db = Db::open(dir.path().join("library.db")).unwrap();
        let root = db.add_root("/photos", "reference").unwrap();
        let mut request_ids = Vec::new();

        for index in 0..=100i64 {
            let name = format!("p{index:03}.png");
            let photo_id = db
                .upsert_photo(&vrc_photon_core::model::NewPhoto {
                    root_id: root,
                    rel_path: name.clone(),
                    abs_path: format!("/photos/{name}"),
                    file_name: name,
                    captured_at_utc: 1_700_000_000 + index,
                    captured_at_local: "2026-10-21 21:14:03".to_string(),
                    tz_offset_sec: 9 * 3600,
                    width: 16,
                    height: 10,
                    file_size: 10,
                    mtime: 1_700_000_000 + index,
                    blake3: None,
                })
                .unwrap();
            if index < 100 {
                db.enqueue_job(photo_id, "missing-profile", "keep").unwrap();
            }
            let request = db
                .request_deletion(&[photo_id], DeletionReason::Manual, false)
                .unwrap()[0];
            request_ids.push(request);
        }

        daemon.process_deletions().unwrap();

        let ready = db
            .deletion_request_by_id(request_ids[100])
            .unwrap()
            .unwrap();
        assert_eq!(ready.status, DeletionStatus::Completed);
        for request_id in &request_ids[..100] {
            let pending = db.deletion_request_by_id(*request_id).unwrap().unwrap();
            assert_eq!(pending.status, DeletionStatus::Pending);
        }
    }

    #[test]
    fn manual_deletion_preempts_an_older_auto_replace_backlog() {
        let dir = tempfile::tempdir().unwrap();
        let mut daemon = test_daemon(&dir);
        let db = Db::open(dir.path().join("library.db")).unwrap();
        let root = db.add_root("/photos", "reference").unwrap();

        for index in 0..100i64 {
            let name = format!("auto{index:03}.png");
            let photo_id = db
                .upsert_photo(&vrc_photon_core::model::NewPhoto {
                    root_id: root,
                    rel_path: name.clone(),
                    abs_path: format!("/photos/{name}"),
                    file_name: name,
                    captured_at_utc: 1_700_000_000 + index,
                    captured_at_local: "2026-10-21 21:14:03".to_string(),
                    tz_offset_sec: 9 * 3600,
                    width: 16,
                    height: 10,
                    file_size: 10,
                    mtime: 1_700_000_000 + index,
                    blake3: None,
                })
                .unwrap();
            let request = db
                .request_deletion(&[photo_id], DeletionReason::AutoReplace, false)
                .unwrap()[0];
            db.connection()
                .execute(
                    "UPDATE deletion_requests SET requested_at = ?1 WHERE id = ?2",
                    ("2026-10-21T00:00:01Z", request),
                )
                .unwrap();
        }

        let manual_path = dir.path().join("photos/manual.png");
        write_photo(&manual_path);
        let manual_id = db
            .upsert_photo(&vrc_photon_core::model::NewPhoto {
                root_id: root,
                rel_path: "manual.png".to_string(),
                abs_path: manual_path.to_string_lossy().to_string(),
                file_name: "manual.png".to_string(),
                captured_at_utc: 1_700_000_500,
                captured_at_local: "2026-10-21 21:14:03".to_string(),
                tz_offset_sec: 9 * 3600,
                width: 16,
                height: 10,
                file_size: 10,
                mtime: 1_700_000_500,
                blake3: None,
            })
            .unwrap();
        let manual_request = db
            .request_deletion(&[manual_id], DeletionReason::Manual, false)
            .unwrap()[0];
        db.connection()
            .execute(
                "UPDATE deletion_requests SET requested_at = ?1 WHERE id = ?2",
                ("2026-10-21T00:00:02Z", manual_request),
            )
            .unwrap();

        daemon.process_deletions().unwrap();

        let request = db.deletion_request_by_id(manual_request).unwrap().unwrap();
        assert_eq!(
            request.status,
            DeletionStatus::Completed,
            "the manual deletion must not wait behind the auto_replace backlog"
        );
        assert!(
            !manual_path.exists(),
            "the manual deletion must remove the file"
        );
    }

    #[test]
    fn unknown_deletion_reason_is_blocked_without_deleting() {
        let dir = tempfile::tempdir().unwrap();
        let mut daemon = test_daemon(&dir);
        let original = dir
            .path()
            .join("photos/VRChat_2026-10-21_21-24-00.000_4x3.png");
        write_photo(&original);
        daemon.run_once().unwrap();

        let db = Db::open(dir.path().join("library.db")).unwrap();
        let photo = db
            .photo_by_path(original.to_str().unwrap())
            .unwrap()
            .unwrap();
        let derivative = dir.path().join("photos/kept.webp");
        std::fs::write(&derivative, b"kept derivative").unwrap();
        db.add_derivative(
            photo.id,
            derivative.to_str().unwrap(),
            "webp",
            16,
            10,
            std::fs::metadata(&derivative).unwrap().len(),
            "p",
            "keep",
        )
        .unwrap();
        db.connection()
            .execute(
                "INSERT INTO deletion_requests(photo_id, reason, status, force, requested_at) \
                 VALUES (?1, 'future_reason', 'pending', 0, ?2)",
                (photo.id, chrono::Utc::now().to_rfc3339()),
            )
            .unwrap();

        daemon.process_deletions().unwrap();

        assert!(original.exists(), "unknown reason must not delete files");
        assert!(
            derivative.exists(),
            "unknown reason must not delete derivatives"
        );
        assert_eq!(
            db.derivatives_for_photo(photo.id).unwrap().len(),
            1,
            "unknown reason must not drop the derivative row"
        );
        let request = db.active_deletion_request(photo.id).unwrap().unwrap();
        assert_eq!(
            request.reason,
            vrc_photon_core::model::DeletionReason::Unknown
        );
        assert_eq!(request.status, DeletionStatus::Blocked);
        assert_eq!(request.blocked_reason.as_deref(), Some("unknown_reason"));
    }

    #[test]
    fn unknown_reason_upgraded_to_manual_is_executed_not_blocked() {
        let dir = tempfile::tempdir().unwrap();
        let mut daemon = test_daemon(&dir);
        let original = dir
            .path()
            .join("photos/VRChat_2026-10-21_21-24-00.000_4x3.png");
        write_photo(&original);
        daemon.run_once().unwrap();

        let db = Db::open(dir.path().join("library.db")).unwrap();
        let photo = db
            .photo_by_path(original.to_str().unwrap())
            .unwrap()
            .unwrap();
        db.connection()
            .execute(
                "INSERT INTO deletion_requests(photo_id, reason, status, force, requested_at) \
                 VALUES (?1, 'future_reason', 'pending', 0, ?2)",
                (photo.id, chrono::Utc::now().to_rfc3339()),
            )
            .unwrap();
        // The UI upgrades the request to manual before the executor acts on
        // its snapshot of the unknown reason.
        let request_id = db
            .request_deletion(
                &[photo.id],
                vrc_photon_core::model::DeletionReason::Manual,
                false,
            )
            .unwrap()[0];

        daemon.process_deletions().unwrap();

        assert!(
            !original.exists(),
            "the upgraded manual deletion must remove the file"
        );
        let request = db.deletion_request_by_id(request_id).unwrap().unwrap();
        assert_eq!(
            request.reason,
            vrc_photon_core::model::DeletionReason::Manual
        );
        assert_eq!(request.status, DeletionStatus::Completed);
        assert!(request.blocked_reason.is_none());
    }

    #[test]
    fn auto_replace_keeps_the_preferred_display_derivative() {
        let dir = tempfile::tempdir().unwrap();
        let mut daemon = multi_profile_test_daemon(&dir);
        let db_path = dir.path().join("library.db");
        let original = original_path(&dir);
        write_photo(&original);
        {
            let db = Db::open(&db_path).unwrap();
            add_replace_profiles(&db);
        }
        daemon.run_once().unwrap();

        let db = Db::open(&db_path).unwrap();
        let photo = db
            .photo_by_path(original.to_str().unwrap())
            .unwrap()
            .unwrap();
        let lossless_path = dir.path().join("lossless.webp");
        let lossy_path = dir.path().join("lossy.webp");
        std::fs::write(&lossless_path, b"lossless").unwrap();
        std::fs::write(&lossy_path, b"lossy").unwrap();
        let lossy = db
            .add_derivative(
                photo.id,
                lossy_path.to_str().unwrap(),
                "webp",
                16,
                10,
                5,
                "lossy",
                "replace",
            )
            .unwrap();
        // Larger derivative id, but second in the profile list.
        let lossless = db
            .add_derivative(
                photo.id,
                lossless_path.to_str().unwrap(),
                "webp",
                16,
                10,
                8,
                "lossless",
                "replace",
            )
            .unwrap();
        db.set_replaced(photo.id, lossless).unwrap();
        db.request_deletion(&[photo.id], DeletionReason::AutoReplace, false)
            .unwrap();

        daemon.process_deletions().unwrap();

        let photo = db.photo_by_id(photo.id).unwrap().unwrap();
        assert_eq!(photo.display_derivative_id, Some(lossy));
        assert_ne!(photo.display_derivative_id, Some(lossless));
        assert!(lossy_path.is_file());
    }

    #[test]
    fn multi_profile_replace_is_worker_count_independent() {
        for workers in 1usize..=4 {
            let dir = tempfile::tempdir().unwrap();
            let mut daemon = multi_profile_test_daemon(&dir);
            let db_path = dir.path().join("library.db");
            {
                let db = Db::open(&db_path).unwrap();
                add_replace_profiles(&db);
            }
            save_daemon_settings(&daemon, |settings| {
                settings.auto_compress_backlog = true;
                settings.auto_compress_keep_original = false;
                settings.auto_compress_immediate = true;
            });
            let original = original_path(&dir);
            write_gradient_photo(&original);
            let expected = vrc_photon_core::image_decode::decode_image(&original)
                .unwrap()
                .to_rgba8();
            daemon.run_once().unwrap();
            daemon.settings.compression_workers = workers;
            daemon.start_workers();
            wait_until_idle(&daemon.db, Duration::from_secs(15));
            daemon.stop_workers();
            daemon.process_deletions().unwrap();

            let db = Db::open(&db_path).unwrap();
            let photo = db
                .photo_by_path(original.to_str().unwrap())
                .unwrap()
                .unwrap();
            let derivatives = db.derivatives_for_photo(photo.id).unwrap();
            assert_eq!(derivatives.len(), 2, "workers={workers}");
            assert!(derivatives.iter().all(|d| Path::new(&d.path).is_file()));
            assert!(!original.exists(), "workers={workers}");
            assert_eq!(photo.status, PhotoStatus::Replaced);
            assert_eq!(db.count_jobs(QueueStatus::Completed).unwrap(), 2);
            assert_eq!(db.count_jobs(QueueStatus::Failed).unwrap(), 0);
            let completed: i64 = db
                .connection()
                .query_row(
                    "SELECT COUNT(*) FROM deletion_requests WHERE status = 'completed'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(completed, 1, "workers={workers}");

            let display_id = photo.display_derivative_id.expect("display derivative");
            let display = derivatives
                .iter()
                .find(|derivative| derivative.id == display_id)
                .expect("display derivative row");
            assert_eq!(display.profile_id, "lossy", "workers={workers}");
            assert!(Path::new(&display.path).is_file(), "workers={workers}");

            let lossless = derivatives
                .iter()
                .find(|d| d.profile_id == "lossless")
                .unwrap();
            let actual = vrc_photon_core::image_decode::decode_image(Path::new(&lossless.path))
                .unwrap()
                .to_rgba8();
            assert_eq!(
                actual, expected,
                "workers={workers}: second job read a derivative"
            );
        }
    }

    #[test]
    fn failed_job_blocks_deletion_until_retry_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let mut daemon = multi_profile_test_daemon(&dir);
        let db_path = dir.path().join("library.db");
        {
            let db = Db::open(&db_path).unwrap();
            add_replace_profiles(&db);
            std::fs::write(dir.path().join("photos/lossy"), b"not a directory").unwrap();
        }
        save_daemon_settings(&daemon, |settings| {
            settings.auto_compress_backlog = true;
            settings.auto_compress_keep_original = false;
            settings.auto_compress_immediate = true;
        });
        let original = original_path(&dir);
        write_gradient_photo(&original);
        daemon.run_once().unwrap();
        daemon.compress_pending().unwrap();
        daemon.process_deletions().unwrap();

        let db = Db::open(&db_path).unwrap();
        let photo = db
            .photo_by_path(original.to_str().unwrap())
            .unwrap()
            .unwrap();
        assert!(original.exists(), "a failed job must keep the original");
        assert_eq!(photo.status, PhotoStatus::Replaced);
        let request = db.active_deletion_request(photo.id).unwrap().unwrap();
        assert_eq!(request.status, DeletionStatus::Blocked);
        assert_eq!(request.blocked_reason.as_deref(), Some("job_failed"));

        std::fs::remove_file(dir.path().join("photos/lossy")).unwrap();
        let failed = db.list_jobs_by_status(QueueStatus::Failed, 10).unwrap();
        assert_eq!(failed.len(), 1);
        db.retry_job(&failed[0].id).unwrap();
        drop(db);
        daemon.compress_pending().unwrap();
        daemon.process_deletions().unwrap();

        let db = Db::open(&db_path).unwrap();
        assert!(!original.exists());
        assert!(db.active_deletion_request(photo.id).unwrap().is_none());
        assert_eq!(db.count_jobs(QueueStatus::Failed).unwrap(), 0);
    }

    #[test]
    fn compression_workers_process_a_batch_without_idle_delay() {
        let dir = tempfile::tempdir().unwrap();
        let mut daemon = test_daemon(&dir);
        let photo_path = dir
            .path()
            .join("photos/VRChat_2026-10-21_21-14-03.221_4x3.png");
        write_photo(&photo_path);
        daemon.run_once().unwrap();
        std::fs::remove_file(&photo_path).unwrap();

        {
            let db = daemon.db.lock().unwrap();
            let photo = db
                .photo_by_path(&photo_path.to_string_lossy())
                .unwrap()
                .expect("indexed photo");
            let profile = db
                .ensure_default_profiles()
                .unwrap()
                .into_iter()
                .next()
                .expect("default profile");
            db.enqueue_job(photo.id, &profile.id, "keep").unwrap();
            db.enqueue_job(photo.id, &profile.id, "keep").unwrap();
        }

        daemon.settings.compression_workers = 2;
        let events = daemon.events.subscribe();
        let started = Instant::now();
        daemon.start_workers();
        let mut failures = 0;
        while failures < 2 {
            let event = events
                .recv_timeout(Duration::from_millis(300))
                .expect("worker idle delay");
            if let Event::CompressionProgress {
                status: JobStatus::Failed,
                ..
            } = event
            {
                failures += 1;
            }
        }
        assert!(
            started.elapsed() < Duration::from_millis(300),
            "batch took {:?}",
            started.elapsed()
        );
        daemon.stop_workers();
    }

    #[test]
    fn compress_pending_processes_every_profile_for_one_photo() {
        let dir = tempfile::tempdir().unwrap();
        let mut daemon = manual_test_daemon(&dir);
        daemon.run_once().unwrap();
        let db = Db::open(dir.path().join("library.db")).unwrap();
        let photo = db
            .photo_by_path(original_path(&dir).to_str().unwrap())
            .unwrap()
            .unwrap();
        save_batch_profile(
            &db,
            "webp",
            vrc_photon_core::model::CompressionFormat::Webp,
            "_webp",
            None,
        );
        save_batch_profile(
            &db,
            "jpeg",
            vrc_photon_core::model::CompressionFormat::Jpeg,
            "_jpeg",
            None,
        );
        db.enqueue_job(photo.id, "webp", "keep").unwrap();
        db.enqueue_job(photo.id, "jpeg", "keep").unwrap();

        daemon.compress_pending().unwrap();

        assert!(
            daemon.running.load(Ordering::Relaxed),
            "compress_pending must not start and stop the worker set"
        );
        assert!(daemon.workers.is_empty());
        let db = Db::open(dir.path().join("library.db")).unwrap();
        let jobs = db.list_jobs(10).unwrap();
        assert_eq!(jobs.len(), 2);
        assert!(jobs.iter().all(|job| job.status == QueueStatus::Completed));
        assert_eq!(db.derivatives_for_photo(photo.id).unwrap().len(), 2);
    }

    #[test]
    fn batch_keeps_the_original_source_after_a_replace_job() {
        let dir = tempfile::tempdir().unwrap();
        let mut daemon = manual_test_daemon(&dir);
        daemon.run_once().unwrap();
        let db = Db::open(dir.path().join("library.db")).unwrap();
        let photo = db
            .photo_by_path(original_path(&dir).to_str().unwrap())
            .unwrap()
            .unwrap();
        save_batch_profile(
            &db,
            "small",
            vrc_photon_core::model::CompressionFormat::Webp,
            "_small",
            Some((8, 8)),
        );
        save_batch_profile(
            &db,
            "full",
            vrc_photon_core::model::CompressionFormat::Webp,
            "_full",
            None,
        );
        db.enqueue_job(photo.id, "small", "replace").unwrap();
        db.enqueue_job(photo.id, "full", "keep").unwrap();

        daemon.compress_pending().unwrap();

        let db = Db::open(dir.path().join("library.db")).unwrap();
        let derivatives = db.derivatives_for_photo(photo.id).unwrap();
        let small = derivatives
            .iter()
            .find(|derivative| derivative.profile_id == "small")
            .unwrap();
        let full = derivatives
            .iter()
            .find(|derivative| derivative.profile_id == "full")
            .unwrap();
        assert_eq!((small.width, small.height), (8, 5));
        assert_eq!(
            (full.width, full.height),
            (16, 10),
            "second job read the derivative"
        );
    }

    #[test]
    fn batch_continues_after_one_profile_fails() {
        let dir = tempfile::tempdir().unwrap();
        let mut daemon = manual_test_daemon(&dir);
        daemon.run_once().unwrap();
        let blocked = dir.path().join("photos/not-a-directory");
        std::fs::write(&blocked, b"x").unwrap();
        let db = Db::open(dir.path().join("library.db")).unwrap();
        let photo = db
            .photo_by_path(original_path(&dir).to_str().unwrap())
            .unwrap()
            .unwrap();
        save_batch_profile(
            &db,
            "good",
            vrc_photon_core::model::CompressionFormat::Webp,
            "_good",
            None,
        );
        save_batch_profile(
            &db,
            "bad",
            vrc_photon_core::model::CompressionFormat::Webp,
            "_bad",
            None,
        );
        db.enqueue_job(photo.id, "good", "keep").unwrap();
        db.enqueue_job_with_output_dir(photo.id, "bad", "keep", Some(blocked.to_str().unwrap()))
            .unwrap();

        daemon.compress_pending().unwrap();

        let db = Db::open(dir.path().join("library.db")).unwrap();
        let jobs = db.list_jobs(10).unwrap();
        let good = jobs.iter().find(|job| job.profile_id == "good").unwrap();
        let bad = jobs.iter().find(|job| job.profile_id == "bad").unwrap();
        assert_eq!(good.status, QueueStatus::Completed);
        assert_eq!(bad.status, QueueStatus::Failed);
        assert_eq!(db.derivatives_for_photo(photo.id).unwrap().len(), 1);
    }

    #[test]
    fn batch_decode_failure_fails_every_job() {
        let dir = tempfile::tempdir().unwrap();
        let mut daemon = manual_test_daemon(&dir);
        daemon.run_once().unwrap();
        let db = Db::open(dir.path().join("library.db")).unwrap();
        let photo = db
            .photo_by_path(original_path(&dir).to_str().unwrap())
            .unwrap()
            .unwrap();
        save_batch_profile(
            &db,
            "first",
            vrc_photon_core::model::CompressionFormat::Webp,
            "_first",
            None,
        );
        save_batch_profile(
            &db,
            "second",
            vrc_photon_core::model::CompressionFormat::Webp,
            "_second",
            None,
        );
        db.enqueue_job(photo.id, "first", "keep").unwrap();
        db.enqueue_job(photo.id, "second", "keep").unwrap();
        std::fs::remove_file(original_path(&dir)).unwrap();

        daemon.compress_pending().unwrap();

        let db = Db::open(dir.path().join("library.db")).unwrap();
        let jobs = db.list_jobs(10).unwrap();
        assert_eq!(jobs.len(), 2);
        assert!(jobs.iter().all(|job| job.status == QueueStatus::Failed));
        assert!(jobs.iter().all(|job| job.error.is_some()));
    }

    #[test]
    fn forced_deletion_cancels_the_rest_of_the_batch() {
        let dir = tempfile::tempdir().unwrap();
        let mut daemon = manual_test_daemon(&dir);
        daemon.run_once().unwrap();
        let db = Db::open(dir.path().join("library.db")).unwrap();
        let photo = db
            .photo_by_path(original_path(&dir).to_str().unwrap())
            .unwrap()
            .unwrap();
        save_batch_profile(
            &db,
            "first",
            vrc_photon_core::model::CompressionFormat::Webp,
            "_first",
            None,
        );
        save_batch_profile(
            &db,
            "second",
            vrc_photon_core::model::CompressionFormat::Webp,
            "_second",
            None,
        );
        db.enqueue_job(photo.id, "first", "keep").unwrap();
        db.enqueue_job(photo.id, "second", "keep").unwrap();
        let jobs = db.next_pending_photo_jobs().unwrap();
        assert_eq!(jobs.len(), 2);
        let request = db
            .request_deletion(&[photo.id], DeletionReason::Manual, false)
            .unwrap()[0];
        assert!(db.force_deletion(request).unwrap());

        let outcomes = process_photo_batch(&daemon.db, &jobs, Some(&daemon.thumbnail_dir));

        assert_eq!(outcomes[0].status, QueueStatus::Completed);
        assert_eq!(outcomes[1].status, QueueStatus::Cancelled);
        let jobs = db.list_jobs(10).unwrap();
        let cancelled = jobs
            .iter()
            .find(|job| job.status == QueueStatus::Cancelled)
            .unwrap();
        assert!(cancelled.finished_at.is_some());
        assert!(db.claim_deletion(request).unwrap().is_some());
    }

    #[test]
    fn scan_root_command_scopes_missing_detection() {
        let dir = tempfile::tempdir().unwrap();
        let root_a = dir.path().join("photos");
        let root_b = dir.path().join("imported");
        let log_dir = dir.path().join("logs");
        std::fs::create_dir_all(&root_a).unwrap();
        std::fs::create_dir_all(&root_b).unwrap();
        std::fs::create_dir_all(&log_dir).unwrap();
        let b_path = root_b.join("VRChat_2026-10-21_21-24-00.000_4x3.png");
        write_photo(&b_path);
        let db_path = dir.path().join("library.db");
        let mut daemon = Daemon::open(&db_path, Some(root_a.clone()), Some(log_dir)).unwrap();
        daemon.set_thumbnail_dir(dir.path().join("thumbnails"));
        save_daemon_settings(&daemon, |settings| {
            settings
                .photo_roots
                .push(root_b.to_string_lossy().to_string());
        });
        {
            let db = Db::open(&db_path).unwrap();
            db.request_daemon_command(DaemonCommand::Rescan).unwrap();
        }
        daemon.poll_commands().unwrap();

        let db = Db::open(&db_path).unwrap();
        assert_eq!(db.count_photos().unwrap(), 1);
        std::fs::remove_file(&b_path).unwrap();
        let a_path = root_a.join("VRChat_2026-10-21_21-25-00.000_4x3.png");
        // Distinct pixels and dimensions keep this from being a rebind
        // candidate for the removed B photo.
        image::RgbaImage::from_pixel(8, 5, image::Rgba([1, 2, 3, 255]))
            .save(&a_path)
            .unwrap();
        assert!(
            db.request_daemon_scan_root(root_a.to_str().unwrap())
                .unwrap()
        );
        daemon.poll_commands().unwrap();

        let db = Db::open(&db_path).unwrap();
        assert_eq!(
            db.photo_by_path(b_path.to_str().unwrap())
                .unwrap()
                .unwrap()
                .status,
            PhotoStatus::Present
        );
        assert!(
            db.photo_by_path(a_path.to_str().unwrap())
                .unwrap()
                .is_some()
        );

        db.request_daemon_command(DaemonCommand::Rescan).unwrap();
        daemon.poll_commands().unwrap();
        assert_eq!(
            db.photo_by_path(b_path.to_str().unwrap())
                .unwrap()
                .unwrap()
                .status,
            PhotoStatus::Missing
        );
    }

    #[test]
    fn scan_root_command_indexes_an_unconfigured_directory() {
        let dir = tempfile::tempdir().unwrap();
        let mut daemon = test_daemon(&dir);
        let imported = dir.path().join("imported");
        std::fs::create_dir_all(&imported).unwrap();
        let path = imported.join("VRChat_2026-10-21_21-24-00.000_4x3.png");
        write_photo(&path);
        {
            let db = daemon.db.lock().unwrap();
            db.request_daemon_scan_root(imported.to_str().unwrap())
                .unwrap();
        }
        daemon.poll_commands().unwrap();

        let db = daemon.db.lock().unwrap();
        assert!(db.photo_by_path(path.to_str().unwrap()).unwrap().is_some());
        assert_eq!(db.count_photos().unwrap(), 1);
    }

    #[test]
    fn scan_root_command_enqueues_auto_jobs_without_backlog() {
        let dir = tempfile::tempdir().unwrap();
        let root_a = dir.path().join("photos");
        let root_b = dir.path().join("imported");
        let log_dir = dir.path().join("logs");
        std::fs::create_dir_all(&root_a).unwrap();
        std::fs::create_dir_all(&root_b).unwrap();
        std::fs::create_dir_all(&log_dir).unwrap();
        write_photo(&root_b.join("VRChat_2026-10-21_21-24-00.000_4x3.png"));
        let db_path = dir.path().join("library.db");
        add_auto_profile(&db_path);
        let mut daemon = Daemon::open(&db_path, Some(root_a), Some(log_dir)).unwrap();
        daemon.set_thumbnail_dir(dir.path().join("thumbnails"));
        save_daemon_settings(&daemon, |settings| {
            settings.auto_compress_backlog = false;
            settings
                .photo_roots
                .push(root_b.to_string_lossy().to_string());
        });
        {
            let db = Db::open(&db_path).unwrap();
            db.request_daemon_scan_root(root_b.to_str().unwrap())
                .unwrap();
        }
        daemon.poll_commands().unwrap();

        let db = Db::open(&db_path).unwrap();
        assert_eq!(db.count_jobs(QueueStatus::Pending).unwrap(), 1);
    }

    #[test]
    fn startup_scan_enqueues_imported_photos_before_backlog() {
        let dir = tempfile::tempdir().unwrap();
        let backlog_root = dir.path().join("photos");
        let import_root = dir.path().join("imported");
        let log_dir = dir.path().join("logs");
        std::fs::create_dir_all(&backlog_root).unwrap();
        std::fs::create_dir_all(&import_root).unwrap();
        std::fs::create_dir_all(&log_dir).unwrap();
        let backlog_photo = backlog_root.join("VRChat_2026-10-21_21-14-03.221_4x3.png");
        let imported_photo = import_root.join("VRChat_2026-10-21_21-24-00.000_4x3.png");
        write_photo(&backlog_photo);
        write_photo(&imported_photo);

        let db_path = dir.path().join("library.db");
        add_auto_profile(&db_path);
        let mut daemon = Daemon::open(&db_path, Some(backlog_root.clone()), Some(log_dir)).unwrap();
        daemon.set_thumbnail_dir(dir.path().join("thumbnails"));
        daemon.set_ipc_endpoint(None);
        save_daemon_settings(&daemon, |settings| {
            settings.auto_compress = true;
            settings.auto_compress_backlog = false;
            settings.auto_compress_immediate = true;
            settings.photo_roots = vec![
                backlog_root.to_string_lossy().to_string(),
                import_root.to_string_lossy().to_string(),
            ];
        });
        {
            let db = Db::open(&db_path).unwrap();
            db.request_daemon_scan_root(import_root.to_str().unwrap())
                .unwrap();
        }

        daemon.startup_scan().unwrap();

        let db = Db::open(&db_path).unwrap();
        assert_eq!(db.count_photos().unwrap(), 2);
        let imported_id = db
            .photo_by_path(imported_photo.to_str().unwrap())
            .unwrap()
            .unwrap()
            .id;
        let backlog_id = db
            .photo_by_path(backlog_photo.to_str().unwrap())
            .unwrap()
            .unwrap()
            .id;
        let jobs = db.list_jobs(10).unwrap();
        assert_eq!(
            jobs.iter()
                .filter(|job| job.photo_id == imported_id)
                .count(),
            1
        );
        assert_eq!(
            jobs.iter().filter(|job| job.photo_id == backlog_id).count(),
            0
        );
        assert_eq!(jobs[0].status, QueueStatus::Pending);
    }

    #[test]
    fn scan_root_finished_event_reports_auto_enqueued_jobs() {
        let dir = tempfile::tempdir().unwrap();
        let photo_dir = dir.path().join("photos");
        let log_dir = dir.path().join("logs");
        std::fs::create_dir_all(&photo_dir).unwrap();
        std::fs::create_dir_all(&log_dir).unwrap();
        write_photo(&photo_dir.join("VRChat_2026-10-21_21-24-00.000_4x3.png"));

        let db_path = dir.path().join("library.db");
        add_auto_profile(&db_path);
        let mut daemon = Daemon::open(&db_path, Some(photo_dir.clone()), Some(log_dir)).unwrap();
        daemon.set_thumbnail_dir(dir.path().join("thumbnails"));
        daemon.set_ipc_endpoint(None);
        let events = daemon.events.subscribe();
        daemon.scan_root_photos(photo_dir.clone()).unwrap();

        let mut finished = None;
        while let Ok(event) = events.try_recv() {
            if matches!(event, Event::ScanFinished { .. }) {
                finished = Some(event);
            }
        }
        match finished {
            Some(Event::ScanFinished {
                auto_enqueued,
                target,
                ..
            }) => {
                assert_eq!(auto_enqueued, 1);
                assert_eq!(target.as_deref(), Some(photo_dir.to_str().unwrap()));
            }
            _ => panic!("scan did not finish"),
        }
    }

    #[test]
    fn enqueue_auto_jobs_skips_profiles_that_are_not_auto_enabled() {
        let dir = tempfile::tempdir().unwrap();
        let mut daemon = test_daemon(&dir);
        let photo = dir
            .path()
            .join("photos/VRChat_2026-10-21_21-24-00.000_4x3.png");
        write_photo(&photo);
        daemon.run_once().unwrap();

        let db = daemon.db.lock().unwrap();
        let photo_id = db
            .photo_by_path(photo.to_str().unwrap())
            .unwrap()
            .unwrap()
            .id;
        db.save_profile(&CompressionProfile {
            id: "manual-only".to_string(),
            name: "Manual only".to_string(),
            enabled: true,
            auto_enabled: false,
            ..CompressionProfile::default()
        })
        .unwrap();

        assert_eq!(enqueue_auto_jobs(&db, &[photo_id], true, true).unwrap(), 0);
        assert!(db.list_jobs(10).unwrap().is_empty());
    }

    #[test]
    fn unchanged_rescan_does_not_bump_the_library_revision() {
        let dir = tempfile::tempdir().unwrap();
        let mut daemon = test_daemon(&dir);
        write_photo(
            &dir.path()
                .join("photos/VRChat_2026-10-21_21-14-03.221_4x3.png"),
        );
        daemon.run_once().unwrap();
        let revision = {
            let db = daemon.db.lock().unwrap();
            db.library_revision().unwrap()
        };
        daemon.run_once().unwrap();
        let db = daemon.db.lock().unwrap();
        assert_eq!(db.library_revision().unwrap(), revision);
    }

    #[test]
    fn scan_root_photos_reports_a_single_root() {
        let dir = tempfile::tempdir().unwrap();
        let mut daemon = test_daemon(&dir);
        write_photo(
            &dir.path()
                .join("photos/VRChat_2026-10-21_21-14-03.221_4x3.png"),
        );
        let events = daemon.events.subscribe();
        let root = dir.path().join("photos");
        let report = daemon.scan_root_photos(root).unwrap();
        assert_eq!(report.indexed, 1);
        match events.try_recv().unwrap() {
            Event::ScanStarted { roots, .. } => assert_eq!(roots, 1),
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn scan_root_missing_path_records_failure_and_keeps_other_photos() {
        let dir = tempfile::tempdir().unwrap();
        let mut daemon = test_daemon(&dir);
        let photo = dir
            .path()
            .join("photos/VRChat_2026-10-21_21-14-03.221_4x3.png");
        write_photo(&photo);
        let root = dir.path().join("photos");
        daemon.scan_root_photos(root).unwrap();

        let missing = dir.path().join("does-not-exist");
        let report = daemon.scan_root_photos(missing.clone()).unwrap();
        assert_eq!(report.failures.len(), 1);
        assert_eq!(report.failures[0].path, missing);

        let not_a_dir = dir.path().join("not-a-dir.png");
        write_photo(&not_a_dir);
        let report = daemon.scan_root_photos(not_a_dir.clone()).unwrap();
        assert_eq!(report.failures.len(), 1);
        assert_eq!(report.failures[0].path, not_a_dir);

        let db = daemon.db.lock().unwrap();
        let indexed = db.photo_by_path(photo.to_str().unwrap()).unwrap().unwrap();
        assert_eq!(indexed.status, PhotoStatus::Present);
        let errors = db.daemon_errors().unwrap();
        assert!(
            errors
                .iter()
                .any(|error| error.operation == DaemonOperation::Scan)
        );
    }

    #[test]
    fn scan_root_sends_new_photos_to_the_thumbnail_queue() {
        let dir = tempfile::tempdir().unwrap();
        let mut daemon = test_daemon(&dir);
        let photo = dir
            .path()
            .join("photos/VRChat_2026-10-21_21-14-03.221_4x3.png");
        write_photo(&photo);
        let (tx, rx) = std::sync::mpsc::channel();
        daemon.thumbnail_tx = Some(tx);
        let events = daemon.events.subscribe();
        let root = dir.path().join("photos");
        let report = daemon.scan_root_photos(root.clone()).unwrap();
        assert_eq!(report.new_ids.len(), 1);

        assert_eq!(rx.try_recv().unwrap(), report.new_ids[0]);
        assert!(rx.try_recv().is_err());

        let mut finished = None;
        while let Ok(event) = events.try_recv() {
            if let Event::ScanFinished { .. } = event {
                finished = Some(event);
            }
        }
        match finished {
            Some(Event::ScanFinished {
                failures,
                failed,
                target,
                ..
            }) => {
                assert_eq!(failures, 0);
                assert!(!failed);
                assert_eq!(target.as_deref(), Some(root.to_str().unwrap()));
            }
            _ => panic!("scan did not finish"),
        }
    }

    #[test]
    fn scan_photos_publishes_started_progress_and_finished() {
        let dir = tempfile::tempdir().unwrap();
        let mut daemon = test_daemon(&dir);
        write_photo(
            &dir.path()
                .join("photos/VRChat_2026-10-21_21-14-03.221_4x3.png"),
        );
        let events = daemon.events.subscribe();
        daemon.run_once().unwrap();

        assert!(matches!(
            events.try_recv().unwrap(),
            Event::ScanStarted { .. }
        ));
        let mut finished = None;
        while let Ok(event) = events.try_recv() {
            if let Event::ScanFinished { .. } = event {
                finished = Some(event);
            }
        }
        match finished {
            Some(Event::ScanFinished { indexed, .. }) => assert_eq!(indexed, 1),
            _ => panic!("scan did not finish"),
        }
    }

    #[test]
    fn scan_photos_stops_between_roots_when_stopping() {
        let dir = tempfile::tempdir().unwrap();
        let mut daemon = test_daemon(&dir);
        let second = dir.path().join("photos2");
        std::fs::create_dir_all(&second).unwrap();
        write_photo(&second.join("VRChat_2026-10-21_21-14-04.221_4x3.png"));
        daemon.settings.photo_roots = vec![
            dir.path().join("photos").to_string_lossy().to_string(),
            second.to_string_lossy().to_string(),
        ];
        daemon.running.store(false, Ordering::Relaxed);

        let report = daemon.scan_photos().unwrap();
        assert_eq!(report.indexed, 0);
        assert_eq!(daemon.db.lock().unwrap().count_photos().unwrap(), 0);
    }

    #[test]
    fn set_paused_publishes_status_changed_and_wakes() {
        let dir = tempfile::tempdir().unwrap();
        let daemon = test_daemon(&dir);
        let events = daemon.events.subscribe();
        daemon.set_paused(true);
        assert!(matches!(
            events.try_recv().unwrap(),
            Event::StatusChanged { .. }
        ));
        assert!(daemon.wake.is_pending());
    }

    #[test]
    fn report_daemon_error_publishes_changed_once() {
        let dir = tempfile::tempdir().unwrap();
        let mut daemon = test_daemon(&dir);
        let events = daemon.events.subscribe();
        daemon
            .report_daemon_error(DaemonOperation::Scan, "boom", None)
            .unwrap();
        assert!(matches!(
            events.try_recv().unwrap(),
            Event::DaemonErrorChanged {
                operation: Operation::Scan
            }
        ));
        daemon
            .report_daemon_error(DaemonOperation::Scan, "boom", None)
            .unwrap();
        assert!(events.try_recv().is_err());
    }

    #[test]
    fn start_ipc_serves_the_daemon_status() {
        let dir = tempfile::tempdir().unwrap();
        let mut daemon = test_daemon(&dir);
        let endpoint = Endpoint::for_tests(dir.path());
        daemon.set_ipc_endpoint(Some(endpoint.clone()));
        daemon.start_ipc();

        let response = vrc_photon_ipc::client::request(
            &endpoint,
            vrc_photon_ipc::Request::Status,
            Duration::from_secs(1),
        )
        .unwrap();
        assert!(matches!(response, vrc_photon_ipc::Response::Status { .. }));
        daemon.ipc = None;
    }

    #[test]
    fn auto_jobs_follow_profile_sort_order() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path().join("library.db")).unwrap();
        let root = db.add_root("/photos", "reference").unwrap();
        let photo_id = db
            .upsert_photo(&vrc_photon_core::model::NewPhoto {
                root_id: root,
                rel_path: "a.png".to_string(),
                abs_path: "/photos/a.png".to_string(),
                file_name: "a.png".to_string(),
                captured_at_utc: 1_700_000_000,
                captured_at_local: "2026-10-21 21:14:03".to_string(),
                tz_offset_sec: 9 * 3600,
                width: 16,
                height: 10,
                file_size: 10,
                mtime: 1_700_000_000,
                blake3: None,
            })
            .unwrap();

        let make = |id: &str, name: &str| CompressionProfile {
            id: id.to_string(),
            name: name.to_string(),
            auto_enabled: true,
            ..CompressionProfile::default()
        };
        let a = make("a", "A");
        let b = make("b", "B");
        let c = make("c", "C");
        for profile in [&a, &b, &c] {
            db.save_profile(profile).unwrap();
        }
        db.reorder_profiles(&[c.id.clone(), a.id.clone(), b.id.clone()])
            .unwrap();

        // One photo: its auto profiles are enqueued in the saved order.
        enqueue_auto_jobs(&db, &[photo_id], true, true).unwrap();
        // Force the tie the rowid tie-break exists for.
        db.connection()
            .execute(
                "UPDATE compression_jobs SET created_at = '2026-01-01T00:00:00+00:00'",
                [],
            )
            .unwrap();

        let mut order = Vec::new();
        while let Some(job) = db.next_pending_job().unwrap() {
            order.push(job.profile_id);
        }
        assert_eq!(order, vec![c.id, a.id, b.id]);
    }

    #[test]
    fn auto_jobs_use_replace_policy_and_waiting_status() {
        let dir = tempfile::tempdir().unwrap();
        let photo_dir = dir.path().join("photos");
        let log_dir = dir.path().join("logs");
        std::fs::create_dir_all(&photo_dir).unwrap();
        std::fs::create_dir_all(&log_dir).unwrap();
        let original = photo_dir.join("VRChat_2026-10-21_21-24-00.000_4x3.png");
        write_photo(&original);

        let db_path = dir.path().join("library.db");
        add_auto_profile(&db_path);
        let mut daemon = Daemon::open(&db_path, Some(photo_dir), Some(log_dir)).unwrap();
        daemon.set_thumbnail_dir(dir.path().join("thumbnails"));
        daemon.run_once().unwrap();

        let db = Db::open(&db_path).unwrap();
        let photo_id = db
            .photo_by_path(original.to_str().unwrap())
            .unwrap()
            .unwrap()
            .id;
        enqueue_auto_jobs(&db, &[photo_id], false, false).unwrap();

        let job = db.list_jobs(1).unwrap().remove(0);
        assert_eq!(job.policy, "replace");
        assert_eq!(job.status, QueueStatus::Waiting);
        assert!(db.next_pending_job().unwrap().is_none());
    }

    #[test]
    fn periodic_settings_refresh_applies_saved_thumbnail_limit() {
        let dir = tempfile::tempdir().unwrap();
        let mut daemon = test_daemon(&dir);
        save_daemon_settings(&daemon, |settings| settings.thumbnail_cache_mb = 1);
        // The main loop calls this on every iteration, paused or not; make
        // the refresh due as it is half a second after the previous one.
        daemon.last_settings_check = Instant::now() - Duration::from_secs(5);
        daemon.refresh_periodic_settings();
        assert_eq!(
            daemon.thumbnail_cache_limit.load(Ordering::Relaxed),
            1024 * 1024
        );
    }

    #[test]
    fn periodic_settings_refresh_keeps_limit_until_due() {
        let dir = tempfile::tempdir().unwrap();
        let mut daemon = test_daemon(&dir);
        let before = daemon.thumbnail_cache_limit.load(Ordering::Relaxed);
        save_daemon_settings(&daemon, |settings| settings.thumbnail_cache_mb = 1);
        daemon.last_settings_check = Instant::now();
        daemon.refresh_periodic_settings();
        assert_eq!(daemon.thumbnail_cache_limit.load(Ordering::Relaxed), before);
    }

    #[test]
    fn malformed_settings_abort_open_without_overwriting_the_row() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("library.db");
        let raw = r#"{"photo_roots":["/tmp/photos"],"language":"#;
        {
            let db = Db::open(&db_path).unwrap();
            db.set_setting("app_settings", raw).unwrap();
        }

        let error = match Daemon::open(&db_path, None, None) {
            Ok(_) => panic!("the daemon must not start with malformed settings"),
            Err(error) => error,
        };
        let message = format!("{error:#}");
        assert!(message.contains("cannot load settings"), "{message}");
        assert!(message.contains("json error"), "{message}");

        let db = Db::open(&db_path).unwrap();
        assert_eq!(
            db.get_setting("app_settings").unwrap().as_deref(),
            Some(raw),
            "open must not save defaults over the malformed row"
        );
        assert_eq!(
            db.get_setting("app_settings_corrupt").unwrap().as_deref(),
            Some(raw)
        );
    }

    #[test]
    fn periodic_settings_refresh_keeps_in_memory_values_when_the_row_is_malformed() {
        let dir = tempfile::tempdir().unwrap();
        let mut daemon = test_daemon(&dir);
        daemon.settings.auto_compress = false;
        daemon.settings.autostart = false;
        {
            let db = daemon.db.lock().unwrap();
            db.set_setting("app_settings", "{not json").unwrap();
        }
        daemon.last_settings_check = Instant::now() - Duration::from_secs(5);
        daemon.last_autostart_check = Instant::now() - Duration::from_secs(11);

        daemon.refresh_periodic_settings();

        assert!(!daemon.settings.auto_compress);
        assert!(!daemon.settings.autostart);
        let db = daemon.db.lock().unwrap();
        assert_eq!(
            db.get_setting("app_settings").unwrap().as_deref(),
            Some("{not json"),
            "refresh must not save settings"
        );
    }

    #[test]
    fn refresh_compression_settings_applies_live_policy_and_waiting_mode() {
        let dir = tempfile::tempdir().unwrap();
        let photo_dir = dir.path().join("photos");
        let log_dir = dir.path().join("logs");
        std::fs::create_dir_all(&photo_dir).unwrap();
        std::fs::create_dir_all(&log_dir).unwrap();
        let first = photo_dir.join("VRChat_2026-10-21_21-24-00.000_4x3.png");
        write_photo(&first);

        let db_path = dir.path().join("library.db");
        add_auto_profile(&db_path);
        let mut daemon = Daemon::open(&db_path, Some(photo_dir.clone()), Some(log_dir)).unwrap();
        daemon.set_thumbnail_dir(dir.path().join("thumbnails"));
        daemon.run_once().unwrap();

        {
            let db = Db::open(&db_path).unwrap();
            let mut settings = AppSettings::load(&db).unwrap_or_default();
            settings.auto_compress = true;
            settings.auto_compress_immediate = false;
            settings.auto_compress_keep_original = false;
            settings.save(&db).unwrap();
        }
        let second = photo_dir.join("VRChat_2026-10-21_21-25-00.000_4x3.png");
        write_photo(&second);
        daemon.dirty_paths.push(second);
        daemon.index_dirty_paths().unwrap();

        assert!(!daemon.settings.auto_compress_immediate);
        assert!(!daemon.settings.auto_compress_keep_original);
        let db = Db::open(&db_path).unwrap();
        let job = db.list_jobs(1).unwrap().remove(0);
        assert_eq!(job.status, QueueStatus::Waiting);
        assert_eq!(job.policy, "replace");
    }

    #[test]
    fn auto_compress_holds_waiting_jobs_until_released() {
        let dir = tempfile::tempdir().unwrap();
        let photo_dir = dir.path().join("photos");
        let log_dir = dir.path().join("logs");
        std::fs::create_dir_all(&photo_dir).unwrap();
        std::fs::create_dir_all(&log_dir).unwrap();
        let original = photo_dir.join("VRChat_2026-10-21_21-24-00.000_4x3.png");
        write_photo(&original);

        let db_path = dir.path().join("library.db");
        add_auto_profile(&db_path);
        let mut daemon = Daemon::open(&db_path, Some(photo_dir), Some(log_dir)).unwrap();
        daemon.set_thumbnail_dir(dir.path().join("thumbnails"));
        save_daemon_settings(&daemon, |settings| {
            settings.auto_compress_backlog = true;
            settings.auto_compress_immediate = false;
        });
        daemon.run_once().unwrap();

        let db = Db::open(&db_path).unwrap();
        let photo = db
            .photo_by_path(original.to_str().unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(db.count_jobs(QueueStatus::Waiting).unwrap(), 1);
        daemon.compress_pending().unwrap();
        assert!(db.derivatives_for_photo(photo.id).unwrap().is_empty());

        db.release_waiting_jobs("keep").unwrap();
        daemon.compress_pending().unwrap();
        assert_eq!(db.derivatives_for_photo(photo.id).unwrap().len(), 1);
    }

    #[test]
    fn auto_replace_failure_leaves_the_original_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let photo_dir = dir.path().join("photos");
        let log_dir = dir.path().join("logs");
        std::fs::create_dir_all(&photo_dir).unwrap();
        std::fs::create_dir_all(&log_dir).unwrap();
        let original = photo_dir.join("VRChat_2026-10-21_21-24-00.000_4x3.png");
        write_photo(&original);
        // Make the default output directory impossible to create.
        std::fs::write(photo_dir.join("compressed"), b"not a directory").unwrap();

        let db_path = dir.path().join("library.db");
        add_auto_profile(&db_path);
        let mut daemon = Daemon::open(&db_path, Some(photo_dir), Some(log_dir)).unwrap();
        daemon.set_thumbnail_dir(dir.path().join("thumbnails"));
        save_daemon_settings(&daemon, |settings| {
            settings.auto_compress_backlog = true;
            settings.auto_compress_keep_original = false;
        });
        daemon.run_once().unwrap();
        daemon.compress_pending().unwrap();

        let db = Db::open(&db_path).unwrap();
        assert!(original.exists());
        let photo = db
            .photo_by_path(original.to_str().unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(photo.status, PhotoStatus::Present);
        assert!(db.derivatives_for_photo(photo.id).unwrap().is_empty());
        assert_eq!(db.list_jobs(1).unwrap()[0].status, QueueStatus::Failed);
    }

    #[test]
    fn end_to_end_auto_compress() {
        let dir = tempfile::tempdir().unwrap();
        let photo_dir = dir.path().join("photos");
        let log_dir = dir.path().join("logs");
        std::fs::create_dir_all(&photo_dir).unwrap();
        std::fs::create_dir_all(&log_dir).unwrap();
        let photo_path = photo_dir.join("VRChat_2026-10-21_21-24-00.000_4x3.png");
        write_photo(&photo_path);

        let db_path = dir.path().join("library.db");
        add_auto_profile(&db_path);
        let mut daemon = Daemon::open(&db_path, Some(photo_dir), Some(log_dir)).unwrap();
        daemon.set_thumbnail_dir(dir.path().join("thumbnails"));
        save_daemon_settings(&daemon, |settings| {
            settings.auto_compress_backlog = true;
        });
        daemon.run_once().unwrap();
        daemon.compress_pending().unwrap();

        let db = Db::open(&db_path).unwrap();
        let photo = db
            .photo_by_path(photo_path.to_str().unwrap())
            .unwrap()
            .expect("photo");
        let derivatives = db.derivatives_for_photo(photo.id).unwrap();
        assert_eq!(derivatives.len(), 1);
        assert!(Path::new(&derivatives[0].path).is_file());
        assert!(derivatives[0].file_size > 0);
    }

    #[test]
    fn initial_scan_skips_backlog_when_disabled() {
        let dir = tempfile::tempdir().unwrap();
        let photo_dir = dir.path().join("photos");
        let log_dir = dir.path().join("logs");
        std::fs::create_dir_all(&photo_dir).unwrap();
        std::fs::create_dir_all(&log_dir).unwrap();
        write_photo(&photo_dir.join("VRChat_2026-10-21_21-24-00.000_4x3.png"));

        let db_path = dir.path().join("library.db");
        add_auto_profile(&db_path);
        let mut daemon = Daemon::open(&db_path, Some(photo_dir.clone()), Some(log_dir)).unwrap();
        daemon.set_thumbnail_dir(dir.path().join("thumbnails"));
        daemon.run_once().unwrap();

        let db = Db::open(&db_path).unwrap();
        assert_eq!(db.count_jobs(QueueStatus::Pending).unwrap(), 0);

        let new_path = photo_dir.join("VRChat_2026-10-21_21-25-00.000_4x3.png");
        write_photo(&new_path);
        daemon.dirty_paths.push(new_path);
        daemon.index_dirty_paths().unwrap();
        assert_eq!(db.count_jobs(QueueStatus::Pending).unwrap(), 1);
    }

    #[test]
    fn initial_scan_enqueues_backlog_when_enabled() {
        let dir = tempfile::tempdir().unwrap();
        let photo_dir = dir.path().join("photos");
        let log_dir = dir.path().join("logs");
        std::fs::create_dir_all(&photo_dir).unwrap();
        std::fs::create_dir_all(&log_dir).unwrap();
        write_photo(&photo_dir.join("VRChat_2026-10-21_21-24-00.000_4x3.png"));

        let db_path = dir.path().join("library.db");
        add_auto_profile(&db_path);
        let mut daemon = Daemon::open(&db_path, Some(photo_dir), Some(log_dir)).unwrap();
        daemon.set_thumbnail_dir(dir.path().join("thumbnails"));
        save_daemon_settings(&daemon, |settings| {
            settings.auto_compress_backlog = true;
        });
        daemon.run_once().unwrap();

        let db = Db::open(&db_path).unwrap();
        assert_eq!(db.count_jobs(QueueStatus::Pending).unwrap(), 1);
    }

    #[test]
    fn incremental_events_index_and_mark_missing() {
        let dir = tempfile::tempdir().unwrap();
        let photo_dir = dir.path().join("photos");
        let log_dir = dir.path().join("logs");
        std::fs::create_dir_all(&photo_dir).unwrap();
        std::fs::create_dir_all(&log_dir).unwrap();
        let db_path = dir.path().join("library.db");
        let mut daemon = Daemon::open(&db_path, Some(photo_dir.clone()), Some(log_dir)).unwrap();
        daemon.set_thumbnail_dir(dir.path().join("thumbnails"));

        let path = photo_dir.join("VRChat_2026-10-21_21-24-00.000_4x3.png");
        image::RgbaImage::from_pixel(4, 3, image::Rgba([10, 20, 30, 255]))
            .save(&path)
            .unwrap();
        daemon.dirty_paths.push(path.clone());
        daemon.index_dirty_paths().unwrap();

        let db = Db::open(&db_path).unwrap();
        let photo = db
            .photo_by_path(path.to_str().unwrap())
            .unwrap()
            .expect("indexed");
        assert_eq!(photo.status, vrc_photon_core::model::PhotoStatus::Present);

        std::fs::remove_file(&path).unwrap();
        daemon.dirty_paths.push(path.clone());
        daemon.index_dirty_paths().unwrap();
        let photo = db
            .photo_by_path(path.to_str().unwrap())
            .unwrap()
            .expect("row");
        assert_eq!(photo.status, vrc_photon_core::model::PhotoStatus::Missing);
    }

    #[test]
    fn a_moved_file_keeps_the_same_row_and_tags() {
        let dir = tempfile::tempdir().unwrap();
        let photo_dir = dir.path().join("photos");
        let log_dir = dir.path().join("logs");
        std::fs::create_dir_all(&photo_dir).unwrap();
        std::fs::create_dir_all(&log_dir).unwrap();
        let db_path = dir.path().join("library.db");
        let mut daemon = Daemon::open(&db_path, Some(photo_dir.clone()), Some(log_dir)).unwrap();
        daemon.set_thumbnail_dir(dir.path().join("thumbnails"));

        let old_path = photo_dir.join("VRChat_2026-10-21_21-24-00.000_4x3.png");
        write_photo(&old_path);
        daemon.dirty_paths.push(old_path.clone());
        daemon.index_dirty_paths().unwrap();

        let db = Db::open(&db_path).unwrap();
        let photo = db
            .photo_by_path(old_path.to_str().unwrap())
            .unwrap()
            .expect("indexed");
        let photo_id = photo.id;
        db.set_photo_tags(photo_id, &["night".to_string()]).unwrap();
        let revision = db.library_revision().unwrap();

        let moved_dir = photo_dir.join("2026");
        std::fs::create_dir(&moved_dir).unwrap();
        let new_path = moved_dir.join("VRChat_2026-10-21_21-24-00.000_4x3.png");
        std::fs::rename(&old_path, &new_path).unwrap();
        // A real watcher may deliver the destination before the source.
        daemon.dirty_paths.push(new_path.clone());
        daemon.dirty_paths.push(old_path.clone());
        daemon.index_dirty_paths().unwrap();

        let photo = db.photo_by_id(photo_id).unwrap().unwrap();
        assert_eq!(photo.abs_path, new_path.to_string_lossy());
        assert_eq!(photo.status, vrc_photon_core::model::PhotoStatus::Present);
        assert_eq!(db.photo_tags(photo_id).unwrap(), vec!["night".to_string()]);
        assert_eq!(db.count_photos().unwrap(), 1);
        assert!(db.library_revision().unwrap() > revision);
    }

    #[test]
    fn a_full_scan_rebinds_a_photo_moved_while_the_daemon_was_stopped() {
        let dir = tempfile::tempdir().unwrap();
        let photo_dir = dir.path().join("photos");
        let log_dir = dir.path().join("logs");
        std::fs::create_dir_all(&photo_dir).unwrap();
        std::fs::create_dir_all(&log_dir).unwrap();
        let db_path = dir.path().join("library.db");
        let mut daemon = Daemon::open(&db_path, Some(photo_dir.clone()), Some(log_dir)).unwrap();
        daemon.set_thumbnail_dir(dir.path().join("thumbnails"));

        let old_path = photo_dir.join("VRChat_2026-10-21_21-24-00.000_4x3.png");
        write_photo(&old_path);
        daemon.run_once().unwrap();
        let db = Db::open(&db_path).unwrap();
        let photo_id = db
            .photo_by_path(old_path.to_str().unwrap())
            .unwrap()
            .unwrap()
            .id;
        db.set_photo_tags(photo_id, &["night".to_string()]).unwrap();

        let moved_dir = photo_dir.join("2026");
        std::fs::create_dir(&moved_dir).unwrap();
        let new_path = moved_dir.join("VRChat_2026-10-21_21-24-00.000_4x3.png");
        std::fs::rename(&old_path, &new_path).unwrap();
        daemon.run_once().unwrap();

        let photo = db.photo_by_id(photo_id).unwrap().unwrap();
        assert_eq!(photo.abs_path, new_path.to_string_lossy());
        assert_eq!(photo.status, vrc_photon_core::model::PhotoStatus::Present);
        assert_eq!(db.photo_tags(photo_id).unwrap(), vec!["night".to_string()]);
        assert_eq!(db.count_photos().unwrap(), 1);
    }

    #[test]
    fn a_renamed_file_keeps_the_same_row() {
        let dir = tempfile::tempdir().unwrap();
        let photo_dir = dir.path().join("photos");
        let log_dir = dir.path().join("logs");
        std::fs::create_dir_all(&photo_dir).unwrap();
        std::fs::create_dir_all(&log_dir).unwrap();
        let db_path = dir.path().join("library.db");
        let mut daemon = Daemon::open(&db_path, Some(photo_dir.clone()), Some(log_dir)).unwrap();
        daemon.set_thumbnail_dir(dir.path().join("thumbnails"));

        let old_path = photo_dir.join("VRChat_2026-10-21_21-24-00.000_4x3.png");
        write_photo(&old_path);
        daemon.dirty_paths.push(old_path.clone());
        daemon.index_dirty_paths().unwrap();

        let db = Db::open(&db_path).unwrap();
        let photo = db
            .photo_by_path(old_path.to_str().unwrap())
            .unwrap()
            .expect("indexed");
        let photo_id = photo.id;
        let captured_at = photo.captured_at_utc;
        db.set_photo_tags(photo_id, &["night".to_string()]).unwrap();

        let new_path = photo_dir.join("renamed.png");
        std::fs::rename(&old_path, &new_path).unwrap();
        daemon.dirty_paths.push(new_path.clone());
        daemon.dirty_paths.push(old_path.clone());
        daemon.index_dirty_paths().unwrap();

        let photo = db.photo_by_id(photo_id).unwrap().unwrap();
        assert_eq!(photo.abs_path, new_path.to_string_lossy());
        assert_eq!(photo.captured_at_utc, captured_at);
        assert_eq!(photo.status, vrc_photon_core::model::PhotoStatus::Present);
        assert_eq!(db.photo_tags(photo_id).unwrap(), vec!["night".to_string()]);
        assert_eq!(db.count_photos().unwrap(), 1);
    }

    #[test]
    fn a_move_between_configured_roots_keeps_the_same_row() {
        let dir = tempfile::tempdir().unwrap();
        let first_root = dir.path().join("photos");
        let second_root = dir.path().join("imported");
        let log_dir = dir.path().join("logs");
        std::fs::create_dir_all(&first_root).unwrap();
        std::fs::create_dir_all(&second_root).unwrap();
        std::fs::create_dir_all(&log_dir).unwrap();

        let old_path = first_root.join("VRChat_2026-10-21_21-24-00.000_4x3.png");
        write_photo(&old_path);
        let db_path = dir.path().join("library.db");
        let mut daemon = Daemon::open(&db_path, Some(first_root.clone()), Some(log_dir)).unwrap();
        daemon.settings.photo_roots = vec![
            first_root.to_string_lossy().to_string(),
            second_root.to_string_lossy().to_string(),
        ];
        daemon.set_thumbnail_dir(dir.path().join("thumbnails"));
        daemon.run_once().unwrap();

        let db = Db::open(&db_path).unwrap();
        let photo = db
            .photo_by_path(old_path.to_str().unwrap())
            .unwrap()
            .unwrap();
        let photo_id = photo.id;
        let new_path = second_root.join("VRChat_2026-10-21_21-24-00.000_4x3.png");
        std::fs::rename(&old_path, &new_path).unwrap();
        daemon.run_once().unwrap();

        let photo = db.photo_by_id(photo_id).unwrap().unwrap();
        assert_eq!(photo.abs_path, new_path.to_string_lossy());
        assert_eq!(
            photo.root_id,
            db.add_root(second_root.to_str().unwrap(), "reference")
                .unwrap()
        );
        assert_eq!(db.count_photos().unwrap(), 1);
    }

    #[test]
    fn deleting_the_original_promotes_an_existing_derivative() {
        let dir = tempfile::tempdir().unwrap();
        let photo_dir = dir.path().join("photos");
        let log_dir = dir.path().join("logs");
        std::fs::create_dir_all(&photo_dir).unwrap();
        std::fs::create_dir_all(&log_dir).unwrap();
        let db_path = dir.path().join("library.db");
        let mut daemon = Daemon::open(&db_path, Some(photo_dir.clone()), Some(log_dir)).unwrap();
        daemon.set_thumbnail_dir(dir.path().join("thumbnails"));

        let original = photo_dir.join("VRChat_2026-10-21_21-24-00.000_4x3.png");
        write_photo(&original);
        daemon.dirty_paths.push(original.clone());
        daemon.index_dirty_paths().unwrap();

        let db = Db::open(&db_path).unwrap();
        let photo = db
            .photo_by_path(original.to_str().unwrap())
            .unwrap()
            .expect("indexed");
        let compressed = photo_dir.join("compressed").join("kept.webp");
        std::fs::create_dir_all(compressed.parent().unwrap()).unwrap();
        std::fs::copy(&original, &compressed).unwrap();
        let derivative_id = db
            .add_derivative(
                photo.id,
                compressed.to_str().unwrap(),
                "webp",
                16,
                10,
                std::fs::metadata(&compressed).unwrap().len(),
                "p",
                "keep",
            )
            .unwrap();

        std::fs::remove_file(&original).unwrap();
        daemon.dirty_paths.push(original.clone());
        daemon.index_dirty_paths().unwrap();

        let photo = db.photo_by_id(photo.id).unwrap().unwrap();
        assert_eq!(photo.status, vrc_photon_core::model::PhotoStatus::Replaced);
        assert_eq!(photo.display_derivative_id, Some(derivative_id));
        assert_eq!(photo.effective_path(), compressed.to_str().unwrap());
        assert_eq!(db.count_photos().unwrap(), 1);
    }

    #[test]
    fn missing_original_promotes_the_preferred_profile() {
        let dir = tempfile::tempdir().unwrap();
        let photo_dir = dir.path().join("photos");
        let log_dir = dir.path().join("logs");
        std::fs::create_dir_all(&photo_dir).unwrap();
        std::fs::create_dir_all(&log_dir).unwrap();
        let db_path = dir.path().join("library.db");
        let mut daemon = Daemon::open(&db_path, Some(photo_dir.clone()), Some(log_dir)).unwrap();
        daemon.set_thumbnail_dir(dir.path().join("thumbnails"));

        let original = photo_dir.join("VRChat_2026-10-21_21-24-00.000_4x3.png");
        write_photo(&original);
        daemon.dirty_paths.push(original.clone());
        daemon.index_dirty_paths().unwrap();

        let db = Db::open(&db_path).unwrap();
        add_replace_profiles(&db);
        let photo = db
            .photo_by_path(original.to_str().unwrap())
            .unwrap()
            .expect("indexed");
        let lossless_path = dir.path().join("derivatives/lossless.webp");
        let lossy_path = dir.path().join("derivatives/lossy.webp");
        std::fs::create_dir_all(lossless_path.parent().unwrap()).unwrap();
        std::fs::write(&lossless_path, b"lossless").unwrap();
        std::fs::write(&lossy_path, b"lossy").unwrap();
        let lossy = db
            .add_derivative(
                photo.id,
                lossy_path.to_str().unwrap(),
                "webp",
                16,
                10,
                5,
                "lossy",
                "keep",
            )
            .unwrap();
        // Larger derivative id, but second in the profile list.
        let lossless = db
            .add_derivative(
                photo.id,
                lossless_path.to_str().unwrap(),
                "webp",
                16,
                10,
                8,
                "lossless",
                "keep",
            )
            .unwrap();

        std::fs::remove_file(&original).unwrap();
        daemon.dirty_paths.push(original.clone());
        daemon.index_dirty_paths().unwrap();

        let photo = db.photo_by_id(photo.id).unwrap().unwrap();
        assert_eq!(photo.display_derivative_id, Some(lossy));
        assert_ne!(photo.display_derivative_id, Some(lossless));
        assert!(lossy_path.is_file());
    }

    #[test]
    fn replace_compression_keeps_the_photo_visible() {
        let dir = tempfile::tempdir().unwrap();
        let photo_dir = dir.path().join("photos");
        let log_dir = dir.path().join("logs");
        std::fs::create_dir_all(&photo_dir).unwrap();
        std::fs::create_dir_all(&log_dir).unwrap();
        let original = photo_dir.join("VRChat_2026-10-21_21-24-00.000_4x3.png");
        write_photo(&original);

        let db_path = dir.path().join("library.db");
        add_auto_profile(&db_path);
        let mut daemon = Daemon::open(&db_path, Some(photo_dir.clone()), Some(log_dir)).unwrap();
        daemon.set_thumbnail_dir(dir.path().join("thumbnails"));
        daemon.run_once().unwrap();

        let db = Db::open(&db_path).unwrap();
        let photo = db
            .photo_by_path(original.to_str().unwrap())
            .unwrap()
            .expect("indexed");
        let profile = db.list_profiles().unwrap()[0].id.clone();
        db.enqueue_job(photo.id, &profile, "replace").unwrap();
        daemon.compress_pending().unwrap();

        let photo = db.photo_by_id(photo.id).unwrap().unwrap();
        assert_eq!(photo.status, vrc_photon_core::model::PhotoStatus::Replaced);
        assert!(photo.display_derivative_id.is_some());
        assert!(
            photo
                .display_path
                .as_deref()
                .is_some_and(|path| Path::new(path).is_file())
        );
        assert!(!original.exists());
        assert_eq!(db.count_photos().unwrap(), 1);
        assert_eq!(db.stats().unwrap().photo_count, 1);

        // A subsequent full scan must not turn the replaced photo into a
        // missing one just because its original path is gone.
        daemon.run_once().unwrap();
        let photo = db.photo_by_id(photo.id).unwrap().unwrap();
        assert_eq!(photo.status, vrc_photon_core::model::PhotoStatus::Replaced);

        let derivative_path = photo.display_path.clone().expect("display derivative");
        std::fs::remove_file(&derivative_path).unwrap();
        daemon.verify_replaced_assets().unwrap();
        let photo = db.photo_by_id(photo.id).unwrap().unwrap();
        assert_eq!(photo.status, vrc_photon_core::model::PhotoStatus::Missing);
        assert!(photo.display_derivative_id.is_none());
        assert_eq!(db.count_photos().unwrap(), 0);
    }

    #[test]
    fn verify_replaced_assets_keeps_live_files_and_marks_deleted_ones() {
        let dir = tempfile::tempdir().unwrap();
        let mut daemon = test_daemon(&dir);
        let photo_dir = dir.path().join("photos");
        let compressed = photo_dir.join("compressed");
        std::fs::create_dir_all(&compressed).unwrap();

        let live_derivative = compressed.join("live.webp");
        std::fs::write(&live_derivative, b"live").unwrap();
        let live_id = add_replaced_photo(
            &daemon,
            &photo_dir.join("VRChat_live.png"),
            &live_derivative,
        );
        let missing_id = add_replaced_photo(
            &daemon,
            &photo_dir.join("VRChat_missing.png"),
            &compressed.join("missing.webp"),
        );
        let no_display_id = add_replaced_photo(
            &daemon,
            &photo_dir.join("VRChat_no_display.png"),
            &compressed.join("no_display.webp"),
        );
        {
            let db = daemon.db.lock().unwrap();
            db.clear_display_derivative(no_display_id).unwrap();
        }
        let revision = {
            let db = daemon.db.lock().unwrap();
            db.library_revision().unwrap()
        };

        daemon.verify_replaced_assets().unwrap();

        let db = daemon.db.lock().unwrap();
        let live = db.photo_by_id(live_id).unwrap().unwrap();
        assert_eq!(live.status, vrc_photon_core::model::PhotoStatus::Replaced);
        assert!(live.display_derivative_id.is_some());
        for id in [missing_id, no_display_id] {
            let photo = db.photo_by_id(id).unwrap().unwrap();
            assert_eq!(photo.status, vrc_photon_core::model::PhotoStatus::Missing);
            assert!(photo.display_derivative_id.is_none());
            assert!(photo.display_path.is_none());
        }
        assert_eq!(db.library_revision().unwrap(), revision + 1);
        assert_eq!(db.count_photos().unwrap(), 1);
    }

    #[test]
    fn verify_replaced_assets_skips_photos_when_the_root_is_offline() {
        let dir = tempfile::tempdir().unwrap();
        let mut daemon = test_daemon(&dir);
        let offline = dir.path().join("offline-drive");
        let id = add_replaced_photo(
            &daemon,
            &offline.join("VRChat_offline.png"),
            &offline.join("compressed/offline.webp"),
        );
        let (revision, derivative_id) = {
            let db = daemon.db.lock().unwrap();
            (
                db.library_revision().unwrap(),
                db.photo_by_id(id).unwrap().unwrap().display_derivative_id,
            )
        };
        assert!(!offline.is_dir());

        daemon.verify_replaced_assets().unwrap();

        let db = daemon.db.lock().unwrap();
        let photo = db.photo_by_id(id).unwrap().unwrap();
        assert_eq!(photo.status, vrc_photon_core::model::PhotoStatus::Replaced);
        assert_eq!(photo.display_derivative_id, derivative_id);
        assert_eq!(db.library_revision().unwrap(), revision);
    }

    #[test]
    fn verify_replaced_assets_releases_the_db_lock_while_checking_files() {
        let dir = tempfile::tempdir().unwrap();
        let daemon = test_daemon(&dir);
        let root = dir.path().join("photos");
        let rows = vec![
            (
                1,
                Some(root.join("a.webp").to_string_lossy().to_string()),
                root.to_string_lossy().to_string(),
            ),
            (
                2,
                Some(root.join("b.webp").to_string_lossy().to_string()),
                root.to_string_lossy().to_string(),
            ),
        ];
        let db = Arc::clone(&daemon.db);
        let root_checks = std::cell::Cell::new(0);
        let missing = missing_display_derivatives(
            rows,
            |path| {
                assert!(
                    db.try_lock().is_ok(),
                    "root check must not hold the DB lock"
                );
                root_checks.set(root_checks.get() + 1);
                path.is_dir()
            },
            |path| {
                assert!(
                    db.try_lock().is_ok(),
                    "file check must not hold the DB lock"
                );
                path.is_file()
            },
        );
        assert_eq!(root_checks.get(), 1, "root checks are deduplicated");
        assert_eq!(missing, vec![1, 2]);
    }

    #[test]
    fn manual_output_directory_overrides_the_profile() {
        let dir = tempfile::tempdir().unwrap();
        let mut daemon = test_daemon(&dir);
        let photo_path = dir
            .path()
            .join("photos/VRChat_2026-10-21_21-24-00.000_4x3.png");
        write_photo(&photo_path);
        daemon.run_once().unwrap();

        let export_dir = dir.path().join("exports");
        let job_id = {
            let db = daemon.db.lock().unwrap();
            let photo = db
                .photo_by_path(&photo_path.to_string_lossy())
                .unwrap()
                .expect("indexed photo");
            let profile = db
                .ensure_default_profiles()
                .unwrap()
                .into_iter()
                .next()
                .expect("default profile");
            db.enqueue_job_with_output_dir(
                photo.id,
                &profile.id,
                "keep",
                Some(export_dir.to_str().unwrap()),
            )
            .unwrap()
        };

        daemon.compress_pending().unwrap();

        let db = daemon.db.lock().unwrap();
        let stored = db
            .list_jobs(1)
            .unwrap()
            .into_iter()
            .find(|stored| stored.id == job_id)
            .unwrap();
        assert_eq!(stored.status, QueueStatus::Completed);
        let output = stored.output_path.expect("output path");
        assert!(Path::new(&output).starts_with(&export_dir));
        assert!(Path::new(&output).is_file());
        assert!(
            photo_path.is_file(),
            "keep must leave the original in place"
        );
    }

    #[test]
    fn finds_latest_log() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("output_log_2026-10-20_10-00-00.txt"), "").unwrap();
        std::fs::write(dir.path().join("output_log_2026-10-21_10-00-00.txt"), "").unwrap();
        std::fs::write(dir.path().join("other.txt"), "").unwrap();
        let latest = latest_log_file(dir.path()).unwrap().unwrap();
        assert_eq!(
            latest.file_name().unwrap(),
            "output_log_2026-10-21_10-00-00.txt"
        );
    }

    #[test]
    fn supervisor_label_detects_systemd_main_process() {
        assert_eq!(
            supervisor_label(
                Some(std::ffi::OsStr::new("invocation-id")),
                Some(std::ffi::OsStr::new("42")),
                42
            ),
            "systemd"
        );
        assert_eq!(
            supervisor_label(
                Some(std::ffi::OsStr::new("invocation-id")),
                Some(std::ffi::OsStr::new("41")),
                42
            ),
            "app"
        );
        assert_eq!(
            supervisor_label(Some(std::ffi::OsStr::new("invocation-id")), None, 42),
            "app"
        );
        assert_eq!(
            supervisor_label(None, Some(std::ffi::OsStr::new("42")), 42),
            "app"
        );
    }

    #[test]
    fn daemon_status_reports_pending_sends() {
        let dir = tempfile::tempdir().unwrap();
        let mut daemon = manual_test_daemon(&dir);
        daemon.scan_photos().unwrap();
        let photo = {
            let db = daemon.db.lock().unwrap();
            db.list_photos(1, 0, vrc_photon_core::model::PhotoSort::Newest)
                .unwrap()[0]
                .id
        };
        {
            let db = daemon.db.lock().unwrap();
            db.enqueue_send(
                photo,
                vrc_photon_core::model::SendKind::Photo,
                vrc_photon_core::model::SendOrigin::Manual,
                vrc_photon_core::model::SendStatus::Pending,
            )
            .unwrap();
        }
        daemon.refresh_status().unwrap();
        assert_eq!(daemon.status.lock().unwrap().pending_sends, 1);
    }

    #[test]
    fn auto_send_runs_after_linking_and_skips_old_photos() {
        let dir = tempfile::tempdir().unwrap();
        let mut daemon = test_daemon(&dir);
        let log_dir = dir.path().join("logs");
        std::fs::write(
            log_dir.join("output_log_2026-10-21_21-00-00.txt"),
            concat!(
                "2026.10.21 21:23:12 Debug      -  [Behaviour] Joining wrld_test:12345~region(jp)\n",
                "2026.10.21 21:23:13 Debug      -  [Behaviour] Joining or Creating Room: Test World\n",
            ),
        )
        .unwrap();
        write_photo(&original_path(&dir));
        daemon.ingest_logs().unwrap();
        daemon.scan_photos().unwrap();

        let (captured, _) = {
            let db = Db::open(dir.path().join("library.db")).unwrap();
            let photo = db
                .photo_by_path(&original_path(&dir).to_string_lossy())
                .unwrap()
                .unwrap();
            (photo.captured_at_utc, photo.id)
        };
        save_daemon_settings(&daemon, |settings| {
            settings.discord_enabled = true;
            settings.discord_auto_mode = DiscordAutoMode::Immediate;
            settings.discord_auto_since = Some(captured + 1);
        });
        {
            let db = daemon.db.lock().unwrap();
            save_discord_webhook(&db, "https://discord.com/api/webhooks/1/t").unwrap();
        }

        let second = dir
            .path()
            .join("photos/VRChat_2026-10-21_21-25-00.000_4x3.png");
        write_photo(&second);
        daemon.dirty_paths.push(second.clone());
        daemon.index_dirty_paths().unwrap();

        let db = Db::open(dir.path().join("library.db")).unwrap();
        let history = db.send_history(10).unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].entry.origin, SendOrigin::Auto);
        assert_eq!(history[0].entry.status, SendStatus::Pending);
        assert_eq!(
            history[0].photo_name.as_deref(),
            Some("VRChat_2026-10-21_21-25-00.000_4x3.png")
        );
        let second_photo = db
            .photo_by_path(&second.to_string_lossy())
            .unwrap()
            .unwrap();
        assert_eq!(
            db.photo_context(second_photo.id)
                .unwrap()
                .unwrap()
                .world_name
                .as_deref(),
            Some("Test World")
        );
    }

    #[test]
    fn auto_send_runs_after_a_full_scan() {
        let dir = tempfile::tempdir().unwrap();
        let mut daemon = test_daemon(&dir);
        std::fs::write(
            dir.path().join("logs/output_log_2026-10-21_21-00-00.txt"),
            concat!(
                "2026.10.21 21:23:12 Debug      -  [Behaviour] Joining wrld_test:12345~region(jp)\n",
                "2026.10.21 21:23:13 Debug      -  [Behaviour] Joining or Creating Room: Test World\n",
            ),
        )
        .unwrap();
        save_daemon_settings(&daemon, |settings| {
            settings.discord_enabled = true;
            settings.discord_auto_mode = DiscordAutoMode::Immediate;
            settings.discord_auto_since = Some(0);
        });
        {
            let db = daemon.db.lock().unwrap();
            save_discord_webhook(&db, "https://discord.com/api/webhooks/1/t").unwrap();
        }
        write_photo(&original_path(&dir));
        daemon.ingest_logs().unwrap();
        // The first full scan of an empty library is still "initial": auto
        // sends must not be skipped there, because `discord_auto_since`
        // already filters out old photos.
        daemon.scan_photos().unwrap();

        let db = Db::open(dir.path().join("library.db")).unwrap();
        let history = db.send_history(10).unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].entry.origin, SendOrigin::Auto);
        assert_eq!(history[0].entry.status, SendStatus::Pending);
        assert_eq!(
            history[0].photo_name.as_deref(),
            Some("VRChat_2026-10-21_21-24-00.000_4x3.png")
        );
    }

    #[test]
    fn auto_send_enqueue_failure_does_not_fail_the_scan() {
        let dir = tempfile::tempdir().unwrap();
        let mut daemon = test_daemon(&dir);
        save_daemon_settings(&daemon, |settings| {
            settings.discord_enabled = true;
            settings.discord_auto_mode = DiscordAutoMode::Immediate;
            settings.discord_auto_since = Some(0);
        });
        {
            let db = daemon.db.lock().unwrap();
            save_discord_webhook(&db, "https://discord.com/api/webhooks/1/t").unwrap();
            db.connection()
                .execute_batch(
                    "CREATE TRIGGER fail_auto_send BEFORE INSERT ON send_outbox \
                     BEGIN SELECT RAISE(ABORT, 'send outbox unavailable'); END;",
                )
                .unwrap();
        }
        write_photo(&original_path(&dir));
        let report = daemon
            .scan_photos()
            .expect("an auto-send enqueue error must not fail the scan");
        assert_eq!(report.indexed, 1);
        assert!(
            daemon
                .db
                .lock()
                .unwrap()
                .send_history(10)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn cgroup_detects_only_the_daemon_unit() {
        assert!(cgroup_has_unit(
            "0::/user.slice/user-1000.slice/user@1000.service/app.slice/vrc-photon-daemon.service\n",
            "vrc-photon-daemon.service"
        ));
        assert!(!cgroup_has_unit(
            "0::/user.slice/user-1000.slice/user@1000.service/app.slice/app-gnome-firefox-123.scope\n",
            "vrc-photon-daemon.service"
        ));
        assert!(!cgroup_has_unit(
            "0::/user.slice/user-1000.slice/user@1000.service/app.slice/foo-vrc-photon-daemon.service\n",
            "vrc-photon-daemon.service"
        ));
    }

    fn insert_unresolved_world_photo(db: &Db) -> (i64, i64) {
        let root = db.add_root("/photos", "reference").unwrap();
        let photo = db
            .upsert_photo(&NewPhoto {
                root_id: root,
                rel_path: "a.png".to_string(),
                abs_path: "/photos/a.png".to_string(),
                file_name: "a.png".to_string(),
                captured_at_utc: 100,
                captured_at_local: "2026-10-21 21:14:03".to_string(),
                tz_offset_sec: 0,
                width: 1,
                height: 1,
                file_size: 1,
                mtime: 100,
                blake3: None,
            })
            .unwrap();
        let world = db.upsert_world("wrld_test", None, None).unwrap();
        db.set_photo_context(&PhotoContext {
            photo_id: photo,
            session_id: None,
            world_id: Some(world),
            world_name: None,
            instance_id: None,
            link_method: "log".to_string(),
            confidence: 0.9,
            author: None,
            linked_at: "2026-11-03T00:00:00+00:00".to_string(),
        })
        .unwrap();
        (world, photo)
    }

    #[test]
    fn start_workers_spawns_and_stops_the_world_name_worker() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("library.db");
        let mut daemon = Daemon::open(&db_path, Some(dir.path().to_path_buf()), None).unwrap();
        daemon.settings.world_name_resolution_enabled = false;
        daemon.start_workers();
        assert!(daemon.world_name_handle.is_some());
        daemon.stop_workers();
        assert!(daemon.world_name_handle.is_none());
    }

    #[test]
    fn reload_settings_syncs_the_world_name_switch() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("library.db");
        let mut daemon = Daemon::open(&db_path, Some(dir.path().to_path_buf()), None).unwrap();
        daemon
            .world_name_resolution_enabled
            .store(false, Ordering::Relaxed);
        AppSettings {
            world_name_resolution_enabled: true,
            ..daemon.settings.clone()
        }
        .save(&daemon.db.lock().unwrap())
        .unwrap();

        daemon.reload_settings().unwrap();

        assert!(daemon.world_name_resolution_enabled.load(Ordering::Relaxed));
        assert!(daemon.settings.world_name_resolution_enabled);
    }

    #[test]
    fn start_workers_backfills_when_world_name_resolution_is_enabled() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("library.db");
        let mut daemon = Daemon::open(&db_path, Some(dir.path().to_path_buf()), None).unwrap();
        let (world, _photo) = {
            let db = daemon.db.lock().unwrap();
            insert_unresolved_world_photo(&db)
        };
        AppSettings {
            world_name_resolution_enabled: true,
            ..daemon.settings.clone()
        }
        .save(&daemon.db.lock().unwrap())
        .unwrap();
        daemon.settings.world_name_resolution_enabled = true;
        daemon
            .world_name_resolution_enabled
            .store(true, Ordering::Relaxed);
        // Keep the worker away from the real network while the test runs.
        daemon
            .db
            .lock()
            .unwrap()
            .set_world_name_next_request_at(chrono::Utc::now().timestamp() + 3_600)
            .unwrap();

        daemon.start_workers();

        assert!(
            daemon
                .db
                .lock()
                .unwrap()
                .world_name_lookup(world)
                .unwrap()
                .is_some(),
            "an enabled startup must enqueue unresolved worlds"
        );
        daemon.stop_workers();
    }

    #[test]
    fn enabling_world_name_resolution_enqueues_backfill() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("library.db");
        let mut daemon = Daemon::open(&db_path, Some(dir.path().to_path_buf()), None).unwrap();
        let (world, photo) = {
            let db = daemon.db.lock().unwrap();
            insert_unresolved_world_photo(&db)
        };
        daemon.settings.world_name_resolution_enabled = false;
        AppSettings {
            world_name_resolution_enabled: false,
            ..daemon.settings.clone()
        }
        .save(&daemon.db.lock().unwrap())
        .unwrap();

        AppSettings {
            world_name_resolution_enabled: true,
            ..daemon.settings.clone()
        }
        .save(&daemon.db.lock().unwrap())
        .unwrap();
        daemon.refresh_compression_settings();

        assert!(daemon.world_name_resolution_enabled.load(Ordering::Relaxed));
        assert!(
            daemon
                .db
                .lock()
                .unwrap()
                .world_name_lookup(world)
                .unwrap()
                .is_some()
        );
        assert_eq!(
            daemon
                .db
                .lock()
                .unwrap()
                .photo_context(photo)
                .unwrap()
                .unwrap()
                .world_name,
            None
        );
    }

    #[test]
    fn run_once_does_not_start_the_world_name_worker() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("library.db");
        let mut daemon = Daemon::open(&db_path, Some(dir.path().to_path_buf()), None).unwrap();
        daemon.run_once().unwrap();
        assert!(daemon.world_name_handle.is_none());
    }
}
