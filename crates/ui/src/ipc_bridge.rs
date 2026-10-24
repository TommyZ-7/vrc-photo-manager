use std::thread;
use std::time::Duration;

use vrc_photo_ipc::client::{self, IpcError, Subscription};
use vrc_photo_ipc::{Endpoint, Event, Request, Response, StatusSnapshot};

/// Messages sent from the IPC subscription thread to the GPUI task.
#[derive(Clone, Debug, PartialEq)]
pub enum Incoming {
    Connected(StatusSnapshot),
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

fn bridge_loop(endpoint: Endpoint, tx: async_channel::Sender<Incoming>) {
    loop {
        match Subscription::open(&endpoint, Duration::from_millis(500)) {
            Ok((mut subscription, status)) => {
                if tx.send_blocking(Incoming::Connected(status)).is_err() {
                    return;
                }
                loop {
                    match subscription.next_event() {
                        Ok(Some(event)) => {
                            if tx.send_blocking(Incoming::Event(event)).is_err() {
                                return;
                            }
                        }
                        Ok(None) => {
                            if tx
                                .send_blocking(Incoming::Disconnected(
                                    "デーモンとの接続が終了しました".to_string(),
                                ))
                                .is_err()
                            {
                                return;
                            }
                            break;
                        }
                        Err(error) => {
                            if tx
                                .send_blocking(Incoming::Disconnected(error.to_string()))
                                .is_err()
                            {
                                return;
                            }
                            break;
                        }
                    }
                }
            }
            Err(error) => {
                if tx
                    .send_blocking(Incoming::Disconnected(error.to_string()))
                    .is_err()
                {
                    return;
                }
            }
        }
        thread::sleep(Duration::from_secs(2));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufReader;
    use std::time::Duration;
    use vrc_photo_ipc::server::Listener;
    use vrc_photo_ipc::{
        Event, Request, RequestEnvelope, Response, ServerMessage, StatusSnapshot, framing,
    };

    fn hello_response(id: u64) -> ServerMessage {
        ServerMessage::Response {
            id,
            response: Response::Hello {
                protocol_version: vrc_photo_ipc::PROTOCOL_VERSION,
                daemon_version: "test".to_string(),
                pid: 1,
                status: StatusSnapshot::default(),
            },
        }
    }

    #[test]
    fn bridge_delivers_connected_then_event() {
        let dir = tempfile::tempdir().unwrap();
        let endpoint = vrc_photo_ipc::Endpoint::for_tests(dir.path());
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
                            ..StatusSnapshot::default()
                        },
                    },
                },
            )
            .unwrap();
            framing::write_message(
                reader.get_mut(),
                &ServerMessage::Event {
                    event: Event::ScanStarted { roots: 1 },
                },
            )
            .unwrap();
            std::thread::sleep(Duration::from_millis(300));
        });

        let (tx, rx) = async_channel::unbounded();
        start(endpoint, tx);
        match rx.recv_blocking().unwrap() {
            Incoming::Connected(status) => assert_eq!(status.photo_count, 5),
            other => panic!("expected connected, got {other:?}"),
        }
        match rx.recv_blocking().unwrap() {
            Incoming::Event(Event::ScanStarted { roots }) => assert_eq!(roots, 1),
            other => panic!("expected scan event, got {other:?}"),
        }
        drop(rx);
        server.join().unwrap();
    }

    #[test]
    fn bridge_reports_disconnected_without_a_daemon() {
        let dir = tempfile::tempdir().unwrap();
        let endpoint = vrc_photo_ipc::Endpoint::for_tests(dir.path());
        let (tx, rx) = async_channel::unbounded();
        start(endpoint, tx);
        match rx.recv_blocking().unwrap() {
            Incoming::Disconnected(error) => assert!(!error.is_empty()),
            other => panic!("expected disconnected, got {other:?}"),
        }
    }
}
