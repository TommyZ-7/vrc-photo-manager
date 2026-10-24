use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Result, UpdateError};
use crate::install::UpdatePaths;

/// The result of the last apply attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LastStatus {
    Success,
    RolledBack,
    Failed,
}

/// Written before launching the new version and updated on rollback.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LastResult {
    pub status: LastStatus,
    pub from: String,
    pub to: String,
    #[serde(default)]
    pub reason: Option<String>,
}

impl LastResult {
    pub fn read(paths: &UpdatePaths) -> Option<Self> {
        match std::fs::read(paths.result_file()) {
            Ok(bytes) => serde_json::from_slice(&bytes).ok(),
            Err(_) => None,
        }
    }

    pub fn write(paths: &UpdatePaths, result: &Self) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(result)
            .map_err(|error| UpdateError::Other(format!("cannot serialize result: {error}")))?;
        write_atomic(&paths.result_file(), &bytes, 0o600)
    }

    /// Reads the result and removes the file so it is shown only once.
    pub fn take(paths: &UpdatePaths) -> Option<Self> {
        let result = Self::read(paths);
        if result.is_some() {
            let _ = std::fs::remove_file(paths.result_file());
        }
        result
    }
}

/// Written by the new UI once it reaches its first frame with a real DB.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthMarker {
    pub version: String,
    pub mode: String,
}

pub fn write_health_marker(paths: &UpdatePaths, token: &str, version: &str) -> Result<()> {
    let marker = HealthMarker {
        version: version.to_string(),
        mode: "normal".to_string(),
    };
    let bytes = serde_json::to_vec_pretty(&marker)
        .map_err(|error| UpdateError::Other(format!("cannot serialize health marker: {error}")))?;
    write_atomic(&paths.health_file(token), &bytes, 0o600)
}

pub fn read_health_marker(paths: &UpdatePaths, token: &str) -> Result<Option<HealthMarker>> {
    let path = paths.health_file(token);
    match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|error| UpdateError::InvalidJob(format!("invalid health marker: {error}"))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(UpdateError::io(path, error)),
    }
}

/// Atomically replaces `target` with `staged`, keeping the old file as
/// `<target>.previous`.
pub fn replace_appimage(target: &Path, staged: &Path) -> Result<PathBuf> {
    if !target.is_file() {
        return Err(UpdateError::io(target, std::io::ErrorKind::NotFound.into()));
    }
    if !staged.is_file() {
        return Err(UpdateError::io(staged, std::io::ErrorKind::NotFound.into()));
    }
    let file_name = target
        .file_name()
        .ok_or_else(|| UpdateError::InvalidJob("target has no file name".to_string()))?;
    let backup = target.with_file_name(format!("{}.previous", file_name.to_string_lossy()));
    let metadata = std::fs::metadata(target).map_err(|error| UpdateError::io(target, error))?;

    if backup.exists() {
        std::fs::remove_file(&backup).map_err(|error| UpdateError::io(&backup, error))?;
    }
    std::fs::rename(target, &backup).map_err(|error| UpdateError::io(target, error))?;

    let result = (|| -> Result<()> {
        std::fs::copy(staged, target).map_err(|error| UpdateError::io(target, error))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = metadata.permissions();
            permissions.set_mode((permissions.mode() & 0o777) | 0o111);
            std::fs::set_permissions(target, permissions)
                .map_err(|error| UpdateError::io(target, error))?;
        }
        let file = OpenOptions::new()
            .read(true)
            .open(target)
            .map_err(|error| UpdateError::io(target, error))?;
        file.sync_all()
            .map_err(|error| UpdateError::io(target, error))?;
        Ok(())
    })();

    if let Err(error) = result {
        let _ = std::fs::remove_file(target);
        let _ = std::fs::rename(&backup, target);
        return Err(error);
    }
    Ok(backup)
}

pub fn restore_appimage(target: &Path, backup: &Path) -> Result<()> {
    if !backup.is_file() {
        return Err(UpdateError::io(backup, std::io::ErrorKind::NotFound.into()));
    }
    if target.exists() {
        std::fs::remove_file(target).map_err(|error| UpdateError::io(target, error))?;
    }
    std::fs::rename(backup, target).map_err(|error| UpdateError::io(target, error))
}

/// Whether a live helper holds the apply lock. A stale lock is removed.
pub fn apply_in_progress(paths: &UpdatePaths) -> bool {
    let lock = paths.lock_file();
    let Ok(text) = std::fs::read_to_string(&lock) else {
        return false;
    };
    let Ok(pid) = text.trim().parse::<u32>() else {
        let _ = std::fs::remove_file(&lock);
        return false;
    };
    if process_alive(pid) {
        true
    } else {
        let _ = std::fs::remove_file(&lock);
        false
    }
}

pub(crate) fn process_alive(pid: u32) -> bool {
    #[cfg(target_os = "linux")]
    {
        Path::new("/proc").join(pid.to_string()).exists()
    }
    #[cfg(all(unix, not(target_os = "linux")))]
    {
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .status()
            .is_ok_and(|status| status.success())
    }
    #[cfg(windows)]
    {
        use windows::Win32::Foundation::CloseHandle;
        use windows::Win32::System::Threading::{
            GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };
        const STILL_ACTIVE: u32 = 259;
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok();
        let Some(handle) = handle else {
            return false;
        };
        let mut code = 0u32;
        let read = unsafe { GetExitCodeProcess(handle, &mut code) }.is_ok();
        let _ = unsafe { CloseHandle(handle) };
        read && code == STILL_ACTIVE
    }
}

pub(crate) fn write_atomic(path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| UpdateError::io(parent, error))?;
    }
    let file_name = path
        .file_name()
        .ok_or_else(|| UpdateError::Other("atomic write path has no file name".to_string()))?;
    let temp = path.with_file_name(format!("{}.tmp", file_name.to_string_lossy()));
    let _ = std::fs::remove_file(&temp);

    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(mode);
    }
    let mut file = options
        .open(&temp)
        .map_err(|error| UpdateError::io(&temp, error))?;
    file.write_all(bytes)
        .map_err(|error| UpdateError::io(&temp, error))?;
    file.sync_all()
        .map_err(|error| UpdateError::io(&temp, error))?;
    drop(file);

    #[cfg(windows)]
    {
        if path.exists() {
            let _ = std::fs::remove_file(path);
        }
    }
    std::fs::rename(&temp, path).map_err(|error| UpdateError::io(path, error))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::install::UpdatePaths;

    fn paths(dir: &Path) -> UpdatePaths {
        let paths = UpdatePaths::new(dir);
        paths.ensure().unwrap();
        paths
    }

    #[test]
    fn replaces_and_restores_an_appimage() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("app.AppImage");
        let staged = dir.path().join("new.AppImage");
        std::fs::write(&target, b"old").unwrap();
        std::fs::write(&staged, b"new").unwrap();

        let backup = replace_appimage(&target, &staged).unwrap();
        assert_eq!(backup, dir.path().join("app.AppImage.previous"));
        assert_eq!(std::fs::read(&target).unwrap(), b"new");
        assert_eq!(std::fs::read(&backup).unwrap(), b"old");

        restore_appimage(&target, &backup).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"old");
        assert!(!backup.exists());
    }

    #[test]
    fn missing_staged_file_leaves_the_target_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("app.AppImage");
        std::fs::write(&target, b"old").unwrap();
        let error = replace_appimage(&target, &dir.path().join("missing.AppImage")).unwrap_err();
        assert!(matches!(error, crate::error::UpdateError::Io { .. }));
        assert_eq!(std::fs::read(&target).unwrap(), b"old");
        assert!(!dir.path().join("app.AppImage.previous").exists());
    }

    #[test]
    fn last_result_round_trips_and_is_consumed_once() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());
        let result = LastResult {
            status: LastStatus::RolledBack,
            from: "0.3.0".to_string(),
            to: "0.4.0".to_string(),
            reason: Some("did not start".to_string()),
        };
        LastResult::write(&paths, &result).unwrap();
        assert_eq!(LastResult::take(&paths).unwrap(), result);
        assert!(LastResult::take(&paths).is_none());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            LastResult::write(&paths, &result).unwrap();
            assert_eq!(
                std::fs::metadata(paths.result_file())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn health_marker_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());
        write_health_marker(&paths, "token-1", "0.4.0").unwrap();
        let marker = read_health_marker(&paths, "token-1").unwrap().unwrap();
        assert_eq!(marker.version, "0.4.0");
        assert_eq!(marker.mode, "normal");
        assert!(read_health_marker(&paths, "other").unwrap().is_none());
    }

    #[test]
    fn detects_a_live_apply_lock_and_reclaims_a_stale_one() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());
        std::fs::write(paths.lock_file(), std::process::id().to_string()).unwrap();
        assert!(apply_in_progress(&paths));
        assert!(paths.lock_file().exists());

        let mut command = std::process::Command::new(if cfg!(windows) { "cmd" } else { "true" });
        if cfg!(windows) {
            command.args(["/C", "exit 0"]);
        }
        let mut child = command.spawn().unwrap();
        let dead_pid = child.id();
        child.wait().unwrap();
        std::fs::write(paths.lock_file(), dead_pid.to_string()).unwrap();
        assert!(!apply_in_progress(&paths));
        assert!(!paths.lock_file().exists());
    }
}
