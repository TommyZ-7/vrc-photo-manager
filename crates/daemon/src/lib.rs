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
use vrc_photo_core::model::{CompressionJob, CompressionProfile, QueueStatus};
use vrc_photo_core::scanner::{ScanReport, scan_and_index};
use vrc_photo_core::settings::{AppSettings, detect_vrchat_paths};

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
    photo_dirty: bool,
    last_scan: std::time::Instant,
    last_log_poll: std::time::Instant,
    tray: Option<Tray>,
    tray_enabled: bool,
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

        Ok(Self {
            db: Arc::new(Mutex::new(db)),
            settings,
            status: Arc::new(Mutex::new(DaemonStatus::default())),
            running: Arc::new(AtomicBool::new(true)),
            watchers: Vec::new(),
            watcher_rx: None,
            log_tail: None,
            workers: Vec::new(),
            photo_dirty: true,
            last_scan: std::time::Instant::now() - SCAN_INTERVAL,
            last_log_poll: std::time::Instant::now() - LOG_INTERVAL,
            tray: None,
            tray_enabled,
        })
    }

    pub fn status(&self) -> Arc<Mutex<DaemonStatus>> {
        self.status.clone()
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

    pub fn run_once(&mut self) -> Result<()> {
        self.scan_photos()?;
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
        self.setup_watchers()?;
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
            if self.photo_dirty && self.last_scan.elapsed() >= SCAN_INTERVAL {
                if let Err(error) = self.scan_photos() {
                    tracing::error!(%error, "photo scan failed");
                }
                self.photo_dirty = false;
                self.last_scan = std::time::Instant::now();
            }
            if self.last_log_poll.elapsed() >= LOG_INTERVAL {
                if let Err(error) = self.ingest_logs() {
                    tracing::error!(%error, "log ingestion failed");
                }
                self.last_log_poll = std::time::Instant::now();
            }
            self.refresh_status().ok();
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

    fn scan_photos(&mut self) -> Result<ScanReport> {
        let mut report = ScanReport::default();
        let roots = self.settings.photo_roots();
        if roots.is_empty() {
            return Ok(report);
        }
        let db = self.db.lock().unwrap();
        for root in roots {
            if !root.is_dir() {
                tracing::warn!(path = %root.display(), "photo root does not exist");
                continue;
            }
            let root_id = db.add_root(&root.to_string_lossy(), "reference")?;
            let root_report = scan_and_index(&db, root_id, &root)?;
            report.found += root_report.found;
            report.indexed += root_report.indexed;
            report.skipped += root_report.skipped;
            report.new_ids.extend(root_report.new_ids);
        }
        if self.settings.auto_compress && !report.new_ids.is_empty() {
            enqueue_auto_jobs(&db, &report.new_ids)?;
        }
        drop(db);
        if report.indexed > 0 {
            self.link_unlinked()?;
        }
        Ok(report)
    }

    fn ingest_logs(&mut self) -> Result<()> {
        let Some(log_dir) = self.settings.log_dir() else {
            return Ok(());
        };
        let Some(path) = latest_log_file(&log_dir) else {
            return Ok(());
        };
        let path_string = path.to_string_lossy().to_string();
        let metadata = std::fs::metadata(&path)?;
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
                path: path.clone(),
                ingestor: LogIngestor::new(&path_string),
                offset,
            });
        }

        let tail = self.log_tail.as_mut().unwrap();
        if size < tail.offset {
            tail.offset = 0;
            tail.ingestor = LogIngestor::new(&path_string);
        }
        let (lines, new_offset) = read_new_lines(&path, tail.offset)?;
        if lines.is_empty() {
            return Ok(());
        }

        let db = self.db.lock().unwrap();
        for line in &lines {
            if let Err(error) = tail.ingestor.ingest_line(&db, line) {
                tracing::debug!(%error, "cannot parse log line");
            }
        }
        db.set_log_offset(&path_string, size, mtime, new_offset, PARSER_VERSION)?;
        drop(db);
        tail.offset = new_offset;
        self.link_unlinked()?;
        self.refresh_status()?;
        Ok(())
    }

    fn link_unlinked(&self) -> Result<()> {
        let db = self.db.lock().unwrap();
        let linked = linker::relink_unlinked(&db, 2000)?;
        if linked > 0 {
            tracing::info!(linked, "linked photos to log context");
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
                        self.photo_dirty = true;
                    }
                }
                Err(error) => tracing::warn!(%error, "photo watcher error"),
            }
        }
    }

    fn start_workers(&mut self) {
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
            db.enqueue_job(*photo_id, &profile.id, "keep")?;
        }
    }
    Ok(())
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

fn latest_log_file(dir: &Path) -> Option<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
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
    files.pop()
}

/// Reads complete lines from `offset`, leaving a trailing partial line for the
/// next poll.
fn read_new_lines(path: &Path, offset: i64) -> Result<(Vec<String>, i64)> {
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(offset.max(0) as u64))?;
    let mut buffer = Vec::new();
    file.read_to_end(&mut buffer)?;
    let Some(last_newline) = buffer.iter().rposition(|byte| *byte == b'\n') else {
        return Ok((Vec::new(), offset));
    };
    let consumed = last_newline + 1;
    let text = String::from_utf8_lossy(&buffer[..consumed]);
    let lines = text
        .lines()
        .map(|line| line.trim_end_matches('\r').to_string())
        .filter(|line| !line.is_empty())
        .collect();
    Ok((lines, offset + consumed as i64))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn reads_only_complete_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("output_log_test.txt");
        let mut file = File::create(&path).unwrap();
        write!(file, "line1\nline2\npart").unwrap();
        drop(file);
        let (lines, offset) = read_new_lines(&path, 0).unwrap();
        assert_eq!(lines, vec!["line1", "line2"]);
        assert_eq!(offset, 12);

        let mut file = File::open(&path).unwrap();
        file.seek(SeekFrom::Start(offset as u64)).unwrap();
        let mut rest = String::new();
        file.read_to_string(&mut rest).unwrap();
        assert_eq!(rest, "part");
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
    }

    #[test]
    fn finds_latest_log() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("output_log_2026-10-20_10-00-00.txt"), "").unwrap();
        std::fs::write(dir.path().join("output_log_2026-10-21_10-00-00.txt"), "").unwrap();
        std::fs::write(dir.path().join("other.txt"), "").unwrap();
        let latest = latest_log_file(dir.path()).unwrap();
        assert_eq!(
            latest.file_name().unwrap(),
            "output_log_2026-10-21_10-00-00.txt"
        );
    }
}
