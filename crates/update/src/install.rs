use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use semver::Version;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::{IoOperation, Result, UpdateError};
use crate::helper::write_atomic;

pub const APPLY_SCHEMA: u32 = 1;

/// The package family used to show the right update command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackageFamily {
    Deb,
    Pacman,
    Unknown,
}

impl PackageFamily {
    /// Reads `ID` and `ID_LIKE` from `/etc/os-release`-style text.
    pub fn from_os_release(text: &str) -> Self {
        let mut id = String::new();
        let mut id_like = String::new();
        for line in text.lines() {
            if let Some(value) = line.strip_prefix("ID=") {
                id = value.trim_matches('"').to_ascii_lowercase();
            } else if let Some(value) = line.strip_prefix("ID_LIKE=") {
                id_like = value.trim_matches('"').to_ascii_lowercase();
            }
        }
        let family = |token: &str| {
            id.split_whitespace().any(|value| value == token)
                || id_like.split_whitespace().any(|value| value == token)
        };
        if family("debian") || family("ubuntu") {
            Self::Deb
        } else if family("arch") || family("manjaro") {
            Self::Pacman
        } else {
            Self::Unknown
        }
    }

    /// The command shown to package-manager users.
    pub fn update_command(self) -> Option<&'static str> {
        match self {
            Self::Deb => Some("sudo apt install --only-upgrade vrc-photo-manager"),
            Self::Pacman => Some("sudo pacman -Syu vrc-photo-manager"),
            Self::Unknown => None,
        }
    }
}

/// How this copy of the application was installed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallKind {
    AppImage { path: PathBuf },
    WindowsCurrentUser { install_dir: PathBuf },
    WindowsPerMachine { install_dir: PathBuf },
    WindowsOther,
    SystemPackage { family: PackageFamily },
    Development,
}

impl InstallKind {
    /// Whether the helper may replace this installation.
    pub fn automatic_apply(&self) -> bool {
        matches!(
            self,
            Self::AppImage { .. } | Self::WindowsCurrentUser { .. }
        )
    }

    /// Whether a startup timer should check for updates.
    pub fn automatic_check(&self) -> bool {
        matches!(
            self,
            Self::AppImage { .. } | Self::WindowsCurrentUser { .. } | Self::SystemPackage { .. }
        )
    }
}

/// Detects the running installation.
pub fn install_kind() -> InstallKind {
    #[cfg(windows)]
    {
        install_kind_windows()
    }

    #[cfg(not(windows))]
    {
        let appimage = std::env::var_os("APPIMAGE").map(PathBuf::from);
        let os_release = std::fs::read_to_string("/etc/os-release").unwrap_or_default();
        match std::env::current_exe() {
            Ok(exe) => classify_linux(&exe, appimage.as_deref(), &os_release),
            Err(_) => InstallKind::Development,
        }
    }
}

/// Classifies a Linux install from its executable path and environment.
pub fn classify_linux(
    current_exe: &Path,
    appimage: Option<&Path>,
    os_release: &str,
) -> InstallKind {
    if let Some(path) = appimage.filter(|path| path.is_file()) {
        return InstallKind::AppImage {
            path: std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()),
        };
    }
    if current_exe.starts_with("/usr/bin") || current_exe.starts_with("/usr/local/bin") {
        return InstallKind::SystemPackage {
            family: PackageFamily::from_os_release(os_release),
        };
    }
    InstallKind::Development
}

/// Classifies a Windows install by comparing the registry install location
/// with the running executable's directory.
pub fn classify_windows(
    current_exe: &Path,
    current_user: Option<&Path>,
    per_machine: Option<&Path>,
) -> InstallKind {
    let Some(install_dir) = current_exe.parent() else {
        return InstallKind::WindowsOther;
    };
    if current_user.is_some_and(|path| same_windows_path(path, install_dir)) {
        InstallKind::WindowsCurrentUser {
            install_dir: install_dir.to_path_buf(),
        }
    } else if per_machine.is_some_and(|path| same_windows_path(path, install_dir)) {
        InstallKind::WindowsPerMachine {
            install_dir: install_dir.to_path_buf(),
        }
    } else {
        InstallKind::WindowsOther
    }
}

fn same_windows_path(left: &Path, right: &Path) -> bool {
    normalize_windows_path(left) == normalize_windows_path(right)
}

fn normalize_windows_path(path: &Path) -> String {
    path.to_string_lossy()
        .trim()
        .trim_matches('"')
        .replace('/', "\\")
        .trim_end_matches('\\')
        .to_ascii_lowercase()
}

#[cfg(windows)]
fn install_kind_windows() -> InstallKind {
    use winreg::RegKey;
    use winreg::enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE};

    const UNINSTALL_KEY: &str =
        r"Software\Microsoft\Windows\CurrentVersion\Uninstall\VRC Photo Manager";

    fn location(root: RegKey) -> Option<PathBuf> {
        root.open_subkey(UNINSTALL_KEY)
            .ok()?
            .get_value::<String, _>("InstallLocation")
            .ok()
            .map(|value| PathBuf::from(value.trim_matches('"')))
    }

    let current_user = location(RegKey::predef(HKEY_CURRENT_USER));
    let per_machine = location(RegKey::predef(HKEY_LOCAL_MACHINE));
    match std::env::current_exe() {
        Ok(exe) => classify_windows(&exe, current_user.as_deref(), per_machine.as_deref()),
        Err(_) => InstallKind::WindowsOther,
    }
}

/// Per-user files used by the updater.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdatePaths {
    pub root: PathBuf,
}

impl UpdatePaths {
    pub fn new(data_dir: &Path) -> Self {
        Self {
            root: data_dir.join("updates"),
        }
    }

    /// Creates the directories the UI and helper write to.
    pub fn ensure(&self) -> Result<()> {
        for path in [&self.root, &self.downloads_dir(), &self.bin_dir()] {
            create_private_dir(path)?;
        }
        Ok(())
    }

    pub fn downloads_dir(&self) -> PathBuf {
        self.root.join("downloads")
    }

    pub fn bin_dir(&self) -> PathBuf {
        self.root.join("bin")
    }

    pub fn job_file(&self) -> PathBuf {
        self.root.join("job.json")
    }

    pub fn lock_file(&self) -> PathBuf {
        self.root.join("apply.lock")
    }

    pub fn result_file(&self) -> PathBuf {
        self.root.join("last-result.json")
    }

    pub fn log_file(&self) -> PathBuf {
        self.root.join("updater.log")
    }

    pub fn health_file(&self, token: &str) -> PathBuf {
        self.root
            .join(format!("health-{}.json", safe_component(token)))
    }

    pub fn backup_dir(&self, version: &str) -> PathBuf {
        self.root.join("backup").join(safe_component(version))
    }
}

fn safe_component(value: &str) -> String {
    let safe: String = value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '.' | '+') {
                character
            } else {
                '_'
            }
        })
        .collect();
    if safe.is_empty() {
        "invalid".to_string()
    } else {
        safe
    }
}

/// The installation type an apply job targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApplyKind {
    AppImage,
    Windows,
}

/// The handoff file written by the UI and consumed by the helper.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApplyJob {
    pub schema: u32,
    pub kind: ApplyKind,
    pub from_version: String,
    pub to_version: String,
    pub target: PathBuf,
    pub staged: PathBuf,
    pub signature: String,
    pub sha256: String,
    pub ui_pid: u32,
    pub daemon_pid: Option<u32>,
    pub restart_args: Vec<String>,
    pub health_token: String,
    pub data_dir: PathBuf,
    pub created_at: String,
}

impl ApplyJob {
    pub fn new_health_token() -> String {
        Uuid::new_v4().to_string()
    }

    pub fn validate(&self) -> Result<()> {
        if self.schema != APPLY_SCHEMA {
            return Err(UpdateError::InvalidJob(format!(
                "unsupported apply schema {}",
                self.schema
            )));
        }
        parse_job_version(&self.from_version)?;
        parse_job_version(&self.to_version)?;
        for (label, path) in [
            ("target", &self.target),
            ("staged", &self.staged),
            ("data_dir", &self.data_dir),
        ] {
            if !path.is_absolute() {
                return Err(UpdateError::InvalidJob(format!(
                    "{label} must be an absolute path"
                )));
            }
        }
        if self.target == self.staged {
            return Err(UpdateError::InvalidJob(
                "target and staged must differ".to_string(),
            ));
        }
        if self.health_token.is_empty()
            || !self
                .health_token
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || character == '-')
        {
            return Err(UpdateError::InvalidJob(
                "health token contains invalid characters".to_string(),
            ));
        }
        if self.signature.trim().is_empty() {
            return Err(UpdateError::InvalidJob("signature is empty".to_string()));
        }
        if self.sha256.len() != 64
            || !self
                .sha256
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            return Err(UpdateError::InvalidJob(
                "sha256 must be 64 lowercase hex characters".to_string(),
            ));
        }
        if self
            .restart_args
            .iter()
            .any(|argument| argument.contains('\0'))
        {
            return Err(UpdateError::InvalidJob(
                "restart arguments contain a NUL".to_string(),
            ));
        }
        Ok(())
    }
}

fn parse_job_version(raw: &str) -> Result<Version> {
    Version::parse(raw.strip_prefix('v').unwrap_or(raw))
        .map_err(|error| UpdateError::InvalidJob(format!("invalid version {raw}: {error}")))
}

/// Copies the helper next to the staged job and writes the job atomically.
///
/// This does not spawn the helper; see [`start_apply`].
pub fn prepare_apply(job: &ApplyJob, current_exe: &Path, data_dir: &Path) -> Result<PathBuf> {
    job.validate()?;
    if job.data_dir != data_dir {
        return Err(UpdateError::InvalidJob(
            "job data_dir does not match the caller".to_string(),
        ));
    }

    let paths = UpdatePaths::new(data_dir);
    paths.ensure()?;

    let helper_name = if cfg!(windows) {
        "vrc-photo-updater.exe"
    } else {
        "vrc-photo-updater"
    };
    let source = current_exe
        .parent()
        .ok_or_else(|| UpdateError::InvalidJob("current executable has no parent".to_string()))?
        .join(helper_name);
    if !source.is_file() {
        return Err(UpdateError::HelperMissing);
    }

    let helper = paths.bin_dir().join(helper_name);
    let _ = std::fs::remove_file(&helper);
    std::fs::copy(&source, &helper)
        .map_err(|error| UpdateError::io_at(IoOperation::Helper, &helper, error))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o700))
            .map_err(|error| UpdateError::io_at(IoOperation::Helper, &helper, error))?;
    }

    let bytes = serde_json::to_vec_pretty(job)
        .map_err(|error| UpdateError::Other(format!("cannot serialize apply job: {error}")))?;
    write_atomic(&paths.job_file(), &bytes, 0o600)
        .map_err(|error| error.with_io_operation(IoOperation::Job))?;
    Ok(helper)
}

/// Prepares the job and spawns the helper, redirecting its output to the
/// updater log. The caller is expected to exit after this returns.
pub fn start_apply(job: &ApplyJob, current_exe: &Path, data_dir: &Path) -> Result<()> {
    let helper = prepare_apply(job, current_exe, data_dir)?;
    let paths = UpdatePaths::new(data_dir);
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(paths.log_file())
        .map_err(|error| UpdateError::io_at(IoOperation::Job, paths.log_file(), error))?;
    let stderr = log
        .try_clone()
        .map_err(|error| UpdateError::io_at(IoOperation::Job, paths.log_file(), error))?;
    Command::new(&helper)
        .arg("--apply")
        .arg(paths.job_file())
        .current_dir(&paths.root)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(stderr))
        .spawn()
        .map_err(|error| UpdateError::io_at(IoOperation::Spawn, &helper, error))?;
    Ok(())
}

fn create_private_dir(path: &Path) -> Result<()> {
    std::fs::create_dir_all(path).map_err(|error| UpdateError::io(path, error))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .map_err(|error| UpdateError::io(path, error))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::manifest::{Manifest, UpdateFormat};

    #[test]
    fn classifies_debian_and_arch_families() {
        assert_eq!(
            PackageFamily::from_os_release("ID=ubuntu\nID_LIKE=debian\n"),
            PackageFamily::Deb
        );
        assert_eq!(
            PackageFamily::from_os_release("ID=debian\n"),
            PackageFamily::Deb
        );
        assert_eq!(
            PackageFamily::from_os_release("ID=arch\n"),
            PackageFamily::Pacman
        );
        assert_eq!(
            PackageFamily::from_os_release("ID=manjaro\nID_LIKE=arch\n"),
            PackageFamily::Pacman
        );
        assert_eq!(
            PackageFamily::from_os_release("ID=fedora\n"),
            PackageFamily::Unknown
        );
    }

    #[cfg(unix)]
    #[test]
    fn classifies_linux_paths() {
        let dir = tempfile::tempdir().unwrap();
        let appimage = dir.path().join("VRC.AppImage");
        std::fs::write(&appimage, b"appimage").unwrap();

        assert_eq!(
            classify_linux(
                Path::new("/usr/bin/vrc-photo-manager"),
                None,
                "ID=ubuntu\nID_LIKE=debian\n"
            ),
            InstallKind::SystemPackage {
                family: PackageFamily::Deb
            }
        );
        assert_eq!(
            classify_linux(
                Path::new("/home/me/project/target/debug/vrc-photo-manager"),
                None,
                ""
            ),
            InstallKind::Development
        );
        assert_eq!(
            classify_linux(
                Path::new("/home/me/project/target/debug/vrc-photo-manager"),
                Some(&appimage),
                ""
            ),
            InstallKind::AppImage {
                path: appimage.clone()
            }
        );
        assert_eq!(
            classify_linux(
                Path::new("/home/me/vrc-photo-manager"),
                Some(Path::new("/missing/VRC.AppImage")),
                ""
            ),
            InstallKind::Development
        );
    }

    #[test]
    fn only_appimage_and_current_user_windows_apply_automatically() {
        assert!(
            InstallKind::AppImage {
                path: "/tmp/VRC.AppImage".into()
            }
            .automatic_apply()
        );
        assert!(
            InstallKind::WindowsCurrentUser {
                install_dir: "C:\\Users\\me\\AppData\\Local\\VRC Photo Manager".into()
            }
            .automatic_apply()
        );
        assert!(
            !InstallKind::WindowsPerMachine {
                install_dir: "C:\\Program Files\\VRC Photo Manager".into()
            }
            .automatic_apply()
        );
        assert!(
            !InstallKind::SystemPackage {
                family: PackageFamily::Deb
            }
            .automatic_apply()
        );
        assert!(!InstallKind::Development.automatic_apply());
    }

    #[test]
    fn automatic_check_excludes_unsupported_windows_and_development() {
        assert!(
            InstallKind::SystemPackage {
                family: PackageFamily::Pacman
            }
            .automatic_check()
        );
        assert!(
            !InstallKind::WindowsPerMachine {
                install_dir: "C:\\Program Files\\VRC Photo Manager".into()
            }
            .automatic_check()
        );
        assert!(!InstallKind::WindowsOther.automatic_check());
        assert!(!InstallKind::Development.automatic_check());
    }

    #[test]
    fn selects_the_asset_for_the_install_kind() {
        let json = br#"{
          "schema": 1,
          "version": "0.4.0",
          "platforms": {
            "linux-x86_64": {
              "url": "https://example.com/vrc-photo-manager_0.4.0_x86_64.AppImage",
              "format": "appimage",
              "size": 10,
              "sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
              "signature": "__SIGNATURE__"
            },
            "windows-x86_64": {
              "url": "https://example.com/vrc-photo-manager_0.4.0_x64-setup.exe",
              "format": "nsis",
              "size": 10,
              "sha256": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
              "signature": "__SIGNATURE__"
            }
          }
        }"#;
        let json = String::from_utf8_lossy(json)
            .replace("__SIGNATURE__", &crate::verify::test_signature());
        let (manifest, _) = Manifest::parse_and_validate(json.as_bytes(), false).unwrap();

        let appimage = InstallKind::AppImage {
            path: "/tmp/VRC.AppImage".into(),
        };
        assert_eq!(
            manifest.asset_for(&appimage).unwrap().format,
            UpdateFormat::AppImage
        );

        let windows = InstallKind::WindowsCurrentUser {
            install_dir: "C:\\Users\\me\\AppData\\Local\\VRC Photo Manager".into(),
        };
        assert_eq!(
            manifest.asset_for(&windows).unwrap().format,
            UpdateFormat::Nsis
        );

        assert!(
            manifest
                .asset_for(&InstallKind::SystemPackage {
                    family: PackageFamily::Deb
                })
                .is_none()
        );
    }

    #[test]
    fn update_paths_create_private_directories() {
        let dir = tempfile::tempdir().unwrap();
        let paths = UpdatePaths::new(dir.path());
        paths.ensure().unwrap();
        assert!(paths.root.is_dir());
        assert!(paths.downloads_dir().is_dir());
        assert!(paths.bin_dir().is_dir());
        assert_eq!(paths.job_file(), paths.root.join("job.json"));
        assert_eq!(
            paths.health_file("abc-123"),
            paths.root.join("health-abc-123.json")
        );
        assert_eq!(
            paths.backup_dir("0.4.0"),
            paths.root.join("backup").join("0.4.0")
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for path in [&paths.root, &paths.downloads_dir(), &paths.bin_dir()] {
                assert_eq!(
                    std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                    0o700
                );
            }
        }
    }

    fn test_job(dir: &Path) -> ApplyJob {
        let target = dir.join("app.AppImage");
        let staged = dir.join("new.AppImage");
        std::fs::write(&target, b"old").unwrap();
        std::fs::write(&staged, b"new").unwrap();
        ApplyJob {
            schema: APPLY_SCHEMA,
            kind: ApplyKind::AppImage,
            from_version: "0.3.0".to_string(),
            to_version: "0.4.0".to_string(),
            target,
            staged,
            signature: "c2ln".to_string(),
            sha256: "a".repeat(64),
            ui_pid: 1,
            daemon_pid: Some(2),
            restart_args: Vec::new(),
            health_token: "abc-123".to_string(),
            data_dir: dir.to_path_buf(),
            created_at: "2026-10-24T12:00:00Z".to_string(),
        }
    }

    #[test]
    fn apply_job_round_trips_and_validates() {
        let dir = tempfile::tempdir().unwrap();
        let job = test_job(dir.path());
        job.validate().unwrap();
        let json = serde_json::to_string(&job).unwrap();
        let decoded: ApplyJob = serde_json::from_str(&json).unwrap();
        assert_eq!(job, decoded);

        let mut relative = job.clone();
        relative.target = "app.AppImage".into();
        assert!(matches!(
            relative.validate(),
            Err(crate::error::UpdateError::InvalidJob(_))
        ));

        let mut empty_token = job.clone();
        empty_token.health_token.clear();
        assert!(empty_token.validate().is_err());

        let mut slash_token = job.clone();
        slash_token.health_token = "abc/def".to_string();
        assert!(slash_token.validate().is_err());

        let mut bad_version = job.clone();
        bad_version.to_version = "latest".to_string();
        assert!(bad_version.validate().is_err());

        let mut bad_schema = job.clone();
        bad_schema.schema = 2;
        assert!(bad_schema.validate().is_err());
    }

    #[test]
    fn new_health_token_is_a_uuid() {
        let token = ApplyJob::new_health_token();
        assert_eq!(token.len(), 36);
        assert!(token.chars().all(|c| c.is_ascii_hexdigit() || c == '-'));
    }

    #[test]
    fn prepare_apply_copies_the_helper_and_writes_the_job() {
        let dir = tempfile::tempdir().unwrap();
        let install = dir.path().join("install");
        std::fs::create_dir_all(&install).unwrap();
        let helper_name = if cfg!(windows) {
            "vrc-photo-updater.exe"
        } else {
            "vrc-photo-updater"
        };
        let current_exe = install.join(if cfg!(windows) {
            "vrc-photo-manager.exe"
        } else {
            "vrc-photo-manager"
        });
        std::fs::write(&current_exe, b"ui").unwrap();
        std::fs::write(install.join(helper_name), b"helper").unwrap();

        let data_dir = dir.path().join("data");
        let mut job = test_job(dir.path());
        job.data_dir = data_dir.clone();
        let copied = prepare_apply(&job, &current_exe, &data_dir).unwrap();
        assert_eq!(copied, data_dir.join("updates/bin").join(helper_name));
        assert_eq!(std::fs::read(&copied).unwrap(), b"helper");
        let job_file = data_dir.join("updates/job.json");
        let stored: ApplyJob = serde_json::from_slice(&std::fs::read(job_file).unwrap()).unwrap();
        assert_eq!(stored, job);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&copied).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn classifies_windows_install_locations() {
        let exe = Path::new(r"C:\Users\me\AppData\Local\VRC Photo Manager\vrc-photo-manager.exe");
        assert_eq!(
            classify_windows(
                exe,
                Some(Path::new(r"C:\Users\me\AppData\Local\VRC Photo Manager")),
                None
            ),
            InstallKind::WindowsCurrentUser {
                install_dir: exe.parent().unwrap().into()
            }
        );
        let machine_exe = Path::new(r"C:\Program Files\VRC Photo Manager\vrc-photo-manager.exe");
        assert_eq!(
            classify_windows(
                machine_exe,
                None,
                Some(Path::new(r"C:\Program Files\VRC Photo Manager"))
            ),
            InstallKind::WindowsPerMachine {
                install_dir: machine_exe.parent().unwrap().into()
            }
        );
        assert_eq!(classify_windows(exe, None, None), InstallKind::WindowsOther);
    }
}
