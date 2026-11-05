use std::collections::HashMap;
use std::io::{self, BufReader};
#[cfg(unix)]
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

use interprocess::ConnectWaitMode;
#[cfg(unix)]
use interprocess::local_socket::traits::Stream as _;
use interprocess::local_socket::{ConnectOptions, Stream};

use crate::endpoint::Endpoint;
use crate::framing;
use crate::protocol::{
    ClientInfo, Event, Request, RequestEnvelope, Response, ServerMessage, StatusSnapshot,
};
use crate::{APP_VERSION, PROTOCOL_VERSION};

/// Maximum number of blocking IPC request operations that may be in flight
/// per endpoint.
///
/// A hung daemon can hold every slot until its socket read fails, so callers
/// beyond the limit receive [`IpcError::Busy`] instead of leaking another
/// thread and file descriptor. The UI keeps its three-second database polling
/// fallback while requests are rejected this way.
const MAX_IN_FLIGHT_REQUESTS: usize = 8;

/// A subscription that sees no event for this long is treated as
/// disconnected. The daemon emits a `StatusChanged` heartbeat every ten
/// seconds, so this tolerates two missed beats before reconnecting.
///
/// Windows named pipes do not expose read timeouts through `interprocess`;
/// there a hung daemon can still leave `next_event` blocked until the pipe
/// closes. The daemon-side heartbeat and its send timeout bound the cases
/// where the connection itself is still writable.
#[cfg(all(unix, test))]
const SUBSCRIPTION_READ_TIMEOUT: Duration = Duration::from_millis(500);
#[cfg(all(unix, not(test)))]
const SUBSCRIPTION_READ_TIMEOUT: Duration = Duration::from_secs(30);

/// Errors returned by the IPC client.
#[derive(Debug, thiserror::Error)]
pub enum IpcError {
    #[error("protocol version mismatch: daemon {daemon}, client {client}")]
    ProtocolMismatch { daemon: u32, client: u32 },
    #[error("daemon error: {0}")]
    Daemon(String),
    #[error("protocol error: {0}")]
    Protocol(String),
    #[error("connection closed")]
    Disconnected,
    #[error("IPC request timed out")]
    Timeout,
    #[error("too many IPC requests are already in flight")]
    Busy,
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// A bounded counter for blocking IPC operations.
///
/// The guard is moved into the worker thread so it is released when the
/// operation actually finishes, even after the caller has already timed out.
struct InFlightLimiter {
    in_flight: AtomicUsize,
    limit: usize,
}

impl InFlightLimiter {
    const fn new(limit: usize) -> Self {
        Self {
            in_flight: AtomicUsize::new(0),
            limit,
        }
    }

    fn try_acquire(self: &Arc<Self>) -> Option<InFlightGuard> {
        if self.in_flight.fetch_add(1, Ordering::AcqRel) >= self.limit {
            self.in_flight.fetch_sub(1, Ordering::AcqRel);
            return None;
        }
        Some(InFlightGuard {
            limiter: self.clone(),
        })
    }

    #[cfg(test)]
    fn in_flight(&self) -> usize {
        self.in_flight.load(Ordering::Acquire)
    }
}

struct InFlightGuard {
    limiter: Arc<InFlightLimiter>,
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.limiter.in_flight.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Process-wide registry of per-endpoint limiters. One daemon cannot starve
/// another endpoint (and tests using isolated sockets do not share slots),
/// while every endpoint still gets a fixed ceiling.
static REQUEST_LIMITERS: OnceLock<Mutex<HashMap<String, Weak<InFlightLimiter>>>> = OnceLock::new();

fn request_limiter(endpoint: &Endpoint) -> Arc<InFlightLimiter> {
    let key = endpoint.display();
    let mut limiters = REQUEST_LIMITERS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap();
    if let Some(limiter) = limiters.get(&key).and_then(Weak::upgrade) {
        return limiter;
    }
    let limiter = Arc::new(InFlightLimiter::new(MAX_IN_FLIGHT_REQUESTS));
    limiters.insert(key, Arc::downgrade(&limiter));
    limiter
}

/// Connects to `endpoint` with a bounded wait.
pub(crate) fn connect_io(endpoint: &Endpoint, timeout: Duration) -> io::Result<Stream> {
    #[cfg(unix)]
    if endpoint.is_fallback()
        && let Some(parent) = endpoint.path().and_then(Path::parent)
    {
        // Never connect to a socket planted in an unsafe fallback
        // directory (symlink or owned by another user).
        crate::endpoint::verify_fallback_dir(parent)?;
    }
    let name = endpoint.name()?;
    ConnectOptions::new()
        .name(name)
        .wait_mode(ConnectWaitMode::Timeout(timeout))
        .connect_sync()
}

/// Connects to `endpoint` with a bounded wait.
pub fn connect(endpoint: &Endpoint, timeout: Duration) -> Result<Stream, IpcError> {
    connect_io(endpoint, timeout).map_err(IpcError::Io)
}

/// Sends one request after the version handshake and returns its response.
///
/// `Hello` and `Subscribe` are handled by the handshake and subscription
/// helpers instead.
pub fn request(
    endpoint: &Endpoint,
    request: Request,
    timeout: Duration,
) -> Result<Response, IpcError> {
    if matches!(&request, Request::Hello { .. } | Request::Subscribe) {
        return Err(IpcError::Protocol(
            "hello/subscribe must use the dedicated helper".to_string(),
        ));
    }

    let limiter = request_limiter(endpoint);
    let endpoint = endpoint.clone();
    with_timeout_on(limiter, timeout, move || {
        request_blocking(&endpoint, request)
    })
}

fn request_blocking(endpoint: &Endpoint, request: Request) -> Result<Response, IpcError> {
    let stream = connect(endpoint, Duration::from_secs(2))?;
    let mut reader = BufReader::new(stream);
    handshake(&mut reader)?;

    let id = 2;
    framing::write_message(reader.get_mut(), &RequestEnvelope { id, request })?;
    match read_server_message(&mut reader)? {
        ServerMessage::Response {
            id: response_id,
            response,
        } if response_id == id => match response {
            Response::Error { message, .. } => Err(IpcError::Daemon(message)),
            response => Ok(response),
        },
        ServerMessage::Response { .. } => {
            Err(IpcError::Protocol("response id mismatch".to_string()))
        }
        ServerMessage::Event { .. } => Err(IpcError::Protocol("unexpected event".to_string())),
    }
}

/// Identity and status returned by the subscription handshake.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubscriptionInfo {
    pub status: StatusSnapshot,
    pub daemon_pid: u32,
    pub daemon_version: String,
}

/// A live event subscription.
#[derive(Debug)]
pub struct Subscription {
    reader: BufReader<Stream>,
}

impl Subscription {
    /// Performs the handshake and starts an event subscription.
    pub fn open(
        endpoint: &Endpoint,
        timeout: Duration,
    ) -> Result<(Self, SubscriptionInfo), IpcError> {
        let limiter = request_limiter(endpoint);
        let endpoint = endpoint.clone();
        with_timeout_on(limiter, timeout, move || {
            subscription_open_blocking(&endpoint)
        })
    }

    /// Reads the next event, or `None` when the daemon closes the connection.
    ///
    /// On Unix an idle subscription that receives no event within 30 seconds
    /// returns [`IpcError::Disconnected`] so the bridge can reconnect instead
    /// of staying blocked forever. Windows named pipes have no equivalent
    /// read timeout, so there a hung daemon can leave `next_event` blocked
    /// until the pipe closes.
    pub fn next_event(&mut self) -> Result<Option<Event>, IpcError> {
        let message = framing::read_message(&mut self.reader).map_err(|error| {
            if is_read_timeout(&error) {
                IpcError::Disconnected
            } else {
                IpcError::Io(error)
            }
        })?;
        match message {
            Some(ServerMessage::Event { event }) => Ok(Some(event)),
            Some(ServerMessage::Response { .. }) => {
                Err(IpcError::Protocol("unexpected response".to_string()))
            }
            None => Ok(None),
        }
    }
}

fn is_read_timeout(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
    )
}

fn subscription_open_blocking(
    endpoint: &Endpoint,
) -> Result<(Subscription, SubscriptionInfo), IpcError> {
    let stream = connect(endpoint, Duration::from_secs(2))?;
    let mut reader = BufReader::new(stream);
    let hello = handshake(&mut reader)?;

    let id = 2;
    framing::write_message(
        reader.get_mut(),
        &RequestEnvelope {
            id,
            request: Request::Subscribe,
        },
    )?;
    match read_server_message(&mut reader)? {
        ServerMessage::Response {
            id: response_id,
            response: Response::Subscribed { status },
        } if response_id == id => {
            #[cfg(unix)]
            reader
                .get_mut()
                .set_recv_timeout(Some(SUBSCRIPTION_READ_TIMEOUT))?;
            Ok((
                Subscription { reader },
                SubscriptionInfo {
                    status,
                    daemon_pid: hello.daemon_pid,
                    daemon_version: hello.daemon_version,
                },
            ))
        }
        ServerMessage::Response {
            response: Response::Error { message, .. },
            ..
        } => Err(IpcError::Daemon(message)),
        _ => Err(IpcError::Protocol(
            "unexpected subscription response".to_string(),
        )),
    }
}

/// Runs `operation` on a worker thread with a caller deadline and a bounded
/// number of concurrent workers.
fn with_timeout_on<T, F>(
    limiter: Arc<InFlightLimiter>,
    timeout: Duration,
    operation: F,
) -> Result<T, IpcError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, IpcError> + Send + 'static,
{
    let guard = limiter.try_acquire().ok_or(IpcError::Busy)?;
    let (sender, receiver) = mpsc::channel();
    // The guard lives in the worker. A timed-out caller drops its receiver
    // but the slot stays reserved until the blocking operation returns, so a
    // hung daemon can never accumulate more than `limit` abandoned workers.
    std::thread::Builder::new()
        .name("ipc-request".to_string())
        .spawn(move || {
            let _guard = guard;
            let _ = sender.send(operation());
        })?;
    match receiver.recv_timeout(timeout) {
        Ok(result) => result,
        Err(_) => Err(IpcError::Timeout),
    }
}

#[derive(Debug)]
struct HelloInfo {
    daemon_pid: u32,
    daemon_version: String,
}

fn handshake(reader: &mut BufReader<Stream>) -> Result<HelloInfo, IpcError> {
    let id = 1;
    framing::write_message(
        reader.get_mut(),
        &RequestEnvelope {
            id,
            request: Request::Hello {
                protocol_version: PROTOCOL_VERSION,
                client: ClientInfo {
                    name: "vrc-photon".to_string(),
                    version: APP_VERSION.to_string(),
                },
            },
        },
    )?;

    match read_server_message(reader)? {
        ServerMessage::Response {
            id: response_id,
            response:
                Response::Hello {
                    protocol_version,
                    daemon_version,
                    pid,
                    ..
                },
        } if response_id == id => {
            if protocol_version == PROTOCOL_VERSION {
                Ok(HelloInfo {
                    daemon_pid: pid,
                    daemon_version,
                })
            } else {
                Err(IpcError::ProtocolMismatch {
                    daemon: protocol_version,
                    client: PROTOCOL_VERSION,
                })
            }
        }
        ServerMessage::Response {
            response: Response::Error { message, .. },
            ..
        } => Err(IpcError::Daemon(message)),
        _ => Err(IpcError::Protocol(
            "unexpected handshake response".to_string(),
        )),
    }
}

fn read_server_message(reader: &mut BufReader<Stream>) -> Result<ServerMessage, IpcError> {
    framing::read_message(reader)?.ok_or(IpcError::Disconnected)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::StatusSnapshot;
    use crate::server;
    use interprocess::TryClone;
    use std::sync::{Arc, Mutex};
    use std::time::Instant;

    fn hello_response(id: u64, protocol_version: u32) -> ServerMessage {
        ServerMessage::Response {
            id,
            response: Response::Hello {
                protocol_version,
                daemon_version: "test".to_string(),
                pid: 1,
                status: StatusSnapshot::default(),
            },
        }
    }

    #[test]
    fn request_round_trips_over_a_local_socket() {
        let dir = tempfile::tempdir().unwrap();
        let endpoint = Endpoint::for_tests(dir.path());
        let listener = server::Listener::bind(endpoint.clone()).unwrap();
        let server = std::thread::spawn(move || {
            let mut stream = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let hello: RequestEnvelope = framing::read_message(&mut reader).unwrap().unwrap();
            assert!(matches!(&hello.request, Request::Hello { .. }));
            framing::write_message(&mut stream, &hello_response(hello.id, PROTOCOL_VERSION))
                .unwrap();
            let request: RequestEnvelope = framing::read_message(&mut reader).unwrap().unwrap();
            assert!(matches!(&request.request, Request::WakeCommands));
            framing::write_message(
                &mut stream,
                &ServerMessage::Response {
                    id: request.id,
                    response: Response::Ok,
                },
            )
            .unwrap();
        });

        let response = request(&endpoint, Request::WakeCommands, Duration::from_secs(1)).unwrap();
        assert!(matches!(response, Response::Ok));
        server.join().unwrap();
    }

    #[test]
    fn protocol_mismatch_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let endpoint = Endpoint::for_tests(dir.path());
        let listener = server::Listener::bind(endpoint.clone()).unwrap();
        let server = std::thread::spawn(move || {
            let mut stream = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let hello: RequestEnvelope = framing::read_message(&mut reader).unwrap().unwrap();
            framing::write_message(&mut stream, &hello_response(hello.id, PROTOCOL_VERSION + 1))
                .unwrap();
        });

        let error = request(&endpoint, Request::Status, Duration::from_secs(1)).unwrap_err();
        match error {
            IpcError::ProtocolMismatch { daemon, client } => {
                assert_eq!(daemon, PROTOCOL_VERSION + 1);
                assert_eq!(client, PROTOCOL_VERSION);
            }
            other => panic!("unexpected error: {other}"),
        }
        server.join().unwrap();
    }

    #[test]
    fn request_times_out_when_the_server_does_not_reply() {
        let dir = tempfile::tempdir().unwrap();
        let endpoint = Endpoint::for_tests(dir.path());
        let listener = server::Listener::bind(endpoint.clone()).unwrap();
        let server = std::thread::spawn(move || {
            let _stream = listener.accept().unwrap();
            std::thread::sleep(Duration::from_millis(300));
        });

        let error = request(&endpoint, Request::Status, Duration::from_millis(100)).unwrap_err();
        assert!(matches!(error, IpcError::Timeout));
        server.join().unwrap();
    }

    #[test]
    fn subscription_receives_events() {
        let dir = tempfile::tempdir().unwrap();
        let endpoint = Endpoint::for_tests(dir.path());
        let listener = server::Listener::bind(endpoint.clone()).unwrap();
        let event = Event::ScanStarted {
            roots: 2,
            target: None,
        };
        let event_for_server = event.clone();
        let server = std::thread::spawn(move || {
            let mut stream = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let hello: RequestEnvelope = framing::read_message(&mut reader).unwrap().unwrap();
            framing::write_message(&mut stream, &hello_response(hello.id, PROTOCOL_VERSION))
                .unwrap();
            let subscribe: RequestEnvelope = framing::read_message(&mut reader).unwrap().unwrap();
            assert!(matches!(&subscribe.request, Request::Subscribe));
            framing::write_message(
                &mut stream,
                &ServerMessage::Response {
                    id: subscribe.id,
                    response: Response::Subscribed {
                        status: StatusSnapshot {
                            photo_count: 3,
                            pending_sends: 0,
                            ..StatusSnapshot::default()
                        },
                    },
                },
            )
            .unwrap();
            framing::write_message(
                &mut stream,
                &ServerMessage::Event {
                    event: event_for_server,
                },
            )
            .unwrap();
        });

        let (mut subscription, info) =
            Subscription::open(&endpoint, Duration::from_secs(1)).unwrap();
        assert_eq!(info.status.photo_count, 3);
        assert_eq!(info.daemon_pid, 1);
        assert_eq!(info.daemon_version, "test");
        assert_eq!(subscription.next_event().unwrap(), Some(event));
        server.join().unwrap();
    }

    #[test]
    fn request_limit_returns_busy_and_releases_when_workers_finish() {
        let limiter = Arc::new(InFlightLimiter::new(2));
        let (release_tx, release_rx) = mpsc::channel();
        let release_rx = Arc::new(Mutex::new(release_rx));

        let handles: Vec<_> = (0..2)
            .map(|_| {
                let release_rx = release_rx.clone();
                let limiter = limiter.clone();
                std::thread::spawn(move || {
                    with_timeout_on(limiter, Duration::from_secs(5), move || {
                        let _ = release_rx.lock().unwrap().recv();
                        Ok::<(), IpcError>(())
                    })
                })
            })
            .collect();

        let deadline = Instant::now() + Duration::from_secs(1);
        while limiter.in_flight() < 2 {
            assert!(Instant::now() < deadline, "workers did not start");
            std::thread::sleep(Duration::from_millis(5));
        }

        let error = with_timeout_on(limiter.clone(), Duration::from_secs(1), || {
            Ok::<(), IpcError>(())
        })
        .unwrap_err();
        assert!(matches!(error, IpcError::Busy), "{error}");

        release_tx.send(()).unwrap();
        release_tx.send(()).unwrap();
        for handle in handles {
            handle.join().unwrap().unwrap();
        }
        assert_eq!(limiter.in_flight(), 0);
    }

    #[test]
    fn timed_out_worker_keeps_its_slot_until_the_operation_finishes() {
        let limiter = Arc::new(InFlightLimiter::new(1));
        let (release_tx, release_rx) = mpsc::channel();
        let release_rx = Arc::new(Mutex::new(release_rx));

        let caller_limiter = limiter.clone();
        let caller = std::thread::spawn(move || {
            with_timeout_on(caller_limiter, Duration::from_millis(50), move || {
                let _ = release_rx.lock().unwrap().recv();
                Ok::<(), IpcError>(())
            })
        });

        let error = caller.join().unwrap().unwrap_err();
        assert!(matches!(error, IpcError::Timeout), "{error}");
        // The caller timed out, but the abandoned worker still owns the slot
        // until its blocking operation returns.
        assert_eq!(limiter.in_flight(), 1);

        release_tx.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        while limiter.in_flight() > 0 {
            assert!(Instant::now() < deadline, "the worker did not release");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[cfg(unix)]
    #[test]
    fn idle_subscription_times_out_and_reports_disconnect() {
        let dir = tempfile::tempdir().unwrap();
        let endpoint = Endpoint::for_tests(dir.path());
        let listener = server::Listener::bind(endpoint.clone()).unwrap();
        let (hold_tx, hold_rx) = mpsc::channel::<()>();
        let server = std::thread::spawn(move || {
            let mut stream = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let hello: RequestEnvelope = framing::read_message(&mut reader).unwrap().unwrap();
            framing::write_message(&mut stream, &hello_response(hello.id, PROTOCOL_VERSION))
                .unwrap();
            let subscribe: RequestEnvelope = framing::read_message(&mut reader).unwrap().unwrap();
            assert!(matches!(&subscribe.request, Request::Subscribe));
            framing::write_message(
                &mut stream,
                &ServerMessage::Response {
                    id: subscribe.id,
                    response: Response::Subscribed {
                        status: StatusSnapshot::default(),
                    },
                },
            )
            .unwrap();
            // Keep the connection open without sending any event, so only
            // the client's read timeout can end it.
            let _ = hold_rx.recv();
        });

        let (mut subscription, _) = Subscription::open(&endpoint, Duration::from_secs(1)).unwrap();
        let result = subscription.next_event();
        assert!(
            matches!(result, Err(IpcError::Disconnected)),
            "unexpected idle result: {result:?}"
        );
        drop(hold_tx);
        server.join().unwrap();
    }
}
