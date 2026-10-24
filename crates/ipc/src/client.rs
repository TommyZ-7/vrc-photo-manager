use std::io::{self, BufReader};
use std::time::Duration;

use interprocess::ConnectWaitMode;
use interprocess::local_socket::{ConnectOptions, Stream, traits::Stream as _};

use crate::endpoint::Endpoint;
use crate::framing;
use crate::protocol::{
    ClientInfo, Event, Request, RequestEnvelope, Response, ServerMessage, StatusSnapshot,
};
use crate::{APP_VERSION, PROTOCOL_VERSION};

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
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// Connects to `endpoint` with a bounded wait.
pub(crate) fn connect_io(endpoint: &Endpoint, timeout: Duration) -> io::Result<Stream> {
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

    let stream = connect(endpoint, timeout)?;
    stream.set_recv_timeout(Some(timeout))?;
    stream.set_send_timeout(Some(timeout))?;
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
    ) -> Result<(Self, StatusSnapshot), IpcError> {
        let stream = connect(endpoint, timeout)?;
        stream.set_recv_timeout(Some(timeout))?;
        stream.set_send_timeout(Some(timeout))?;
        let mut reader = BufReader::new(stream);
        handshake(&mut reader)?;

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
                reader.get_mut().set_recv_timeout(None)?;
                Ok((Self { reader }, status))
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

    /// Reads the next event, or `None` when the daemon closes the connection.
    pub fn next_event(&mut self) -> Result<Option<Event>, IpcError> {
        match framing::read_message(&mut self.reader)? {
            Some(ServerMessage::Event { event }) => Ok(Some(event)),
            Some(ServerMessage::Response { .. }) => {
                Err(IpcError::Protocol("unexpected response".to_string()))
            }
            None => Ok(None),
        }
    }
}

fn handshake(reader: &mut BufReader<Stream>) -> Result<(), IpcError> {
    let id = 1;
    framing::write_message(
        reader.get_mut(),
        &RequestEnvelope {
            id,
            request: Request::Hello {
                protocol_version: PROTOCOL_VERSION,
                client: ClientInfo {
                    name: "vrc-photo-manager".to_string(),
                    version: APP_VERSION.to_string(),
                },
            },
        },
    )?;

    match read_server_message(reader)? {
        ServerMessage::Response {
            id: response_id,
            response: Response::Hello {
                protocol_version, ..
            },
        } if response_id == id => {
            if protocol_version == PROTOCOL_VERSION {
                Ok(())
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
    fn subscription_receives_events() {
        let dir = tempfile::tempdir().unwrap();
        let endpoint = Endpoint::for_tests(dir.path());
        let listener = server::Listener::bind(endpoint.clone()).unwrap();
        let event = Event::ScanStarted { roots: 2 };
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

        let (mut subscription, status) =
            Subscription::open(&endpoint, Duration::from_secs(1)).unwrap();
        assert_eq!(status.photo_count, 3);
        assert_eq!(subscription.next_event().unwrap(), Some(event));
        server.join().unwrap();
    }
}
