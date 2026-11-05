use std::io::{self, BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use interprocess::TryClone as _;
#[cfg(unix)]
use interprocess::local_socket::traits::Stream as _;
use vrc_photon_core::db::Db;
use vrc_photon_ipc::framing;
use vrc_photon_ipc::server::Listener;
use vrc_photon_ipc::{
    APP_VERSION, Endpoint, ErrorCode, Event, PROTOCOL_VERSION, Request, RequestEnvelope, Response,
    ServerMessage, StatusSnapshot,
};

use crate::DaemonStatus;
use crate::plugin::{AI_PLUGIN_ID, PluginError, PluginService};
use crate::signals::{JobSignal, WakeSignal};
use crate::tagging::TaggingControl;

/// Maximum events queued per subscriber. Publishing never blocks: a slow
/// subscriber drops new events (and stays subscribed), while a disconnected
/// one is pruned. Status events are best-effort and latest-wins through
/// `StatusSnapshot::version`.
const EVENT_QUEUE_CAPACITY: usize = 256;

/// Maximum concurrent IPC connections. Request connections are short-lived,
/// but the UI also keeps one long-lived subscription, so this stays above the
/// client's eight in-flight request slots to avoid rejecting a busy UI's own
/// requests.
const MAX_IPC_CONNECTIONS: usize = 16;

/// How often an idle subscription receives a `StatusChanged` heartbeat. The
/// client treats 30 seconds without any event as a disconnect, so two missed
/// heartbeats are tolerated.
#[cfg(test)]
const SUBSCRIPTION_HEARTBEAT_INTERVAL: Duration = Duration::from_millis(200);
#[cfg(not(test))]
const SUBSCRIPTION_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);

/// Fan-out for daemon events. Dead subscribers are pruned on publish and
/// each live subscriber has a bounded queue.
#[derive(Clone)]
pub(crate) struct EventBus {
    subscribers: Arc<Mutex<Vec<SyncSender<vrc_photon_ipc::Event>>>>,
}

impl Default for EventBus {
    fn default() -> Self {
        Self {
            subscribers: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

impl EventBus {
    pub(crate) fn publish(&self, event: vrc_photon_ipc::Event) {
        let mut subscribers = self.subscribers.lock().unwrap();
        subscribers.retain(|sender| match sender.try_send(event.clone()) {
            Ok(()) => true,
            // The subscriber is behind: drop this event for it, but keep the
            // subscription so it can catch up from the database later.
            Err(TrySendError::Full(_)) => true,
            Err(TrySendError::Disconnected(_)) => false,
        });
    }

    pub(crate) fn subscribe(&self) -> Receiver<vrc_photon_ipc::Event> {
        let (sender, receiver) = mpsc::sync_channel(EVENT_QUEUE_CAPACITY);
        self.subscribers.lock().unwrap().push(sender);
        receiver
    }
}

/// Bounded connection slots for the accept loop.
struct ConnectionSlots {
    in_use: AtomicUsize,
    limit: usize,
}

impl ConnectionSlots {
    const fn new(limit: usize) -> Self {
        Self {
            in_use: AtomicUsize::new(0),
            limit,
        }
    }

    fn try_acquire(self: &Arc<Self>) -> Option<ConnectionSlot> {
        if self.in_use.fetch_add(1, Ordering::AcqRel) >= self.limit {
            self.in_use.fetch_sub(1, Ordering::AcqRel);
            return None;
        }
        Some(ConnectionSlot {
            slots: self.clone(),
        })
    }
}

struct ConnectionSlot {
    slots: Arc<ConnectionSlots>,
}

impl Drop for ConnectionSlot {
    fn drop(&mut self) {
        self.slots.in_use.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Shared daemon handles used by the IPC server.
#[derive(Clone)]
pub(crate) struct IpcControl {
    pub(crate) running: Arc<AtomicBool>,
    pub(crate) status: Arc<Mutex<DaemonStatus>>,
    pub(crate) wake: Arc<WakeSignal>,
    pub(crate) jobs: Arc<JobSignal>,
    pub(crate) sends: Arc<JobSignal>,
    pub(crate) events: EventBus,
    pub(crate) thumbnail_dir: PathBuf,
    pub(crate) thumbnail_cache_epoch: Arc<AtomicU64>,
    pub(crate) thumbnail_external_bytes: Arc<AtomicU64>,
    pub(crate) plugin: PluginService,
    pub(crate) tagging: Arc<JobSignal>,
    pub(crate) tagging_control: Arc<TaggingControl>,
    pub(crate) db: Arc<Mutex<Db>>,
}

impl IpcControl {
    fn snapshot(&self) -> StatusSnapshot {
        status_snapshot(&mut self.status.lock().unwrap())
    }

    fn set_paused(&self, paused: bool) {
        {
            let mut status = self.status.lock().unwrap();
            status.paused = paused;
        }
        self.wake.notify();
        self.events.publish(Event::StatusChanged {
            status: self.snapshot(),
        });
    }

    fn shutdown(&self) {
        self.running.store(false, Ordering::Relaxed);
        self.wake.notify();
    }
}

/// Takes a snapshot of the status, stamping it with the next version.
/// Callers hold the status lock, so the version orders the snapshot
/// against the state it was read from: a snapshot read before a state
/// change always carries a lower version than one read after it, no
/// matter when each is published.
pub(crate) fn status_snapshot(status: &mut DaemonStatus) -> StatusSnapshot {
    status.version += 1;
    StatusSnapshot {
        paused: status.paused,
        watching: status.watching,
        scanning: status.scanning,
        scan_phase: status.scan_phase,
        scan_processed: status.scan_processed,
        scan_total: status.scan_total,
        scan_indexed: status.scan_indexed,
        scan_root: status.scan_root,
        scan_roots: status.scan_roots,
        photo_count: status.photo_count,
        pending_jobs: status.pending_jobs,
        pending_sends: status.pending_sends,
        version: status.version,
        pending_tagging_jobs: status.pending_tagging_jobs,
        tagging_ready: status.tagging_ready,
    }
}

/// Reads the AI tagging status fields without holding the status lock.
pub(crate) fn tagging_status(db: &Arc<Mutex<Db>>, plugin: &PluginService) -> (i64, bool) {
    let pending_tagging_jobs = {
        let db = db.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        match db.tagging_job_counts(AI_PLUGIN_ID) {
            Ok(counts) => counts.unfinished(),
            Err(error) => {
                tracing::warn!(%error, "cannot read tagging job counts");
                0
            }
        }
    };
    (pending_tagging_jobs, plugin.is_ready(AI_PLUGIN_ID))
}

/// The listening IPC server. Dropping it stops the accept thread and removes
/// the Unix socket.
pub(crate) struct IpcServer {
    stop: Arc<AtomicBool>,
    accept: Option<JoinHandle<()>>,
}

impl IpcServer {
    pub(crate) fn start(endpoint: Endpoint, control: IpcControl) -> io::Result<Self> {
        Self::start_with_limit(endpoint, control, MAX_IPC_CONNECTIONS)
    }

    fn start_with_limit(
        endpoint: Endpoint,
        control: IpcControl,
        connection_limit: usize,
    ) -> io::Result<Self> {
        let listener = Listener::bind(endpoint)?;
        listener.set_nonblocking_accept(true)?;
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let accept = thread::Builder::new()
            .name("ipc-server".to_string())
            .spawn(move || accept_loop(listener, control, thread_stop, connection_limit))?;
        Ok(Self {
            stop,
            accept: Some(accept),
        })
    }
}

impl Drop for IpcServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.accept.take() {
            let _ = handle.join();
        }
    }
}

fn accept_loop(
    listener: Listener,
    control: IpcControl,
    stop: Arc<AtomicBool>,
    connection_limit: usize,
) {
    let slots = Arc::new(ConnectionSlots::new(connection_limit));
    while !stop.load(Ordering::Relaxed) && control.running.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok(stream) => {
                let Some(slot) = slots.try_acquire() else {
                    tracing::debug!("rejecting IPC connection: all slots are in use");
                    // Dropping the stream closes the connection immediately.
                    // The client sees a disconnect and retries with backoff.
                    drop(stream);
                    continue;
                };
                let control = control.clone();
                if let Err(error) = thread::Builder::new()
                    .name("ipc-connection".to_string())
                    .spawn(move || {
                        let _slot = slot;
                        if let Err(error) = handle_connection(stream, control) {
                            tracing::debug!(%error, "IPC connection ended");
                        }
                    })
                {
                    tracing::warn!(%error, "cannot spawn IPC connection thread");
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(50));
            }
            Err(error) => {
                tracing::warn!(%error, "IPC accept failed");
                thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

fn handle_connection(
    stream: vrc_photon_ipc::server::Stream,
    control: IpcControl,
) -> io::Result<()> {
    let mut reader = BufReader::new(stream);
    #[cfg(unix)]
    {
        reader
            .get_mut()
            .set_recv_timeout(Some(Duration::from_secs(5)))?;
        reader
            .get_mut()
            .set_send_timeout(Some(Duration::from_secs(5)))?;
    }

    let Some(hello) = framing::read_message::<_, RequestEnvelope>(&mut reader)? else {
        return Ok(());
    };
    let Request::Hello {
        protocol_version,
        client: _,
    } = hello.request
    else {
        write_response(
            reader.get_mut(),
            hello.id,
            Response::Error {
                code: ErrorCode::InvalidRequest,
                message: "hello required".to_string(),
            },
        )?;
        return Ok(());
    };

    write_response(
        reader.get_mut(),
        hello.id,
        Response::Hello {
            protocol_version: PROTOCOL_VERSION,
            daemon_version: APP_VERSION.to_string(),
            pid: std::process::id(),
            status: control.snapshot(),
        },
    )?;
    if protocol_version != PROTOCOL_VERSION {
        return Ok(());
    }

    loop {
        let Some(envelope) = framing::read_message::<_, RequestEnvelope>(&mut reader)? else {
            return Ok(());
        };
        let id = envelope.id;
        match envelope.request {
            Request::Hello { .. } => {
                write_response(
                    reader.get_mut(),
                    id,
                    Response::Error {
                        code: ErrorCode::InvalidRequest,
                        message: "hello was already completed".to_string(),
                    },
                )?;
            }
            Request::Status => {
                write_response(
                    reader.get_mut(),
                    id,
                    Response::Status {
                        status: control.snapshot(),
                    },
                )?;
            }
            Request::WakeCommands => {
                control.wake.notify();
                write_response(reader.get_mut(), id, Response::Ok)?;
            }
            Request::WakeJobs => {
                control.jobs.notify();
                write_response(reader.get_mut(), id, Response::Ok)?;
            }
            Request::WakeSends => {
                control.sends.notify();
                write_response(reader.get_mut(), id, Response::Ok)?;
            }
            Request::SetPaused { paused } => {
                control.set_paused(paused);
                write_response(reader.get_mut(), id, Response::Ok)?;
            }
            Request::ThumbnailsGenerated { files, bytes } => {
                control
                    .thumbnail_external_bytes
                    .fetch_add(bytes, Ordering::Relaxed);
                tracing::debug!(files, bytes, "UI reported generated thumbnails");
                write_response(reader.get_mut(), id, Response::Ok)?;
            }
            Request::ClearThumbnailCache => {
                let response = match vrc_photon_core::thumbnail::clear_thumbnail_cache_in(
                    &control.thumbnail_dir,
                ) {
                    Ok(report) => {
                        control.thumbnail_external_bytes.store(0, Ordering::Relaxed);
                        control
                            .thumbnail_cache_epoch
                            .fetch_add(1, Ordering::Relaxed);
                        tracing::info!(
                            removed_files = report.removed_files,
                            removed_bytes = report.removed_bytes,
                            "cleared thumbnail cache"
                        );
                        Response::ThumbnailCacheCleared {
                            removed_files: report.removed_files as u64,
                            removed_bytes: report.removed_bytes,
                            remaining_files: report.remaining_files as u64,
                            remaining_bytes: report.remaining_bytes,
                        }
                    }
                    Err(error) => {
                        tracing::warn!(%error, "cannot clear thumbnail cache");
                        Response::Error {
                            code: ErrorCode::InvalidRequest,
                            message: error.to_string(),
                        }
                    }
                };
                write_response(reader.get_mut(), id, response)?;
            }
            Request::Shutdown => {
                // Clear the running flag before acknowledging so a client that
                // observes `Ok` also observes a stopped daemon.
                control.shutdown();
                write_response(reader.get_mut(), id, Response::Ok)?;
                return Ok(());
            }
            Request::Subscribe => {
                let events = control.events.subscribe();
                write_response(
                    reader.get_mut(),
                    id,
                    Response::Subscribed {
                        status: control.snapshot(),
                    },
                )?;
                stream_events(&mut reader, &control, events)?;
                return Ok(());
            }
            Request::WakeTagging => {
                control.tagging.notify();
                write_response(reader.get_mut(), id, Response::Ok)?;
            }
            Request::InstallPlugin { zip_path } => {
                let response = plugin_ack(control.plugin.install_from_zip(Path::new(&zip_path)));
                write_response(reader.get_mut(), id, response)?;
            }
            Request::UninstallPlugin {
                plugin_id,
                variant,
                version,
                delete_ai_tags,
            } => {
                let response = plugin_ack(control.plugin.uninstall_async(
                    &plugin_id,
                    variant.as_deref(),
                    version.as_deref(),
                    delete_ai_tags,
                ));
                write_response(reader.get_mut(), id, response)?;
            }
            Request::ProbePlugin { plugin_id } => {
                let response = plugin_ack(control.plugin.probe_async(&plugin_id));
                write_response(reader.get_mut(), id, response)?;
            }
            Request::SetActiveVariant {
                plugin_id,
                variant,
                version,
            } => {
                let response = plugin_ack(control.plugin.set_active_variant_async(
                    &plugin_id,
                    &variant,
                    version.as_deref(),
                ));
                write_response(reader.get_mut(), id, response)?;
            }
            Request::StartModelDownload { plugin_id } => {
                let response = plugin_ack(control.plugin.start_model_download(&plugin_id));
                write_response(reader.get_mut(), id, response)?;
            }
            Request::CancelModelDownload { plugin_id } => {
                let response = match control.plugin.cancel_model_download(&plugin_id) {
                    Ok(_) => Response::Ok,
                    Err(error) => plugin_error_response(error),
                };
                write_response(reader.get_mut(), id, response)?;
            }
            Request::ImportModelFiles { plugin_id, paths } => {
                let paths: Vec<PathBuf> = paths.into_iter().map(PathBuf::from).collect();
                let response = plugin_ack(control.plugin.import_model_files(&plugin_id, &paths));
                write_response(reader.get_mut(), id, response)?;
            }
            Request::DeleteModel { plugin_id } => {
                let response = plugin_ack(control.plugin.delete_model_async(&plugin_id));
                write_response(reader.get_mut(), id, response)?;
            }
            Request::CancelTagging { job_id } => {
                let cancelled = {
                    let db = control
                        .db
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    db.cancel_tagging_job(&job_id)
                };
                let response = match cancelled {
                    Ok(_) => {
                        control.tagging_control.cancel(&job_id);
                        Response::Ok
                    }
                    Err(error) => Response::Error {
                        code: ErrorCode::InvalidRequest,
                        message: error.to_string(),
                    },
                };
                write_response(reader.get_mut(), id, response)?;
            }
        }
    }
}

fn plugin_ack(result: crate::plugin::Result<()>) -> Response {
    match result {
        Ok(()) => Response::Ok,
        Err(error) => plugin_error_response(error),
    }
}

/// Maps an immediate plugin failure onto the protocol error codes. Only
/// validation failures reach IPC here; worker failures are published as
/// events instead.
fn plugin_error_response(error: PluginError) -> Response {
    let code = if error.is_consent_required() {
        ErrorCode::ConsentRequired
    } else {
        match &error {
            PluginError::NotFound(_) => ErrorCode::NotFound,
            PluginError::Busy(_) => ErrorCode::Busy,
            PluginError::NotReady(_) => ErrorCode::NotReady,
            PluginError::Host(_)
            | PluginError::Core(_)
            | PluginError::Json(_)
            | PluginError::Io(_) => ErrorCode::InvalidRequest,
        }
    };
    Response::Error {
        code,
        message: error.to_string(),
    }
}

fn stream_events(
    reader: &mut BufReader<vrc_photon_ipc::server::Stream>,
    control: &IpcControl,
    events: Receiver<vrc_photon_ipc::Event>,
) -> io::Result<()> {
    // Watch the read half on a cloned handle: after subscribing, the client
    // sends nothing, so EOF or a read error means it is gone. Without this,
    // a disconnect is only noticed when the next event write fails, and an
    // idle subscription leaks this thread and its EventBus sender.
    let disconnected = Arc::new(AtomicBool::new(false));
    let stop_watcher = Arc::new(AtomicBool::new(false));
    match reader.get_ref().try_clone() {
        Ok(probe) => spawn_disconnect_watcher(probe, disconnected.clone(), stop_watcher.clone()),
        Err(error) => {
            tracing::debug!(%error, "cannot watch subscription for disconnects");
        }
    }
    let mut result = Ok(());
    let mut last_write = Instant::now();
    while control.running.load(Ordering::Relaxed) && !disconnected.load(Ordering::Relaxed) {
        match events.recv_timeout(Duration::from_millis(100)) {
            Ok(event) => {
                if let Err(error) =
                    framing::write_message(reader.get_mut(), &ServerMessage::Event { event })
                {
                    result = Err(error);
                    break;
                }
                last_write = Instant::now();
            }
            Err(RecvTimeoutError::Timeout) => {
                // An idle but healthy subscription gets a status heartbeat so
                // the client's read timeout only fires for a hung daemon.
                if last_write.elapsed() >= SUBSCRIPTION_HEARTBEAT_INTERVAL {
                    let event = Event::StatusChanged {
                        status: control.snapshot(),
                    };
                    if let Err(error) =
                        framing::write_message(reader.get_mut(), &ServerMessage::Event { event })
                    {
                        result = Err(error);
                        break;
                    }
                    last_write = Instant::now();
                }
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    // The watcher is detached; the stop flag lets it exit on its next read
    // timeout instead of lingering until the client disconnects.
    stop_watcher.store(true, Ordering::Relaxed);
    result
}

/// Marks `disconnected` when the subscription's read half reaches EOF or
/// fails. Unexpected post-subscribe bytes are discarded, matching the
/// previous behavior of never reading from a subscribed connection.
fn spawn_disconnect_watcher(
    mut probe: vrc_photon_ipc::server::Stream,
    disconnected: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
) {
    let spawned = thread::Builder::new()
        .name("ipc-subscription-watch".to_string())
        .spawn(move || {
            #[cfg(unix)]
            let _ = probe.set_recv_timeout(Some(Duration::from_millis(200)));
            let mut buf = [0u8; 64];
            while !stop.load(Ordering::Relaxed) {
                match probe.read(&mut buf) {
                    Ok(0) => break,
                    Ok(_) => {}
                    Err(error)
                        if matches!(
                            error.kind(),
                            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                        ) => {}
                    Err(_) => break,
                }
            }
            disconnected.store(true, Ordering::Relaxed);
        });
    if let Err(error) = spawned {
        tracing::warn!(%error, "cannot spawn subscription watcher");
    }
}

fn write_response(
    stream: &mut vrc_photon_ipc::server::Stream,
    id: u64,
    response: Response,
) -> io::Result<()> {
    framing::write_message(stream, &ServerMessage::Response { id, response })
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert_cmd::cargo::cargo_bin;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex, OnceLock};
    use std::time::{Duration, Instant};
    use vrc_photon_ai_host::ManifestFile;
    use vrc_photon_ai_host::UreqModelClient;
    use vrc_photon_ai_host::test_support::{
        TEST_ENTRYPOINT, TestPackage, sample_manifest, sha256_hex,
    };
    use vrc_photon_ai_protocol::{ModelState, Probe, TaggingJobStatus};
    use vrc_photon_core::ai::PluginInstallation;
    use vrc_photon_core::db::Db;
    use vrc_photon_core::model::NewPhoto;
    use vrc_photon_ipc::client::{IpcError, Subscription, request};
    use vrc_photon_ipc::{Endpoint, Event, PluginInstallStatus, Request, Response};

    use crate::plugin::{AI_PLUGIN_ID, PluginService};
    use crate::tagging::TaggingControl;

    fn mock_sidecar() -> std::path::PathBuf {
        static MOCK: OnceLock<std::path::PathBuf> = OnceLock::new();
        MOCK.get_or_init(|| {
            let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
            let status = std::process::Command::new(cargo)
                .args(["build", "--package", "vrc-photon-ai-mock-sidecar"])
                .current_dir(env!("CARGO_MANIFEST_DIR"))
                .status()
                .expect("build the mock sidecar");
            assert!(status.success(), "cargo build failed for the mock sidecar");
            cargo_bin("mock-sidecar")
        })
        .clone()
    }

    /// Replaces the inert test entrypoint with the mock sidecar so the install
    /// probe succeeds the same way it does in the daemon's other tests.
    fn use_mock_sidecar(package: &mut TestPackage) {
        let entrypoint = std::fs::read(mock_sidecar()).expect("read the mock sidecar binary");
        let entry = package
            .entries_mut()
            .iter_mut()
            .find(|entry| entry.path == TEST_ENTRYPOINT)
            .expect("the test package has an entrypoint");
        entry.data = entrypoint;
        entry.unix_mode = Some(0o755);
        let catalog = package
            .entries_mut()
            .iter_mut()
            .find(|entry| entry.path == "catalog.json")
            .expect("the test package has a catalog");
        catalog.data = br#"{"version":"2026-11-03.1","outdoor":[],"indoor":[]}"#.to_vec();
        let files = package
            .entries()
            .iter()
            .map(|entry| ManifestFile {
                path: entry.path.clone(),
                size: entry.data.len() as u64,
                sha256: sha256_hex(&entry.data),
            })
            .collect();
        package.manifest_mut().files = files;
    }

    fn test_control() -> (tempfile::TempDir, Endpoint, IpcControl) {
        test_control_with_keys(Vec::new())
    }

    fn test_control_with_keys(keys: Vec<String>) -> (tempfile::TempDir, Endpoint, IpcControl) {
        let dir = tempfile::tempdir().unwrap();
        let endpoint = Endpoint::for_tests(dir.path());
        let db = Arc::new(Mutex::new(Db::open(dir.path().join("library.db")).unwrap()));
        let events = EventBus::default();
        let tagging = Arc::new(JobSignal::default());
        let plugin = PluginService::new(
            db.clone(),
            dir.path().join("ai"),
            events.clone(),
            keys,
            Arc::new(UreqModelClient::new()),
        );
        let control = IpcControl {
            running: Arc::new(AtomicBool::new(true)),
            status: Arc::new(Mutex::new(DaemonStatus::default())),
            wake: Arc::new(WakeSignal::default()),
            jobs: Arc::new(JobSignal::default()),
            sends: Arc::new(JobSignal::default()),
            events,
            thumbnail_dir: dir.path().join("thumbnails"),
            thumbnail_cache_epoch: Arc::new(AtomicU64::new(0)),
            thumbnail_external_bytes: Arc::new(AtomicU64::new(0)),
            plugin,
            tagging: tagging.clone(),
            tagging_control: Arc::new(TaggingControl::new(tagging)),
            db,
        };
        (dir, endpoint, control)
    }

    fn install_plugin_row(control: &IpcControl) {
        let manifest = sample_manifest();
        let install = PluginInstallation {
            plugin_id: AI_PLUGIN_ID.to_string(),
            variant: manifest.variant.clone(),
            version: manifest.plugin_version.clone(),
            protocol_version: manifest.protocol_version,
            path: "/nonexistent/plugin".to_string(),
            manifest_json: serde_json::to_string(&manifest).unwrap(),
            catalog_version: Some(manifest.catalog_version.clone()),
            active: true,
            installed_at: "2026-11-04T00:00:00Z".to_string(),
            updated_at: "2026-11-04T00:00:00Z".to_string(),
        };
        control
            .db
            .lock()
            .unwrap()
            .upsert_plugin_installation(&install)
            .unwrap();
    }

    fn mark_plugin_ready(control: &IpcControl) {
        install_plugin_row(control);
        let manifest = sample_manifest();
        let probe = Probe {
            protocol_version: vrc_photon_ai_protocol::PROTOCOL_VERSION,
            sidecar_version: "1.0.0".to_string(),
            plugin_id: AI_PLUGIN_ID.to_string(),
            variant: manifest.variant.clone(),
            catalog_version: manifest.catalog_version.clone(),
            backends: Vec::new(),
        };
        let db = control.db.lock().unwrap();
        db.set_plugin_model_state(
            AI_PLUGIN_ID,
            ModelState::Ready,
            Some(&manifest.model.revision),
            11,
            Some(11),
            None,
        )
        .unwrap();
        db.set_plugin_probe_json(AI_PLUGIN_ID, Some(&serde_json::to_string(&probe).unwrap()))
            .unwrap();
    }

    /// Sends one request without the public client's error flattening, so
    /// tests can assert the protocol error code.
    fn request_raw(endpoint: &Endpoint, request: Request) -> Response {
        let stream = vrc_photon_ipc::client::connect(endpoint, Duration::from_secs(1)).unwrap();
        let mut reader = BufReader::new(stream);
        framing::write_message(
            reader.get_mut(),
            &RequestEnvelope {
                id: 1,
                request: Request::Hello {
                    protocol_version: PROTOCOL_VERSION,
                    client: vrc_photon_ipc::ClientInfo {
                        name: "test".to_string(),
                        version: "0".to_string(),
                    },
                },
            },
        )
        .unwrap();
        match framing::read_message(&mut reader).unwrap() {
            Some(ServerMessage::Response {
                id: 1,
                response: Response::Hello { .. },
            }) => {}
            other => panic!("unexpected hello response: {other:?}"),
        }
        framing::write_message(reader.get_mut(), &RequestEnvelope { id: 2, request }).unwrap();
        match framing::read_message(&mut reader).unwrap() {
            Some(ServerMessage::Response { id: 2, response }) => response,
            other => panic!("unexpected response: {other:?}"),
        }
    }

    fn add_tagging_job(control: &IpcControl, name: &str, status: TaggingJobStatus) -> String {
        let db = control.db.lock().unwrap();
        let root = db.add_root("/photos", "reference").unwrap();
        let photo_id = db
            .upsert_photo(&NewPhoto {
                root_id: root,
                rel_path: format!("{name}.png"),
                abs_path: format!("/photos/{name}.png"),
                file_name: format!("{name}.png"),
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
        db.enqueue_tagging_jobs(&[photo_id], AI_PLUGIN_ID, status)
            .unwrap();
        db.active_tagging_jobs(10)
            .unwrap()
            .into_iter()
            .find(|job| job.photo_id == photo_id)
            .unwrap()
            .id
    }

    #[test]
    fn wake_commands_wakes_without_touching_sqlite() {
        let (_dir, endpoint, control) = test_control();
        let _server = IpcServer::start(endpoint.clone(), control.clone()).unwrap();
        let response = request(&endpoint, Request::WakeCommands, Duration::from_secs(1)).unwrap();
        assert!(matches!(response, Response::Ok));
        assert!(control.wake.is_pending());
    }

    #[test]
    fn wake_sends_wakes_the_send_worker_without_touching_sqlite() {
        let (_dir, endpoint, control) = test_control();
        let _server = IpcServer::start(endpoint.clone(), control.clone()).unwrap();
        let mut seen = 0;
        let request = std::thread::spawn(move || {
            request(&endpoint, Request::WakeSends, Duration::from_secs(1)).unwrap()
        });
        assert!(control.sends.wait(&mut seen, Duration::from_secs(1)));
        assert!(matches!(request.join().unwrap(), Response::Ok));
    }

    #[test]
    fn set_paused_updates_status_and_publishes_an_event() {
        let (_dir, endpoint, control) = test_control();
        let _server = IpcServer::start(endpoint.clone(), control.clone()).unwrap();
        let (mut subscription, _) = Subscription::open(&endpoint, Duration::from_secs(1)).unwrap();
        let response = request(
            &endpoint,
            Request::SetPaused { paused: true },
            Duration::from_secs(1),
        )
        .unwrap();
        assert!(matches!(response, Response::Ok));
        assert!(control.status.lock().unwrap().paused);
        assert!(matches!(
            subscription.next_event().unwrap(),
            Some(Event::StatusChanged { .. })
        ));
    }

    #[test]
    fn a_snapshot_read_before_a_state_change_versions_below_one_read_after() {
        // The stale-publish race from #66: a snapshot read while a scan is
        // running must stay older than the snapshot read after the scan
        // finished, so the receiver can drop it even when it is published
        // after the fresher one.
        let (_dir, _endpoint, control) = test_control();
        control.status.lock().unwrap().scanning = true;
        let stale = control.snapshot();
        control.status.lock().unwrap().scanning = false;
        let fresh = control.snapshot();

        assert!(stale.scanning);
        assert!(!fresh.scanning);
        assert!(
            stale.version < fresh.version,
            "stale {} must version below fresh {}",
            stale.version,
            fresh.version
        );
    }

    #[test]
    fn status_snapshot_copies_pending_sends() {
        let (_dir, _endpoint, control) = test_control();
        control.status.lock().unwrap().pending_sends = 3;
        let snapshot = control.snapshot();
        assert_eq!(snapshot.pending_sends, 3);
    }

    #[test]
    fn thumbnails_generated_adds_external_bytes() {
        let (_dir, endpoint, control) = test_control();
        let _server = IpcServer::start(endpoint.clone(), control.clone()).unwrap();
        let response = request(
            &endpoint,
            Request::ThumbnailsGenerated {
                files: 3,
                bytes: 4096,
            },
            Duration::from_secs(1),
        )
        .unwrap();
        assert!(matches!(response, Response::Ok));
        assert_eq!(
            control.thumbnail_external_bytes.load(Ordering::Relaxed),
            4096
        );
    }

    #[test]
    fn clear_thumbnail_cache_removes_files_and_resets_the_ledger() {
        let (_dir, endpoint, control) = test_control();
        let cache = control.thumbnail_dir.clone();
        std::fs::create_dir_all(&cache).unwrap();
        std::fs::write(cache.join("1-q80-s256.webp"), b"thumb").unwrap();
        std::fs::write(cache.join("2-q80-s256.webp.tmp"), b"tmp").unwrap();
        control
            .thumbnail_external_bytes
            .store(1234, Ordering::Relaxed);
        let _server = IpcServer::start(endpoint.clone(), control.clone()).unwrap();

        let response = request(
            &endpoint,
            Request::ClearThumbnailCache,
            Duration::from_secs(1),
        )
        .unwrap();

        match response {
            Response::ThumbnailCacheCleared {
                removed_files,
                removed_bytes,
                remaining_files,
                ..
            } => {
                assert_eq!(removed_files, 2);
                assert_eq!(removed_bytes, 8);
                assert_eq!(remaining_files, 0);
            }
            other => panic!("unexpected response: {other:?}"),
        }
        assert!(!cache.join("1-q80-s256.webp").exists());
        assert!(!cache.join("2-q80-s256.webp.tmp").exists());
        assert_eq!(control.thumbnail_external_bytes.load(Ordering::Relaxed), 0);
        assert_eq!(control.thumbnail_cache_epoch.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn shutdown_clears_running_and_wakes() {
        let (_dir, endpoint, control) = test_control();
        let _server = IpcServer::start(endpoint.clone(), control.clone()).unwrap();
        let response = request(&endpoint, Request::Shutdown, Duration::from_secs(1)).unwrap();
        assert!(matches!(response, Response::Ok));
        assert!(!control.running.load(Ordering::Relaxed));
        assert!(control.wake.is_pending());
    }

    #[test]
    fn disconnect_releases_the_subscription_without_an_event() {
        let (_dir, endpoint, control) = test_control();
        let _server = IpcServer::start(endpoint.clone(), control.clone()).unwrap();
        let (subscription, _) = Subscription::open(&endpoint, Duration::from_secs(1)).unwrap();
        // The sender is registered before the Subscribed response is
        // written, so it is already counted when open() returns.
        assert_eq!(control.events.subscribers.lock().unwrap().len(), 1);

        drop(subscription);
        // No event is published while the client is gone: the disconnect
        // alone must end the connection thread and drop its receiver, so
        // the next publish prunes the dead sender.
        thread::sleep(Duration::from_secs(2));
        control.events.publish(Event::StatusChanged {
            status: control.snapshot(),
        });
        assert_eq!(control.events.subscribers.lock().unwrap().len(), 0);
    }

    #[test]
    fn idle_subscription_receives_a_heartbeat() {
        let (_dir, endpoint, control) = test_control();
        let _server = IpcServer::start(endpoint.clone(), control.clone()).unwrap();
        let (mut subscription, _) = Subscription::open(&endpoint, Duration::from_secs(1)).unwrap();

        let event = subscription
            .next_event()
            .unwrap()
            .expect("the daemon still runs");
        assert!(
            matches!(event, Event::StatusChanged { .. }),
            "unexpected heartbeat: {event:?}"
        );
    }

    #[test]
    fn event_bus_bounds_slow_subscribers_and_prunes_dead_ones() {
        let bus = EventBus::default();
        let slow = bus.subscribe();
        let healthy = bus.subscribe();

        for roots in 0..(EVENT_QUEUE_CAPACITY + 32) {
            bus.publish(Event::ScanStarted {
                roots,
                target: None,
            });
        }

        // Publishing never blocks and never grows a subscriber's queue past
        // its capacity; the slow subscriber stays registered.
        assert_eq!(bus.subscribers.lock().unwrap().len(), 2);
        let mut drained = 0;
        while slow.try_recv().is_ok() {
            drained += 1;
        }
        assert_eq!(drained, EVENT_QUEUE_CAPACITY);
        while healthy.try_recv().is_ok() {}

        drop(slow);
        bus.publish(Event::StatusChanged {
            status: StatusSnapshot::default(),
        });
        assert_eq!(bus.subscribers.lock().unwrap().len(), 1);
    }

    #[test]
    fn connection_limit_rejects_excess_connections() {
        let (_dir, endpoint, control) = test_control();
        let _server = IpcServer::start_with_limit(endpoint.clone(), control, 2).unwrap();
        let (first, _) = Subscription::open(&endpoint, Duration::from_secs(1)).unwrap();
        let (second, _) = Subscription::open(&endpoint, Duration::from_secs(1)).unwrap();

        let error = request(&endpoint, Request::Status, Duration::from_secs(2)).unwrap_err();
        assert!(
            matches!(error, IpcError::Disconnected | IpcError::Io(_)),
            "unexpected excess-connection error: {error}"
        );

        drop(first);
        drop(second);
    }

    #[test]
    fn hello_and_status_use_the_cached_snapshot() {
        let (_dir, endpoint, control) = test_control();
        control.status.lock().unwrap().photo_count = 7;
        let _server = IpcServer::start(endpoint.clone(), control.clone()).unwrap();
        let (_subscription, info) = Subscription::open(&endpoint, Duration::from_secs(1)).unwrap();
        assert_eq!(info.status.photo_count, 7);
        let response = request(&endpoint, Request::Status, Duration::from_secs(1)).unwrap();
        match response {
            Response::Status { status } => assert_eq!(status.photo_count, 7),
            other => panic!("unexpected response: {other:?}"),
        }
    }

    #[test]
    fn wake_tagging_notifies_the_worker() {
        let (_dir, endpoint, control) = test_control();
        let _server = IpcServer::start(endpoint.clone(), control.clone()).unwrap();
        let before = control.tagging.generation();
        let response = request(&endpoint, Request::WakeTagging, Duration::from_secs(1)).unwrap();
        assert!(matches!(response, Response::Ok));
        assert!(control.tagging.generation() > before);
    }

    #[test]
    fn unknown_plugin_returns_not_found() {
        let (_dir, endpoint, control) = test_control();
        let _server = IpcServer::start(endpoint.clone(), control.clone()).unwrap();
        let response = request_raw(
            &endpoint,
            Request::ProbePlugin {
                plugin_id: "missing-plugin".to_string(),
            },
        );
        assert!(
            matches!(
                response,
                Response::Error {
                    code: ErrorCode::NotFound,
                    ..
                }
            ),
            "{response:?}"
        );
    }

    #[test]
    fn uninstall_plugin_returns_ok_and_removes_rows_in_the_background() {
        let (_dir, endpoint, control) = test_control();
        install_plugin_row(&control);
        let _server = IpcServer::start(endpoint.clone(), control.clone()).unwrap();

        let response = request(
            &endpoint,
            Request::UninstallPlugin {
                plugin_id: AI_PLUGIN_ID.to_string(),
                variant: None,
                version: None,
                delete_ai_tags: false,
            },
            Duration::from_secs(1),
        )
        .unwrap();
        assert!(matches!(response, Response::Ok));

        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let removed = control
                .db
                .lock()
                .unwrap()
                .active_plugin_installation(AI_PLUGIN_ID)
                .unwrap()
                .is_none();
            if removed {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "the uninstall worker did not remove the installation"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn model_download_requires_license_consent() {
        let (_dir, endpoint, control) = test_control();
        install_plugin_row(&control);
        let _server = IpcServer::start(endpoint.clone(), control.clone()).unwrap();
        let response = request_raw(
            &endpoint,
            Request::StartModelDownload {
                plugin_id: AI_PLUGIN_ID.to_string(),
            },
        );
        assert!(
            matches!(
                response,
                Response::Error {
                    code: ErrorCode::ConsentRequired,
                    ..
                }
            ),
            "{response:?}"
        );
    }

    #[test]
    fn install_plugin_returns_ok_and_emits_installed_progress() {
        let mut package = TestPackage::new();
        use_mock_sidecar(&mut package);
        let manifest = package.manifest().clone();
        let (_dir, endpoint, control) = test_control_with_keys(vec![package.public_key()]);
        let _server = IpcServer::start(endpoint.clone(), control.clone()).unwrap();
        let zip = package.temp_zip().unwrap();
        let events = control.events.subscribe();
        let response = request(
            &endpoint,
            Request::InstallPlugin {
                zip_path: zip.path().to_string_lossy().into_owned(),
            },
            Duration::from_secs(1),
        )
        .unwrap();
        assert!(matches!(response, Response::Ok));

        // Wait for the terminal event too, so the install worker cannot
        // outlive the temporary directory.
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match events.recv_timeout(remaining) {
                Ok(Event::PluginInstallProgress {
                    status: PluginInstallStatus::Installed,
                    ..
                }) => break,
                Ok(Event::PluginInstallProgress {
                    status: PluginInstallStatus::Failed,
                    error,
                    ..
                }) => panic!("plugin install failed: {error:?}"),
                Ok(_) => {}
                Err(error) => panic!("no install progress was published: {error}"),
            }
        }

        let db = control.db.lock().unwrap();
        let installs = db.list_plugin_installations(AI_PLUGIN_ID).unwrap();
        assert_eq!(installs.len(), 1);
        assert!(installs[0].active);
        assert_eq!(installs[0].variant, manifest.variant);
        assert_eq!(installs[0].version, manifest.plugin_version);
        assert!(
            db.active_plugin_installation(AI_PLUGIN_ID)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn cancel_tagging_cancels_db_and_token() {
        let (_dir, endpoint, control) = test_control();
        let job_id = add_tagging_job(&control, "cancel", TaggingJobStatus::Pending);
        let claimed = control
            .db
            .lock()
            .unwrap()
            .next_pending_tagging_job()
            .unwrap()
            .expect("claim the pending job");
        assert_eq!(claimed.id, job_id);
        let token = control.tagging_control.register(&job_id);
        let _server = IpcServer::start(endpoint.clone(), control.clone()).unwrap();

        let response = request(
            &endpoint,
            Request::CancelTagging {
                job_id: job_id.clone(),
            },
            Duration::from_secs(1),
        )
        .unwrap();
        assert!(matches!(response, Response::Ok));
        assert!(token.is_cancelled());
        let job = control
            .db
            .lock()
            .unwrap()
            .tagging_job(&job_id)
            .unwrap()
            .unwrap();
        assert_eq!(job.status, TaggingJobStatus::Cancelled);
    }

    #[test]
    fn status_includes_ai_counts() {
        let (_dir, endpoint, control) = test_control();
        mark_plugin_ready(&control);
        add_tagging_job(&control, "waiting", TaggingJobStatus::Waiting);
        add_tagging_job(&control, "pending", TaggingJobStatus::Pending);
        // The daemon refreshes these cached fields; snapshots do not query the
        // database on the IPC hot path.
        let (pending_tagging_jobs, tagging_ready) = tagging_status(&control.db, &control.plugin);
        {
            let mut status = control.status.lock().unwrap();
            status.pending_tagging_jobs = pending_tagging_jobs;
            status.tagging_ready = tagging_ready;
        }
        let _server = IpcServer::start(endpoint.clone(), control.clone()).unwrap();

        let response = request(&endpoint, Request::Status, Duration::from_secs(1)).unwrap();
        match response {
            Response::Status { status } => {
                assert_eq!(status.pending_tagging_jobs, 2);
                assert!(status.tagging_ready);
            }
            other => panic!("unexpected response: {other:?}"),
        }
    }
}
