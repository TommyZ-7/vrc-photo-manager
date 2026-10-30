use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use image::DynamicImage;
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use vrc_photo_core::compressor;
use vrc_photo_core::db::Db;
use vrc_photo_core::hashing;
use vrc_photo_core::linker;
use vrc_photo_core::log_parser::LogIngestor;
use vrc_photo_core::model::{
    CompressionJob, CompressionProfile, DaemonCommand, DaemonOperation, DeletionReason,
    DeletionRequest, Derivative, Photo, PhotoStatus, QueueStatus,
};
use vrc_photo_core::photo_filename::is_source_photo;
use vrc_photo_core::scanner::{
    ScanFailure, ScanPhase as CoreScanPhase, ScanProgress, ScanReport, scan_and_index_observed,
    scan_one,
};
use vrc_photo_core::settings::{AppSettings, detect_vrchat_paths};
use vrc_photo_core::thumbnail;
use vrc_photo_ipc::{Endpoint, Event, JobStatus, Operation, ScanPhase as IpcScanPhase};

mod ipc_server;
mod signals;
mod tray;
use ipc_server::{EventBus, IpcControl, IpcServer, status_snapshot};
use signals::{JobSignal, WakeSignal};
use tray::{Tray, TrayAction};

const PARSER_VERSION: &str = "1";
/// Number of skipped-line sentences kept in one log error detail.
const MAX_SKIPPED_DETAILS: usize = 5;
const SCAN_INTERVAL: Duration = Duration::from_millis(500);
const LOG_INTERVAL: Duration = Duration::from_millis(750);
const THUMBNAIL_PRUNE_BATCH: usize = 128;
const THUMBNAIL_PRUNE_IDLE: Duration = Duration::from_millis(500);
const THUMBNAIL_PRUNE_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Default)]
pub struct DaemonStatus {
    pub paused: bool,
    pub watching: bool,
    pub world_name: Option<String>,
    pub players: usize,
    pub last_log: Option<String>,
    pub photo_count: i64,
    pub pending_jobs: i64,
    pub scanning: bool,
    pub scan_phase: Option<vrc_photo_ipc::ScanPhase>,
    pub scan_processed: usize,
    pub scan_total: Option<usize>,
    pub scan_indexed: usize,
    pub scan_root: usize,
    pub scan_roots: usize,
}

#[derive(Debug)]
struct LogTail {
    path: PathBuf,
    ingestor: LogIngestor,
    offset: i64,
    failed_offset: Option<i64>,
    failed_attempts: u8,
}

impl LogTail {
    /// Remembers a failed line at `offset`. The caller retries the line until it
    /// has failed three times, then this returns `true` so the poison line can
    /// be skipped.
    fn note_line_failure(&mut self, offset: i64) -> bool {
        if self.failed_offset != Some(offset) {
            self.failed_offset = Some(offset);
            self.failed_attempts = 1;
            return false;
        }
        self.failed_attempts = self.failed_attempts.saturating_add(1);
        if self.failed_attempts >= 3 {
            self.failed_offset = None;
            self.failed_attempts = 0;
            return true;
        }
        false
    }
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
    events: EventBus,
    ipc_endpoint: Option<Endpoint>,
    ipc: Option<IpcServer>,
    watchers: Vec<RecommendedWatcher>,
    watcher_rx: Option<Receiver<notify::Result<notify::Event>>>,
    log_tail: Option<LogTail>,
    workers: Vec<JoinHandle<()>>,
    dirty_paths: Vec<PathBuf>,
    last_log_poll: std::time::Instant,
    tray: Option<Tray>,
    tray_enabled: bool,
    thumbnail_tx: Option<mpsc::Sender<i64>>,
    thumbnail_handle: Option<JoinHandle<()>>,
    hash_handle: Option<JoinHandle<()>>,
    owns_deletion_executor: bool,
    deletion_handle: Option<JoinHandle<()>>,
    last_autostart_check: std::time::Instant,
    last_settings_check: std::time::Instant,
    last_heartbeat: std::time::Instant,
    last_asset_check: Instant,
    thumbnail_dir: PathBuf,
    thumbnail_cache_limit: Arc<AtomicU64>,
    last_errors: HashMap<DaemonOperation, String>,
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
        } else {
            tracing::debug!(
                "preserving running deletions because this process does not own the executor lock"
            );
        }
        let mut settings = AppSettings::load(&db).context("cannot load settings")?;
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
        let thumbnail_dir = vrc_photo_core::thumbnail::thumbnails_dir()
            .unwrap_or_else(|| PathBuf::from("thumbnails"));
        let thumbnail_cache_limit = Arc::new(AtomicU64::new(thumbnail_cache_limit_bytes(
            settings.thumbnail_cache_mb,
        )));
        let last_errors = db
            .daemon_errors()
            .context("cannot load daemon errors")?
            .into_iter()
            .map(|error| {
                (
                    error.operation,
                    error_signature(&error.message, error.detail.as_deref()),
                )
            })
            .collect();
        let ipc_endpoint =
            vrc_photo_core::data_dir().and_then(|dir| match Endpoint::for_data_dir(&dir) {
                Ok(endpoint) => Some(endpoint),
                Err(error) => {
                    tracing::warn!(%error, "cannot resolve IPC endpoint");
                    None
                }
            });

        Ok(Self {
            db: Arc::new(Mutex::new(db)),
            settings,
            status: Arc::new(Mutex::new(DaemonStatus::default())),
            running: Arc::new(AtomicBool::new(true)),
            wake: Arc::new(WakeSignal::default()),
            jobs: Arc::new(JobSignal::default()),
            events: EventBus::default(),
            ipc_endpoint,
            ipc: None,
            watchers: Vec::new(),
            watcher_rx: None,
            log_tail: None,
            workers: Vec::new(),
            dirty_paths: Vec::new(),
            last_log_poll: std::time::Instant::now() - LOG_INTERVAL,
            tray: None,
            tray_enabled,
            thumbnail_tx: None,
            thumbnail_handle: None,
            hash_handle: None,
            owns_deletion_executor,
            deletion_handle: None,
            last_autostart_check: std::time::Instant::now(),
            last_settings_check: Instant::now() - Duration::from_secs(2),
            last_heartbeat: std::time::Instant::now() - Duration::from_secs(10),
            last_asset_check: Instant::now() - Duration::from_secs(30),
            thumbnail_dir,
            thumbnail_cache_limit,
            last_errors,
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
        let status = status_snapshot(&self.status.lock().unwrap());
        self.events.publish(Event::StatusChanged { status });
    }

    pub fn settings(&self) -> &AppSettings {
        &self.settings
    }

    /// Persists a daemon failure unless the same message/detail was already
    /// reported, so `attempts` counts distinct reports rather than poll ticks.
    fn report_daemon_error(
        &mut self,
        operation: DaemonOperation,
        message: &str,
        detail: Option<&str>,
    ) -> Result<()> {
        let signature = error_signature(message, detail);
        if self.last_errors.get(&operation) == Some(&signature) {
            return Ok(());
        }
        {
            let db = self.db.lock().unwrap();
            db.record_daemon_error(operation, message, detail)?;
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

    /// Consumes queued UI requests once: reloads settings, rebuilds the photo
    /// watchers for the new roots, runs each command in request order, then
    /// refreshes the status view. Commands are consumed even when they fail so
    /// a broken request cannot loop forever; the failure is recorded under its
    /// operation instead.
    fn poll_commands(&mut self) -> Result<()> {
        let commands = {
            let db = self.db.lock().unwrap();
            db.take_daemon_commands()?
        };
        if commands.is_empty() {
            return Ok(());
        }
        self.reload_settings()?;
        self.restart_watchers();
        for command in commands {
            if let Err(error) = self.run_command(command) {
                tracing::error!(command = command.as_str(), %error, "daemon command failed");
                let (operation, message) = match command {
                    DaemonCommand::Rescan => (
                        DaemonOperation::Scan,
                        "写真フォルダの再スキャンに失敗しました",
                    ),
                    DaemonCommand::Relog => {
                        (DaemonOperation::Log, "ログの再取り込みに失敗しました")
                    }
                    DaemonCommand::Relink => {
                        (DaemonOperation::Link, "写真とログの紐付けに失敗しました")
                    }
                };
                if let Err(report_error) =
                    self.report_daemon_error(operation, message, Some(&error.to_string()))
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
        let settings = {
            let db = self.db.lock().unwrap();
            AppSettings::load(&db)?
        };
        if settings.autostart != self.settings.autostart {
            self.apply_autostart(settings.autostart);
        }
        self.thumbnail_cache_limit.store(
            thumbnail_cache_limit_bytes(settings.thumbnail_cache_mb),
            Ordering::Relaxed,
        );
        self.settings = settings;
        Ok(())
    }

    /// Stops the current watchers and sets up new ones for the reloaded roots.
    /// A failure leaves them stopped so the next command or daemon start can
    /// retry the setup.
    fn rebuild_watchers(&mut self) -> Result<()> {
        self.watchers.clear();
        self.watcher_rx = None;
        let result = self.setup_watchers();
        if result.is_err() {
            self.watchers.clear();
        }
        result
    }

    /// Rebuilds the watchers, recording a `Scan` failure instead of aborting
    /// the daemon when the operating system refuses a watch.
    fn restart_watchers(&mut self) {
        if let Err(error) = self.rebuild_watchers() {
            tracing::error!(%error, "cannot start photo watchers");
            if let Err(report_error) = self.report_daemon_error(
                DaemonOperation::Scan,
                "写真フォルダの監視を開始できませんでした",
                Some(&error.to_string()),
            ) {
                tracing::error!(%report_error, "cannot record watcher error");
            }
        }
    }

    /// Runs one queued command. `poll_commands` records a failure under the
    /// command's operation.
    fn run_command(&mut self, command: DaemonCommand) -> Result<()> {
        match command {
            DaemonCommand::Rescan => {
                self.scan_photos()?;
                self.link_unlinked()?;
            }
            DaemonCommand::Relog => {
                self.ingest_logs()?;
                self.link_unlinked()?;
            }
            DaemonCommand::Relink => self.link_unlinked()?,
        }
        Ok(())
    }

    pub fn run_once(&mut self) -> Result<()> {
        self.poll_commands()?;
        self.write_heartbeat();
        let report = self.scan_photos()?;
        for id in &report.new_ids {
            let path = {
                let db = self.db.lock().unwrap();
                db.photo_by_id(*id)
                    .ok()
                    .flatten()
                    .map(|photo| photo.effective_path().to_string())
            };
            if let Some(path) = path {
                match thumbnail::ensure_thumbnail_in(*id, Path::new(&path), &self.thumbnail_dir) {
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
        prune_thumbnail_cache(
            &self.thumbnail_dir,
            self.thumbnail_cache_limit.load(Ordering::Relaxed),
        );
        self.ingest_logs()?;
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
            process_photo_batch(&self.db, &jobs);
        }
        self.process_deletions()?;
        Ok(())
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

    pub fn run(&mut self) -> Result<()> {
        self.start_ipc();
        if self.stopping() {
            self.shutdown();
            return Ok(());
        }
        self.record_supervisor();
        self.ensure_roots()?;
        if self.stopping() {
            self.shutdown();
            return Ok(());
        }
        self.scan_photos()?;
        if self.stopping() {
            self.shutdown();
            return Ok(());
        }
        self.ingest_logs()?;
        self.link_unlinked()?;
        if self.stopping() {
            self.shutdown();
            return Ok(());
        }
        self.write_heartbeat();
        self.restart_watchers();
        self.apply_autostart(self.settings.autostart);
        self.ensure_tray();
        self.start_workers();

        tracing::info!(
            photos = self.settings.photo_roots.len(),
            log_dir = ?self.settings.log_dir,
            "daemon running"
        );

        while self.running.load(Ordering::Relaxed) {
            if let Err(error) = self.poll_commands() {
                tracing::error!(%error, "cannot handle daemon commands");
            }
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
                if let Err(report_error) = self.report_daemon_error(
                    DaemonOperation::Scan,
                    "写真フォルダの差分スキャンに失敗しました",
                    Some(&error.to_string()),
                ) {
                    tracing::error!(%report_error, "cannot record scan error");
                }
            }
            if self.last_log_poll.elapsed() >= LOG_INTERVAL {
                if let Err(error) = self.ingest_logs() {
                    tracing::error!(%error, "log ingestion failed");
                }
                self.last_log_poll = std::time::Instant::now();
            }
            self.refresh_status().ok();
            if self.last_asset_check.elapsed() >= Duration::from_secs(30) {
                if let Err(error) = self.verify_replaced_assets() {
                    tracing::warn!(%error, "cannot verify replaced photo assets");
                }
                self.last_asset_check = Instant::now();
            }
            if self.last_autostart_check.elapsed() >= Duration::from_secs(10) {
                self.refresh_autostart_setting();
                self.last_autostart_check = std::time::Instant::now();
            }
            if self.last_settings_check.elapsed() >= Duration::from_secs(2) {
                self.refresh_compression_settings();
                self.last_settings_check = Instant::now();
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
            events: self.events.clone(),
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
                    "no VRChat photo folder detected; set VRC_PHOTO_DIR or configure settings"
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
        paths.sort_by_key(|path| !path.is_file());
        paths.dedup();
        let roots = self.settings.photo_roots();
        let mut new_ids = Vec::new();
        let mut thumbnail_ids = Vec::new();
        let mut missing_ids = Vec::new();
        let mut changed = false;
        {
            let db = self.db.lock().unwrap();
            for path in paths {
                let Some(root) = roots.iter().find(|root| path.starts_with(root)) else {
                    continue;
                };
                let root_id = db.add_root(&root.to_string_lossy(), "reference")?;
                if path.is_file() {
                    let Some(scanned) = scan_one(root, &path) else {
                        continue;
                    };
                    let mut new_photo = scanned.into_new_photo(root_id);
                    let abs_path = new_photo.abs_path.clone();
                    let existing = db.photo_by_path(&abs_path)?;
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

                    if !existed && let Some(candidate) = db.find_rebind_candidate(&new_photo)? {
                        let old_path_exists = Path::new(&candidate.abs_path).exists();
                        if !old_path_exists && db.rebind_photo(candidate.id, &new_photo)? {
                            thumbnail_ids.push(candidate.id);
                            changed = true;
                            continue;
                        }
                    }

                    let id = db.upsert_photo(&new_photo)?;
                    changed = true;
                    if !existed {
                        new_ids.push(id);
                        thumbnail_ids.push(id);
                    }
                } else {
                    let Some(photo) = db.photo_by_path(&path.to_string_lossy())? else {
                        continue;
                    };
                    if photo.status == PhotoStatus::Present
                        && db.mark_missing_if_path(photo.id, &photo.abs_path)?
                    {
                        missing_ids.push(photo.id);
                        changed = true;
                    }
                }
            }
            // A remove event and its matching create event can arrive in one
            // batch. Only after every addition has had a chance to rebind do
            // we promote a derivative for rows that are still missing.
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
            if self.settings.auto_compress && !new_ids.is_empty() {
                enqueue_auto_jobs(
                    &db,
                    &new_ids,
                    self.settings.auto_compress_keep_original,
                    self.settings.auto_compress_immediate,
                )?;
                self.jobs.notify();
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
        }
        Ok(())
    }

    fn scan_photos(&mut self) -> Result<ScanReport> {
        self.begin_scan();
        let result = self.scan_photos_inner();
        self.finish_scan(&result);
        result
    }

    fn begin_scan(&self) {
        let roots = self.settings.photo_roots().len();
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
        self.events.publish(Event::ScanStarted { roots });
    }

    fn finish_scan(&self, result: &Result<ScanReport>) {
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
        });
    }

    fn scan_photos_inner(&mut self) -> Result<ScanReport> {
        self.refresh_compression_settings();
        let mut report = ScanReport::default();
        let roots = self.settings.photo_roots();
        let root_count = roots.len();
        if !roots.is_empty() {
            let db = self.db.lock().unwrap();
            let initial_scan = db.count_photos()? == 0;
            let mut changed = false;
            let mut missing_ids = Vec::new();
            for (index, root) in roots.into_iter().enumerate() {
                if self.stopping() {
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
                        message: "写真フォルダが見つかりません".to_string(),
                    });
                    continue;
                }
                let root_id = db.add_root(&root.to_string_lossy(), "reference")?;
                let existing = db.photos_for_root(root_id)?;
                let indexed_before = report.indexed;
                let mut observer = ScanObserver::new(
                    self.events.clone(),
                    self.status.clone(),
                    indexed_before,
                    root_number,
                    root_count,
                );
                let root_report = match scan_and_index_observed(&db, root_id, &root, |progress| {
                    observer.observe(progress);
                }) {
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
                if root_report.failures.is_empty() {
                    let seen: std::collections::HashSet<&str> =
                        root_report.seen_paths.iter().map(String::as_str).collect();
                    let rebound: std::collections::HashSet<i64> =
                        root_report.rebound_ids.iter().copied().collect();
                    for (id, path) in existing {
                        if !seen.contains(path.as_str()) && !rebound.contains(&id) {
                            match db.mark_missing_if_path(id, &path) {
                                Ok(true) => {
                                    changed = true;
                                    missing_ids.push(id);
                                }
                                Ok(false) => {}
                                Err(error) => {
                                    tracing::warn!(%error, photo_id = id, "cannot mark missing photo")
                                }
                            }
                        }
                    }
                }
                changed |= root_report.indexed > 0;
                report.found += root_report.found;
                report.indexed += root_report.indexed;
                report.skipped += root_report.skipped;
                report.new_ids.extend(root_report.new_ids);
                report.rebound_ids.extend(root_report.rebound_ids);
                report.failures.extend(root_report.failures);
            }
            // Give every root a chance to rebind a moved file before falling
            // back to a compressed derivative for a missing original.
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
            if self.settings.auto_compress
                && !report.new_ids.is_empty()
                && (!initial_scan || self.settings.auto_compress_backlog)
            {
                enqueue_auto_jobs(
                    &db,
                    &report.new_ids,
                    self.settings.auto_compress_keep_original,
                    self.settings.auto_compress_immediate,
                )?;
                self.jobs.notify();
            }
        }
        self.verify_replaced_assets()?;
        self.report_scan_outcome(&report)?;
        if report.indexed > 0 {
            self.link_unlinked()?;
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
        }
        Ok(report)
    }

    /// Records one `Scan` error summarizing the current failures, or clears a
    /// stale error when the full scan had none.
    fn report_scan_outcome(&mut self, report: &ScanReport) -> Result<()> {
        if report.failures.is_empty() {
            return self.clear_daemon_error(DaemonOperation::Scan);
        }
        let detail = report
            .failures
            .iter()
            .take(5)
            .map(|failure| format!("{}: {}", failure.path.display(), failure.message))
            .collect::<Vec<_>>()
            .join("\n");
        let message = format!(
            "写真フォルダのスキャンで{}件の失敗がありました",
            report.failures.len()
        );
        self.report_daemon_error(DaemonOperation::Scan, &message, Some(&detail))
    }

    fn ingest_logs(&mut self) -> Result<()> {
        let Some(log_dir) = self.settings.log_dir() else {
            self.clear_daemon_error(DaemonOperation::Log)?;
            return Ok(());
        };
        if !log_dir.is_dir() {
            self.report_daemon_error(
                DaemonOperation::Log,
                &format!("ログフォルダが見つかりません: {}", log_dir.display()),
                None,
            )?;
            return Ok(());
        }
        let path = match latest_log_file(&log_dir) {
            Ok(Some(path)) => path,
            Ok(None) => return Ok(()),
            Err(error) => {
                self.report_daemon_error(
                    DaemonOperation::Log,
                    "ログフォルダを読み取れません",
                    Some(&error.to_string()),
                )?;
                return Ok(());
            }
        };
        let pass = self.ingest_log_pass(&path)?;
        if let Some(failure) = pass.failure_detail() {
            self.report_daemon_error(
                DaemonOperation::Log,
                "ログの取り込みに失敗しました",
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
                let finish_at = previous
                    .path
                    .metadata()
                    .ok()
                    .and_then(|meta| meta.modified().ok())
                    .map(vrc_photo_core::time::from_system_time)
                    .map(|stamp| stamp.utc)
                    .unwrap_or_else(|| chrono::Utc::now().timestamp());
                let db = self.db.lock().unwrap();
                previous.ingestor.finish(&db, finish_at)?;
            }
            let offset = {
                let db = self.db.lock().unwrap();
                db.log_offset(&path_string)?.unwrap_or(0)
            };
            self.log_tail = Some(LogTail {
                path: path.to_path_buf(),
                ingestor: LogIngestor::new(&path_string),
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
        if lines.is_empty() {
            return Ok(pass);
        }

        let db = self.db.lock().unwrap();
        let mut line_start = tail.offset;
        for (line, line_end) in lines {
            match tail.ingestor.ingest_line(&db, &line) {
                Ok(_) => {
                    db.set_log_offset(&path_string, size, mtime, line_end, PARSER_VERSION)?;
                    tail.offset = line_end;
                    line_start = line_end;
                    pass.processed += 1;
                }
                Err(error) => {
                    tracing::debug!(%error, "cannot parse log line");
                    pass.failure
                        .get_or_insert_with(|| format!("{}: {error}", path.display()));
                    if !tail.note_line_failure(line_start) {
                        break;
                    }
                    pass.record_skip(path, line_start, line_end);
                    db.set_log_offset(&path_string, size, mtime, line_end, PARSER_VERSION)?;
                    tail.offset = line_end;
                    line_start = line_end;
                }
            }
        }
        if pass.failure.is_none() && tail.offset != new_offset {
            db.set_log_offset(&path_string, size, mtime, new_offset, PARSER_VERSION)?;
            tail.offset = new_offset;
        }
        drop(db);
        self.refresh_status()?;
        self.link_unlinked()?;
        Ok(pass)
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
                }
                self.clear_daemon_error(DaemonOperation::Link)
            }
            Err(error) => {
                tracing::error!(%error, "cannot link photos to log context");
                self.report_daemon_error(
                    DaemonOperation::Link,
                    "写真とログの紐付けに失敗しました",
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
        let db = self.db.lock().unwrap();
        let mut changed = false;
        for (photo_id, path, root) in db.display_derivative_paths(100_000)? {
            // A disconnected external drive must not mark every replaced photo
            // missing. Only validate assets whose root is currently available.
            if !Path::new(&root).is_dir() {
                continue;
            }
            let valid = path
                .as_deref()
                .is_some_and(|path| Path::new(path).is_file());
            if !valid {
                db.mark_missing(photo_id)?;
                db.clear_display_derivative(photo_id)?;
                changed = true;
            }
        }
        if changed {
            db.bump_library_revision()?;
        }
        Ok(())
    }

    fn refresh_status(&self) -> Result<()> {
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
        let photo_count = db.count_photos()?;
        let pending_jobs = db.count_jobs(QueueStatus::Pending)?;
        let mut status = self.status.lock().unwrap();
        status.watching = self.settings.log_dir.is_some();
        status.world_name = world_name;
        status.players = players;
        status.photo_count = photo_count;
        status.pending_jobs = pending_jobs;
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
            .is_some_and(|cgroup| cgroup_has_unit(cgroup, "vrc-photo-daemon.service"))
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

    fn refresh_autostart_setting(&mut self) {
        let enabled = {
            let db = self.db.lock().unwrap();
            AppSettings::load(&db)
                .map(|settings| settings.autostart)
                .unwrap_or(self.settings.autostart)
        };
        if enabled != self.settings.autostart {
            self.settings.autostart = enabled;
            self.apply_autostart(enabled);
        }
    }

    /// Applies the compression switches and thumbnail cache limit without
    /// touching paths or restarting watchers. The UI saves these immediately;
    /// a running daemon must not keep enqueuing with the previous policy or
    /// prune with a stale cache limit.
    fn refresh_compression_settings(&mut self) {
        let loaded = {
            let db = self.db.lock().unwrap();
            AppSettings::load(&db).ok()
        };
        let Some(settings) = loaded else {
            return;
        };
        self.settings.auto_compress = settings.auto_compress;
        self.settings.auto_compress_backlog = settings.auto_compress_backlog;
        self.settings.auto_compress_immediate = settings.auto_compress_immediate;
        self.settings.auto_compress_keep_original = settings.auto_compress_keep_original;
        self.thumbnail_cache_limit.store(
            thumbnail_cache_limit_bytes(settings.thumbnail_cache_mb),
            Ordering::Relaxed,
        );
    }

    fn apply_autostart(&self, enabled: bool) {
        let Ok(exe) = std::env::current_exe() else {
            return;
        };
        let exe = exe.to_string_lossy();
        let mut builder = auto_launch::AutoLaunchBuilder::new();
        builder.set_app_name("VRC Photo Manager").set_app_path(&exe);
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
        }
        self.watcher_rx = Some(rx);
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

    fn start_workers(&mut self) {
        if self.thumbnail_tx.is_none() {
            let (tx, rx) = mpsc::channel();
            let db = self.db.clone();
            let running = self.running.clone();
            let cache_dir = self.thumbnail_dir.clone();
            let cache_limit = self.thumbnail_cache_limit.clone();
            let handle = thread::Builder::new()
                .name("thumbnails".to_string())
                .spawn(move || thumbnail_loop(db, running, rx, cache_dir, cache_limit))
                .expect("cannot spawn thumbnail worker");
            self.thumbnail_tx = Some(tx.clone());
            self.thumbnail_handle = Some(handle);
            if let Ok(db) = self.db.lock()
                && let Ok(photos) =
                    db.list_photos(5000, 0, vrc_photo_core::model::PhotoSort::Newest)
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
        if !self.workers.is_empty() {
            return;
        }
        let count = self.settings.compression_workers.clamp(1, 4);
        for index in 0..count {
            let db = self.db.clone();
            let running = self.running.clone();
            let jobs = self.jobs.clone();
            let events = self.events.clone();
            let handle = thread::Builder::new()
                .name(format!("compress-{index}"))
                .spawn(move || worker_loop(db, running, jobs, events))
                .expect("cannot spawn compression worker");
            self.workers.push(handle);
        }
    }

    fn stop_workers(&mut self) {
        self.running.store(false, Ordering::Relaxed);
        self.jobs.notify();
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
        if let Some(mut tail) = self.log_tail.take() {
            let db = self.db.lock().unwrap();
            if let Err(error) = tail.ingestor.finish(&db, chrono::Utc::now().timestamp()) {
                tracing::warn!(%error, "cannot finish log ingestion on shutdown");
            }
        }
        self.tray = None;
        self.stop_workers();
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

fn enqueue_auto_jobs(db: &Db, new_ids: &[i64], keep_original: bool, immediate: bool) -> Result<()> {
    let profiles = db.list_profiles()?;
    let auto_profiles: Vec<&CompressionProfile> = profiles
        .iter()
        .filter(|profile| profile.enabled && profile.auto_enabled)
        .collect();
    if auto_profiles.is_empty() {
        return Ok(());
    }
    let policy = if keep_original { "keep" } else { "replace" };
    let status = if immediate {
        QueueStatus::Pending
    } else {
        QueueStatus::Waiting
    };
    for photo_id in new_ids {
        for profile in &auto_profiles {
            db.enqueue_job_for_existing_profile_with_status(
                *photo_id,
                &profile.id,
                policy,
                status,
            )?;
        }
    }
    Ok(())
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

struct ThumbnailPruneSchedule {
    batch: usize,
    interval: Duration,
    generated: usize,
    last_prune: Instant,
}

impl ThumbnailPruneSchedule {
    fn new(batch: usize, interval: Duration) -> Self {
        Self {
            batch,
            interval,
            generated: 0,
            last_prune: Instant::now(),
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

    fn periodic_due(&mut self) -> bool {
        if self.last_prune.elapsed() < self.interval {
            return false;
        }
        self.note_pruned();
        true
    }

    fn has_pending(&self) -> bool {
        self.generated > 0
    }

    fn note_pruned(&mut self) {
        self.generated = 0;
        self.last_prune = Instant::now();
    }
}

fn thumbnail_cache_limit_bytes(megabytes: u64) -> u64 {
    megabytes.saturating_mul(1024 * 1024)
}

fn prune_thumbnail_cache(cache_dir: &Path, limit_bytes: u64) {
    if limit_bytes == 0 {
        return;
    }
    match thumbnail::prune_thumbnails_in(cache_dir, limit_bytes) {
        Ok(report) if report.removed_files > 0 => tracing::info!(
            removed_files = report.removed_files,
            removed_bytes = report.removed_bytes,
            remaining_bytes = report.remaining_bytes,
            limit_bytes,
            dir = %cache_dir.display(),
            "pruned thumbnail cache"
        ),
        Ok(_) => {}
        Err(error) => tracing::warn!(
            %error,
            dir = %cache_dir.display(),
            "cannot prune thumbnail cache"
        ),
    }
}

fn thumbnail_loop(
    db: Arc<Mutex<Db>>,
    running: Arc<AtomicBool>,
    rx: Receiver<i64>,
    cache_dir: PathBuf,
    cache_limit: Arc<AtomicU64>,
) {
    let mut last_limit = cache_limit.load(Ordering::Relaxed);
    let mut schedule = ThumbnailPruneSchedule::new(THUMBNAIL_PRUNE_BATCH, THUMBNAIL_PRUNE_INTERVAL);
    prune_thumbnail_cache(&cache_dir, last_limit);
    schedule.note_pruned();

    while running.load(Ordering::Relaxed) {
        let limit = cache_limit.load(Ordering::Relaxed);
        if limit != last_limit {
            last_limit = limit;
            schedule.note_pruned();
            prune_thumbnail_cache(&cache_dir, limit);
        }
        if schedule.periodic_due() {
            last_limit = cache_limit.load(Ordering::Relaxed);
            prune_thumbnail_cache(&cache_dir, last_limit);
        }
        match rx.recv_timeout(THUMBNAIL_PRUNE_IDLE) {
            Ok(id) => {
                let path = {
                    let db = db.lock().unwrap();
                    db.photo_by_id(id)
                        .ok()
                        .flatten()
                        .map(|photo| photo.effective_path().to_string())
                };
                if let Some(path) = path {
                    let cache_path = cache_dir.join(format!("{id}.webp"));
                    let generated = !cache_path.is_file();
                    match thumbnail::ensure_thumbnail_in(id, Path::new(&path), &cache_dir) {
                        Ok(Some(_)) => {
                            if generated && schedule.note_generated() {
                                last_limit = cache_limit.load(Ordering::Relaxed);
                                prune_thumbnail_cache(&cache_dir, last_limit);
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
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if schedule.has_pending() {
                    schedule.note_pruned();
                    last_limit = cache_limit.load(Ordering::Relaxed);
                    prune_thumbnail_cache(&cache_dir, last_limit);
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

/// Claims and executes every pending deletion whose jobs are terminal. A
/// dependency failure blocks the request instead of busy-polling it; a file
/// failure blocks it until the user retries or forces.
fn run_deletions_once(db: &Arc<Mutex<Db>>) -> Result<usize> {
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
) {
    let mut seen_generation = 0;
    while running.load(Ordering::Relaxed) {
        let batch = {
            let db = db.lock().unwrap();
            db.next_pending_photo_jobs().ok().unwrap_or_default()
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
        for outcome in process_photo_batch(&db, &batch) {
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
fn process_photo_batch(db: &Arc<Mutex<Db>>, jobs: &[CompressionJob]) -> Vec<JobOutcome> {
    match process_photo_batch_inner(db, jobs) {
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

    let Some(image) = vrc_photo_core::image_decode::decode_image(&source_path) else {
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
        match process_decoded_job(db, &photo, &source_path, &image, job) {
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
    let mut outcomes = process_photo_batch(db, std::slice::from_ref(job));
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

/// Reads complete lines from `offset`, leaving a trailing partial line for the
/// next poll. Each line is returned with its end byte offset.
fn read_new_lines(path: &Path, offset: i64) -> Result<(Vec<(String, i64)>, i64)> {
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(offset.max(0) as u64))?;
    let mut buffer = Vec::new();
    file.read_to_end(&mut buffer)?;
    let mut lines = Vec::new();
    let mut line_start = 0;
    for (index, byte) in buffer.iter().enumerate() {
        if *byte != b'\n' {
            continue;
        }
        let line_end = index + 1;
        let text = String::from_utf8_lossy(&buffer[line_start..index]);
        let line = text.trim_end_matches('\r');
        if !line.is_empty() {
            lines.push((line.to_string(), offset + line_end as i64));
        }
        line_start = line_end;
    }
    Ok((lines, offset + line_start as i64))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::time::SystemTime;
    use vrc_photo_core::model::DeletionStatus;

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
    fn poison_line_is_skipped_only_after_three_attempts() {
        let mut tail = LogTail {
            path: PathBuf::new(),
            ingestor: LogIngestor::new("log"),
            offset: 0,
            failed_offset: None,
            failed_attempts: 0,
        };
        assert!(!tail.note_line_failure(0));
        assert!(!tail.note_line_failure(0));
        assert!(tail.note_line_failure(0));
        assert_eq!(tail.failed_attempts, 0);
        assert!(!tail.note_line_failure(10));
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
        let joined_at = vrc_photo_core::log_parser::parse_line(
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
            let captured_at = vrc_photo_core::log_parser::parse_line(
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
                .upsert_photo(&vrc_photo_core::model::NewPhoto {
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
                format: vrc_photo_core::model::CompressionFormat::Webp,
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
        assert_eq!(photo.status, vrc_photo_core::model::PhotoStatus::Present);
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
        assert_eq!(photo.status, vrc_photo_core::model::PhotoStatus::Present);
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
        assert_eq!(photo.status, vrc_photo_core::model::PhotoStatus::Missing);
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
            format: vrc_photo_core::model::CompressionFormat::Webp,
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
        format: vrc_photo_core::model::CompressionFormat,
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
            format: vrc_photo_core::model::CompressionFormat::Webp,
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
            format: vrc_photo_core::model::CompressionFormat::Webp,
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

    #[test]
    fn thumbnail_prune_schedule_fires_after_a_batch() {
        let mut schedule = ThumbnailPruneSchedule::new(2, Duration::from_secs(3600));

        assert!(!schedule.note_generated());
        assert!(schedule.note_generated());
        assert!(!schedule.has_pending());
    }

    #[test]
    fn thumbnail_prune_schedule_fires_when_the_interval_elapses() {
        let mut schedule = ThumbnailPruneSchedule::new(128, Duration::from_millis(5));

        assert!(!schedule.periodic_due());
        thread::sleep(Duration::from_millis(10));
        assert!(schedule.periodic_due());
    }

    #[test]
    fn thumbnail_prune_schedule_resets_pending_after_an_idle_prune() {
        let mut schedule = ThumbnailPruneSchedule::new(128, Duration::from_secs(3600));

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
        let derivative = |id: i64, profile: &str, path: &Path| vrc_photo_core::model::Derivative {
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
                .upsert_photo(&vrc_photo_core::model::NewPhoto {
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
                .upsert_photo(&vrc_photo_core::model::NewPhoto {
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
            .upsert_photo(&vrc_photo_core::model::NewPhoto {
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
        db.connection()
            .execute(
                "INSERT INTO deletion_requests(photo_id, reason, status, force, requested_at) \
                 VALUES (?1, 'future_reason', 'pending', 0, ?2)",
                (photo.id, chrono::Utc::now().to_rfc3339()),
            )
            .unwrap();

        daemon.process_deletions().unwrap();

        assert!(original.exists(), "unknown reason must not delete files");
        let request = db.active_deletion_request(photo.id).unwrap().unwrap();
        assert_eq!(
            request.reason,
            vrc_photo_core::model::DeletionReason::Unknown
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
                vrc_photo_core::model::DeletionReason::Manual,
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
            vrc_photo_core::model::DeletionReason::Manual
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
            let expected = vrc_photo_core::image_decode::decode_image(&original)
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
            let actual = vrc_photo_core::image_decode::decode_image(Path::new(&lossless.path))
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
            vrc_photo_core::model::CompressionFormat::Webp,
            "_webp",
            None,
        );
        save_batch_profile(
            &db,
            "jpeg",
            vrc_photo_core::model::CompressionFormat::Jpeg,
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
            vrc_photo_core::model::CompressionFormat::Webp,
            "_small",
            Some((8, 8)),
        );
        save_batch_profile(
            &db,
            "full",
            vrc_photo_core::model::CompressionFormat::Webp,
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
            vrc_photo_core::model::CompressionFormat::Webp,
            "_good",
            None,
        );
        save_batch_profile(
            &db,
            "bad",
            vrc_photo_core::model::CompressionFormat::Webp,
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
            vrc_photo_core::model::CompressionFormat::Webp,
            "_first",
            None,
        );
        save_batch_profile(
            &db,
            "second",
            vrc_photo_core::model::CompressionFormat::Webp,
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
            vrc_photo_core::model::CompressionFormat::Webp,
            "_first",
            None,
        );
        save_batch_profile(
            &db,
            "second",
            vrc_photo_core::model::CompressionFormat::Webp,
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

        let outcomes = process_photo_batch(&daemon.db, &jobs);

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

        let response = vrc_photo_ipc::client::request(
            &endpoint,
            vrc_photo_ipc::Request::Status,
            Duration::from_secs(1),
        )
        .unwrap();
        assert!(matches!(response, vrc_photo_ipc::Response::Status { .. }));
        daemon.ipc = None;
    }

    #[test]
    fn auto_jobs_follow_profile_sort_order() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path().join("library.db")).unwrap();
        let root = db.add_root("/photos", "reference").unwrap();
        let photo_id = db
            .upsert_photo(&vrc_photo_core::model::NewPhoto {
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
        assert_eq!(photo.status, vrc_photo_core::model::PhotoStatus::Present);

        std::fs::remove_file(&path).unwrap();
        daemon.dirty_paths.push(path.clone());
        daemon.index_dirty_paths().unwrap();
        let photo = db
            .photo_by_path(path.to_str().unwrap())
            .unwrap()
            .expect("row");
        assert_eq!(photo.status, vrc_photo_core::model::PhotoStatus::Missing);
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
        assert_eq!(photo.status, vrc_photo_core::model::PhotoStatus::Present);
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
        assert_eq!(photo.status, vrc_photo_core::model::PhotoStatus::Present);
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
        assert_eq!(photo.status, vrc_photo_core::model::PhotoStatus::Present);
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
        assert_eq!(photo.status, vrc_photo_core::model::PhotoStatus::Replaced);
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
        assert_eq!(photo.status, vrc_photo_core::model::PhotoStatus::Replaced);
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
        assert_eq!(photo.status, vrc_photo_core::model::PhotoStatus::Replaced);

        let derivative_path = photo.display_path.clone().expect("display derivative");
        std::fs::remove_file(&derivative_path).unwrap();
        daemon.verify_replaced_assets().unwrap();
        let photo = db.photo_by_id(photo.id).unwrap().unwrap();
        assert_eq!(photo.status, vrc_photo_core::model::PhotoStatus::Missing);
        assert!(photo.display_derivative_id.is_none());
        assert_eq!(db.count_photos().unwrap(), 0);
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
    fn cgroup_detects_only_the_daemon_unit() {
        assert!(cgroup_has_unit(
            "0::/user.slice/user-1000.slice/user@1000.service/app.slice/vrc-photo-daemon.service\n",
            "vrc-photo-daemon.service"
        ));
        assert!(!cgroup_has_unit(
            "0::/user.slice/user-1000.slice/user@1000.service/app.slice/app-gnome-firefox-123.scope\n",
            "vrc-photo-daemon.service"
        ));
        assert!(!cgroup_has_unit(
            "0::/user.slice/user-1000.slice/user@1000.service/app.slice/foo-vrc-photo-daemon.service\n",
            "vrc-photo-daemon.service"
        ));
    }
}
