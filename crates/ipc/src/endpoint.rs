use std::io;
use std::path::{Path, PathBuf};

use interprocess::local_socket::Name;
#[cfg(unix)]
use interprocess::local_socket::{GenericFilePath, ToFsName};
#[cfg(windows)]
use interprocess::local_socket::{GenericNamespaced, ToNsName};

/// A platform-specific local socket address.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoint {
    inner: EndpointInner,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum EndpointInner {
    #[cfg(unix)]
    Path(PathBuf),
    #[cfg(windows)]
    Namespaced(String),
}

impl Endpoint {
    /// Resolves the per-user daemon endpoint below the data directory.
    #[cfg(unix)]
    pub fn for_data_dir(data_dir: &Path) -> io::Result<Self> {
        let path = match std::env::var_os("XDG_RUNTIME_DIR") {
            Some(runtime) if !runtime.is_empty() => PathBuf::from(runtime)
                .join("vrc-photo-manager")
                .join("daemon.sock"),
            _ => data_dir.join("run").join("daemon.sock"),
        };
        // Unix domain socket paths are limited to 108 bytes on Linux. The
        // fallback is a private per-user directory rather than a shared path.
        let path = if path.as_os_str().len() > 100 {
            let user = std::env::var("USER").unwrap_or_else(|_| "user".to_string());
            std::env::temp_dir()
                .join(format!("vrc-photo-manager-{user}"))
                .join("daemon.sock")
        } else {
            path
        };
        Ok(Self {
            inner: EndpointInner::Path(path),
        })
    }

    /// Resolves the per-user daemon endpoint on Windows.
    #[cfg(windows)]
    pub fn for_data_dir(_data_dir: &Path) -> io::Result<Self> {
        Ok(Self {
            inner: EndpointInner::Namespaced(user_namespaced("vrc-photo-manager-daemon")),
        })
    }

    /// Creates a unique endpoint for tests.
    #[cfg(unix)]
    pub fn for_tests(dir: &Path) -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
        Self {
            inner: EndpointInner::Path(
                dir.join(format!("daemon-{}-{counter}.sock", std::process::id())),
            ),
        }
    }

    /// Creates a unique endpoint for tests.
    #[cfg(windows)]
    pub fn for_tests(_dir: &Path) -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
        Self {
            inner: EndpointInner::Namespaced(format!(
                "vrc-photo-manager-test-{}-{counter}",
                std::process::id()
            )),
        }
    }

    /// Human-readable endpoint used in errors and logs.
    pub fn display(&self) -> String {
        match &self.inner {
            #[cfg(unix)]
            EndpointInner::Path(path) => path.display().to_string(),
            #[cfg(windows)]
            EndpointInner::Namespaced(name) => name.clone(),
        }
    }

    /// Filesystem path for Unix cleanup and permission handling.
    #[cfg(unix)]
    pub fn path(&self) -> Option<&Path> {
        match &self.inner {
            EndpointInner::Path(path) => Some(path),
        }
    }

    /// Named pipes do not have a filesystem path.
    #[cfg(windows)]
    pub fn path(&self) -> Option<&Path> {
        None
    }

    pub(crate) fn name(&self) -> io::Result<Name<'_>> {
        match &self.inner {
            #[cfg(unix)]
            EndpointInner::Path(path) => path.as_path().to_fs_name::<GenericFilePath>(),
            #[cfg(windows)]
            EndpointInner::Namespaced(name) => name.as_str().to_ns_name::<GenericNamespaced>(),
        }
    }
}

#[cfg(windows)]
fn user_namespaced(prefix: &str) -> String {
    let user = std::env::var("USERNAME")
        .or_else(|_| std::env::var("USER"))
        .unwrap_or_else(|_| "user".to_string());
    let user: String = user
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character
            } else {
                '_'
            }
        })
        .collect();
    format!("{prefix}-{user}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_endpoints_are_unique() {
        let dir = tempfile::tempdir().unwrap();
        assert_ne!(
            Endpoint::for_tests(dir.path()),
            Endpoint::for_tests(dir.path())
        );
    }

    #[test]
    fn endpoint_display_is_non_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!Endpoint::for_tests(dir.path()).display().is_empty());
    }
}
