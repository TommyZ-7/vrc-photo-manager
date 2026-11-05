//! Log file path resolution, private modes, and rotation.
//!
//! The core crate stays free of tracing-subscriber and process concerns; this
//! module only decides where log files live, how they are opened, and rotates
//! oversized ones.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::data_dir;

/// Maximum size of a log file before it is rotated to `<name>.1`.
pub const MAX_LOG_BYTES: u64 = 10 * 1024 * 1024;

/// File name of the UI application log.
pub const UI_LOG_FILE: &str = "vrc-photon.log";

/// File name of the daemon log.
pub const DAEMON_LOG_FILE: &str = "vrc-photon-daemon.log";

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
///
/// On Unix the directory is kept private to the owner (no group or other
/// access) and the log file is created with mode `0600`. Existing permissions
/// are only ever narrowed, never widened.
pub fn prepare_log_file_in(dir: &Path, process: &str, max_bytes: u64) -> io::Result<PathBuf> {
    create_private_dir_all(dir)?;
    let path = dir.join(format!("{process}.log"));
    open_private_append(&path)?;

    if fs::metadata(&path)?.len() >= max_bytes {
        let rotated = rotated_path(&path);
        remove_rotated(&rotated)?;
        fs::rename(&path, &rotated)?;
        open_private_append(&path)?;
    }

    Ok(path)
}

/// Opens `path` for appending, creating it with private permissions on Unix.
///
/// On Unix a newly created file gets mode `0600`; an existing file that is
/// readable or writable by the group / other is tightened to `0600`. Existing
/// owner bits are never widened, so a deliberately narrower mode is preserved
/// (and the append then fails as usual if the owner cannot write it).
pub fn open_private_append(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path)?;
    #[cfg(unix)]
    restrict_file_mode(path)?;
    Ok(file)
}

/// Append-only log file that can rotate itself while writers hold it open.
///
/// Share one `RotatingLog` behind an `Arc<Mutex<_>>` between every writer
/// (protocol records and sidecar stderr, for example). A writer with its own
/// file handle would otherwise keep appending to `<name>.1` after a rotation.
///
/// Rotation happens before the next record is written, so a record is never
/// split between the live and rotated files. A single record larger than the
/// limit is written whole; the following record rotates it away.
#[derive(Debug)]
pub struct RotatingLog {
    path: PathBuf,
    rotated: PathBuf,
    max_bytes: u64,
    file: File,
    len: u64,
    /// The open handle no longer points at `path` (the reopen after a rename
    /// failed). Writers try to reattach before every record so bytes do not
    /// stay stranded in the rotated file.
    detached: bool,
}

impl RotatingLog {
    /// Opens `path` for appending and rotates it once it would exceed
    /// `max_bytes`.
    pub fn open(path: &Path, max_bytes: u64) -> io::Result<Self> {
        let file = open_private_append(path)?;
        let len = file.metadata()?.len();
        Ok(Self {
            path: path.to_path_buf(),
            rotated: rotated_path(path),
            max_bytes,
            file,
            len,
            detached: false,
        })
    }

    /// Appends one record, adding a trailing newline when it has none.
    pub fn write_line(&mut self, line: &str) -> io::Result<()> {
        let mut record = Vec::with_capacity(line.len() + 1);
        record.extend_from_slice(line.as_bytes());
        if !record.ends_with(b"\n") {
            record.push(b'\n');
        }
        self.write_record(&record)
    }

    /// Appends `record` as one unit, rotating first when the write would
    /// exceed the limit.
    ///
    /// Logging is best effort: if rotation fails, the record still goes to the
    /// currently open file instead of being dropped. The next record retries
    /// the rotation.
    pub fn write_record(&mut self, record: &[u8]) -> io::Result<()> {
        if self.detached {
            self.reattach();
        }

        let record_len = u64::try_from(record.len()).unwrap_or(u64::MAX);
        if self.len > 0 && self.len.saturating_add(record_len) > self.max_bytes {
            let _ = self.rotate();
        }
        self.file.write_all(record)?;
        self.len = self.len.saturating_add(record_len);
        Ok(())
    }

    /// Renames the live file to `<name>.1` and opens a fresh one.
    fn rotate(&mut self) -> io::Result<()> {
        remove_rotated(&self.rotated)?;
        fs::rename(&self.path, &self.rotated)?;
        match open_private_append(&self.path) {
            Ok(file) => {
                self.file = file;
                self.len = 0;
                self.detached = false;
                Ok(())
            }
            Err(error) => {
                // The old handle now points at the rotated file. Keep it so
                // the caller can still write the record, and retry the reopen
                // on the next write.
                self.detached = true;
                Err(error)
            }
        }
    }

    fn reattach(&mut self) {
        if let Ok(file) = open_private_append(&self.path) {
            self.file = file;
            self.len = 0;
            self.detached = false;
        }
    }
}

fn rotated_path(path: &Path) -> PathBuf {
    let mut rotated = path.as_os_str().to_os_string();
    rotated.push(".1");
    PathBuf::from(rotated)
}

fn remove_rotated(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn create_private_dir_all(dir: &Path) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(dir)?.permissions().mode() & 0o777;
        // Keep the owner's bits, drop group and other access. This only
        // narrows permissions, so a directory that is already private is left
        // untouched.
        let private = mode & 0o700;
        if private != mode {
            fs::set_permissions(dir, fs::Permissions::from_mode(private))?;
        }
    }
    Ok(())
}

#[cfg(unix)]
fn restrict_file_mode(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = fs::metadata(path)?.permissions().mode() & 0o777;
    // Keep only the owner read/write bits and never add a bit the file did
    // not already have.
    let private = mode & 0o600;
    if private != mode {
        fs::set_permissions(path, fs::Permissions::from_mode(private))?;
    }
    Ok(())
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

    #[test]
    fn prepare_reports_an_unusable_directory() {
        let dir = tempdir().unwrap();
        let not_a_dir = dir.path().join("not-a-dir");
        std::fs::write(&not_a_dir, b"x").unwrap();
        assert!(prepare_log_file_in(&not_a_dir, "ui", 8).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn log_files_and_directory_are_private() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempdir().unwrap();
        let logs = dir.path().join("logs");
        fs::create_dir_all(&logs).unwrap();
        fs::set_permissions(&logs, fs::Permissions::from_mode(0o755)).unwrap();

        let path = prepare_log_file_in(&logs, "sidecar", 8).unwrap();

        assert_eq!(
            fs::metadata(&logs).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );

        std::fs::write(&path, b"12345678").unwrap();
        prepare_log_file_in(&logs, "sidecar", 8).unwrap();
        assert_eq!(
            fs::metadata(logs.join("sidecar.log.1"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[cfg(unix)]
    #[test]
    fn opening_an_existing_log_tightens_its_mode() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempdir().unwrap();
        let path = dir.path().join("sidecar.log");
        fs::write(&path, b"old").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();

        let _file = open_private_append(&path).unwrap();

        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn rotating_while_open_keeps_writers_on_the_live_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sidecar.log");
        let rotated = dir.path().join("sidecar.log.1");
        let mut log = RotatingLog::open(&path, 10).unwrap();

        log.write_line("aaaa").unwrap();
        log.write_line("bbbb").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"aaaa\nbbbb\n");
        assert!(!rotated.exists());

        // The next record makes the file exceed the limit: rotate first, then
        // write the record to the fresh live file.
        log.write_line("cccc").unwrap();
        assert_eq!(fs::read(&rotated).unwrap(), b"aaaa\nbbbb\n");
        assert_eq!(fs::read(&path).unwrap(), b"cccc\n");

        // The already-open handle must follow the rotation, not keep writing
        // to the rotated file.
        log.write_line("dddd").unwrap();
        log.write_line("eeee").unwrap();
        assert_eq!(fs::read(&rotated).unwrap(), b"cccc\ndddd\n");
        assert_eq!(fs::read(&path).unwrap(), b"eeee\n");
        assert!(!fs::read_to_string(&path).unwrap().contains("cccc"));
    }

    #[test]
    fn rotation_replaces_an_older_rotated_file_instead_of_appending() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sidecar.log");
        let rotated = dir.path().join("sidecar.log.1");
        let mut log = RotatingLog::open(&path, 6).unwrap();

        log.write_line("first").unwrap();
        fs::write(&rotated, b"stale\n").unwrap();
        log.write_line("second").unwrap();

        assert_eq!(fs::read(&rotated).unwrap(), b"first\n");
        assert_eq!(fs::read(&path).unwrap(), b"second\n");
        assert!(!fs::read_to_string(&rotated).unwrap().contains("stale"));
    }

    #[test]
    fn an_oversized_record_is_written_whole() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sidecar.log");
        let mut log = RotatingLog::open(&path, 4).unwrap();

        log.write_line("123456789").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"123456789\n");

        log.write_line("x").unwrap();
        assert_eq!(
            fs::read(dir.path().join("sidecar.log.1")).unwrap(),
            b"123456789\n"
        );
        assert_eq!(fs::read(&path).unwrap(), b"x\n");
    }
}
