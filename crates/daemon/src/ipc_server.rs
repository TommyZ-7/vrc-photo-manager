use std::io::{self, BufReader, Read};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use interprocess::TryClone as _;
#[cfg(unix)]
use interprocess::local_socket::traits::Stream as _;
use vrc_photon_ipc::framing;
use vrc_photon_ipc::server::Listener;
use vrc_photon_ipc::{
    APP_VERSION, Endpoint, ErrorCode, Event, PROTOCOL_VERSION, Request, RequestEnvelope, Response,
    ServerMessage, StatusSnapshot,
};

use crate::DaemonStatus;
use crate::signals::{JobSignal, WakeSignal};

/// Fan-out for daemon events. Dead subscribers are pruned on publish.
#[derive(Clone, Default)]
pub(crate) struct EventBus {
    subscribers: Arc<Mutex<Vec<Sender<vrc_photon_ipc::Event>>>>,
}

impl EventBus {
    pub(crate) fn publish(&self, event: vrc_photon_ipc::Event) {
        let mut subscribers = self.subscribers.lock().unwrap();
        subscribers.retain(|sender| sender.send(event.clone()).is_ok());
    }

    pub(crate) fn subscribe(&self) -> Receiver<vrc_photon_ipc::Event> {
        let (sender, receiver) = mpsc::channel();
        self.subscribers.lock().unwrap().push(sender);
        receiver
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
    }
}

/// The listening IPC server. Dropping it stops the accept thread and removes
/// the Unix socket.
pub(crate) struct IpcServer {
    stop: Arc<AtomicBool>,
    accept: Option<JoinHandle<()>>,
}

impl IpcServer {
    pub(crate) fn start(endpoint: Endpoint, control: IpcControl) -> io::Result<Self> {
        let listener = Listener::bind(endpoint)?;
        listener.set_nonblocking_accept(true)?;
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let accept = thread::Builder::new()
            .name("ipc-server".to_string())
            .spawn(move || accept_loop(listener, control, thread_stop))?;
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

fn accept_loop(listener: Listener, control: IpcControl, stop: Arc<AtomicBool>) {
    while !stop.load(Ordering::Relaxed) && control.running.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok(stream) => {
                let control = control.clone();
                if let Err(error) = thread::Builder::new()
                    .name("ipc-connection".to_string())
                    .spawn(move || {
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
        }
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
    while control.running.load(Ordering::Relaxed) && !disconnected.load(Ordering::Relaxed) {
        match events.recv_timeout(Duration::from_millis(100)) {
            Ok(event) => {
                if let Err(error) =
                    framing::write_message(reader.get_mut(), &ServerMessage::Event { event })
                {
                    result = Err(error);
                    break;
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
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
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use vrc_photon_ipc::client::{Subscription, request};
    use vrc_photon_ipc::{Endpoint, Event, Request, Response};

    fn test_control() -> (tempfile::TempDir, Endpoint, IpcControl) {
        let dir = tempfile::tempdir().unwrap();
        let endpoint = Endpoint::for_tests(dir.path());
        let control = IpcControl {
            running: Arc::new(AtomicBool::new(true)),
            status: Arc::new(Mutex::new(DaemonStatus::default())),
            wake: Arc::new(WakeSignal::default()),
            jobs: Arc::new(JobSignal::default()),
            sends: Arc::new(JobSignal::default()),
            events: EventBus::default(),
            thumbnail_dir: dir.path().join("thumbnails"),
            thumbnail_cache_epoch: Arc::new(AtomicU64::new(0)),
            thumbnail_external_bytes: Arc::new(AtomicU64::new(0)),
        };
        (dir, endpoint, control)
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
}
