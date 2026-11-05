use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use vrc_photon_i18n::{tr, tr_args};
use vrc_photon_ipc::client::{self, IpcError, Subscription};
use vrc_photon_ipc::{Endpoint, Event, Request, Response, StatusSnapshot};

/// Bound for the bridge-to-UI queue. The bridge applies backpressure once
/// the UI falls this far behind instead of growing without limit.
pub const INCOMING_QUEUE_CAPACITY: usize = 256;

/// First reconnection delay and the cap after exponential growth.
const BACKOFF_INITIAL: Duration = Duration::from_secs(2);
const BACKOFF_MAX: Duration = Duration::from_secs(30);
/// Jitter applied around the nominal delay (±20%).
const BACKOFF_JITTER_PERCENT: u64 = 20;

/// Messages sent from the IPC subscription thread to the GPUI task.
#[derive(Clone, Debug, PartialEq)]
pub enum Incoming {
    Connected {
        status: StatusSnapshot,
        daemon_pid: u32,
    },
    Disconnected(String),
    Event(Event),
}

/// Starts the reconnecting subscription pump. The thread exits when the
/// receiver is dropped.
pub fn start(endpoint: Endpoint, tx: async_channel::Sender<Incoming>) {
    if let Err(error) = thread::Builder::new()
        .name("ipc-bridge".to_string())
        .spawn(move || bridge_loop(endpoint, tx))
    {
        tracing::warn!(%error, "cannot spawn IPC bridge thread");
    }
}

/// Sends one request with the short UI timeout.
pub fn request(endpoint: Endpoint, request: Request) -> Result<Response, IpcError> {
    client::request(&endpoint, request, Duration::from_secs(2))
}

/// Converts an IPC failure into a user-facing message.
pub fn error_message(error: &IpcError) -> String {
    match error {
        IpcError::ProtocolMismatch { daemon, client } => {
            tr_args!("ipc.error.protocol", daemon = daemon, client = client)
        }
        IpcError::Timeout => tr!("ipc.error.timeout"),
        IpcError::Busy => tr!("ipc.error.busy"),
        IpcError::Disconnected => tr!("ipc.error.disconnected"),
        other => tr_args!("ipc.error.connect", error = other),
    }
}

/// Nominal reconnection delay before jitter: 2s, 4s, 8s, 16s, then 30s.
fn backoff_base(attempt: u32) -> Duration {
    let factor = 1u64 << attempt.min(5);
    BACKOFF_INITIAL
        .saturating_mul(factor as u32)
        .min(BACKOFF_MAX)
}

/// Exponential backoff with ±20% jitter. `jitter_sample` is mapped onto
/// 0..=1000, where 500 is the un-jittered nominal delay. The result never
/// exceeds `BACKOFF_MAX`.
fn backoff_delay(attempt: u32, jitter_sample: u64) -> Duration {
    let base = backoff_base(attempt);
    let spread_ms = (base.as_millis() as u64 * BACKOFF_JITTER_PERCENT / 100) as i64;
    let offset = if spread_ms == 0 {
        0
    } else {
        (jitter_sample % 1001) as i64 * 2 * spread_ms / 1000 - spread_ms
    };
    let millis = (base.as_millis() as i64 + offset).max(0) as u64;
    Duration::from_millis(millis).min(BACKOFF_MAX)
}

fn jitter_sample() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos() as u64)
        .unwrap_or(0)
}

fn bridge_loop(endpoint: Endpoint, tx: async_channel::Sender<Incoming>) {
    let mut attempt: u32 = 0;
    let mut announced_disconnect = false;
    loop {
        match Subscription::open(&endpoint, Duration::from_millis(500)) {
            Ok((mut subscription, info)) => {
                // A successful connection resets the backoff sequence.
                attempt = 0;
                if tx
                    .send_blocking(Incoming::Connected {
                        status: info.status,
                        daemon_pid: info.daemon_pid,
                    })
                    .is_err()
                {
                    return;
                }
                let message = loop {
                    match subscription.next_event() {
                        Ok(Some(event)) => {
                            if tx.send_blocking(Incoming::Event(event)).is_err() {
                                return;
                            }
                        }
                        Ok(None) => break tr!("ipc.error.disconnected"),
                        Err(error) => break error_message(&error),
                    }
                };
                if tx.send_blocking(Incoming::Disconnected(message)).is_err() {
                    return;
                }
                announced_disconnect = true;
                // Retry an established-then-lost link at the initial delay;
                // repeated open failures grow the delay.
                thread::sleep(backoff_delay(attempt, jitter_sample()));
                attempt = attempt.saturating_add(1);
            }
            Err(error) => {
                // Notify only on the transition into a disconnected state,
                // not on every failed retry.
                if !announced_disconnect {
                    if tx
                        .send_blocking(Incoming::Disconnected(error_message(&error)))
                        .is_err()
                    {
                        return;
                    }
                    announced_disconnect = true;
                }
                thread::sleep(backoff_delay(attempt, jitter_sample()));
                attempt = attempt.saturating_add(1);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufReader;
    use std::time::Duration;
    use vrc_photon_ipc::server::Listener;
    use vrc_photon_ipc::{
        Event, Request, RequestEnvelope, Response, ServerMessage, StatusSnapshot, framing,
    };

    fn hello_response(id: u64) -> ServerMessage {
        ServerMessage::Response {
            id,
            response: Response::Hello {
                protocol_version: vrc_photon_ipc::PROTOCOL_VERSION,
                daemon_version: "test".to_string(),
                pid: 1,
                status: StatusSnapshot::default(),
            },
        }
    }

    #[test]
    fn bridge_delivers_connected_then_event() {
        let dir = tempfile::tempdir().unwrap();
        let endpoint = vrc_photon_ipc::Endpoint::for_tests(dir.path());
        let listener = Listener::bind(endpoint.clone()).unwrap();
        let server = std::thread::spawn(move || {
            let stream = listener.accept().unwrap();
            let mut reader = BufReader::new(stream);
            let hello: RequestEnvelope = framing::read_message(&mut reader).unwrap().unwrap();
            framing::write_message(reader.get_mut(), &hello_response(hello.id)).unwrap();
            let subscribe: RequestEnvelope = framing::read_message(&mut reader).unwrap().unwrap();
            assert!(matches!(&subscribe.request, Request::Subscribe));
            framing::write_message(
                reader.get_mut(),
                &ServerMessage::Response {
                    id: subscribe.id,
                    response: Response::Subscribed {
                        status: StatusSnapshot {
                            photo_count: 5,
                            pending_sends: 0,
                            ..StatusSnapshot::default()
                        },
                    },
                },
            )
            .unwrap();
            framing::write_message(
                reader.get_mut(),
                &ServerMessage::Event {
                    event: Event::ScanStarted {
                        roots: 1,
                        target: None,
                    },
                },
            )
            .unwrap();
            std::thread::sleep(Duration::from_millis(300));
        });

        let (tx, rx) = async_channel::unbounded();
        start(endpoint, tx);
        match rx.recv_blocking().unwrap() {
            Incoming::Connected { status, daemon_pid } => {
                assert_eq!(status.photo_count, 5);
                assert_eq!(daemon_pid, 1);
            }
            other => panic!("expected connected, got {other:?}"),
        }
        match rx.recv_blocking().unwrap() {
            Incoming::Event(Event::ScanStarted { roots, .. }) => assert_eq!(roots, 1),
            other => panic!("expected scan event, got {other:?}"),
        }
        drop(rx);
        server.join().unwrap();
    }

    #[test]
    fn protocol_mismatch_reports_the_update_message() {
        let dir = tempfile::tempdir().unwrap();
        let endpoint = vrc_photon_ipc::Endpoint::for_tests(dir.path());
        let listener = Listener::bind(endpoint.clone()).unwrap();
        let server = std::thread::spawn(move || {
            let stream = listener.accept().unwrap();
            let mut reader = BufReader::new(stream);
            let hello: RequestEnvelope = framing::read_message(&mut reader).unwrap().unwrap();
            framing::write_message(
                reader.get_mut(),
                &ServerMessage::Response {
                    id: hello.id,
                    response: Response::Hello {
                        protocol_version: vrc_photon_ipc::PROTOCOL_VERSION + 1,
                        daemon_version: "test".to_string(),
                        pid: 1,
                        status: StatusSnapshot::default(),
                    },
                },
            )
            .unwrap();
        });

        let (tx, rx) = async_channel::unbounded();
        start(endpoint, tx);
        match rx.recv_blocking().unwrap() {
            Incoming::Disconnected(error) => {
                assert!(error.contains("デーモンの更新が必要です"), "{error}");
            }
            other => panic!("expected protocol mismatch, got {other:?}"),
        }
        server.join().unwrap();
    }

    #[test]
    fn request_round_trips_and_maps_daemon_errors() {
        let dir = tempfile::tempdir().unwrap();
        let endpoint = vrc_photon_ipc::Endpoint::for_tests(dir.path());
        let listener = Listener::bind(endpoint.clone()).unwrap();
        let server = std::thread::spawn(move || {
            for response in [
                Response::Ok,
                Response::Error {
                    code: vrc_photon_ipc::ErrorCode::InvalidRequest,
                    message: "nope".to_string(),
                },
            ] {
                let stream = listener.accept().unwrap();
                let mut reader = BufReader::new(stream);
                let hello: RequestEnvelope = framing::read_message(&mut reader).unwrap().unwrap();
                framing::write_message(reader.get_mut(), &hello_response(hello.id)).unwrap();
                let request: RequestEnvelope = framing::read_message(&mut reader).unwrap().unwrap();
                framing::write_message(
                    reader.get_mut(),
                    &ServerMessage::Response {
                        id: request.id,
                        response,
                    },
                )
                .unwrap();
            }
        });

        assert!(matches!(
            request(endpoint.clone(), Request::WakeCommands).unwrap(),
            Response::Ok
        ));
        let error = request(endpoint, Request::Status).unwrap_err();
        assert!(matches!(error, IpcError::Daemon(_)));
        server.join().unwrap();
    }

    #[test]
    fn bridge_reports_disconnected_without_a_daemon() {
        let dir = tempfile::tempdir().unwrap();
        let endpoint = vrc_photon_ipc::Endpoint::for_tests(dir.path());
        let (tx, rx) = async_channel::unbounded();
        start(endpoint, tx);
        match rx.recv_blocking().unwrap() {
            Incoming::Disconnected(error) => assert!(!error.is_empty()),
            other => panic!("expected disconnected, got {other:?}"),
        }
    }

    #[test]
    fn reconnect_backoff_grows_caps_and_jitters_around_the_nominal_delay() {
        let nominal: Vec<u64> = (0..6)
            .map(|attempt| backoff_base(attempt).as_secs())
            .collect();
        assert_eq!(nominal, vec![2, 4, 8, 16, 30, 30]);

        for attempt in 0..10 {
            let base = backoff_base(attempt);
            let spread =
                Duration::from_millis(base.as_millis() as u64 * BACKOFF_JITTER_PERCENT / 100);
            assert_eq!(backoff_delay(attempt, 500), base);
            for sample in [0, 1, 250, 750, 999, 1000, u64::MAX] {
                let delay = backoff_delay(attempt, sample);
                assert!(delay <= BACKOFF_MAX, "{attempt}/{sample}: {delay:?}");
                assert!(
                    delay + spread >= base,
                    "{attempt}/{sample}: {delay:?} is below {base:?} - {spread:?}"
                );
                assert!(
                    delay <= (base + spread).min(BACKOFF_MAX),
                    "{attempt}/{sample}: {delay:?} is above {base:?} + {spread:?}"
                );
            }
        }
    }
}
