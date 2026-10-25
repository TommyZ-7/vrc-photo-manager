use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use chrono::Utc;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::{IoOperation, Result, UpdateError};
use crate::install::{ApplyJob, ApplyKind, UpdatePaths};
use crate::verify::{decode_signature_base64, effective_public_keys, verify_file};

/// Timeouts for the helper. Tests may skip the signature check to exercise
/// launch/rollback with generated scripts.
#[derive(Debug, Clone)]
pub struct HelperOptions {
    pub exit_timeout: Duration,
    pub health_timeout: Duration,
    #[cfg(test)]
    pub skip_signature_check: bool,
    /// Test-only trusted keys; empty means the embedded production keys.
    #[cfg(test)]
    pub public_keys: Vec<String>,
}

impl Default for HelperOptions {
    fn default() -> Self {
        Self {
            exit_timeout: Duration::from_secs(30),
            health_timeout: Duration::from_secs(45),
            #[cfg(test)]
            skip_signature_check: false,
            #[cfg(test)]
            public_keys: Vec::new(),
        }
    }
}

/// Applies the job with production timeouts.
pub fn run_helper(job_path: &Path) -> Result<()> {
    run_helper_with_options(job_path, HelperOptions::default())
}

/// Reads the job, waits for the old processes, applies the update, verifies
/// the new UI's health marker, and rolls back on failure.
pub fn run_helper_with_options(job_path: &Path, options: HelperOptions) -> Result<()> {
    let bytes = std::fs::read(job_path)
        .map_err(|error| UpdateError::io_at(IoOperation::Job, job_path, error))?;
    let job: ApplyJob = serde_json::from_slice(&bytes)
        .map_err(|error| UpdateError::InvalidJob(format!("cannot parse apply job: {error}")))?;
    job.validate()?;

    let paths = UpdatePaths::new(&job.data_dir);
    if let Err(error) = paths.ensure() {
        let _ = LastResult::write(
            &paths,
            &LastResult {
                status: LastStatus::Failed,
                from: job.from_version.clone(),
                to: job.to_version.clone(),
                reason: Some(error.to_string()),
            },
        );
        relaunch_old(&paths, &job);
        return Err(error);
    }
    log(
        &paths,
        &format!(
            "apply start: {} -> {} ({:?})",
            job.from_version, job.to_version, job.kind
        ),
    );
    let _lock = match acquire_lock(&paths) {
        Ok(lock) => lock,
        Err(error) => {
            log(&paths, &format!("cannot acquire the apply lock: {error}"));
            // Another update is already running. Relaunch this UI but leave
            // the other helper's job/result untouched.
            relaunch_old(&paths, &job);
            return Err(error);
        }
    };

    if !wait_for_process_exit(job.ui_pid, options.exit_timeout) {
        return abort_before_apply(
            &paths,
            &job,
            job_path,
            "the UI did not exit before the update timeout",
        );
    }
    if let Some(daemon_pid) = job.daemon_pid
        && !wait_for_process_exit(daemon_pid, options.exit_timeout)
    {
        return abort_before_apply(
            &paths,
            &job,
            job_path,
            "the daemon did not exit before the update timeout",
        );
    }

    if let Err(error) = verify_staged(&job, &options) {
        log(
            &paths,
            &format!("staged artifact verification failed: {error}"),
        );
        let _ = LastResult::write(
            &paths,
            &LastResult {
                status: LastStatus::Failed,
                from: job.from_version.clone(),
                to: job.to_version.clone(),
                reason: Some(error.to_string()),
            },
        );
        relaunch_old(&paths, &job);
        cleanup(&paths, &job, job_path);
        return Err(error);
    }

    let backup = match apply(&paths, &job) {
        Ok(backup) => backup,
        Err(error) => {
            log(&paths, &format!("apply failed: {error}"));
            // The replacement may have failed after touching the target. Keep
            // the job, staged artifact, and log so the backup can be restored
            // manually if the automatic path did not finish.
            let _ = LastResult::write(
                &paths,
                &LastResult {
                    status: LastStatus::Failed,
                    from: job.from_version.clone(),
                    to: job.to_version.clone(),
                    reason: Some(format!(
                        "{error} (job: {}, staged: {}, log: {})",
                        job_path.display(),
                        job.staged.display(),
                        paths.log_file().display()
                    )),
                },
            );
            relaunch_old(&paths, &job);
            return Err(error);
        }
    };

    let _ = LastResult::write(
        &paths,
        &LastResult {
            status: LastStatus::Success,
            from: job.from_version.clone(),
            to: job.to_version.clone(),
            reason: None,
        },
    );

    let mut child = match launch(&job.target, &job.restart_args, Some(&job.health_token)) {
        Ok(child) => child,
        Err(error) => {
            log(&paths, &format!("cannot launch the new version: {error}"));
            return rollback_after_failure(&paths, &job, job_path, &backup, &error);
        }
    };

    match wait_for_health(
        &paths,
        &job.health_token,
        &job.to_version,
        &mut child,
        options.health_timeout,
    ) {
        Ok(()) => {
            let _ = std::fs::remove_file(paths.health_file(&job.health_token));
            log(&paths, "update healthy");
            cleanup(&paths, &job, job_path);
            Ok(())
        }
        Err(error) => {
            log(&paths, &format!("health check failed: {error}"));
            let _ = child.kill();
            let _ = child.wait();
            rollback_after_failure(&paths, &job, job_path, &backup, &error)
        }
    }
}

/// Restores the previous version after a failed launch/health check. If the
/// restore itself fails, the job and backup are kept for manual recovery and
/// a `rolled_back` result records the paths.
fn rollback_after_failure(
    paths: &UpdatePaths,
    job: &ApplyJob,
    job_path: &Path,
    backup: &Backup,
    error: &UpdateError,
) -> Result<()> {
    let backup_hint = match backup {
        Backup::AppImage(path) | Backup::Windows(path) => path.display().to_string(),
    };
    let (restored, reason) = match restore(job, backup) {
        Ok(()) => (true, error.to_string()),
        Err(restore_error) => (
            false,
            format!(
                "{error}; the previous version could not be restored: {restore_error} (backup: {backup_hint}, job: {}, log: {})",
                job_path.display(),
                paths.log_file().display()
            ),
        ),
    };

    let _ = LastResult::write(
        paths,
        &LastResult {
            status: LastStatus::RolledBack,
            from: job.from_version.clone(),
            to: job.to_version.clone(),
            reason: Some(reason.clone()),
        },
    );
    relaunch_old(paths, job);
    if restored {
        cleanup(paths, job, job_path);
    } else {
        log(
            paths,
            &format!(
                "manual recovery required; keeping {} and {}: {reason}",
                job_path.display(),
                job.staged.display()
            ),
        );
    }
    Err(UpdateError::Other(reason))
}

fn abort_before_apply(
    paths: &UpdatePaths,
    job: &ApplyJob,
    job_path: &Path,
    reason: &str,
) -> Result<()> {
    log(paths, reason);
    let _ = LastResult::write(
        paths,
        &LastResult {
            status: LastStatus::RolledBack,
            from: job.from_version.clone(),
            to: job.to_version.clone(),
            reason: Some(reason.to_string()),
        },
    );
    relaunch_old(paths, job);
    cleanup(paths, job, job_path);
    Err(UpdateError::Other(reason.to_string()))
}

fn verify_staged(job: &ApplyJob, options: &HelperOptions) -> Result<()> {
    if !job.staged.is_file() {
        return Err(UpdateError::io_at(
            IoOperation::Apply,
            &job.staged,
            std::io::ErrorKind::NotFound.into(),
        ));
    }
    let digest = sha256_file(&job.staged)?;
    if !digest.eq_ignore_ascii_case(&job.sha256) {
        return Err(UpdateError::Integrity(
            "staged artifact SHA-256 does not match the job".to_string(),
        ));
    }
    #[cfg(test)]
    if options.skip_signature_check {
        return Ok(());
    }
    let signature = decode_signature_base64(&job.signature)?;
    #[cfg(test)]
    let keys = if options.public_keys.is_empty() {
        effective_public_keys()
    } else {
        options.public_keys.clone()
    };
    #[cfg(not(test))]
    let keys = {
        let _ = options;
        effective_public_keys()
    };
    verify_file(&job.staged, &signature, &keys)
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut file =
        File::open(path).map_err(|error| UpdateError::io_at(IoOperation::Apply, path, error))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| UpdateError::io_at(IoOperation::Apply, path, error))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

enum Backup {
    AppImage(PathBuf),
    #[cfg_attr(not(windows), allow(dead_code))]
    Windows(PathBuf),
}

fn apply(paths: &UpdatePaths, job: &ApplyJob) -> Result<Backup> {
    match job.kind {
        ApplyKind::AppImage => replace_appimage(&job.target, &job.staged).map(Backup::AppImage),
        ApplyKind::Windows => {
            #[cfg(windows)]
            {
                apply_windows(paths, job).map(Backup::Windows)
            }
            #[cfg(not(windows))]
            {
                let _ = paths;
                Err(UpdateError::Other(
                    "Windows updates cannot be applied on this platform".to_string(),
                ))
            }
        }
    }
}

fn restore(job: &ApplyJob, backup: &Backup) -> Result<()> {
    match backup {
        Backup::AppImage(path) => restore_appimage(&job.target, path),
        Backup::Windows(path) => {
            #[cfg(windows)]
            {
                let install_dir = job.target.parent().ok_or_else(|| {
                    UpdateError::InvalidJob("Windows target has no parent".to_string())
                })?;
                restore_windows(path, install_dir)
            }
            #[cfg(not(windows))]
            {
                let _ = path;
                Err(UpdateError::Other(
                    "a Windows backup cannot be restored on this platform".to_string(),
                ))
            }
        }
    }
}

#[cfg(windows)]
fn apply_windows(paths: &UpdatePaths, job: &ApplyJob) -> Result<PathBuf> {
    let install_dir = job
        .target
        .parent()
        .ok_or_else(|| UpdateError::InvalidJob("Windows target has no parent".to_string()))?;
    let backup = paths.backup_dir(&job.from_version);
    std::fs::create_dir_all(&backup)
        .map_err(|error| UpdateError::io_at(IoOperation::Apply, &backup, error))?;
    for name in ["vrc-photo-manager.exe", "vrc-photo-daemon.exe"] {
        let source = install_dir.join(name);
        if source.is_file() {
            std::fs::copy(&source, backup.join(name))
                .map_err(|error| UpdateError::io_at(IoOperation::Apply, &source, error))?;
        }
    }

    let mut child = Command::new(&job.staged)
        .arg("/S")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| UpdateError::io_at(IoOperation::Apply, &job.staged, error))?;
    let status = match wait_for_exit(&mut child, Duration::from_secs(10 * 60)) {
        Ok(status) => status,
        Err(error) => {
            let reason = match restore_windows(&backup, install_dir) {
                Ok(()) => error.to_string(),
                Err(restore_error) => {
                    format!("{error}; restoring the previous executables failed: {restore_error}")
                }
            };
            return Err(UpdateError::Other(reason));
        }
    };
    if !status.success() {
        restore_windows(&backup, install_dir)?;
        return Err(UpdateError::Other(format!(
            "the installer exited with {status}"
        )));
    }
    if !job.target.is_file() {
        restore_windows(&backup, install_dir)?;
        return Err(UpdateError::Other(
            "the installer did not leave the UI executable in place".to_string(),
        ));
    }
    Ok(backup)
}

#[cfg(windows)]
fn restore_windows(backup: &Path, install_dir: &Path) -> Result<()> {
    for name in ["vrc-photo-manager.exe", "vrc-photo-daemon.exe"] {
        let source = backup.join(name);
        if source.is_file() {
            let target = install_dir.join(name);
            std::fs::copy(&source, &target)
                .map_err(|error| UpdateError::io_at(IoOperation::Apply, &target, error))?;
        }
    }
    Ok(())
}

#[cfg(windows)]
fn wait_for_exit(child: &mut Child, timeout: Duration) -> Result<std::process::ExitStatus> {
    let started = Instant::now();
    loop {
        if let Some(status) = child
            .try_wait()
            .map_err(|error| UpdateError::Other(error.to_string()))?
        {
            return Ok(status);
        }
        if started.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            return Err(UpdateError::Other(
                "the installer did not finish in time".to_string(),
            ));
        }
        thread::sleep(Duration::from_millis(500));
    }
}

fn launch(target: &Path, args: &[String], health_token: Option<&str>) -> Result<Child> {
    let mut command = Command::new(target);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(token) = health_token {
        command.env("VRC_PHOTO_HEALTH_TOKEN", token);
    }
    command
        .spawn()
        .map_err(|error| UpdateError::io_at(IoOperation::Spawn, target, error))
}

fn relaunch_old(paths: &UpdatePaths, job: &ApplyJob) {
    if let Err(error) = launch(&job.target, &job.restart_args, None) {
        log(
            paths,
            &format!("cannot relaunch the previous version: {error}"),
        );
    }
}

fn cleanup(paths: &UpdatePaths, job: &ApplyJob, job_path: &Path) {
    let _ = std::fs::remove_file(&job.staged);
    let _ = std::fs::remove_file(job_path);
    let _ = std::fs::remove_file(paths.health_file(&job.health_token));
}

/// Returns true when the process exits before the timeout.
pub fn wait_for_process_exit(pid: u32, timeout: Duration) -> bool {
    if pid == 0 {
        return true;
    }
    let started = Instant::now();
    loop {
        if !process_alive(pid) {
            return true;
        }
        let elapsed = started.elapsed();
        if elapsed >= timeout {
            return false;
        }
        thread::sleep(Duration::from_millis(250).min(timeout - elapsed));
    }
}

/// Waits for the new UI's health marker and verifies its version.
pub fn wait_for_health(
    paths: &UpdatePaths,
    token: &str,
    expected: &str,
    child: &mut Child,
    timeout: Duration,
) -> Result<()> {
    // The marker proves that the new UI reached its first frame, but a crash
    // immediately afterwards must still roll back. Require the child to stay
    // alive for this grace period after the marker appears.
    const GRACE: Duration = Duration::from_secs(1);
    let started = Instant::now();
    let mut marker_seen = false;
    let mut marker_since = started;
    loop {
        if let Some(status) = child
            .try_wait()
            .map_err(|error| UpdateError::Other(error.to_string()))?
        {
            return Err(UpdateError::Other(format!(
                "the new version exited before becoming healthy ({status})"
            )));
        }

        match read_health_marker(paths, token)? {
            Some(marker) if marker.version == expected && marker.mode == "normal" => {
                if !marker_seen {
                    marker_seen = true;
                    marker_since = Instant::now();
                }
                if marker_since.elapsed() >= GRACE {
                    return Ok(());
                }
            }
            Some(marker) => {
                return Err(UpdateError::Other(format!(
                    "health marker reported version {} instead of {expected}",
                    marker.version
                )));
            }
            None => marker_seen = false,
        }

        if !marker_seen && started.elapsed() >= timeout {
            return Err(UpdateError::Other(
                "timed out waiting for the new version to become healthy".to_string(),
            ));
        }
        thread::sleep(Duration::from_millis(100));
    }
}

struct LockGuard {
    path: PathBuf,
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn acquire_lock(paths: &UpdatePaths) -> Result<LockGuard> {
    let path = paths.lock_file();
    for attempt in 0..2 {
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut file) => {
                write!(file, "{}", std::process::id())
                    .map_err(|error| UpdateError::io(&path, error))?;
                file.sync_all()
                    .map_err(|error| UpdateError::io(&path, error))?;
                return Ok(LockGuard { path });
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists && attempt == 0 => {
                if apply_in_progress(paths) {
                    return Err(UpdateError::Other(
                        "another update is already being applied".to_string(),
                    ));
                }
            }
            Err(error) => return Err(UpdateError::io(&path, error)),
        }
    }
    Err(UpdateError::Other(
        "cannot acquire the update lock".to_string(),
    ))
}

fn log(paths: &UpdatePaths, message: &str) {
    let path = paths.log_file();
    if std::fs::metadata(&path).is_ok_and(|metadata| metadata.len() > 1024 * 1024) {
        let rotated = path.with_extension("log.1");
        let _ = std::fs::rename(&path, rotated);
    }
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(file, "[{}] {message}", Utc::now().to_rfc3339());
    }
}

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
        return Err(UpdateError::io_at(
            IoOperation::Apply,
            target,
            std::io::ErrorKind::NotFound.into(),
        ));
    }
    if !staged.is_file() {
        return Err(UpdateError::io_at(
            IoOperation::Apply,
            staged,
            std::io::ErrorKind::NotFound.into(),
        ));
    }
    let file_name = target
        .file_name()
        .ok_or_else(|| UpdateError::InvalidJob("target has no file name".to_string()))?;
    let backup = target.with_file_name(format!("{}.previous", file_name.to_string_lossy()));
    #[cfg(unix)]
    let metadata = std::fs::metadata(target)
        .map_err(|error| UpdateError::io_at(IoOperation::Apply, target, error))?;

    if backup.exists() {
        std::fs::remove_file(&backup)
            .map_err(|error| UpdateError::io_at(IoOperation::Apply, &backup, error))?;
    }
    std::fs::rename(target, &backup)
        .map_err(|error| UpdateError::io_at(IoOperation::Apply, target, error))?;

    let result = (|| -> Result<()> {
        std::fs::copy(staged, target)
            .map_err(|error| UpdateError::io_at(IoOperation::Apply, target, error))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = metadata.permissions();
            permissions.set_mode((permissions.mode() & 0o777) | 0o111);
            std::fs::set_permissions(target, permissions)
                .map_err(|error| UpdateError::io_at(IoOperation::Apply, target, error))?;
        }
        // Windows requires write access for FlushFileBuffers (sync_all).
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(target)
            .map_err(|error| UpdateError::io_at(IoOperation::Apply, target, error))?;
        file.sync_all()
            .map_err(|error| UpdateError::io_at(IoOperation::Apply, target, error))?;
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
        return Err(UpdateError::io_at(
            IoOperation::Apply,
            backup,
            std::io::ErrorKind::NotFound.into(),
        ));
    }
    if target.exists() {
        std::fs::remove_file(target)
            .map_err(|error| UpdateError::io_at(IoOperation::Apply, target, error))?;
    }
    std::fs::rename(backup, target)
        .map_err(|error| UpdateError::io_at(IoOperation::Apply, target, error))
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
        // ERROR_INVALID_PARAMETER means the PID does not exist. Any other
        // failure (notably access denied) could be a live process this user
        // cannot inspect, so fail safe and treat it as alive.
        let handle = match unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) } {
            Ok(handle) => handle,
            Err(error) => return (error.code().0 as u32 & 0xffff) != 87,
        };
        let mut code = 0u32;
        let read = unsafe { GetExitCodeProcess(handle, &mut code) }.is_ok();
        let _ = unsafe { CloseHandle(handle) };
        read && code == STILL_ACTIVE
    }
}

pub(crate) fn write_atomic(path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    #[cfg(not(unix))]
    let _ = mode;
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
    use std::time::Duration;

    #[cfg(unix)]
    use base64::Engine;
    #[cfg(unix)]
    use minisign::KeyPair;
    #[cfg(unix)]
    use sha2::Digest;

    use super::*;
    use crate::install::UpdatePaths;
    #[cfg(unix)]
    use crate::install::{APPLY_SCHEMA, ApplyJob, ApplyKind};

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

    #[cfg(unix)]
    fn dead_pid() -> u32 {
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        pid
    }

    #[test]
    fn waits_for_a_process_to_exit_with_a_bound() {
        let mut child = if cfg!(windows) {
            let mut command = std::process::Command::new("cmd");
            command.args(["/C", "exit 0"]);
            command.spawn().unwrap()
        } else {
            std::process::Command::new("true").spawn().unwrap()
        };
        let pid = child.id();
        child.wait().unwrap();
        assert!(wait_for_process_exit(pid, Duration::from_secs(2)));
        assert!(!wait_for_process_exit(
            std::process::id(),
            Duration::from_millis(100)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn health_marker_must_match_the_expected_version() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());
        let mut child = std::process::Command::new("sleep")
            .arg("2")
            .spawn()
            .unwrap();

        write_health_marker(&paths, "token", "9.9.9").unwrap();
        assert!(
            wait_for_health(&paths, "token", "9.9.9", &mut child, Duration::from_secs(1)).is_ok()
        );
        let _ = child.kill();
        let _ = child.wait();

        write_health_marker(&paths, "wrong", "8.8.8").unwrap();
        let mut child = std::process::Command::new("sleep")
            .arg("2")
            .spawn()
            .unwrap();
        assert!(
            wait_for_health(&paths, "wrong", "9.9.9", &mut child, Duration::from_secs(1)).is_err()
        );
        let _ = child.kill();
        let _ = child.wait();

        let mut child = std::process::Command::new("sleep")
            .arg("2")
            .spawn()
            .unwrap();
        assert!(
            wait_for_health(
                &paths,
                "missing",
                "9.9.9",
                &mut child,
                Duration::from_millis(100)
            )
            .is_err()
        );
        let _ = child.kill();
        let _ = child.wait();
    }

    #[cfg(unix)]
    fn make_executable(path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[cfg(unix)]
    fn signed_job(dir: &Path, staged: &Path, token: &str) -> (ApplyJob, String) {
        let keypair = KeyPair::generate_unencrypted_keypair().unwrap();
        let public_key = keypair
            .pk
            .to_box()
            .unwrap()
            .to_string()
            .lines()
            .find(|line| line.starts_with("RW"))
            .unwrap()
            .to_string();
        let mut job = integration_job(dir, staged, token);
        let bytes = std::fs::read(staged).unwrap();
        let signature = minisign::sign(None, &keypair.sk, std::io::Cursor::new(&bytes), None, None)
            .unwrap()
            .into_string();
        job.signature = base64::engine::general_purpose::STANDARD.encode(signature.as_bytes());
        job.sha256 = hex::encode(sha2::Sha256::digest(&bytes));
        (job, public_key)
    }

    #[cfg(unix)]
    #[test]
    fn applies_when_the_staged_signature_is_valid() {
        let dir = tempfile::tempdir().unwrap();
        let token = "token-signed";
        let marker_path = dir
            .path()
            .join("updates")
            .join(format!("health-{token}.json"));
        let marker_tmp = marker_path.with_extension("json.tmp");
        let new_script = dir.path().join("new.AppImage");
        std::fs::write(
            &new_script,
            format!(
                "#!/bin/sh\nmkdir -p '{}'\nprintf '%s' '{{\"version\":\"9.9.9\",\"mode\":\"normal\"}}' > '{}'\nmv '{}' '{}'\nsleep 2\n",
                marker_path.parent().unwrap().display(),
                marker_tmp.display(),
                marker_tmp.display(),
                marker_path.display()
            ),
        )
        .unwrap();
        make_executable(&new_script);

        let (job, public_key) = signed_job(dir.path(), &new_script, token);
        let job_file = dir.path().join("updates/job.json");
        std::fs::create_dir_all(job_file.parent().unwrap()).unwrap();
        std::fs::write(&job_file, serde_json::to_vec(&job).unwrap()).unwrap();

        run_helper_with_options(
            &job_file,
            HelperOptions {
                health_timeout: Duration::from_secs(5),
                skip_signature_check: false,
                public_keys: vec![public_key],
                ..HelperOptions::default()
            },
        )
        .unwrap();
        let result = LastResult::read(&UpdatePaths::new(dir.path())).unwrap();
        assert_eq!(result.status, LastStatus::Success);
    }

    #[cfg(unix)]
    #[test]
    fn rejects_a_staged_artifact_with_a_bad_signature() {
        let dir = tempfile::tempdir().unwrap();
        let token = "token-bad-signature";
        let new_script = dir.path().join("new.AppImage");
        std::fs::write(&new_script, "#!/bin/sh\nexit 0\n").unwrap();
        make_executable(&new_script);

        let (mut job, public_key) = signed_job(dir.path(), &new_script, token);
        let bytes = std::fs::read(&new_script).unwrap();
        let other_signature = minisign::sign(
            None,
            &KeyPair::generate_unencrypted_keypair().unwrap().sk,
            std::io::Cursor::new(b"different"),
            None,
            None,
        )
        .unwrap()
        .into_string();
        job.signature =
            base64::engine::general_purpose::STANDARD.encode(other_signature.as_bytes());
        job.sha256 = hex::encode(sha2::Sha256::digest(&bytes));
        let target = job.target.clone();
        let old_content = std::fs::read_to_string(&target).unwrap();
        let job_file = dir.path().join("updates/job.json");
        std::fs::create_dir_all(job_file.parent().unwrap()).unwrap();
        std::fs::write(&job_file, serde_json::to_vec(&job).unwrap()).unwrap();

        let error = run_helper_with_options(
            &job_file,
            HelperOptions {
                health_timeout: Duration::from_secs(2),
                skip_signature_check: false,
                public_keys: vec![public_key],
                ..HelperOptions::default()
            },
        )
        .unwrap_err();
        assert!(matches!(error, UpdateError::Signature));
        assert_eq!(std::fs::read_to_string(&target).unwrap(), old_content);
        assert!(!job_file.exists());
        let result = LastResult::read(&UpdatePaths::new(dir.path())).unwrap();
        assert_eq!(result.status, LastStatus::Failed);
    }

    #[cfg(unix)]
    fn integration_job(dir: &Path, staged: &Path, token: &str) -> ApplyJob {
        let target = dir.join("app.AppImage");
        if !target.exists() {
            std::fs::write(&target, "#!/bin/sh\nexit 0\n").unwrap();
            make_executable(&target);
        }
        let bytes = std::fs::read(staged).unwrap();
        ApplyJob {
            schema: APPLY_SCHEMA,
            kind: ApplyKind::AppImage,
            from_version: "0.3.0".to_string(),
            to_version: "9.9.9".to_string(),
            target,
            staged: staged.to_path_buf(),
            signature: "c2ln".to_string(),
            sha256: hex::encode(sha2::Sha256::digest(&bytes)),
            ui_pid: dead_pid(),
            daemon_pid: None,
            restart_args: Vec::new(),
            health_token: token.to_string(),
            data_dir: dir.to_path_buf(),
            created_at: "2026-10-24T12:00:00Z".to_string(),
        }
    }

    #[cfg(unix)]
    #[test]
    fn applies_an_appimage_when_the_health_marker_appears() {
        let dir = tempfile::tempdir().unwrap();
        let token = "token-success";
        let marker_path = dir
            .path()
            .join("updates")
            .join(format!("health-{token}.json"));
        let marker_tmp = marker_path.with_extension("json.tmp");
        let new_script = dir.path().join("new.AppImage");
        std::fs::write(
            &new_script,
            format!(
                "#!/bin/sh\nmkdir -p '{}'\nprintf '%s' '{{\"version\":\"9.9.9\",\"mode\":\"normal\"}}' > '{}'\nmv '{}' '{}'\nsleep 2\n",
                marker_path.parent().unwrap().display(),
                marker_tmp.display(),
                marker_tmp.display(),
                marker_path.display()
            ),
        )
        .unwrap();
        make_executable(&new_script);

        let job = integration_job(dir.path(), &new_script, token);
        let new_content = std::fs::read_to_string(&new_script).unwrap();
        let job_file = dir.path().join("updates/job.json");
        std::fs::create_dir_all(job_file.parent().unwrap()).unwrap();
        std::fs::write(&job_file, serde_json::to_vec(&job).unwrap()).unwrap();
        let target = job.target.clone();

        run_helper_with_options(
            &job_file,
            HelperOptions {
                health_timeout: Duration::from_secs(5),
                skip_signature_check: true,
                ..HelperOptions::default()
            },
        )
        .unwrap();

        assert_eq!(std::fs::read_to_string(&target).unwrap(), new_content);
        assert!(target.with_file_name("app.AppImage.previous").exists());
        assert!(!job_file.exists());
        let result = LastResult::read(&UpdatePaths::new(dir.path())).unwrap();
        assert_eq!(result.status, LastStatus::Success);
    }

    #[cfg(unix)]
    #[test]
    fn apply_failure_keeps_the_job_for_manual_recovery() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());
        let staged = dir.path().join("new.AppImage");
        std::fs::write(&staged, "#!/bin/sh\nexit 0\n").unwrap();
        make_executable(&staged);
        let job = ApplyJob {
            schema: APPLY_SCHEMA,
            kind: ApplyKind::AppImage,
            from_version: "0.3.0".to_string(),
            to_version: "0.4.0".to_string(),
            target: dir.path().join("missing.AppImage"),
            staged: staged.clone(),
            signature: "c2ln".to_string(),
            sha256: hex::encode(sha2::Sha256::digest(std::fs::read(&staged).unwrap())),
            ui_pid: 0,
            daemon_pid: None,
            restart_args: Vec::new(),
            health_token: "token-apply-failure".to_string(),
            data_dir: dir.path().to_path_buf(),
            created_at: "2026-10-24T12:00:00Z".to_string(),
        };
        let job_file = dir.path().join("updates/job.json");
        std::fs::write(&job_file, serde_json::to_vec(&job).unwrap()).unwrap();

        let error = run_helper_with_options(
            &job_file,
            HelperOptions {
                skip_signature_check: true,
                ..HelperOptions::default()
            },
        )
        .unwrap_err();
        assert!(matches!(error, UpdateError::Io { .. }));
        assert!(job_file.exists());
        assert!(staged.exists());
        let result = LastResult::read(&paths).unwrap();
        assert_eq!(result.status, LastStatus::Failed);
    }

    #[cfg(unix)]
    #[test]
    fn keeps_the_job_and_records_failure_when_restore_fails() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());
        let target = dir.path().join("app.AppImage");
        std::fs::write(&target, "#!/bin/sh\nexit 0\n").unwrap();
        make_executable(&target);
        let staged = dir.path().join("new.AppImage");
        std::fs::write(&staged, b"new").unwrap();
        let job_path = paths.job_file();
        let job = ApplyJob {
            schema: APPLY_SCHEMA,
            kind: ApplyKind::AppImage,
            from_version: "0.3.0".to_string(),
            to_version: "0.4.0".to_string(),
            target,
            staged,
            signature: "c2ln".to_string(),
            sha256: "a".repeat(64),
            ui_pid: 0,
            daemon_pid: None,
            restart_args: Vec::new(),
            health_token: "token".to_string(),
            data_dir: dir.path().to_path_buf(),
            created_at: "2026-10-24T12:00:00Z".to_string(),
        };
        std::fs::write(&job_path, serde_json::to_vec(&job).unwrap()).unwrap();

        let backup = Backup::AppImage(dir.path().join("missing.previous"));
        let error = rollback_after_failure(
            &paths,
            &job,
            &job_path,
            &backup,
            &UpdateError::Other("health failed".to_string()),
        )
        .unwrap_err();
        assert!(matches!(error, UpdateError::Other(_)));
        assert!(
            job_path.exists(),
            "the job must be kept for manual recovery"
        );
        assert!(job.staged.exists());
        let result = LastResult::read(&paths).unwrap();
        assert_eq!(result.status, LastStatus::RolledBack);
        assert!(result.reason.unwrap().contains("could not be restored"));
    }

    #[cfg(unix)]
    #[test]
    fn a_live_lock_relaunches_the_ui_without_deleting_the_job() {
        let dir = tempfile::tempdir().unwrap();
        let sentinel = dir.path().join("old-ran");
        let target = dir.path().join("app.AppImage");
        std::fs::write(
            &target,
            format!("#!/bin/sh\nprintf old > '{}'\n", sentinel.display()),
        )
        .unwrap();
        make_executable(&target);
        let staged = dir.path().join("new.AppImage");
        std::fs::write(&staged, "#!/bin/sh\nexit 0\n").unwrap();
        make_executable(&staged);
        let job = integration_job(dir.path(), &staged, "token-lock");
        let job_file = dir.path().join("updates/job.json");
        std::fs::create_dir_all(job_file.parent().unwrap()).unwrap();
        std::fs::write(&job_file, serde_json::to_vec(&job).unwrap()).unwrap();
        let paths = paths(dir.path());
        std::fs::write(paths.lock_file(), std::process::id().to_string()).unwrap();

        let error = run_helper_with_options(&job_file, HelperOptions::default()).unwrap_err();
        assert!(matches!(error, UpdateError::Other(_)));
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while !sentinel.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(sentinel.exists());
        assert!(job_file.exists(), "the other helper's job must be kept");
        let _ = std::fs::remove_file(paths.lock_file());
    }

    #[cfg(unix)]
    #[test]
    fn rolls_back_when_the_new_version_writes_health_then_exits() {
        let dir = tempfile::tempdir().unwrap();
        let sentinel = dir.path().join("old-ran");
        let token = "token-quick-exit";
        let target = dir.path().join("app.AppImage");
        let old_script = format!("#!/bin/sh\nprintf old > '{}'\n", sentinel.display());
        std::fs::write(&target, &old_script).unwrap();
        make_executable(&target);

        let marker_path = dir
            .path()
            .join("updates")
            .join(format!("health-{token}.json"));
        let marker_tmp = marker_path.with_extension("json.tmp");
        let new_script = dir.path().join("new.AppImage");
        std::fs::write(
            &new_script,
            format!(
                "#!/bin/sh\nmkdir -p '{}'\nprintf '%s' '{{\"version\":\"9.9.9\",\"mode\":\"normal\"}}' > '{}'\nmv '{}' '{}'\n",
                marker_path.parent().unwrap().display(),
                marker_tmp.display(),
                marker_tmp.display(),
                marker_path.display()
            ),
        )
        .unwrap();
        make_executable(&new_script);

        let mut job = integration_job(dir.path(), &new_script, token);
        job.target = target.clone();
        let job_file = dir.path().join("updates/job.json");
        std::fs::create_dir_all(job_file.parent().unwrap()).unwrap();
        std::fs::write(&job_file, serde_json::to_vec(&job).unwrap()).unwrap();

        let error = run_helper_with_options(
            &job_file,
            HelperOptions {
                health_timeout: Duration::from_secs(3),
                skip_signature_check: true,
                ..HelperOptions::default()
            },
        )
        .unwrap_err();
        assert!(matches!(error, UpdateError::Other(_)));
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while !sentinel.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(sentinel.exists(), "the old target was not relaunched");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), old_script);
        let result = LastResult::read(&UpdatePaths::new(dir.path())).unwrap();
        assert_eq!(result.status, LastStatus::RolledBack);
    }

    #[cfg(unix)]
    #[test]
    fn restores_the_previous_appimage_when_the_new_one_exits() {
        let dir = tempfile::tempdir().unwrap();
        let sentinel = dir.path().join("old-ran");
        let token = "token-failure";
        let target = dir.path().join("app.AppImage");
        let old_script = format!("#!/bin/sh\nprintf old > '{}'\n", sentinel.display());
        std::fs::write(&target, &old_script).unwrap();
        make_executable(&target);

        let new_script = dir.path().join("new.AppImage");
        std::fs::write(&new_script, "#!/bin/sh\nexit 0\n").unwrap();
        make_executable(&new_script);

        let mut job = integration_job(dir.path(), &new_script, token);
        job.target = target.clone();
        let job_file = dir.path().join("updates/job.json");
        std::fs::create_dir_all(job_file.parent().unwrap()).unwrap();
        std::fs::write(&job_file, serde_json::to_vec(&job).unwrap()).unwrap();

        let error = run_helper_with_options(
            &job_file,
            HelperOptions {
                health_timeout: Duration::from_secs(2),
                skip_signature_check: true,
                ..HelperOptions::default()
            },
        )
        .unwrap_err();
        assert!(matches!(error, UpdateError::Other(_)));
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while !sentinel.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(sentinel.exists(), "the old target was not relaunched");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), old_script);
        let result = LastResult::read(&UpdatePaths::new(dir.path())).unwrap();
        assert_eq!(result.status, LastStatus::RolledBack);
        assert!(!job_file.exists());
    }
}
