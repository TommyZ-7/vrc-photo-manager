use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{Context, Result};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use vrc_photo_core::compressor;
use vrc_photo_core::db::Db;
use vrc_photo_core::linker;
use vrc_photo_core::log_parser::LogIngestor;
use vrc_photo_core::model::{CompressionJob, CompressionProfile, DaemonOperation, QueueStatus};
use vrc_photo_core::photo_filename::is_source_photo;
use vrc_photo_core::scanner::{ScanFailure, ScanReport, scan_and_index, scan_one};
use vrc_photo_core::settings::{AppSettings, detect_vrchat_paths};
use vrc_photo_core::thumbnail;

mod tray;
use tray::{Tray, TrayAction};

const PARSER_VERSION: &str = "1";
const SCAN_INTERVAL: Duration = Duration::from_millis(500);
const LOG_INTERVAL: Duration = Duration::from_millis(750);

#[derive(Debug, Clone, Default)]
pub struct DaemonStatus {
    pub paused: bool,
    pub watching: bool,
    pub world_name: Option<String>,
    pub players: usize,
    pub last_log: Option<String>,
    pub photo_count: i64,
    pub pending_jobs: i64,
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
}

impl LogPass {
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
    last_autostart_check: std::time::Instant,
    last_heartbeat: std::time::Instant,
    thumbnail_dir: PathBuf,
    last_errors: HashMap<DaemonOperation, String>,
}

impl Daemon {
    pub fn open(
        db_path: impl AsRef<Path>,
        photo_dir: Option<PathBuf>,
        log_dir: Option<PathBuf>,
    ) -> Result<Self> {
        let db = Db::open(db_path).context("cannot open library database")?;
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

        Ok(Self {
            db: Arc::new(Mutex::new(db)),
            settings,
            status: Arc::new(Mutex::new(DaemonStatus::default())),
            running: Arc::new(AtomicBool::new(true)),
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
            last_autostart_check: std::time::Instant::now(),
            last_heartbeat: std::time::Instant::now() - Duration::from_secs(10),
            thumbnail_dir,
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

    pub fn set_paused(&self, paused: bool) {
        if let Ok(mut status) = self.status.lock() {
            status.paused = paused;
        }
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
        Ok(())
    }

    pub fn run_once(&mut self) -> Result<()> {
        self.write_heartbeat();
        let report = self.scan_photos()?;
        for id in &report.new_ids {
            let path = {
                let db = self.db.lock().unwrap();
                db.photo_by_id(*id)
                    .ok()
                    .flatten()
                    .map(|photo| photo.abs_path)
            };
            if let Some(path) = path {
                let _ = thumbnail::ensure_thumbnail_in(*id, Path::new(&path), &self.thumbnail_dir);
            }
        }
        self.ingest_logs()?;
        self.link_unlinked()?;
        self.refresh_status()?;
        Ok(())
    }

    pub fn compress_pending(&mut self) -> Result<()> {
        self.start_workers();
        loop {
            let job = {
                let db = self.db.lock().unwrap();
                db.next_pending_job()?
            };
            let Some(job) = job else {
                break;
            };
            if let Err(error) = process_job(&self.db, &job) {
                tracing::error!(job = %job.id, %error, "compression failed");
                let db = self.db.lock().unwrap();
                db.update_job(
                    &job.id,
                    QueueStatus::Failed,
                    0.0,
                    Some(&error.to_string()),
                    None,
                )?;
            }
        }
        self.stop_workers();
        Ok(())
    }

    pub fn run(&mut self) -> Result<()> {
        self.ensure_roots()?;
        self.scan_photos()?;
        self.ingest_logs()?;
        self.link_unlinked()?;
        self.write_heartbeat();
        self.setup_watchers()?;
        self.apply_autostart(self.settings.autostart);
        self.ensure_tray();
        self.start_workers();

        tracing::info!(
            photos = self.settings.photo_roots.len(),
            log_dir = ?self.settings.log_dir,
            "daemon running"
        );

        while self.running.load(Ordering::Relaxed) {
            if self
                .status
                .lock()
                .map(|status| status.paused)
                .unwrap_or(false)
            {
                thread::sleep(SCAN_INTERVAL);
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
            if self.last_autostart_check.elapsed() >= Duration::from_secs(10) {
                self.refresh_autostart_setting();
                self.last_autostart_check = std::time::Instant::now();
            }
            if self.last_heartbeat.elapsed() >= Duration::from_secs(10) {
                self.write_heartbeat();
            }
            self.poll_tray();
            thread::sleep(SCAN_INTERVAL);
        }
        self.shutdown();
        Ok(())
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
        paths.sort();
        paths.dedup();
        let roots = self.settings.photo_roots();
        let mut new_ids = Vec::new();
        {
            let db = self.db.lock().unwrap();
            for path in paths {
                let Some(root) = roots.iter().find(|root| path.starts_with(root)) else {
                    continue;
                };
                let root_id = db.add_root(&root.to_string_lossy(), "reference")?;
                if path.is_file() {
                    if let Some(scanned) = scan_one(root, &path) {
                        let abs_path = scanned.path.to_string_lossy().to_string();
                        let existed = db.photo_by_path(&abs_path)?.is_some();
                        let id = db.upsert_photo(&scanned.into_new_photo(root_id))?;
                        if !existed {
                            new_ids.push(id);
                        }
                    }
                } else if let Some(photo) = db.photo_by_path(&path.to_string_lossy())? {
                    db.mark_missing(photo.id)?;
                }
            }
            if self.settings.auto_compress && !new_ids.is_empty() {
                enqueue_auto_jobs(&db, &new_ids)?;
            }
        }
        if let Some(tx) = &self.thumbnail_tx {
            for id in &new_ids {
                let _ = tx.send(*id);
            }
        }
        if !new_ids.is_empty() {
            self.link_unlinked()?;
        }
        Ok(())
    }

    fn scan_photos(&mut self) -> Result<ScanReport> {
        let mut report = ScanReport::default();
        let roots = self.settings.photo_roots();
        if !roots.is_empty() {
            let db = self.db.lock().unwrap();
            let initial_scan = db.count_photos()? == 0;
            for root in roots {
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
                let root_report = match scan_and_index(&db, root_id, &root) {
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
                    for (id, path) in existing {
                        if !seen.contains(path.as_str()) {
                            let _ = db.mark_missing(id);
                        }
                    }
                }
                report.found += root_report.found;
                report.indexed += root_report.indexed;
                report.skipped += root_report.skipped;
                report.new_ids.extend(root_report.new_ids);
                report.failures.extend(root_report.failures);
            }
            if self.settings.auto_compress
                && !report.new_ids.is_empty()
                && (!initial_scan || self.settings.auto_compress_backlog)
            {
                enqueue_auto_jobs(&db, &report.new_ids)?;
            }
        }
        self.report_scan_outcome(&report)?;
        if report.indexed > 0 {
            self.link_unlinked()?;
        }
        if let Some(tx) = &self.thumbnail_tx {
            for id in &report.new_ids {
                let _ = tx.send(*id);
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
        if let Some(failure) = &pass.failure {
            self.report_daemon_error(
                DaemonOperation::Log,
                "ログの取り込みに失敗しました",
                Some(failure),
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
                    pass.skipped += 1;
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
        if let Ok(db) = self.db.lock() {
            let _ = db.set_setting(
                "daemon_heartbeat",
                &chrono::Utc::now().timestamp().to_string(),
            );
        }
        self.last_heartbeat = std::time::Instant::now();
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
            TrayAction::Quit => self.running.store(false, Ordering::Relaxed),
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
                let _ = tx.send(event);
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
            let handle = thread::Builder::new()
                .name("thumbnails".to_string())
                .spawn(move || thumbnail_loop(db, running, rx, cache_dir))
                .expect("cannot spawn thumbnail worker");
            self.thumbnail_tx = Some(tx.clone());
            self.thumbnail_handle = Some(handle);
            if let Ok(db) = self.db.lock()
                && let Ok(photos) =
                    db.list_photos(5000, 0, vrc_photo_core::model::PhotoSort::Newest)
            {
                for photo in photos {
                    let _ = tx.send(photo.id);
                }
            }
        }
        if !self.workers.is_empty() {
            return;
        }
        let count = self.settings.compression_workers.clamp(1, 4);
        for index in 0..count {
            let db = self.db.clone();
            let running = self.running.clone();
            let handle = thread::Builder::new()
                .name(format!("compress-{index}"))
                .spawn(move || worker_loop(db, running))
                .expect("cannot spawn compression worker");
            self.workers.push(handle);
        }
    }

    fn stop_workers(&mut self) {
        self.running.store(false, Ordering::Relaxed);
        self.thumbnail_tx = None;
        if let Some(handle) = self.thumbnail_handle.take() {
            let _ = handle.join();
        }
        for handle in self.workers.drain(..) {
            let _ = handle.join();
        }
    }

    fn shutdown(&mut self) {
        self.running.store(false, Ordering::Relaxed);
        if let Some(mut tail) = self.log_tail.take() {
            let db = self.db.lock().unwrap();
            let _ = tail.ingestor.finish(&db, chrono::Utc::now().timestamp());
        }
        self.tray = None;
        self.stop_workers();
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
    }
}

fn error_signature(message: &str, detail: Option<&str>) -> String {
    match detail {
        Some(detail) => format!("{message}\n{detail}"),
        None => message.to_string(),
    }
}

fn enqueue_auto_jobs(db: &Db, new_ids: &[i64]) -> Result<()> {
    let profiles = db.list_profiles()?;
    let auto_profiles: Vec<&CompressionProfile> = profiles
        .iter()
        .filter(|profile| profile.enabled && profile.auto_enabled)
        .collect();
    if auto_profiles.is_empty() {
        return Ok(());
    }
    for photo_id in new_ids {
        for profile in &auto_profiles {
            db.enqueue_job_for_existing_profile(*photo_id, &profile.id, "keep")?;
        }
    }
    Ok(())
}

fn thumbnail_loop(
    db: Arc<Mutex<Db>>,
    running: Arc<AtomicBool>,
    rx: Receiver<i64>,
    cache_dir: PathBuf,
) {
    while running.load(Ordering::Relaxed) {
        match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(id) => {
                let path = {
                    let db = db.lock().unwrap();
                    db.photo_by_id(id)
                        .ok()
                        .flatten()
                        .map(|photo| photo.abs_path)
                };
                if let Some(path) = path {
                    let _ = thumbnail::ensure_thumbnail_in(id, Path::new(&path), &cache_dir);
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
}

fn worker_loop(db: Arc<Mutex<Db>>, running: Arc<AtomicBool>) {
    while running.load(Ordering::Relaxed) {
        let job = {
            let db = db.lock().unwrap();
            db.next_pending_job().ok().flatten()
        };
        let Some(job) = job else {
            thread::sleep(Duration::from_millis(400));
            continue;
        };
        if let Err(error) = process_job(&db, &job) {
            tracing::error!(job = %job.id, %error, "compression failed");
            if let Ok(db) = db.lock() {
                let _ = db.update_job(
                    &job.id,
                    QueueStatus::Failed,
                    0.0,
                    Some(&error.to_string()),
                    None,
                );
            }
        }
    }
}

fn process_job(db: &Arc<Mutex<Db>>, job: &CompressionJob) -> Result<()> {
    let (photo, profile) = {
        let db = db.lock().unwrap();
        let photo = db
            .photo_by_id(job.photo_id)?
            .context("photo no longer exists")?;
        let profile = db
            .list_profiles()?
            .into_iter()
            .find(|profile| profile.id == job.profile_id)
            .context("compression profile no longer exists")?;
        db.update_job(&job.id, QueueStatus::Processing, 0.05, None, None)?;
        (photo, profile)
    };

    let result = compressor::compress_image(Path::new(&photo.abs_path), &profile)?;
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
    if job.policy == "replace" {
        trash::delete(&photo.abs_path)
            .map_err(|error| anyhow::anyhow!("cannot move original to trash: {error}"))?;
        db.mark_missing(photo.id)?;
    }
    db.update_job(
        &job.id,
        QueueStatus::Completed,
        1.0,
        None,
        Some(&result.output_path.to_string_lossy()),
    )?;
    tracing::info!(
        photo = %photo.abs_path,
        output = %result.output_path.display(),
        saved = result.compressed_size,
        "compressed"
    );
    Ok(())
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
        enqueue_auto_jobs(&db, &[photo_id]).unwrap();
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
        daemon.settings.auto_compress_backlog = true;
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
        daemon.settings.auto_compress_backlog = true;
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
}
