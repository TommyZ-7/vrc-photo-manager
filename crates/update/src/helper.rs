use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use chrono::Utc;
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::{IoOperation, LastErrorCode, Result, UpdateError};
use crate::install::{ApplyJob, ApplyKind, PackageFamily, UpdatePaths};
use crate::manifest::{Manifest, PlatformAsset};
use crate::native::NativeString;
use crate::verify::{decode_signature_base64, effective_public_keys, verify_bytes, verify_file};

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
    /// Test-only `PKEXEC_UID` override.
    #[cfg(test)]
    pub pkexec_uid: Option<u32>,
    /// Test-only root temp directory override.
    #[cfg(test)]
    pub root_temp_dir: Option<PathBuf>,
    /// Test-only package manager paths used by the metadata and install steps.
    #[cfg(test)]
    pub pacman_path: Option<PathBuf>,
    #[cfg(test)]
    pub dpkg_path: Option<PathBuf>,
    #[cfg(test)]
    pub dpkg_deb_path: Option<PathBuf>,
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
            #[cfg(test)]
            pkexec_uid: None,
            #[cfg(test)]
            root_temp_dir: None,
            #[cfg(test)]
            pacman_path: None,
            #[cfg(test)]
            dpkg_path: None,
            #[cfg(test)]
            dpkg_deb_path: None,
        }
    }
}

/// Applies the job with production timeouts.
pub fn run_helper(job_path: &Path) -> Result<()> {
    run_helper_with_options(job_path, HelperOptions::default())
}

/// Whether the user-mode helper may run under this effective uid.
///
/// The polkit policy lets `/usr/bin/vrc-photo-updater` run with arbitrary
/// arguments, so a root `--apply` would turn the job file into an arbitrary
/// file replacement primitive.
pub fn user_mode_allowed(euid: u32) -> bool {
    euid != 0
}

/// Whether the privileged system package helper was started through `pkexec`.
pub fn system_mode_allowed(euid: u32, pkexec_uid: Option<u32>) -> bool {
    euid == 0 && pkexec_uid.is_some_and(|uid| uid > 0)
}

#[cfg(unix)]
pub fn effective_uid() -> u32 {
    // SAFETY: `geteuid` only reads the effective uid and cannot fail.
    unsafe { libc::geteuid() }
}

#[cfg(not(unix))]
pub fn effective_uid() -> u32 {
    1
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
                code: None,
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

    // System packages are installed by the privileged helper through pkexec.
    // The user-side helper only orchestrates verification, relaunch, and
    // health checks.
    if matches!(job.kind, ApplyKind::SystemPackage { .. }) {
        return run_system_package_apply(&paths, &job, job_path, &options);
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
                code: None,
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
                    code: None,
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
            code: None,
        },
    );

    let mut child = match launch(
        &job.target,
        &job.restart_args,
        Some(&job.health_token),
        &job.working_dir,
    ) {
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
        job.database.as_ref().map(NativeString::as_path),
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
            code: None,
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
            code: None,
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

/// The uid of the user that invoked the privileged helper through `pkexec`.
fn pkexec_uid(options: &HelperOptions) -> Result<u32> {
    #[cfg(test)]
    if let Some(uid) = options.pkexec_uid {
        return Ok(uid);
    }
    #[cfg(not(test))]
    let _ = options;
    std::env::var("PKEXEC_UID")
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
        .filter(|uid| *uid > 0)
        .ok_or(UpdateError::NotPrivileged)
}

/// Keys used to verify the signed manifest and the package signature.
fn verification_keys(options: &HelperOptions) -> Vec<String> {
    #[cfg(test)]
    if !options.public_keys.is_empty() {
        return options.public_keys.clone();
    }
    let _ = options;
    effective_public_keys()
}

/// The directory the verified package is copied into before installation.
fn root_temp_dir(options: &HelperOptions) -> PathBuf {
    #[cfg(test)]
    if let Some(dir) = &options.root_temp_dir {
        return dir.clone();
    }
    let _ = options;
    PathBuf::from("/var/tmp")
}

/// Rejects staging paths that are not a regular file owned by the invoking
/// user directly under `<data_dir>/updates/downloads/<version>/`.
#[cfg(unix)]
fn verify_staging(job: &ApplyJob, uid: u32) -> Result<()> {
    use std::os::unix::fs::MetadataExt;

    if job
        .staged
        .components()
        .any(|component| component == std::path::Component::ParentDir)
    {
        return Err(UpdateError::StagingRejected(
            "staged package path contains ..".to_string(),
        ));
    }

    let data_meta = std::fs::symlink_metadata(&job.data_dir)
        .map_err(|error| UpdateError::StagingRejected(error.to_string()))?;
    if data_meta.file_type().is_symlink() {
        return Err(UpdateError::StagingRejected(
            "data directory must not be a symlink".to_string(),
        ));
    }
    let data_dir = std::fs::canonicalize(&job.data_dir)
        .map_err(|error| UpdateError::StagingRejected(error.to_string()))?;
    let metadata = std::fs::metadata(&data_dir)
        .map_err(|error| UpdateError::StagingRejected(error.to_string()))?;
    if metadata.uid() != uid || metadata.mode() & 0o022 != 0 {
        return Err(UpdateError::StagingRejected(
            "data directory ownership or mode is invalid".to_string(),
        ));
    }

    let version = Version::parse(job.to_version.trim_start_matches('v'))
        .map_err(|error| UpdateError::StagingRejected(error.to_string()))?;
    let expected = data_dir.join("updates/downloads").join(version.to_string());
    let expected = std::fs::canonicalize(&expected)
        .map_err(|error| UpdateError::StagingRejected(error.to_string()))?;
    if !expected.starts_with(&data_dir) {
        return Err(UpdateError::StagingRejected(
            "downloads directory escapes the data directory".to_string(),
        ));
    }
    let expected_meta = std::fs::metadata(&expected)
        .map_err(|error| UpdateError::StagingRejected(error.to_string()))?;
    if expected_meta.uid() != uid || expected_meta.mode() & 0o022 != 0 {
        return Err(UpdateError::StagingRejected(
            "version directory ownership or mode is invalid".to_string(),
        ));
    }

    let staged_meta = std::fs::symlink_metadata(&job.staged)
        .map_err(|error| UpdateError::StagingRejected(error.to_string()))?;
    if staged_meta.file_type().is_symlink() {
        return Err(UpdateError::StagingRejected(
            "staged package must not be a symlink".to_string(),
        ));
    }
    let staged = std::fs::canonicalize(&job.staged)
        .map_err(|error| UpdateError::StagingRejected(error.to_string()))?;
    if !staged.starts_with(&expected) {
        return Err(UpdateError::StagingRejected(
            "staged package is outside the version directory".to_string(),
        ));
    }
    if staged.parent() != Some(expected.as_path()) {
        return Err(UpdateError::StagingRejected(
            "staged package must be directly under the version directory".to_string(),
        ));
    }
    let metadata = std::fs::metadata(&staged)
        .map_err(|error| UpdateError::StagingRejected(error.to_string()))?;
    if !metadata.is_file() || metadata.uid() != uid || metadata.mode() & 0o022 != 0 {
        return Err(UpdateError::StagingRejected(
            "staged package ownership or mode is invalid".to_string(),
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn verify_staging(_job: &ApplyJob, _uid: u32) -> Result<()> {
    Err(UpdateError::UnsupportedPlatform)
}

/// Verifies the signed manifest and returns the asset for the job's family.
fn verify_manifest(
    job: &ApplyJob,
    options: &HelperOptions,
) -> Result<(Manifest, Version, PlatformAsset)> {
    let bytes = job
        .signed_manifest
        .as_deref()
        .ok_or_else(|| UpdateError::InvalidJob("signed manifest is missing".to_string()))?;
    let signature = job
        .manifest_signature
        .as_deref()
        .ok_or_else(|| UpdateError::InvalidJob("manifest signature is missing".to_string()))?;
    let keys = verification_keys(options);
    verify_bytes(bytes.as_bytes(), signature, &keys)?;
    let allow_http = cfg!(debug_assertions);
    let (manifest, version) = Manifest::parse_and_validate(bytes.as_bytes(), allow_http)?;
    let job_version = Version::parse(job.to_version.trim_start_matches('v'))
        .map_err(|error| UpdateError::InvalidJob(error.to_string()))?;
    if version != job_version {
        return Err(UpdateError::InvalidJob(
            "manifest version does not match the job".to_string(),
        ));
    }
    let current = Version::parse(crate::CURRENT_VERSION)
        .map_err(|error| UpdateError::Other(error.to_string()))?;
    if version <= current {
        return Err(UpdateError::Downgrade {
            current: current.to_string(),
            target: version.to_string(),
        });
    }
    let family = match job.kind {
        ApplyKind::SystemPackage { family } => family,
        _ => {
            return Err(UpdateError::InvalidJob(
                "job is not a system package job".to_string(),
            ));
        }
    };
    let asset = manifest
        .system_package_for(family)
        .cloned()
        .ok_or_else(|| {
            UpdateError::InvalidManifest("system package asset is missing".to_string())
        })?;
    if asset.file_name()?
        != job
            .staged
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default()
    {
        return Err(UpdateError::InvalidJob(
            "manifest asset does not match the staged file name".to_string(),
        ));
    }
    if asset.size
        != std::fs::metadata(&job.staged)
            .map_err(|error| UpdateError::io(&job.staged, error))?
            .len()
    {
        return Err(UpdateError::PackageVerificationFailed(
            "staged package size does not match the manifest".to_string(),
        ));
    }
    Ok((manifest, version, asset))
}

/// A root-owned copy of the verified package, removed when dropped.
pub(crate) struct TempPackage {
    path: PathBuf,
}

impl TempPackage {
    fn create(dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(dir).map_err(|error| UpdateError::io(dir, error))?;
        let path = dir.join(format!("vrc-photo-update-{}.pkg", uuid::Uuid::new_v4()));
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|error| UpdateError::io(&path, error))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
                .map_err(|error| UpdateError::io(&path, error))?;
        }
        drop(file);
        Ok(Self { path })
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempPackage {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Copies the staged package to a root-owned temp file and re-verifies its
/// SHA-256 and minisign signature before any install command sees it.
fn copy_and_verify(
    job: &ApplyJob,
    asset: &PlatformAsset,
    options: &HelperOptions,
) -> Result<TempPackage> {
    let temp = TempPackage::create(&root_temp_dir(options))?;
    let mut source =
        std::fs::File::open(&job.staged).map_err(|error| UpdateError::io(&job.staged, error))?;
    let mut target = std::fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(temp.path())
        .map_err(|error| UpdateError::io(temp.path(), error))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = std::io::Read::read(&mut source, &mut buffer)
            .map_err(|error| UpdateError::io(&job.staged, error))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        std::io::Write::write_all(&mut target, &buffer[..read])
            .map_err(|error| UpdateError::io(temp.path(), error))?;
    }
    target
        .sync_all()
        .map_err(|error| UpdateError::io(temp.path(), error))?;
    let digest = hex::encode(hasher.finalize());
    if digest != asset.sha256 {
        return Err(UpdateError::PackageVerificationFailed(
            "staged package SHA-256 does not match the manifest".to_string(),
        ));
    }
    let signature = decode_signature_base64(&asset.signature)?;
    verify_file(temp.path(), &signature, &verification_keys(options))?;
    Ok(temp)
}

/// Verifies the staging directory, the signed manifest, and the package
/// signature, then returns the root-owned copy to install.
pub(crate) fn verify_system_package(
    job: &ApplyJob,
    options: &HelperOptions,
) -> Result<TempPackage> {
    let uid = pkexec_uid(options)?;
    verify_staging(job, uid)?;
    let (_, _, asset) = verify_manifest(job, options)?;
    copy_and_verify(job, &asset, options)
}

/// The package name and version reported by the package manager.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageMetadata {
    pub name: String,
    pub version: String,
}

/// Whether a package manager version matches the manifest version.
///
/// pacman appends `-pkgrel`, deb appends `-revision`, and both may carry an
/// epoch (`1:`) that is not part of the application version.
fn package_version_matches(raw: &str, expected: &str) -> bool {
    let raw = raw.split_once(':').map(|(_, value)| value).unwrap_or(raw);
    raw == expected
        || raw
            .strip_prefix(expected)
            .is_some_and(|rest| rest.starts_with('-'))
}

/// The package manager binaries used by the metadata and install steps.
fn pacman_program(options: &HelperOptions) -> PathBuf {
    #[cfg(test)]
    if let Some(path) = &options.pacman_path {
        return path.clone();
    }
    let _ = options;
    PathBuf::from("/usr/bin/pacman")
}

fn dpkg_program(options: &HelperOptions) -> PathBuf {
    #[cfg(test)]
    if let Some(path) = &options.dpkg_path {
        return path.clone();
    }
    let _ = options;
    PathBuf::from("/usr/bin/dpkg")
}

fn dpkg_deb_program(options: &HelperOptions) -> PathBuf {
    #[cfg(test)]
    if let Some(path) = &options.dpkg_deb_path {
        return path.clone();
    }
    let _ = options;
    PathBuf::from("/usr/bin/dpkg-deb")
}

fn read_package_metadata(
    family: PackageFamily,
    path: &Path,
    options: &HelperOptions,
) -> Result<PackageMetadata> {
    let (program, args) = match family {
        PackageFamily::Pacman => (
            pacman_program(options),
            vec!["-Qip".to_string(), path.display().to_string()],
        ),
        PackageFamily::Deb => (
            dpkg_deb_program(options),
            vec!["-f".to_string(), path.display().to_string()],
        ),
        PackageFamily::Unknown => return Err(UpdateError::PackageManagerUnavailable),
    };
    let output = Command::new(&program)
        .args(&args)
        .env("LC_ALL", "C")
        .output()
        .map_err(|error| UpdateError::io(&program, error))?;
    if !output.status.success() {
        return Err(UpdateError::PackageManagerFailed {
            family,
            status: output.status.to_string(),
            output: truncate_output(&output.stderr),
        });
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let name_key = if family == PackageFamily::Pacman {
        "Name"
    } else {
        "Package"
    };
    let name = parse_field(&text, name_key).ok_or_else(|| {
        UpdateError::PackageMetadataMismatch("package name is missing".to_string())
    })?;
    let version = parse_field(&text, "Version").ok_or_else(|| {
        UpdateError::PackageMetadataMismatch("package version is missing".to_string())
    })?;
    Ok(PackageMetadata { name, version })
}

fn parse_field(text: &str, key: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        (name.trim() == key).then(|| value.trim().to_string())
    })
}

fn truncate_output(bytes: &[u8]) -> String {
    const LIMIT: usize = 64 * 1024;
    String::from_utf8_lossy(&bytes[..bytes.len().min(LIMIT)]).to_string()
}

fn install_package(family: PackageFamily, path: &Path, options: &HelperOptions) -> Result<()> {
    let (program, args) = match family {
        PackageFamily::Pacman => (
            pacman_program(options),
            vec![
                "-U".to_string(),
                "--noconfirm".to_string(),
                path.display().to_string(),
            ],
        ),
        PackageFamily::Deb => (
            dpkg_program(options),
            vec!["-i".to_string(), path.display().to_string()],
        ),
        PackageFamily::Unknown => return Err(UpdateError::PackageManagerUnavailable),
    };
    let mut command = Command::new(&program);
    command.args(&args).env("LC_ALL", "C");
    if family == PackageFamily::Deb {
        command.env("DEBIAN_FRONTEND", "noninteractive");
    }
    let output = command
        .output()
        .map_err(|error| UpdateError::io(&program, error))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    if is_lock_error(family, &stderr) {
        return Err(UpdateError::PackageManagerLocked);
    }
    Err(UpdateError::PackageManagerFailed {
        family,
        status: output.status.to_string(),
        output: truncate_output(&output.stderr),
    })
}

fn is_lock_error(family: PackageFamily, stderr: &str) -> bool {
    let lower = stderr.to_ascii_lowercase();
    match family {
        PackageFamily::Pacman => {
            lower.contains("unable to lock database")
                || lower.contains("failed to init transaction")
                || lower.contains("could not lock database")
        }
        PackageFamily::Deb => {
            lower.contains("unable to lock the administration directory")
                || lower.contains("frontend lock was locked")
                || lower.contains("is another process using it")
        }
        PackageFamily::Unknown => false,
    }
}

/// Reads and validates the job, then verifies and installs the package.
pub fn run_system_package(job_path: &Path, options: &HelperOptions) -> Result<()> {
    let bytes = std::fs::read(job_path)
        .map_err(|error| UpdateError::io_at(IoOperation::Job, job_path, error))?;
    let job: ApplyJob = serde_json::from_slice(&bytes)
        .map_err(|error| UpdateError::InvalidJob(format!("cannot parse apply job: {error}")))?;
    job.validate()?;
    run_system_package_from_job(&job, options)
}

pub(crate) fn run_system_package_from_job(job: &ApplyJob, options: &HelperOptions) -> Result<()> {
    let temp = verify_system_package(job, options)?;
    let family = match job.kind {
        ApplyKind::SystemPackage { family } => family,
        _ => {
            return Err(UpdateError::InvalidJob(
                "not a system package job".to_string(),
            ));
        }
    };
    let metadata = read_package_metadata(family, temp.path(), options)?;
    if metadata.name != "vrc-photo-manager" {
        return Err(UpdateError::PackageMetadataMismatch(format!(
            "unexpected package name {}",
            metadata.name
        )));
    }
    let expected = job.to_version.trim_start_matches('v');
    if !package_version_matches(&metadata.version, expected) {
        return Err(UpdateError::PackageMetadataMismatch(format!(
            "package version {} does not match {expected}",
            metadata.version
        )));
    }
    install_package(family, temp.path(), options)
}

/// Maps a `pkexec` exit status to a user-facing failure code and reason.
fn map_pkexec_status(status: std::process::ExitStatus, stderr: &str) -> (LastErrorCode, String) {
    let code = match status.code() {
        Some(126) => LastErrorCode::AuthCancelled,
        Some(127) => LastErrorCode::AuthUnavailable,
        Some(14) => LastErrorCode::PackageManagerLocked,
        Some(11) => LastErrorCode::PackageVerificationFailed,
        Some(12) => LastErrorCode::PackageMetadataMismatch,
        Some(13) => LastErrorCode::Downgrade,
        Some(15) => LastErrorCode::PackageManagerFailed,
        Some(10) => LastErrorCode::HelperFailed,
        _ => LastErrorCode::HelperFailed,
    };
    (code, truncate_output(stderr.as_bytes()))
}

/// Whether a failure happened before the package manager changed anything.
///
/// Those failures remove the job and staged package. Failures after the
/// install (`PackageManagerFailed`, health checks) keep them for investigation.
fn is_preinstall_failure(code: LastErrorCode) -> bool {
    matches!(
        code,
        LastErrorCode::AuthCancelled
            | LastErrorCode::AuthUnavailable
            | LastErrorCode::PackageManagerLocked
            | LastErrorCode::PackageVerificationFailed
            | LastErrorCode::PackageMetadataMismatch
            | LastErrorCode::Downgrade
            | LastErrorCode::HelperFailed
    )
}

/// Maps a failed privileged helper exit to a user-side result, cleaning up only
/// when the failure happened before the package manager changed anything.
fn handle_system_package_exit(
    paths: &UpdatePaths,
    job: &ApplyJob,
    job_path: &Path,
    status: std::process::ExitStatus,
    stderr: &str,
) -> Result<()> {
    let (code, reason) = map_pkexec_status(status, stderr);
    fail_system_package(
        paths,
        job,
        job_path,
        code,
        reason,
        is_preinstall_failure(code),
    )
}

/// Verifies the staged package, installs it through `pkexec`, then launches the
/// new version and waits for its health marker.
///
/// System packages are never rolled back automatically: the package manager
/// owns the files and the previous package may no longer be cached.
fn run_system_package_apply(
    paths: &UpdatePaths,
    job: &ApplyJob,
    job_path: &Path,
    options: &HelperOptions,
) -> Result<()> {
    if let Err(error) = verify_staged(job, options) {
        log(
            paths,
            &format!("staged package verification failed: {error}"),
        );
        return fail_system_package(
            paths,
            job,
            job_path,
            LastErrorCode::PackageVerificationFailed,
            error.to_string(),
            true,
        );
    }
    if !crate::install::pkexec_available() {
        return fail_system_package(
            paths,
            job,
            job_path,
            LastErrorCode::AuthUnavailable,
            "pkexec is missing".to_string(),
            true,
        );
    }
    if !Path::new(crate::install::SYSTEM_HELPER_PATH).is_file() {
        return fail_system_package(
            paths,
            job,
            job_path,
            LastErrorCode::HelperFailed,
            "system update helper is missing".to_string(),
            true,
        );
    }

    log(
        paths,
        &format!(
            "running the system package helper through pkexec: {}",
            job_path.display()
        ),
    );
    let output = Command::new("pkexec")
        .arg("--disable-internal-agent")
        .arg(crate::install::SYSTEM_HELPER_PATH)
        .arg("--apply-system-package")
        .arg(job_path)
        .env("LC_ALL", "C")
        .output();
    let output = match output {
        Ok(output) => output,
        Err(error) => {
            return fail_system_package(
                paths,
                job,
                job_path,
                LastErrorCode::AuthUnavailable,
                format!("cannot run pkexec: {error}"),
                true,
            );
        }
    };
    log(
        paths,
        &format!(
            "system package helper exit: {:?}\n{}",
            output.status.code(),
            truncate_output(&output.stderr)
        ),
    );
    if !output.status.success() {
        return handle_system_package_exit(
            paths,
            job,
            job_path,
            output.status,
            &String::from_utf8_lossy(&output.stderr),
        );
    }

    let mut child = match launch(
        &job.target,
        &job.restart_args,
        Some(&job.health_token),
        &job.working_dir,
    ) {
        Ok(child) => child,
        Err(error) => {
            log(paths, &format!("cannot launch the new version: {error}"));
            // The package is installed but the new UI did not start. Keep the
            // job, staged package, and log for investigation.
            let reason = format!(
                "{error} (job: {}, staged: {}, log: {})",
                job_path.display(),
                job.staged.display(),
                paths.log_file().display()
            );
            fail_system_package_after_install(paths, job, reason);
            return Err(error);
        }
    };

    match wait_for_health(
        paths,
        &job.health_token,
        &job.to_version,
        job.database.as_ref().map(NativeString::as_path),
        &mut child,
        options.health_timeout,
    ) {
        Ok(()) => {
            log(paths, "system package update healthy");
            let _ = std::fs::remove_file(paths.health_file(&job.health_token));
            let _ = LastResult::write(
                paths,
                &LastResult {
                    status: LastStatus::Success,
                    from: job.from_version.clone(),
                    to: job.to_version.clone(),
                    reason: None,
                    code: None,
                },
            );
            cleanup(paths, job, job_path);
            Ok(())
        }
        Err(error) => {
            log(paths, &format!("health check failed: {error}"));
            let _ = child.kill();
            let _ = child.wait();
            fail_system_package_after_install(paths, job, error.to_string());
            Err(error)
        }
    }
}

/// Writes the failure result, relaunches the target, and optionally removes the
/// job and staged package.
fn fail_system_package(
    paths: &UpdatePaths,
    job: &ApplyJob,
    job_path: &Path,
    code: LastErrorCode,
    reason: String,
    cleanup_files: bool,
) -> Result<()> {
    log(
        paths,
        &format!("system package update failed ({code:?}): {reason}"),
    );
    let _ = LastResult::write(
        paths,
        &LastResult {
            status: LastStatus::Failed,
            from: job.from_version.clone(),
            to: job.to_version.clone(),
            reason: Some(reason.clone()),
            code: Some(code),
        },
    );
    relaunch_target(paths, job);
    if cleanup_files {
        cleanup(paths, job, job_path);
    }
    Err(UpdateError::Other(reason))
}

/// Records a launch or health failure after a successful install.
///
/// The package is already installed, so the job, staged package, and log are
/// kept for investigation. The target is relaunched without a health token
/// because the new version did not become healthy.
fn fail_system_package_after_install(paths: &UpdatePaths, job: &ApplyJob, reason: String) {
    log(
        paths,
        &format!("system package update failed after install: {reason}"),
    );
    let _ = LastResult::write(
        paths,
        &LastResult {
            status: LastStatus::Failed,
            from: job.from_version.clone(),
            to: job.to_version.clone(),
            reason: Some(reason),
            code: Some(LastErrorCode::HealthCheckFailed),
        },
    );
    relaunch_target(paths, job);
}

/// Launches the target again without a health token after a failed update.
fn relaunch_target(paths: &UpdatePaths, job: &ApplyJob) {
    if let Err(error) = launch(&job.target, &job.restart_args, None, &job.working_dir) {
        log(paths, &format!("cannot relaunch the target: {error}"));
    }
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
        ApplyKind::SystemPackage { .. } => Err(UpdateError::InvalidJob(
            "system package jobs are handled by the privileged helper".to_string(),
        )),
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

fn launch_command(
    target: &Path,
    args: &[NativeString],
    health_token: Option<&str>,
    working_dir: &Path,
) -> Command {
    let mut command = Command::new(target);
    command
        .args(args.iter().map(NativeString::as_os_str))
        .current_dir(working_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(token) = health_token {
        command.env("VRC_PHOTO_HEALTH_TOKEN", token);
    }
    command
}

fn launch(
    target: &Path,
    args: &[NativeString],
    health_token: Option<&str>,
    working_dir: &Path,
) -> Result<Child> {
    launch_command(target, args, health_token, working_dir)
        .spawn()
        .map_err(|error| UpdateError::io_at(IoOperation::Spawn, target, error))
}

fn relaunch_old(paths: &UpdatePaths, job: &ApplyJob) {
    if let Err(error) = launch(&job.target, &job.restart_args, None, &job.working_dir) {
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
    expected_database: Option<&Path>,
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
                let database_matches = expected_database.is_none_or(|expected| {
                    marker.database.as_ref().map(NativeString::as_path) == Some(expected)
                });
                if !database_matches {
                    return Err(UpdateError::Other(
                        "health marker was written for a different database".to_string(),
                    ));
                }
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
    /// Machine-readable reason for a failure; older files have no code.
    #[serde(default)]
    pub code: Option<LastErrorCode>,
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
    #[serde(default)]
    pub database: Option<NativeString>,
}

pub fn write_health_marker(
    paths: &UpdatePaths,
    token: &str,
    version: &str,
    database: Option<&Path>,
) -> Result<()> {
    let marker = HealthMarker {
        version: version.to_string(),
        mode: "normal".to_string(),
        database: database.map(|path| NativeString::new(path.as_os_str())),
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
    replace_appimage_impl(target, staged, &sync_parent_directory)
}

fn replace_appimage_impl(
    target: &Path,
    staged: &Path,
    sync_parent: &dyn Fn(&Path) -> Result<()>,
) -> Result<PathBuf> {
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
        let parent = target
            .parent()
            .ok_or_else(|| UpdateError::InvalidJob("target has no parent".to_string()))?;
        sync_parent(parent)?;
        Ok(())
    })();

    if let Err(error) = result {
        let _ = std::fs::remove_file(target);
        let _ = std::fs::rename(&backup, target);
        return Err(error);
    }
    Ok(backup)
}

#[cfg(unix)]
fn sync_parent_directory(path: &Path) -> Result<()> {
    let dir =
        File::open(path).map_err(|error| UpdateError::io_at(IoOperation::Apply, path, error))?;
    dir.sync_all()
        .map_err(|error| UpdateError::io_at(IoOperation::Apply, path, error))
}

#[cfg(not(unix))]
fn sync_parent_directory(_path: &Path) -> Result<()> {
    Ok(())
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
    use crate::install::{APPLY_SCHEMA, ApplyJob, ApplyKind, PackageFamily};

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
    fn a_parent_directory_sync_failure_restores_the_previous_appimage() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("app.AppImage");
        let staged = dir.path().join("new.AppImage");
        std::fs::write(&target, b"old").unwrap();
        std::fs::write(&staged, b"new").unwrap();

        let error = replace_appimage_impl(&target, &staged, &|path| {
            Err(UpdateError::io_at(
                crate::error::IoOperation::Apply,
                path,
                std::io::Error::other("sync failed"),
            ))
        })
        .unwrap_err();

        assert!(matches!(
            error,
            UpdateError::Io {
                operation: crate::error::IoOperation::Apply,
                ..
            }
        ));
        assert_eq!(std::fs::read(&target).unwrap(), b"old");
        assert!(
            !dir.path().join("app.AppImage.previous").exists(),
            "the rollback renames .previous back to the target"
        );
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
            code: None,
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
        write_health_marker(&paths, "token-1", "0.4.0", Some(dir.path())).unwrap();
        let marker = read_health_marker(&paths, "token-1").unwrap().unwrap();
        assert_eq!(marker.version, "0.4.0");
        assert_eq!(marker.mode, "normal");
        assert_eq!(
            marker.database.as_ref().map(NativeString::as_path),
            Some(dir.path())
        );
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

    #[test]
    fn launch_uses_the_working_directory_and_native_arguments() {
        let dir = tempfile::tempdir().unwrap();
        let args = vec![NativeString::new("--demo")];
        let command = launch_command(Path::new("/bin/true"), &args, Some("token"), dir.path());
        assert_eq!(command.get_current_dir(), Some(dir.path()));
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            vec![std::ffi::OsStr::new("--demo")]
        );
        assert!(command.get_envs().any(|(key, value)| {
            key == std::ffi::OsStr::new("VRC_PHOTO_HEALTH_TOKEN")
                && value == Some(std::ffi::OsStr::new("token"))
        }));
    }

    #[cfg(unix)]
    #[test]
    fn health_marker_must_match_the_database() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());
        let db = dir.path().join("app.db");
        let other = dir.path().join("other.db");
        std::fs::write(&db, b"db").unwrap();
        std::fs::write(&other, b"db").unwrap();
        write_health_marker(&paths, "db-token", "9.9.9", Some(&db)).unwrap();
        let marker = read_health_marker(&paths, "db-token").unwrap().unwrap();
        assert_eq!(
            marker.database.as_ref().map(NativeString::as_path),
            Some(db.as_path())
        );

        let mut child = std::process::Command::new("sleep")
            .arg("2")
            .spawn()
            .unwrap();
        assert!(
            wait_for_health(
                &paths,
                "db-token",
                "9.9.9",
                Some(&other),
                &mut child,
                Duration::from_millis(100)
            )
            .is_err()
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
                "db-token",
                "9.9.9",
                Some(&db),
                &mut child,
                Duration::from_millis(100)
            )
            .is_ok()
        );
        let _ = child.kill();
        let _ = child.wait();
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

        write_health_marker(&paths, "token", "9.9.9", None).unwrap();
        assert!(
            wait_for_health(
                &paths,
                "token",
                "9.9.9",
                None,
                &mut child,
                Duration::from_secs(1)
            )
            .is_ok()
        );
        let _ = child.kill();
        let _ = child.wait();

        write_health_marker(&paths, "wrong", "8.8.8", None).unwrap();
        let mut child = std::process::Command::new("sleep")
            .arg("2")
            .spawn()
            .unwrap();
        assert!(
            wait_for_health(
                &paths,
                "wrong",
                "9.9.9",
                None,
                &mut child,
                Duration::from_secs(1)
            )
            .is_err()
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
                None,
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
    fn write_script(path: &Path, body: &str) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(path, body).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[cfg(unix)]
    fn fake_pacman(dir: &Path, metadata: &str, install_exit: i32, install_stderr: &str) -> PathBuf {
        let path = dir.join("pacman");
        write_script(
            &path,
            &format!(
                "#!/bin/sh\nif [ \"$1\" = \"-Qip\" ]; then\necho \"$@\" >> \"{metadata_log}\"\ncat <<'EOF'\n{metadata}\nEOF\nexit 0\nfi\nif [ \"$1\" = \"-U\" ]; then\necho \"$@\" >> \"{log}\"\necho \"{install_stderr}\" >&2\nexit {install_exit}\nfi\nexit 2\n",
                metadata_log = dir.join("pacman-metadata-args.log").display(),
                log = dir.join("pacman-args.log").display(),
            ),
        );
        path
    }

    #[cfg(unix)]
    fn fake_dpkg_deb(dir: &Path, metadata: &str) -> PathBuf {
        let path = dir.join("dpkg-deb");
        write_script(
            &path,
            &format!(
                "#!/bin/sh\necho \"$@\" >> \"{log}\"\ncat <<'EOF'\n{metadata}\nEOF\n",
                log = dir.join("dpkg-deb-args.log").display(),
            ),
        );
        path
    }

    #[cfg(unix)]
    fn fake_dpkg(dir: &Path, install_exit: i32, install_stderr: &str) -> PathBuf {
        let path = dir.join("dpkg");
        write_script(
            &path,
            &format!(
                "#!/bin/sh\necho \"$@\" >> \"{log}\"\necho \"{install_stderr}\" >&2\nexit {install_exit}\n",
                log = dir.join("dpkg-args.log").display(),
            ),
        );
        path
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
            working_dir: dir.to_path_buf(),
            database: None,
            health_token: token.to_string(),
            data_dir: dir.to_path_buf(),
            created_at: "2026-10-24T12:00:00Z".to_string(),
            signed_manifest: None,
            manifest_signature: None,
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
            working_dir: dir.path().to_path_buf(),
            database: None,
            health_token: "token-apply-failure".to_string(),
            data_dir: dir.path().to_path_buf(),
            created_at: "2026-10-24T12:00:00Z".to_string(),
            signed_manifest: None,
            manifest_signature: None,
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
            working_dir: dir.path().to_path_buf(),
            database: None,
            health_token: "token".to_string(),
            data_dir: dir.path().to_path_buf(),
            created_at: "2026-10-24T12:00:00Z".to_string(),
            signed_manifest: None,
            manifest_signature: None,
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

    #[cfg(unix)]
    fn next_version() -> String {
        let mut version = semver::Version::parse(crate::CURRENT_VERSION).unwrap();
        version.patch += 1;
        version.to_string()
    }

    #[cfg(unix)]
    fn system_package_fixture(dir: &Path, version: &str) -> (ApplyJob, HelperOptions) {
        system_package_fixture_for(PackageFamily::Pacman, dir, version)
    }

    #[cfg(unix)]
    fn system_package_fixture_for(
        family: PackageFamily,
        dir: &Path,
        version: &str,
    ) -> (ApplyJob, HelperOptions) {
        use base64::Engine as _;
        use minisign::KeyPair;
        use sha2::Sha256;
        use std::os::unix::fs::MetadataExt;

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
        let (manifest_key, package, file_name) = match family {
            PackageFamily::Pacman => (
                "pacman",
                &b"fake pacman package"[..],
                format!("vrc-photo-manager-{version}-1-x86_64.pkg.tar.zst"),
            ),
            PackageFamily::Deb => (
                "deb",
                &b"fake deb package"[..],
                format!("vrc-photo-manager_{version}_amd64.deb"),
            ),
            PackageFamily::Unknown => unreachable!(),
        };
        let asset_signature =
            minisign::sign(None, &keypair.sk, std::io::Cursor::new(package), None, None)
                .unwrap()
                .into_string();
        let mut system_packages = serde_json::Map::new();
        system_packages.insert(
            manifest_key.to_string(),
            serde_json::json!({
                "url": format!("https://example.com/{file_name}"),
                "format": manifest_key,
                "size": package.len(),
                "sha256": hex::encode(Sha256::digest(package)),
                "signature": base64::engine::general_purpose::STANDARD
                    .encode(asset_signature.as_bytes())
            }),
        );
        let manifest = serde_json::to_vec(&serde_json::json!({
            "schema": 1,
            "version": version,
            "platforms": {},
            "system_packages": system_packages,
        }))
        .unwrap();
        let manifest_signature = minisign::sign(
            None,
            &keypair.sk,
            std::io::Cursor::new(manifest.as_slice()),
            None,
            None,
        )
        .unwrap()
        .into_string();

        let data_dir = dir.join("data");
        let staged = data_dir
            .join(format!("updates/downloads/{version}"))
            .join(file_name);
        std::fs::create_dir_all(staged.parent().unwrap()).unwrap();
        std::fs::write(&staged, package).unwrap();
        let job = ApplyJob {
            schema: APPLY_SCHEMA,
            kind: ApplyKind::SystemPackage { family },
            from_version: "0.3.0".to_string(),
            to_version: version.to_string(),
            target: "/usr/bin/vrc-photo-manager".into(),
            staged,
            signature: base64::engine::general_purpose::STANDARD.encode(asset_signature.as_bytes()),
            sha256: hex::encode(Sha256::digest(package)),
            ui_pid: 1,
            daemon_pid: Some(2),
            restart_args: Vec::new(),
            working_dir: dir.to_path_buf(),
            database: None,
            health_token: "abc-123".to_string(),
            data_dir,
            created_at: "2026-10-24T12:00:00Z".to_string(),
            signed_manifest: Some(String::from_utf8(manifest).unwrap()),
            manifest_signature: Some(manifest_signature),
        };
        let options = HelperOptions {
            public_keys: vec![public_key],
            pkexec_uid: Some(std::fs::metadata(dir).unwrap().uid()),
            root_temp_dir: Some(dir.join("root-temp")),
            ..HelperOptions::default()
        };
        std::fs::create_dir_all(options.root_temp_dir.as_ref().unwrap()).unwrap();
        (job, options)
    }

    #[cfg(unix)]
    #[test]
    fn system_package_rejects_a_bad_manifest_signature() {
        let dir = tempfile::tempdir().unwrap();
        let (job, mut options) = system_package_fixture(dir.path(), &next_version());
        options.public_keys =
            vec!["RWQf6LRCGA9i53mlYecO4IzT51TGPpvWucNSCh1CBM0QTaLn73Y7GFO3".to_string()];
        assert!(matches!(
            verify_system_package(&job, &options),
            Err(UpdateError::Signature)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn system_package_rejects_a_manifest_version_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let (mut job, options) = system_package_fixture(dir.path(), &next_version());
        let other = "0.0.1";
        let other_dir = job.data_dir.join("updates/downloads").join(other);
        std::fs::create_dir_all(&other_dir).unwrap();
        let staged = other_dir.join(job.staged.file_name().unwrap());
        std::fs::copy(&job.staged, &staged).unwrap();
        job.staged = staged;
        job.to_version = other.to_string();
        assert!(matches!(
            verify_system_package(&job, &options),
            Err(UpdateError::InvalidJob(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn system_package_rejects_a_downgrade() {
        let dir = tempfile::tempdir().unwrap();
        let (job, options) = system_package_fixture(dir.path(), crate::CURRENT_VERSION);
        assert!(matches!(
            verify_system_package(&job, &options),
            Err(UpdateError::Downgrade { .. })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn system_package_rejects_a_staged_file_outside_the_download_dir() {
        let dir = tempfile::tempdir().unwrap();
        let (mut job, options) = system_package_fixture(dir.path(), &next_version());
        job.staged = dir.path().join("outside.pkg.tar.zst");
        std::fs::write(&job.staged, b"fake pacman package").unwrap();
        assert!(matches!(
            verify_system_package(&job, &options),
            Err(UpdateError::StagingRejected(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn system_package_rejects_a_nested_staged_file() {
        let dir = tempfile::tempdir().unwrap();
        let (mut job, options) = system_package_fixture(dir.path(), &next_version());
        let nested_dir = job.staged.parent().unwrap().join("nested");
        std::fs::create_dir_all(&nested_dir).unwrap();
        let nested = nested_dir.join(job.staged.file_name().unwrap());
        std::fs::copy(&job.staged, &nested).unwrap();
        job.staged = nested;
        assert!(matches!(
            verify_system_package(&job, &options),
            Err(UpdateError::StagingRejected(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn system_package_rejects_a_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let (job, options) = system_package_fixture(dir.path(), &next_version());
        let link = job.staged.with_file_name("link.pkg.tar.zst");
        std::os::unix::fs::symlink(&job.staged, &link).unwrap();
        let mut job = job;
        job.staged = link;
        assert!(matches!(
            verify_system_package(&job, &options),
            Err(UpdateError::StagingRejected(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn system_package_rejects_a_tampered_staged_file() {
        let dir = tempfile::tempdir().unwrap();
        let (job, options) = system_package_fixture(dir.path(), &next_version());
        let mut tampered = std::fs::read(&job.staged).unwrap();
        *tampered.last_mut().unwrap() ^= 0xff;
        std::fs::write(&job.staged, &tampered).unwrap();
        assert_eq!(
            tampered.len(),
            std::fs::metadata(&job.staged).unwrap().len() as usize,
            "the tampered file must keep the manifest size to reach the hash re-check"
        );
        assert!(matches!(
            verify_system_package(&job, &options),
            Err(UpdateError::PackageVerificationFailed(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn system_package_rejects_a_staged_file_with_a_wrong_size() {
        let dir = tempfile::tempdir().unwrap();
        let (job, options) = system_package_fixture(dir.path(), &next_version());
        let mut bytes = std::fs::read(&job.staged).unwrap();
        bytes.extend_from_slice(b"extra");
        std::fs::write(&job.staged, bytes).unwrap();
        assert!(matches!(
            verify_system_package(&job, &options),
            Err(UpdateError::PackageVerificationFailed(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn system_package_copies_to_a_root_temp_file_and_removes_it() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let (job, options) = system_package_fixture(dir.path(), &next_version());
        let root_temp = options.root_temp_dir.clone().unwrap();
        let temp = verify_system_package(&job, &options).unwrap();
        let path = temp.path().to_path_buf();
        assert!(path.starts_with(&root_temp));
        assert_eq!(std::fs::read(&path).unwrap(), b"fake pacman package");
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        drop(temp);
        assert!(!path.exists(), "the temp package must be removed on drop");
    }

    #[test]
    fn package_version_normalization_accepts_revisions() {
        assert!(package_version_matches("0.4.0", "0.4.0"));
        assert!(package_version_matches("0.4.0-1", "0.4.0"));
        assert!(package_version_matches("1:0.4.0-1ubuntu1", "0.4.0"));
        assert!(!package_version_matches("0.4.1-1", "0.4.0"));

        assert!(package_version_matches("0.9.3", "0.9.3"));
        assert!(package_version_matches("0.9.3-1", "0.9.3"));
        assert!(package_version_matches("1:0.9.3-1ubuntu1", "0.9.3"));
        assert!(!package_version_matches("0.9.30-1", "0.9.3"));
    }

    #[cfg(unix)]
    #[test]
    fn system_package_rejects_a_package_name_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let version = next_version();
        let (job, mut options) = system_package_fixture(dir.path(), &version);
        options.pacman_path = Some(fake_pacman(
            dir.path(),
            &format!("Name            : evil\nVersion         : {version}-1"),
            0,
            "",
        ));
        let error = run_system_package_from_job(&job, &options).unwrap_err();
        assert!(matches!(&error, UpdateError::PackageMetadataMismatch(_)));
        assert!(
            error.to_string().contains("unexpected package name"),
            "the name check must reject the package before the version check: {error}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn system_package_rejects_a_package_version_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let (job, mut options) = system_package_fixture(dir.path(), &next_version());
        options.pacman_path = Some(fake_pacman(
            dir.path(),
            "Name            : vrc-photo-manager\nVersion         : 0.3.0-1",
            0,
            "",
        ));
        assert!(matches!(
            run_system_package_from_job(&job, &options),
            Err(UpdateError::PackageMetadataMismatch(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn system_package_maps_pacman_lock_to_package_manager_locked() {
        let dir = tempfile::tempdir().unwrap();
        let version = next_version();
        let (job, mut options) = system_package_fixture(dir.path(), &version);
        let metadata =
            format!("Name            : vrc-photo-manager\nVersion         : {version}-1");
        options.pacman_path = Some(fake_pacman(
            dir.path(),
            &metadata,
            1,
            "error: failed to init transaction (unable to lock database)",
        ));
        assert!(matches!(
            run_system_package_from_job(&job, &options),
            Err(UpdateError::PackageManagerLocked)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn system_package_maps_pacman_failure_to_package_manager_failed() {
        let dir = tempfile::tempdir().unwrap();
        let version = next_version();
        let (job, mut options) = system_package_fixture(dir.path(), &version);
        let metadata =
            format!("Name            : vrc-photo-manager\nVersion         : {version}-1");
        options.pacman_path = Some(fake_pacman(
            dir.path(),
            &metadata,
            1,
            "error: failed to commit transaction (conflicting files)",
        ));
        assert!(matches!(
            run_system_package_from_job(&job, &options),
            Err(UpdateError::PackageManagerFailed {
                family: PackageFamily::Pacman,
                ..
            })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn system_package_runs_the_fixed_pacman_command() {
        let dir = tempfile::tempdir().unwrap();
        let version = next_version();
        let (job, mut options) = system_package_fixture(dir.path(), &version);
        let metadata =
            format!("Name            : vrc-photo-manager\nVersion         : {version}-1");
        options.pacman_path = Some(fake_pacman(dir.path(), &metadata, 0, ""));
        run_system_package_from_job(&job, &options).unwrap();
        let metadata_args =
            std::fs::read_to_string(dir.path().join("pacman-metadata-args.log")).unwrap();
        assert!(metadata_args.contains("-Qip"));
        assert!(metadata_args.contains("vrc-photo-update-"));
        assert!(!metadata_args.contains("downloads"));
        let args = std::fs::read_to_string(dir.path().join("pacman-args.log")).unwrap();
        assert!(args.contains("-U --noconfirm"));
        assert!(args.contains("vrc-photo-update-"));
    }

    #[cfg(unix)]
    #[test]
    fn system_package_runs_from_a_job_file() {
        let dir = tempfile::tempdir().unwrap();
        let version = next_version();
        let (job, mut options) = system_package_fixture(dir.path(), &version);
        let metadata =
            format!("Name            : vrc-photo-manager\nVersion         : {version}-1");
        options.pacman_path = Some(fake_pacman(dir.path(), &metadata, 0, ""));
        let job_file = dir.path().join("job.json");
        std::fs::write(&job_file, serde_json::to_vec(&job).unwrap()).unwrap();
        run_system_package(&job_file, &options).unwrap();
        let metadata_args =
            std::fs::read_to_string(dir.path().join("pacman-metadata-args.log")).unwrap();
        assert!(metadata_args.contains("-Qip"));
        assert!(metadata_args.contains("vrc-photo-update-"));
        assert!(!metadata_args.contains("downloads"));
        let args = std::fs::read_to_string(dir.path().join("pacman-args.log")).unwrap();
        assert!(args.contains("-U --noconfirm"));
        assert!(args.contains("vrc-photo-update-"));
    }

    #[cfg(unix)]
    #[test]
    fn system_package_runs_the_fixed_dpkg_command() {
        let dir = tempfile::tempdir().unwrap();
        let version = next_version();
        let (job, mut options) =
            system_package_fixture_for(PackageFamily::Deb, dir.path(), &version);
        let metadata = format!("Package: vrc-photo-manager\nVersion: {version}");
        options.dpkg_deb_path = Some(fake_dpkg_deb(dir.path(), &metadata));
        options.dpkg_path = Some(fake_dpkg(dir.path(), 0, ""));
        run_system_package_from_job(&job, &options).unwrap();
        let metadata_args = std::fs::read_to_string(dir.path().join("dpkg-deb-args.log")).unwrap();
        assert!(metadata_args.contains("-f"));
        assert!(metadata_args.contains("vrc-photo-update-"));
        assert!(!metadata_args.contains("downloads"));
        let args = std::fs::read_to_string(dir.path().join("dpkg-args.log")).unwrap();
        assert!(args.contains("-i "));
        assert!(args.contains("vrc-photo-update-"));
    }

    #[cfg(unix)]
    #[test]
    fn system_package_maps_dpkg_lock_to_package_manager_locked() {
        let dir = tempfile::tempdir().unwrap();
        let version = next_version();
        let (job, mut options) =
            system_package_fixture_for(PackageFamily::Deb, dir.path(), &version);
        let metadata = format!("Package: vrc-photo-manager\nVersion: {version}-1");
        options.dpkg_deb_path = Some(fake_dpkg_deb(dir.path(), &metadata));
        options.dpkg_path = Some(fake_dpkg(
            dir.path(),
            1,
            "dpkg: error: dpkg frontend lock was locked by another process with pid 42",
        ));
        assert!(matches!(
            run_system_package_from_job(&job, &options),
            Err(UpdateError::PackageManagerLocked)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn system_package_maps_dpkg_failure_to_package_manager_failed() {
        let dir = tempfile::tempdir().unwrap();
        let version = next_version();
        let (job, mut options) =
            system_package_fixture_for(PackageFamily::Deb, dir.path(), &version);
        let metadata = format!("Package: vrc-photo-manager\nVersion: {version}");
        options.dpkg_deb_path = Some(fake_dpkg_deb(dir.path(), &metadata));
        options.dpkg_path = Some(fake_dpkg(
            dir.path(),
            1,
            "dpkg: dependency problems prevent configuration",
        ));
        assert!(matches!(
            run_system_package_from_job(&job, &options),
            Err(UpdateError::PackageManagerFailed {
                family: PackageFamily::Deb,
                ..
            })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn system_package_uses_the_c_locale_and_a_noninteractive_deb_frontend() {
        let dir = tempfile::tempdir().unwrap();
        let version = next_version();
        let (job, mut options) =
            system_package_fixture_for(PackageFamily::Deb, dir.path(), &version);

        let metadata_log = dir.path().join("dpkg-deb-env.log");
        let dpkg_deb = dir.path().join("dpkg-deb-env");
        write_script(
            &dpkg_deb,
            &format!(
                "#!/bin/sh\nprintf '%s|%s' \"$LC_ALL\" \"$1\" > \"{}\"\nprintf 'Package: vrc-photo-manager\\nVersion: {version}\\n'\n",
                metadata_log.display()
            ),
        );
        let install_log = dir.path().join("dpkg-env.log");
        let dpkg = dir.path().join("dpkg-env");
        write_script(
            &dpkg,
            &format!(
                "#!/bin/sh\nprintf '%s|%s|%s' \"$LC_ALL\" \"$DEBIAN_FRONTEND\" \"$1\" > \"{}\"\n",
                install_log.display()
            ),
        );
        options.dpkg_deb_path = Some(dpkg_deb);
        options.dpkg_path = Some(dpkg);

        run_system_package_from_job(&job, &options).unwrap();
        assert_eq!(std::fs::read_to_string(&metadata_log).unwrap(), "C|-f");
        assert_eq!(
            std::fs::read_to_string(&install_log).unwrap(),
            "C|noninteractive|-i"
        );
    }

    #[test]
    fn last_result_accepts_a_missing_code() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());
        std::fs::write(
            paths.result_file(),
            br#"{"status":"success","from":"0.3.0","to":"0.4.0"}"#,
        )
        .unwrap();
        let result = LastResult::read(&paths).unwrap();
        assert_eq!(result.code, None);
    }

    #[test]
    fn user_mode_rejects_root() {
        assert!(user_mode_allowed(1000));
        assert!(!user_mode_allowed(0));
    }

    #[test]
    fn system_mode_requires_root_and_pkexec_uid() {
        assert!(system_mode_allowed(0, Some(1000)));
        assert!(!system_mode_allowed(1000, Some(1000)));
        assert!(!system_mode_allowed(0, None));
        assert!(!system_mode_allowed(0, Some(0)));
    }

    #[cfg(unix)]
    #[test]
    fn maps_pkexec_exit_codes() {
        use std::os::unix::process::ExitStatusExt;
        let cases = [
            (126, LastErrorCode::AuthCancelled),
            (127, LastErrorCode::AuthUnavailable),
            (14, LastErrorCode::PackageManagerLocked),
            (11, LastErrorCode::PackageVerificationFailed),
            (12, LastErrorCode::PackageMetadataMismatch),
            (13, LastErrorCode::Downgrade),
            (15, LastErrorCode::PackageManagerFailed),
            (10, LastErrorCode::HelperFailed),
        ];
        for (code, expected) in cases {
            // `from_raw` takes a wait status; the exit code lives in bits 8..16.
            let status = std::process::ExitStatus::from_raw(code << 8);
            assert_eq!(status.code(), Some(code));
            assert_eq!(map_pkexec_status(status, "reason").0, expected);
        }
    }

    #[test]
    fn only_preinstall_failures_cleanup_the_job() {
        assert!(is_preinstall_failure(LastErrorCode::AuthCancelled));
        assert!(is_preinstall_failure(LastErrorCode::AuthUnavailable));
        assert!(is_preinstall_failure(LastErrorCode::PackageManagerLocked));
        assert!(is_preinstall_failure(
            LastErrorCode::PackageVerificationFailed
        ));
        assert!(is_preinstall_failure(
            LastErrorCode::PackageMetadataMismatch
        ));
        assert!(is_preinstall_failure(LastErrorCode::Downgrade));
        assert!(is_preinstall_failure(LastErrorCode::HelperFailed));
        assert!(!is_preinstall_failure(LastErrorCode::PackageManagerFailed));
        assert!(!is_preinstall_failure(LastErrorCode::HealthCheckFailed));
    }

    #[cfg(unix)]
    #[test]
    fn system_package_apply_records_a_verification_failure_and_relaunches() {
        let dir = tempfile::tempdir().unwrap();
        let (mut job, options) = system_package_fixture(dir.path(), &next_version());
        let sentinel = dir.path().join("old-ran");
        let target = dir.path().join("vrc-photo-manager");
        std::fs::write(
            &target,
            format!("#!/bin/sh\nprintf old > '{}'\n", sentinel.display()),
        )
        .unwrap();
        make_executable(&target);
        job.target = target;

        let paths = UpdatePaths::new(&job.data_dir);
        paths.ensure().unwrap();
        let job_path = paths.job_file();
        std::fs::write(&job_path, serde_json::to_vec(&job).unwrap()).unwrap();
        let staged = job.staged.clone();
        std::fs::write(&staged, b"tampered").unwrap();

        let error = run_system_package_apply(&paths, &job, &job_path, &options).unwrap_err();
        assert!(matches!(error, UpdateError::Other(_)));

        let result = LastResult::read(&paths).unwrap();
        assert_eq!(result.status, LastStatus::Failed);
        assert_eq!(result.code, Some(LastErrorCode::PackageVerificationFailed));
        assert!(!job_path.exists(), "the job is cleaned up before install");
        assert!(!staged.exists(), "the staged package is cleaned up");
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while !sentinel.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(sentinel.exists(), "the target is relaunched");
    }

    #[cfg(unix)]
    #[test]
    fn package_manager_failure_keeps_the_job_and_staged_package() {
        let dir = tempfile::tempdir().unwrap();
        let (mut job, _options) = system_package_fixture(dir.path(), &next_version());
        job.target = dir.path().join("missing-target");
        let paths = UpdatePaths::new(&job.data_dir);
        paths.ensure().unwrap();
        let job_path = paths.job_file();
        std::fs::write(&job_path, serde_json::to_vec(&job).unwrap()).unwrap();
        let staged = job.staged.clone();

        let error = fail_system_package(
            &paths,
            &job,
            &job_path,
            LastErrorCode::PackageManagerFailed,
            "pacman failed".to_string(),
            false,
        )
        .unwrap_err();
        assert!(matches!(error, UpdateError::Other(_)));

        let result = LastResult::read(&paths).unwrap();
        assert_eq!(result.status, LastStatus::Failed);
        assert_eq!(result.code, Some(LastErrorCode::PackageManagerFailed));
        assert_eq!(result.reason.as_deref(), Some("pacman failed"));
        assert!(
            job_path.exists(),
            "the job is kept after an install failure"
        );
        assert!(staged.exists(), "the staged package is kept");
        assert!(paths.log_file().exists(), "the log is kept");
    }

    #[cfg(unix)]
    #[test]
    fn post_install_failure_keeps_the_job_and_staged_package() {
        let dir = tempfile::tempdir().unwrap();
        let (mut job, _options) = system_package_fixture(dir.path(), &next_version());
        let sentinel = dir.path().join("relaunched");
        let target = dir.path().join("vrc-photo-manager");
        std::fs::write(
            &target,
            format!("#!/bin/sh\nprintf ok > '{}'\n", sentinel.display()),
        )
        .unwrap();
        make_executable(&target);
        job.target = target;
        let paths = UpdatePaths::new(&job.data_dir);
        paths.ensure().unwrap();
        let job_path = paths.job_file();
        std::fs::write(&job_path, serde_json::to_vec(&job).unwrap()).unwrap();
        let staged = job.staged.clone();

        fail_system_package_after_install(&paths, &job, "health check timed out".to_string());

        let result = LastResult::read(&paths).unwrap();
        assert_eq!(result.status, LastStatus::Failed);
        assert_eq!(result.code, Some(LastErrorCode::HealthCheckFailed));
        assert_eq!(result.reason.as_deref(), Some("health check timed out"));
        assert!(job_path.exists(), "the job is kept after a health failure");
        assert!(staged.exists(), "the staged package is kept");
        assert!(paths.log_file().exists(), "the log is kept");
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while !sentinel.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(
            sentinel.exists(),
            "the target is relaunched without a token"
        );
    }

    #[cfg(unix)]
    fn exit_status(code: i32) -> std::process::ExitStatus {
        use std::os::unix::process::ExitStatusExt;
        std::process::ExitStatus::from_raw(code << 8)
    }

    #[cfg(unix)]
    #[test]
    fn handle_system_package_exit_keeps_files_after_a_package_manager_failure() {
        let dir = tempfile::tempdir().unwrap();
        let (mut job, _options) = system_package_fixture(dir.path(), &next_version());
        job.target = dir.path().join("missing-target");
        let paths = UpdatePaths::new(&job.data_dir);
        paths.ensure().unwrap();
        let job_path = paths.job_file();
        std::fs::write(&job_path, serde_json::to_vec(&job).unwrap()).unwrap();
        let staged = job.staged.clone();

        let error =
            handle_system_package_exit(&paths, &job, &job_path, exit_status(15), "pacman failed")
                .unwrap_err();
        assert!(matches!(error, UpdateError::Other(_)));

        let result = LastResult::read(&paths).unwrap();
        assert_eq!(result.status, LastStatus::Failed);
        assert_eq!(result.code, Some(LastErrorCode::PackageManagerFailed));
        assert_eq!(result.reason.as_deref(), Some("pacman failed"));
        assert!(
            job_path.exists(),
            "the job is kept after an install failure"
        );
        assert!(staged.exists(), "the staged package is kept");
    }

    #[cfg(unix)]
    #[test]
    fn handle_system_package_exit_cleans_up_after_authentication_cancel() {
        let dir = tempfile::tempdir().unwrap();
        let (mut job, _options) = system_package_fixture(dir.path(), &next_version());
        job.target = dir.path().join("missing-target");
        let paths = UpdatePaths::new(&job.data_dir);
        paths.ensure().unwrap();
        let job_path = paths.job_file();
        std::fs::write(&job_path, serde_json::to_vec(&job).unwrap()).unwrap();
        let staged = job.staged.clone();

        let error =
            handle_system_package_exit(&paths, &job, &job_path, exit_status(126), "cancelled")
                .unwrap_err();
        assert!(matches!(error, UpdateError::Other(_)));

        let result = LastResult::read(&paths).unwrap();
        assert_eq!(result.status, LastStatus::Failed);
        assert_eq!(result.code, Some(LastErrorCode::AuthCancelled));
        assert!(!job_path.exists(), "the job is cleaned up before install");
        assert!(!staged.exists(), "the staged package is cleaned up");
    }

    #[cfg(unix)]
    #[test]
    fn handle_system_package_exit_cleans_up_after_a_locked_package_manager() {
        let dir = tempfile::tempdir().unwrap();
        let (mut job, _options) = system_package_fixture(dir.path(), &next_version());
        job.target = dir.path().join("missing-target");
        let paths = UpdatePaths::new(&job.data_dir);
        paths.ensure().unwrap();
        let job_path = paths.job_file();
        std::fs::write(&job_path, serde_json::to_vec(&job).unwrap()).unwrap();
        let staged = job.staged.clone();

        let error = handle_system_package_exit(
            &paths,
            &job,
            &job_path,
            exit_status(14),
            "unable to lock database",
        )
        .unwrap_err();
        assert!(matches!(error, UpdateError::Other(_)));

        let result = LastResult::read(&paths).unwrap();
        assert_eq!(result.status, LastStatus::Failed);
        assert_eq!(result.code, Some(LastErrorCode::PackageManagerLocked));
        assert!(!job_path.exists(), "the job is cleaned up before install");
        assert!(!staged.exists(), "the staged package is cleaned up");
    }
}
