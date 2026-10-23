//! Log file path resolution and rotation.
//!
//! The core crate stays free of tracing-subscriber and process concerns; this
//! module only decides where log files live and rotates oversized ones.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::data_dir;

/// Maximum size of a log file before it is rotated to `<name>.1`.
pub const MAX_LOG_BYTES: u64 = 10 * 1024 * 1024;

/// File name of the UI application log.
pub const UI_LOG_FILE: &str = "vrc-photo-manager.log";

/// File name of the daemon log.
pub const DAEMON_LOG_FILE: &str = "vrc-photo-daemon.log";

/// Returns the per-user log directory, or `None` when no data directory is
/// available.
pub fn logs_dir() -> Option<PathBuf> {
    data_dir().map(|dir| dir.join("logs"))
}

/// Prepares the log file for `process` in the default log directory.
///
/// Returns `Ok(None)` when the data directory cannot be resolved.
pub fn prepare_log_file(process: &str) -> io::Result<Option<PathBuf>> {
    match logs_dir() {
        Some(dir) => prepare_log_file_in(&dir, process, MAX_LOG_BYTES).map(Some),
        None => Ok(None),
    }
}

/// Prepares the log file for `process` in `dir`, rotating it when needed.
///
/// The directory and file are created if missing. When the file is at least
/// `max_bytes` long it is renamed to `<process>.log.1` (replacing any previous
/// rotation) and a fresh empty log file is created. The returned path is left
/// in place for appending.
pub fn prepare_log_file_in(dir: &Path, process: &str, max_bytes: u64) -> io::Result<PathBuf> {
    fs::create_dir_all(dir)?;
    let path = dir.join(format!("{process}.log"));
    open_for_append(&path)?;

    if fs::metadata(&path)?.len() >= max_bytes {
        let rotated = dir.join(format!("{process}.log.1"));
        match fs::remove_file(&rotated) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        fs::rename(&path, &rotated)?;
        open_for_append(&path)?;
    }

    Ok(path)
}

fn open_for_append(path: &Path) -> io::Result<fs::File> {
    fs::OpenOptions::new().create(true).append(true).open(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn prepares_appends_and_rotates_log_file() {
        let dir = tempdir().unwrap();
        let path = prepare_log_file_in(dir.path(), "ui", 8).unwrap();
        assert_eq!(path.file_name().unwrap(), "ui.log");

        std::fs::write(&path, b"12345678").unwrap();
        let path_again = prepare_log_file_in(dir.path(), "ui", 8).unwrap();
        assert_eq!(path_again, path);
        assert_eq!(std::fs::read(&path).unwrap(), b"");
        assert_eq!(
            std::fs::read(dir.path().join("ui.log.1")).unwrap(),
            b"12345678"
        );

        std::fs::write(&path, b"abc").unwrap();
        prepare_log_file_in(dir.path(), "ui", 8).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"abc");
    }
}
