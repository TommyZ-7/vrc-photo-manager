use std::io::{self, BufReader};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use interprocess::local_socket::traits::Stream as _;
use vrc_photo_ipc::framing;
use vrc_photo_ipc::server::Listener;
use vrc_photo_ipc::{
    APP_VERSION, Endpoint, ErrorCode, Event, PROTOCOL_VERSION, Request, RequestEnvelope, Response,
    ServerMessage, StatusSnapshot,
};

use crate::DaemonStatus;
use crate::signals::{JobSignal, WakeSignal};

/// Fan-out for daemon events. Dead subscribers are pruned on publish.
#[derive(Clone, Default)]
pub(crate) struct EventBus {
    subscribers: Arc<Mutex<Vec<Sender<vrc_photo_ipc::Event>>>>,
}

impl EventBus {
    pub(crate) fn publish(&self, event: vrc_photo_ipc::Event) {
        let mut subscribers = self.subscribers.lock().unwrap();
        subscribers.retain(|sender| sender.send(event.clone()).is_ok());
    }

    pub(crate) fn subscribe(&self) -> Receiver<vrc_photo_ipc::Event> {
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
    pub(crate) events: EventBus,
}

impl IpcControl {
    fn snapshot(&self) -> StatusSnapshot {
        status_snapshot(&self.status.lock().unwrap())
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

pub(crate) fn status_snapshot(status: &DaemonStatus) -> StatusSnapshot {
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

fn handle_connection(stream: vrc_photo_ipc::server::Stream, control: IpcControl) -> io::Result<()> {
    let mut reader = BufReader::new(stream);
    reader
        .get_mut()
        .set_recv_timeout(Some(Duration::from_secs(5)))?;
    reader
        .get_mut()
        .set_send_timeout(Some(Duration::from_secs(5)))?;

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
            Request::SetPaused { paused } => {
                control.set_paused(paused);
                write_response(reader.get_mut(), id, Response::Ok)?;
            }
            Request::Shutdown => {
                write_response(reader.get_mut(), id, Response::Ok)?;
                control.shutdown();
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
    reader: &mut BufReader<vrc_photo_ipc::server::Stream>,
    control: &IpcControl,
    events: Receiver<vrc_photo_ipc::Event>,
) -> io::Result<()> {
    while control.running.load(Ordering::Relaxed) {
        match events.recv_timeout(Duration::from_millis(100)) {
            Ok(event) => {
                framing::write_message(reader.get_mut(), &ServerMessage::Event { event })?;
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    Ok(())
}

fn write_response(
    stream: &mut vrc_photo_ipc::server::Stream,
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
    use vrc_photo_ipc::client::{Subscription, request};
    use vrc_photo_ipc::{Endpoint, Event, Request, Response};

    fn test_control() -> (tempfile::TempDir, Endpoint, IpcControl) {
        let dir = tempfile::tempdir().unwrap();
        let endpoint = Endpoint::for_tests(dir.path());
        let control = IpcControl {
            running: Arc::new(AtomicBool::new(true)),
            status: Arc::new(Mutex::new(DaemonStatus::default())),
            wake: Arc::new(WakeSignal::default()),
            jobs: Arc::new(JobSignal::default()),
            events: EventBus::default(),
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
    fn shutdown_clears_running_and_wakes() {
        let (_dir, endpoint, control) = test_control();
        let _server = IpcServer::start(endpoint.clone(), control.clone()).unwrap();
        let response = request(&endpoint, Request::Shutdown, Duration::from_secs(1)).unwrap();
        assert!(matches!(response, Response::Ok));
        assert!(!control.running.load(Ordering::Relaxed));
        assert!(control.wake.is_pending());
    }

    #[test]
    fn hello_and_status_use_the_cached_snapshot() {
        let (_dir, endpoint, control) = test_control();
        control.status.lock().unwrap().photo_count = 7;
        let _server = IpcServer::start(endpoint.clone(), control.clone()).unwrap();
        let (_subscription, status) =
            Subscription::open(&endpoint, Duration::from_secs(1)).unwrap();
        assert_eq!(status.photo_count, 7);
        let response = request(&endpoint, Request::Status, Duration::from_secs(1)).unwrap();
        match response {
            Response::Status { status } => assert_eq!(status.photo_count, 7),
            other => panic!("unexpected response: {other:?}"),
        }
    }
}
