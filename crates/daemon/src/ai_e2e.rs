#![cfg(test)]

//! End-to-end coverage for the AI tagging vertical slice.
//!
//! The test installs a signed plugin ZIP whose entrypoint is the mock sidecar,
//! imports model files, runs the four-stage tagging pipeline through the real
//! daemon database and plugin service, cancels a slow job, and recovers a job
//! left `processing` by a crash.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use tempfile::TempDir;
use vrc_photon_ai_host::test_support::{
    TEST_CATALOG_VERSION, TEST_ENTRYPOINT, TEST_VARIANT, TestEntry, TestKey, TestPackage,
    sha256_hex,
};
use vrc_photon_ai_host::{ManifestFile, ModelHttpClient};
use vrc_photon_ai_protocol::{ModelState, TaggingJobStatus, TaggingStage};
use vrc_photon_core::db::Db;
use vrc_photon_core::settings::AppSettings;
use vrc_photon_ipc::{Event, PluginInstallStatus};

use crate::Daemon;
use crate::plugin::AI_PLUGIN_ID;
use crate::tagging::{TaggingWorker, TaggingWorkerConfig};
use crate::test_support::mock_sidecar;

const PLUGIN: &str = AI_PLUGIN_ID;
const MODEL_BYTES: &[u8] = b"model";
const MMPROJ_BYTES: &[u8] = b"mmproj";
const MODEL_FILE: &str = "d1-2B-Q8_0.gguf";
const MMPROJ_FILE: &str = "mmproj-d1-2B-Q8_0.gguf";
/// Long enough that a broken cancellation cannot hide behind the response.
const SLOW_DELAY_MS: u64 = 30_000;

const CATALOG_JSON: &str = r#"{
  "version": "2026-11-07.1",
  "outdoor": [
    {
      "key": "sea",
      "name": "海",
      "name_en": "Sea",
      "description": "海が見える屋外",
      "default_threshold": 0.5
    }
  ],
  "indoor": [
    {
      "key": "japanese_room",
      "name": "和室",
      "name_en": "Japanese room",
      "description": "畳・障子のある和室",
      "default_threshold": 0.5
    }
  ],
  "atmosphere": [
    {
      "key": "weather_clear",
      "name": "晴れ",
      "name_en": "Clear / sunny",
      "description": "晴れている",
      "default_threshold": 0.5,
      "group": "weather"
    }
  ]
}"#;

#[test]
fn install_import_tag_cancel_and_resume() {
    let mut ctx = DaemonAiE2e::new();
    ctx.install_signed_zip();
    ctx.import_model_files();
    ctx.enable_ai();

    let photo = ctx.add_photo("first.png");
    let job = ctx.enqueue(photo);
    let events = ctx.daemon.events.subscribe();
    assert!(ctx.run_worker_once());

    let ai = ctx
        .db()
        .photo_ai(photo)
        .unwrap()
        .expect("the tagging pipeline wrote a photo_ai row");
    assert!(ai.character_present);
    assert_eq!(ai.people_min, Some(2));
    assert_eq!(ai.people_max, Some(4));
    assert_eq!(ai.people_median, Some(3));
    assert_eq!(ai.place_branch.as_deref(), Some("other"));
    assert_eq!(ai.variant, TEST_VARIANT);
    assert_eq!(ai.catalog_version, TEST_CATALOG_VERSION);
    let tags = ctx.db().photo_ai_tags(photo, true).unwrap();
    assert!(tags.iter().any(|tag| tag.tag_key == "sea"));
    assert!(tags.iter().any(|tag| tag.tag_key == "weather_clear"));

    let events = progress_events(&events, &job);
    let has_stage = |progress: f64, stage: TaggingStage| {
        events.iter().any(|(status, value, event_stage)| {
            *status == TaggingJobStatus::Processing
                && (*value - progress).abs() < 1e-9
                && *event_stage == Some(stage)
        })
    };
    assert!(has_stage(0.1, TaggingStage::Presence));
    assert!(has_stage(0.3, TaggingStage::PeopleCount));
    assert!(has_stage(0.5, TaggingStage::PlaceClass));
    assert!(has_stage(0.7, TaggingStage::PlaceTags));
    assert!(has_stage(0.8, TaggingStage::AtmosphereTags));
    assert!(events.iter().any(|(status, value, _)| {
        *status == TaggingJobStatus::Completed && (*value - 1.0).abs() < 1e-9
    }));
    assert_eq!(
        ctx.db().tagging_job(&job).unwrap().unwrap().status,
        TaggingJobStatus::Completed
    );

    let second = ctx.add_photo("second.png");
    let slow_job = ctx.enqueue_and_start_slow(second);
    let cancelled_at = Instant::now();
    ctx.cancel(&slow_job);
    assert!(ctx.run_worker_once());
    assert!(
        cancelled_at.elapsed() < Duration::from_secs(5),
        "cancellation waited for the slow sidecar response"
    );
    assert_eq!(
        ctx.db().tagging_job(&slow_job).unwrap().unwrap().status,
        TaggingJobStatus::Cancelled
    );
    assert!(ctx.db().photo_ai(second).unwrap().is_none());
    assert!(ctx.db().photo_ai_tags(second, false).unwrap().is_empty());

    let crashed_job = ctx.simulate_crash_processing(second);
    assert_eq!(ctx.db().tagging_job_counts(PLUGIN).unwrap().processing, 1);
    assert_eq!(ctx.resume_processing_jobs(), 1);
    assert_eq!(ctx.db().tagging_job_counts(PLUGIN).unwrap().processing, 0);
    assert_eq!(
        ctx.db().tagging_job(&crashed_job).unwrap().unwrap().status,
        TaggingJobStatus::Pending
    );
    let reclaimed = ctx
        .db()
        .next_pending_tagging_job()
        .unwrap()
        .expect("the resumed job is reclaimable");
    assert_eq!(reclaimed.id, crashed_job);
    assert_eq!(ctx.resume_processing_jobs(), 1);
}

/// One daemon wired to an isolated library, a test signing key, and the mock
/// sidecar.
struct DaemonAiE2e {
    _dir: TempDir,
    daemon: Daemon,
    db: Arc<Mutex<Db>>,
    http: Arc<MockModelClient>,
    mock: PathBuf,
    zip: PathBuf,
    photo_dir: PathBuf,
    worker: Arc<TaggingWorker>,
    slow_worker: Option<JoinHandle<anyhow::Result<bool>>>,
}

impl DaemonAiE2e {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("create the test directory");
        let photo_dir = dir.path().join("photos");
        let log_dir = dir.path().join("logs");
        fs::create_dir_all(&photo_dir).expect("create the photo directory");
        fs::create_dir_all(&log_dir).expect("create the log directory");
        let mut daemon = Daemon::open(
            dir.path().join("library.db"),
            Some(photo_dir.clone()),
            Some(log_dir),
        )
        .expect("open the daemon");
        daemon.set_ipc_endpoint(None);
        daemon.set_thumbnail_dir(dir.path().join("thumbnails"));

        let key = TestKey::generate();
        let http = Arc::new(MockModelClient::new());
        daemon.set_ai_data_dir(dir.path().join("ai"));
        daemon.set_ai_plugin_keys(vec![key.public_key()]);
        daemon.set_ai_http_client(http.clone());

        let mock = mock_sidecar();
        let package = package_with_entrypoint(&key, wrapper_entrypoint(&mock, "normal", None));
        let zip = dir.path().join("plugin.zip");
        package.write_zip(&zip).expect("write the test package");

        let db = daemon.db.clone();
        let worker = Arc::new(TaggingWorker::new(
            db.clone(),
            daemon.ai_data_dir.clone(),
            daemon.plugin_service().clone(),
            daemon.events.clone(),
            daemon.running.clone(),
            daemon.tagging.clone(),
            daemon.tagging_control.clone(),
            TaggingWorkerConfig {
                retry_delays: [Duration::from_millis(1), Duration::from_millis(1)],
                idle_timeout: Duration::from_secs(600),
                poll_interval: Duration::from_millis(5),
            },
        ));

        Self {
            _dir: dir,
            daemon,
            db,
            http,
            mock,
            zip,
            photo_dir,
            worker,
            slow_worker: None,
        }
    }

    fn db(&self) -> MutexGuard<'_, Db> {
        self.db
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn install_signed_zip(&self) {
        let install_rx = self.daemon.events.subscribe();
        self.daemon
            .plugin_service()
            .install_from_zip(&self.zip)
            .expect("accept the signed plugin ZIP");
        wait_for_install(&install_rx);

        let install = self
            .db()
            .active_plugin_installation(PLUGIN)
            .unwrap()
            .expect("the install was activated");
        assert_eq!(install.variant, TEST_VARIANT);
        assert_eq!(self.db().ai_tag_definitions(PLUGIN).unwrap().len(), 3);
        assert!(
            self.db()
                .plugin_state(PLUGIN)
                .unwrap()
                .expect("probe state")
                .probe_json
                .is_some(),
            "install must run and cache the probe"
        );
        assert!(
            !self.daemon.plugin_service().is_ready(PLUGIN),
            "the plugin is not ready before its model files are imported"
        );
    }

    fn import_model_files(&self) {
        let sources = tempfile::tempdir().expect("create the model source directory");
        let model = sources.path().join(MODEL_FILE);
        let mmproj = sources.path().join(MMPROJ_FILE);
        fs::write(&model, MODEL_BYTES).expect("write the model file");
        fs::write(&mmproj, MMPROJ_BYTES).expect("write the mmproj file");

        self.daemon
            .plugin_service()
            .import_model_files(PLUGIN, &[model, mmproj])
            .expect("accept the model import");
        self.wait_for_model_ready();

        assert!(
            self.daemon.plugin_service().is_ready(PLUGIN),
            "the imported model must make the plugin ready"
        );
        assert_eq!(
            self.http.calls(),
            0,
            "importing files must not hit the model download client"
        );
    }

    fn wait_for_model_ready(&self) {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let finished = !self.daemon.plugin_service().operation_active(PLUGIN);
            let ready = self
                .db()
                .plugin_state(PLUGIN)
                .ok()
                .flatten()
                .is_some_and(|state| state.model_state == ModelState::Ready);
            if finished && ready {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for the model import"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn enable_ai(&mut self) {
        {
            let db = self.db();
            let mut settings = AppSettings::load(&db).expect("load the settings");
            settings.ai_tagging_enabled = true;
            settings.ai_tagging_auto = false;
            settings.save(&db).expect("save the settings");
        }
        self.daemon.settings.ai_tagging_enabled = true;
        self.daemon.settings.ai_tagging_auto = false;
    }

    fn add_photo(&mut self, name: &str) -> i64 {
        let path = self.photo_dir.join(name);
        image::RgbaImage::from_pixel(32, 24, image::Rgba([10, 20, 30, 255]))
            .save(&path)
            .expect("write the test photo");
        self.daemon
            .scan_photos()
            .expect("scan the photo into the library");
        self.db()
            .photo_by_path(path.to_str().unwrap())
            .unwrap()
            .expect("the scanned photo is indexed")
            .id
    }

    fn enqueue(&self, photo: i64) -> String {
        let inserted = self
            .db()
            .enqueue_tagging_jobs(&[photo], PLUGIN, TaggingJobStatus::Pending)
            .expect("enqueue the tagging job");
        assert_eq!(inserted, 1, "one new job per enqueue");
        self.db()
            .tagging_jobs_for_photo(photo)
            .expect("read the tagging jobs")
            .into_iter()
            .find(|job| job.status == TaggingJobStatus::Pending)
            .expect("the job is pending")
            .id
    }

    fn run_worker_once(&mut self) -> bool {
        if let Some(handle) = self.slow_worker.take() {
            return handle
                .join()
                .expect("the slow tagging worker panicked")
                .expect("the slow tagging worker failed");
        }
        self.worker.process_once().expect("run the tagging worker")
    }

    /// Rewrites the installed entrypoint to the slow mock mode, stops the
    /// cached sidecar, and runs one job in the background until the presence
    /// request is in flight.
    fn enqueue_and_start_slow(&mut self, photo: i64) -> String {
        let install = self
            .db()
            .active_plugin_installation(PLUGIN)
            .unwrap()
            .expect("the active install");
        let entrypoint = Path::new(&install.path).join(TEST_ENTRYPOINT);
        fs::write(
            entrypoint,
            wrapper_entrypoint(&self.mock, "slow", Some(SLOW_DELAY_MS)),
        )
        .expect("rewrite the entrypoint");
        self.daemon.plugin_service().stop_sidecar();

        let job = self.enqueue(photo);
        let events = self.daemon.events.subscribe();
        let worker = self.worker.clone();
        self.slow_worker = Some(thread::spawn(move || worker.process_once()));
        wait_for_presence_stage(&events, &job);
        job
    }

    fn cancel(&self, job: &str) {
        // Mirrors the IPC handler: the database row is authoritative and the
        // token interrupts the in-flight sidecar request.
        assert!(
            self.db().cancel_tagging_job(job).unwrap(),
            "the job must still be cancellable"
        );
        assert!(
            self.daemon.tagging_control.cancel(job),
            "the running job must have a registered cancel token"
        );
    }

    fn simulate_crash_processing(&self, photo: i64) -> String {
        let inserted = self
            .db()
            .enqueue_tagging_jobs(&[photo], PLUGIN, TaggingJobStatus::Pending)
            .expect("enqueue the crash job");
        assert_eq!(inserted, 1);
        let claimed = self
            .db()
            .next_pending_tagging_job()
            .unwrap()
            .expect("claim the crash job");
        assert_eq!(claimed.photo_id, photo);
        assert_eq!(claimed.status, TaggingJobStatus::Processing);
        claimed.id
    }

    fn resume_processing_jobs(&self) -> usize {
        self.db().resume_interrupted_tagging_jobs().unwrap()
    }
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// A shell entrypoint that exports the mock's scenario and execs the binary.
/// The daemon inherits no test environment, so each spawn reads these values
/// from the package itself.
fn wrapper_entrypoint(mock: &Path, mode: &str, delay_ms: Option<u64>) -> Vec<u8> {
    let delay = delay_ms
        .map(|delay| format!("export MOCK_SIDECAR_DELAY_MS={delay}\n"))
        .unwrap_or_default();
    format!(
        "#!/bin/sh\n\
         export MOCK_SIDECAR_MODE={mode}\n\
         export MOCK_SIDECAR_PRESENT=1\n\
         export MOCK_SIDECAR_PRESENCE_P=0.99\n\
         export MOCK_SIDECAR_PEOPLE_COUNT=3\n\
         export MOCK_SIDECAR_PEOPLE_INTERVAL='2,4'\n\
         export MOCK_SIDECAR_PLACE_BRANCH=other\n\
         {delay}\
         exec {mock} \"$@\"\n",
        mock = shell_quote(&mock.to_string_lossy()),
    )
    .into_bytes()
}

fn package_with_entrypoint(key: &TestKey, entrypoint: Vec<u8>) -> TestPackage {
    let entries = vec![
        TestEntry::with_mode(TEST_ENTRYPOINT, entrypoint, 0o755),
        TestEntry::new("catalog.json", CATALOG_JSON),
        TestEntry::new("calibration.json", b"{}"),
    ];
    let files = entries
        .iter()
        .map(|entry| ManifestFile {
            path: entry.path.clone(),
            size: entry.data.len() as u64,
            sha256: sha256_hex(&entry.data),
        })
        .collect();

    let mut package = TestPackage::with_key(key.clone());
    package.manifest_mut().files = files;
    *package.entries_mut() = entries;
    package
}

struct MockModelClient {
    calls: AtomicUsize,
}

impl MockModelClient {
    fn new() -> Self {
        Self {
            calls: AtomicUsize::new(0),
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl ModelHttpClient for MockModelClient {
    fn download(
        &self,
        url: &str,
        on_chunk: &mut dyn FnMut(&[u8]) -> vrc_photon_ai_host::Result<()>,
    ) -> vrc_photon_ai_host::Result<()> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let bytes = if url.ends_with(MMPROJ_FILE) {
            MMPROJ_BYTES
        } else if url.ends_with(MODEL_FILE) {
            MODEL_BYTES
        } else {
            panic!("unexpected model URL {url}");
        };
        on_chunk(bytes)
    }
}

fn wait_for_install(rx: &Receiver<Event>) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(remaining) {
            Ok(Event::PluginInstallProgress {
                status: PluginInstallStatus::Installed,
                ..
            }) => return,
            Ok(Event::PluginInstallProgress {
                status: PluginInstallStatus::Failed,
                error,
                ..
            }) => panic!("plugin install failed: {error:?}"),
            Ok(_) => {}
            Err(RecvTimeoutError::Timeout) => {
                panic!("timed out waiting for the plugin install")
            }
            Err(RecvTimeoutError::Disconnected) => {
                panic!("the event bus stopped before the install finished")
            }
        }
    }
}

fn wait_for_presence_stage(rx: &Receiver<Event>, job_id: &str) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(remaining) {
            Ok(Event::TaggingProgress {
                job_id: id,
                progress,
                ..
            }) if id == job_id && (progress - 0.1).abs() < 1e-9 => return,
            Ok(_) => {}
            Err(error) => panic!("timed out waiting for the presence stage: {error}"),
        }
    }
}

fn progress_events(
    rx: &Receiver<Event>,
    job_id: &str,
) -> Vec<(TaggingJobStatus, f64, Option<TaggingStage>)> {
    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        if let Event::TaggingProgress {
            job_id: id,
            status,
            progress,
            stage,
            ..
        } = event
            && id == job_id
        {
            events.push((status, progress, stage));
        }
    }
    events
}

/// Manual real-sidecar E2E. Not part of CI: it needs the signed dev ZIP and
/// the 2 GB d1-2B model files. Run with:
///
/// ```text
/// VRC_PHOTON_AUTOTAG_DEV_ZIP=... VRC_PHOTON_AUTOTAG_MODEL_DIR=... \
///   cargo test -p vrc-photon-daemon real_sidecar_install_import_and_tag -- --ignored --nocapture
/// ```
#[test]
#[ignore = "requires VRC_PHOTON_AUTOTAG_DEV_ZIP and VRC_PHOTON_AUTOTAG_MODEL_DIR"]
fn real_sidecar_install_import_and_tag() {
    let zip = std::env::var("VRC_PHOTON_AUTOTAG_DEV_ZIP").expect("dev ZIP path");
    let model_dir =
        PathBuf::from(std::env::var("VRC_PHOTON_AUTOTAG_MODEL_DIR").expect("model dir"));
    let target = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target");
    let dir = tempfile::Builder::new()
        .prefix("real-ai-e2e-")
        .tempdir_in(target)
        .expect("create the test directory");
    let photo_dir = dir.path().join("photos");
    let log_dir = dir.path().join("logs");
    fs::create_dir_all(&photo_dir).unwrap();
    fs::create_dir_all(&log_dir).unwrap();
    let mut daemon = Daemon::open(
        dir.path().join("library.db"),
        Some(photo_dir.clone()),
        Some(log_dir),
    )
    .expect("open the daemon");
    daemon.set_ipc_endpoint(None);
    daemon.set_thumbnail_dir(dir.path().join("thumbnails"));
    daemon.set_ai_data_dir(dir.path().join("ai"));

    let install_rx = daemon.events.subscribe();
    daemon
        .plugin_service()
        .install_from_zip(Path::new(&zip))
        .expect("accept the dev ZIP");
    wait_for_install(&install_rx);
    let install = daemon
        .db
        .lock()
        .unwrap()
        .active_plugin_installation(PLUGIN)
        .unwrap()
        .expect("the dev ZIP is active");
    assert_eq!(install.variant, "linux-cpu");
    assert!(
        daemon
            .db
            .lock()
            .unwrap()
            .plugin_state(PLUGIN)
            .unwrap()
            .expect("plugin state")
            .probe_json
            .is_some(),
        "the dev ZIP must probe"
    );

    let models = [
        model_dir.join("d1-2B-Q8_0.gguf"),
        model_dir.join("mmproj-d1-2B-Q8_0.gguf"),
    ];
    daemon
        .plugin_service()
        .import_model_files(PLUGIN, &models)
        .expect("import the real model");
    let deadline = Instant::now() + Duration::from_secs(180);
    loop {
        let state = daemon.db.lock().unwrap().plugin_state(PLUGIN).unwrap();
        if state
            .as_ref()
            .is_some_and(|state| state.model_state == ModelState::Ready)
            && !daemon.plugin_service().operation_active(PLUGIN)
        {
            break;
        }
        assert!(Instant::now() < deadline, "timed out importing the model");
        thread::sleep(Duration::from_millis(50));
    }
    assert!(daemon.plugin_service().is_ready(PLUGIN));

    {
        let db = daemon.db.lock().unwrap();
        let mut settings = AppSettings::load(&db).unwrap();
        settings.ai_tagging_enabled = true;
        settings.ai_tagging_auto = false;
        settings.save(&db).unwrap();
    }
    daemon.settings.ai_tagging_enabled = true;
    daemon.settings.ai_tagging_auto = false;

    let path = photo_dir.join("VRChat_2026-11-04_20-00-00.000_16x9.png");
    image::RgbaImage::from_pixel(1280, 720, image::Rgba([20, 30, 40, 255]))
        .save(&path)
        .unwrap();
    daemon.scan_photos().expect("scan the photo");
    let photo = daemon
        .db
        .lock()
        .unwrap()
        .photo_by_path(path.to_str().unwrap())
        .unwrap()
        .expect("the photo is indexed")
        .id;
    daemon
        .db
        .lock()
        .unwrap()
        .enqueue_tagging_jobs(&[photo], PLUGIN, TaggingJobStatus::Pending)
        .unwrap();
    let worker = TaggingWorker::new(
        daemon.db.clone(),
        daemon.ai_data_dir.clone(),
        daemon.plugin_service().clone(),
        daemon.events.clone(),
        daemon.running.clone(),
        daemon.tagging.clone(),
        daemon.tagging_control.clone(),
        TaggingWorkerConfig {
            retry_delays: [Duration::from_secs(1), Duration::from_secs(2)],
            idle_timeout: Duration::from_secs(600),
            poll_interval: Duration::from_millis(20),
        },
    );
    assert!(worker.process_once().expect("run the real worker"));
    let ai = daemon
        .db
        .lock()
        .unwrap()
        .photo_ai(photo)
        .unwrap()
        .expect("the real sidecar wrote photo_ai");
    println!(
        "real sidecar: present={} p={} people={:?}..{:?} place={:?} tags={}",
        ai.character_present,
        ai.presence_probability,
        ai.people_min,
        ai.people_max,
        ai.place_branch,
        daemon
            .db
            .lock()
            .unwrap()
            .photo_ai_tags(photo, false)
            .unwrap()
            .len()
    );
    assert!(!ai.variant.is_empty());
    assert_eq!(ai.catalog_version, TEST_CATALOG_VERSION);
}
