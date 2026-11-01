#[cfg(unix)]
use std::fs;
use std::io;
use std::path::Path;
#[cfg(unix)]
use std::path::PathBuf;

use interprocess::local_socket::Name;
#[cfg(unix)]
use interprocess::local_socket::{GenericFilePath, ToFsName};
#[cfg(windows)]
use interprocess::local_socket::{GenericNamespaced, ToNsName};

/// A platform-specific local socket address.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoint {
    inner: EndpointInner,
    /// True when the Unix socket lives in the shared temp directory
    /// fallback, whose parent directory needs ownership checks before use.
    fallback: bool,
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
        let (path, fallback) = if path.as_os_str().len() > 100 {
            (fallback_dir().join("daemon.sock"), true)
        } else {
            (path, false)
        };
        Ok(Self {
            inner: EndpointInner::Path(path),
            fallback,
        })
    }

    /// Resolves the per-user daemon endpoint on Windows.
    #[cfg(windows)]
    pub fn for_data_dir(_data_dir: &Path) -> io::Result<Self> {
        Ok(Self {
            inner: EndpointInner::Namespaced(current_user_namespaced("vrc-photo-manager-daemon")),
            fallback: false,
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
            fallback: false,
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
            fallback: false,
        }
    }

    /// Whether this endpoint uses the shared temp directory fallback.
    #[cfg(unix)]
    pub(crate) fn is_fallback(&self) -> bool {
        self.fallback
    }

    /// Builds a fallback endpoint at an explicit path, for tests.
    #[cfg(all(unix, test))]
    pub(crate) fn fallback_for_tests(path: PathBuf) -> Self {
        Self {
            inner: EndpointInner::Path(path),
            fallback: true,
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

/// The current user's uid, read from `/proc/self` (Linux). `None` when it
/// cannot be determined, in which case ownership checks are skipped.
#[cfg(unix)]
fn current_uid() -> Option<u32> {
    use std::os::unix::fs::MetadataExt;
    fs::metadata("/proc/self")
        .map(|metadata| metadata.uid())
        .ok()
}

/// The per-user fallback directory inside the shared temp directory. The
/// uid form keeps the name from being predictable across users even when
/// `USER` is spoofed or shared.
#[cfg(unix)]
fn fallback_dir() -> PathBuf {
    let name = match current_uid() {
        Some(uid) => format!("vrc-photo-manager-{uid}"),
        None => {
            let user = std::env::var("USER").unwrap_or_else(|_| "user".to_string());
            format!("vrc-photo-manager-{user}")
        }
    };
    std::env::temp_dir().join(name)
}

/// Rejects a fallback directory that already exists in an unsafe state:
/// not a directory, a symlink, or owned by another user. A missing
/// directory is safe by definition (nothing can be spoofed through it yet).
#[cfg(unix)]
pub(crate) fn verify_fallback_dir(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    let metadata = match fs::symlink_metadata(dir) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "IPC fallback directory is not a plain directory: {}",
                dir.display()
            ),
        ));
    }
    if let Some(uid) = current_uid()
        && metadata.uid() != uid
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "IPC fallback directory is owned by another user: {}",
                dir.display()
            ),
        ));
    }
    Ok(())
}

/// Creates the fallback directory if needed and verifies the result, so a
/// directory planted by another user or a symlink is never used for the
/// socket.
#[cfg(unix)]
pub(crate) fn ensure_safe_fallback_dir(dir: &Path) -> io::Result<()> {
    match fs::symlink_metadata(dir) {
        Ok(_) => verify_fallback_dir(dir),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            use std::os::unix::fs::PermissionsExt;
            fs::create_dir_all(dir)?;
            fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
            // Verify what is actually there now: a pre-planted directory or
            // symlink must not slip through the creation path.
            verify_fallback_dir(dir)
        }
        Err(error) => Err(error),
    }
}

#[cfg(any(windows, test))]
fn user_namespaced(prefix: &str, user: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let sanitized: String = user
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character
            } else {
                '_'
            }
        })
        .collect();
    let mut hasher = DefaultHasher::new();
    user.hash(&mut hasher);
    format!("{prefix}-{sanitized}-{:016x}", hasher.finish())
}

#[cfg(windows)]
fn current_user_namespaced(prefix: &str) -> String {
    let user = std::env::var("USERNAME")
        .or_else(|_| std::env::var("USER"))
        .unwrap_or_else(|_| "user".to_string());
    user_namespaced(prefix, &user)
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

    #[cfg(unix)]
    #[test]
    fn fallback_dir_is_named_for_the_current_user() {
        let dir = fallback_dir();
        let name = dir.file_name().unwrap().to_string_lossy().to_string();
        match current_uid() {
            Some(uid) => assert_eq!(name, format!("vrc-photo-manager-{uid}")),
            None => assert!(name.starts_with("vrc-photo-manager-")),
        }
        assert_eq!(dir.parent(), Some(std::env::temp_dir().as_path()));
    }

    #[cfg(unix)]
    #[test]
    fn ensure_safe_fallback_dir_creates_a_private_directory() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("fallback");
        ensure_safe_fallback_dir(&target).unwrap();
        let metadata = std::fs::metadata(&target).unwrap();
        assert!(metadata.is_dir());
        assert_eq!(metadata.permissions().mode() & 0o777, 0o700);
        // An existing, healthy directory stays accepted.
        ensure_safe_fallback_dir(&target).unwrap();
        verify_fallback_dir(&target).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn ensure_safe_fallback_dir_rejects_a_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let error = ensure_safe_fallback_dir(&link).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        let error = verify_fallback_dir(&link).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    }

    #[cfg(unix)]
    #[test]
    fn ensure_safe_fallback_dir_rejects_a_plain_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("file");
        std::fs::write(&file, b"x").unwrap();
        let error = ensure_safe_fallback_dir(&file).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    }

    #[cfg(unix)]
    #[test]
    fn ensure_safe_fallback_dir_rejects_another_users_directory() {
        let Some(uid) = current_uid() else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("foreign");
        std::fs::create_dir(&target).unwrap();
        // Hand the directory to a different uid. Needs privileges (root);
        // skip when the environment cannot chown.
        if std::os::unix::fs::chown(&target, Some(uid + 1), None).is_err() {
            return;
        }
        let error = ensure_safe_fallback_dir(&target).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        let error = verify_fallback_dir(&target).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    }

    #[cfg(unix)]
    #[test]
    fn verify_fallback_dir_accepts_a_missing_directory() {
        let dir = tempfile::tempdir().unwrap();
        verify_fallback_dir(&dir.path().join("missing")).unwrap();
    }

    #[test]
    fn windows_names_include_a_stable_user_hash() {
        assert_ne!(
            user_namespaced("daemon", "山田"),
            user_namespaced("daemon", "田中")
        );
        assert_ne!(
            user_namespaced("daemon", "a-b"),
            user_namespaced("daemon", "a_b")
        );
        assert_eq!(
            user_namespaced("daemon", "same"),
            user_namespaced("daemon", "same")
        );
    }
}
