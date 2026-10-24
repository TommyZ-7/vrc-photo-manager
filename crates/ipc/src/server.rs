#[cfg(unix)]
use std::fs;
use std::io;
#[cfg(unix)]
use std::path::Path;
#[cfg(unix)]
use std::time::Duration;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use interprocess::local_socket::{
    Listener as LocalSocketListener, ListenerNonblockingMode, ListenerOptions,
    traits::Listener as _,
};

pub use interprocess::local_socket::Stream;

#[cfg(unix)]
use crate::client;
use crate::endpoint::Endpoint;

/// A bound local socket listener.
#[derive(Debug)]
pub struct Listener {
    endpoint: Endpoint,
    inner: LocalSocketListener,
}

impl Listener {
    /// Binds `endpoint`, replacing a stale Unix socket but never a live one.
    pub fn bind(endpoint: Endpoint) -> io::Result<Self> {
        #[cfg(unix)]
        if let Some(path) = endpoint.path() {
            prepare_parent(path)?;
            if path.exists() {
                match client::connect_io(&endpoint, Duration::from_millis(200)) {
                    Ok(_) => {
                        return Err(io::Error::new(
                            io::ErrorKind::AddrInUse,
                            "IPC endpoint is already in use",
                        ));
                    }
                    Err(error) if is_stale_connection(&error) => match fs::remove_file(path) {
                        Ok(()) => {}
                        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                        Err(error) => return Err(error),
                    },
                    Err(error) => return Err(error),
                }
            }
        }

        let name = endpoint.name()?;
        let inner = ListenerOptions::new().name(name).create_sync()?;

        #[cfg(unix)]
        if let Some(path) = endpoint.path() {
            fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
        }

        Ok(Self { endpoint, inner })
    }

    /// Enables or disables non-blocking `accept`.
    pub fn set_nonblocking_accept(&self, enabled: bool) -> io::Result<()> {
        self.inner.set_nonblocking(if enabled {
            ListenerNonblockingMode::Accept
        } else {
            ListenerNonblockingMode::Neither
        })
    }

    /// Accepts one client connection.
    pub fn accept(&self) -> io::Result<Stream> {
        self.inner.accept()
    }

    /// The endpoint this listener is bound to.
    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }
}

#[cfg(unix)]
fn prepare_parent(path: &Path) -> io::Result<()> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    fs::create_dir_all(parent)?;
    fs::set_permissions(parent, fs::Permissions::from_mode(0o700))
}

#[cfg(unix)]
fn is_stale_connection(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::ConnectionRefused
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::NotConnected
            | io::ErrorKind::NotFound
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_endpoint_is_not_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let endpoint = Endpoint::for_tests(dir.path());
        let _listener = Listener::bind(endpoint.clone()).unwrap();
        let error = Listener::bind(endpoint).unwrap_err();
        // Windows reports an existing named pipe as PermissionDenied; Unix
        // reports AddrInUse. Both mean the live endpoint was not replaced.
        assert!(matches!(
            error.kind(),
            io::ErrorKind::AddrInUse | io::ErrorKind::PermissionDenied
        ));
    }

    #[cfg(unix)]
    #[test]
    fn stale_socket_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let endpoint = Endpoint::for_tests(dir.path());
        let listener = Listener::bind(endpoint.clone()).unwrap();
        let path = endpoint.path().unwrap().to_path_buf();
        drop(listener);
        let stale = std::os::unix::net::UnixListener::bind(&path).unwrap();
        drop(stale);
        let _listener = Listener::bind(endpoint).unwrap();
    }
}
