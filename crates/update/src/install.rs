use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use semver::Version;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::{IoOperation, Result, UpdateError};
use crate::helper::write_atomic;
use crate::manifest::{MAX_MANIFEST_BYTES, MAX_SIGNATURE_BYTES};
use crate::native::NativeString;

pub const APPLY_SCHEMA: u32 = 1;

/// The package family used to show the right update command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PackageFamily {
    Deb,
    Pacman,
    Unknown,
}

impl PackageFamily {
    /// The key used for this family in the manifest's `system_packages` map.
    pub fn manifest_key(self) -> Option<&'static str> {
        match self {
            Self::Pacman => Some("pacman"),
            Self::Deb => Some("deb"),
            Self::Unknown => None,
        }
    }

    /// Resolves a `system_packages` key to its package family.
    pub fn from_manifest_key(key: &str) -> Option<Self> {
        match key {
            "pacman" => Some(Self::Pacman),
            "deb" => Some(Self::Deb),
            _ => None,
        }
    }

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
            Self::Deb => Some("sudo apt install --only-upgrade vrc-photon"),
            Self::Pacman => Some("sudo pacman -Syu vrc-photon"),
            Self::Unknown => None,
        }
    }
}

/// How this copy of the application was installed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallKind {
    AppImage {
        path: PathBuf,
    },
    WindowsCurrentUser {
        install_dir: PathBuf,
    },
    WindowsPerMachine {
        install_dir: PathBuf,
    },
    WindowsOther,
    SystemPackage {
        family: PackageFamily,
        /// Whether this is the package-managed `/usr/bin` install. A binary in
        /// `/usr/local/bin` shadows the packaged one, so it only gets the
        /// package-manager command.
        managed: bool,
    },
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

    /// Whether a system package may be applied through `pkexec` and the
    /// installed helper. Requires the helper and its polkit policy.
    pub fn system_package_apply(
        &self,
        pkexec_available: bool,
        helper_available: bool,
        policy_available: bool,
    ) -> bool {
        matches!(
            self,
            Self::SystemPackage {
                family: PackageFamily::Pacman | PackageFamily::Deb,
                managed: true,
            }
        ) && pkexec_available
            && helper_available
            && policy_available
    }

    /// Whether an update may be applied without showing a package manager
    /// command.
    pub fn can_apply_automatically(
        &self,
        pkexec_available: bool,
        helper_available: bool,
        policy_available: bool,
    ) -> bool {
        self.automatic_apply()
            || self.system_package_apply(pkexec_available, helper_available, policy_available)
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
        if let Some(path) = appimage.as_deref() {
            // An interrupted replacement may have left the target missing and
            // the previous AppImage next to it. Restore it before classifying.
            let _ = crate::helper::recover_appimage(path);
        }
        let os_release = std::fs::read_to_string("/etc/os-release").unwrap_or_default();
        match std::env::current_exe() {
            Ok(exe) => classify_linux(&exe, appimage.as_deref(), &os_release),
            Err(_) => InstallKind::Development,
        }
    }
}

/// The helper path installed by the pacman and deb packages.
pub const SYSTEM_HELPER_PATH: &str = "/usr/bin/vrc-photon-updater";
/// The polkit policy installed by the pacman and deb packages.
pub const SYSTEM_POLICY_PATH: &str =
    "/usr/share/polkit-1/actions/com.tommyz.vrc-photon.update.policy";

/// Whether an executable named `name` exists in one of `dirs`.
pub fn find_executable_in(dirs: &[PathBuf], name: &str) -> bool {
    dirs.iter().any(|dir| {
        let path = dir.join(name);
        let Ok(metadata) = std::fs::metadata(&path) else {
            return false;
        };
        if !metadata.is_file() {
            return false;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            metadata.permissions().mode() & 0o111 != 0
        }
        #[cfg(not(unix))]
        {
            true
        }
    })
}

/// Whether `pkexec` is available for privilege escalation.
#[cfg(target_os = "linux")]
pub fn pkexec_available() -> bool {
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|value| std::env::split_paths(&value).collect())
        .unwrap_or_default();
    for dir in ["/usr/bin", "/bin", "/usr/local/bin"] {
        dirs.push(PathBuf::from(dir));
    }
    find_executable_in(&dirs, "pkexec")
}

#[cfg(not(target_os = "linux"))]
pub fn pkexec_available() -> bool {
    false
}

/// Whether the system-wide update helper is installed.
pub fn system_helper_available() -> bool {
    std::path::Path::new(SYSTEM_HELPER_PATH).is_file()
}

/// Whether the polkit policy for the system update helper is installed.
pub fn system_policy_available() -> bool {
    std::path::Path::new(SYSTEM_POLICY_PATH).is_file()
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
    // Only the package-managed `/usr/bin` install may auto-apply. A binary in
    // `/usr/local/bin` shadows the packaged one, so installing the system
    // package would not replace what the user is running; it keeps the
    // package-manager command fallback instead.
    if current_exe.starts_with("/usr/bin") || current_exe.starts_with("/usr/local/bin") {
        return InstallKind::SystemPackage {
            family: PackageFamily::from_os_release(os_release),
            managed: current_exe.starts_with("/usr/bin"),
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

    const UNINSTALL_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Uninstall\VRC PhotoN";

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
    data_dir: PathBuf,
}

impl UpdatePaths {
    pub fn new(data_dir: &Path) -> Self {
        Self {
            root: data_dir.join("updates"),
            data_dir: data_dir.to_path_buf(),
        }
    }

    /// Creates the directories the UI and helper write to.
    pub fn ensure(&self) -> Result<()> {
        create_private_dir(&self.data_dir)?;
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
    #[serde(rename = "appimage", alias = "app_image")]
    AppImage,
    Windows,
    SystemPackage {
        family: PackageFamily,
    },
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
    pub restart_args: Vec<NativeString>,
    pub working_dir: PathBuf,
    pub database: Option<NativeString>,
    pub health_token: String,
    pub data_dir: PathBuf,
    pub created_at: String,
    /// The raw `latest.json` bytes covered by `manifest_signature`.
    #[serde(default)]
    pub signed_manifest: Option<String>,
    /// The raw `latest.json.minisig` text.
    #[serde(default)]
    pub manifest_signature: Option<String>,
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
            ("working_dir", &self.working_dir),
        ] {
            if !path.is_absolute() {
                return Err(UpdateError::InvalidJob(format!(
                    "{label} must be an absolute path"
                )));
            }
        }
        if let Some(database) = &self.database
            && !database.as_path().is_absolute()
        {
            return Err(UpdateError::InvalidJob(
                "database must be an absolute path".to_string(),
            ));
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
        if self.restart_args.iter().any(NativeString::contains_nul) {
            return Err(UpdateError::InvalidJob(
                "restart arguments contain a NUL".to_string(),
            ));
        }
        if let ApplyKind::SystemPackage { family } = self.kind {
            if !matches!(family, PackageFamily::Pacman | PackageFamily::Deb) {
                return Err(UpdateError::InvalidJob(
                    "unsupported system package family".to_string(),
                ));
            }
            let manifest = self
                .signed_manifest
                .as_deref()
                .ok_or_else(|| UpdateError::InvalidJob("signed manifest is missing".to_string()))?;
            let signature = self.manifest_signature.as_deref().ok_or_else(|| {
                UpdateError::InvalidJob("manifest signature is missing".to_string())
            })?;
            if manifest.is_empty() || manifest.len() as u64 > MAX_MANIFEST_BYTES {
                return Err(UpdateError::InvalidJob(
                    "signed manifest is empty or too large".to_string(),
                ));
            }
            if signature.is_empty() || signature.len() as u64 > MAX_SIGNATURE_BYTES {
                return Err(UpdateError::InvalidJob(
                    "manifest signature is empty or too large".to_string(),
                ));
            }
            let version = parse_job_version(&self.to_version)?;
            let expected_dir = UpdatePaths::new(&self.data_dir)
                .downloads_dir()
                .join(version.to_string());
            if self.staged.parent() != Some(expected_dir.as_path())
                || self
                    .staged
                    .components()
                    .any(|component| component == std::path::Component::ParentDir)
            {
                return Err(UpdateError::InvalidJob(
                    "staged package is outside the downloads directory".to_string(),
                ));
            }
            let suffix = match family {
                PackageFamily::Pacman => ".pkg.tar.zst",
                PackageFamily::Deb => ".deb",
                PackageFamily::Unknown => unreachable!(),
            };
            let file_name = self
                .staged
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(|| {
                    UpdateError::InvalidJob("staged package has no file name".to_string())
                })?;
            if !file_name.ends_with(suffix) {
                return Err(UpdateError::InvalidJob(format!(
                    "staged package must end with {suffix}"
                )));
            }
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
        "vrc-photon-updater.exe"
    } else {
        "vrc-photon-updater"
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
    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let log = options
        .open(paths.log_file())
        .map_err(|error| UpdateError::io_at(IoOperation::Job, paths.log_file(), error))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(paths.log_file(), std::fs::Permissions::from_mode(0o600));
    }
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
                Path::new("/usr/bin/vrc-photon"),
                None,
                "ID=ubuntu\nID_LIKE=debian\n"
            ),
            InstallKind::SystemPackage {
                family: PackageFamily::Deb,
                managed: true,
            }
        );
        assert_eq!(
            classify_linux(Path::new("/usr/local/bin/vrc-photon"), None, "ID=arch\n"),
            InstallKind::SystemPackage {
                family: PackageFamily::Pacman,
                managed: false,
            }
        );
        assert_eq!(
            classify_linux(
                Path::new("/home/me/project/target/debug/vrc-photon"),
                None,
                ""
            ),
            InstallKind::Development
        );
        assert_eq!(
            classify_linux(
                Path::new("/home/me/project/target/debug/vrc-photon"),
                Some(&appimage),
                ""
            ),
            InstallKind::AppImage {
                path: appimage.clone()
            }
        );
        assert_eq!(
            classify_linux(
                Path::new("/home/me/vrc-photon"),
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
                install_dir: "C:\\Users\\me\\AppData\\Local\\VRC PhotoN".into()
            }
            .automatic_apply()
        );
        assert!(
            !InstallKind::WindowsPerMachine {
                install_dir: "C:\\Program Files\\VRC PhotoN".into()
            }
            .automatic_apply()
        );
        assert!(
            !InstallKind::SystemPackage {
                family: PackageFamily::Deb,
                managed: true,
            }
            .automatic_apply()
        );
        assert!(!InstallKind::Development.automatic_apply());
    }

    #[test]
    fn automatic_check_excludes_unsupported_windows_and_development() {
        assert!(
            InstallKind::SystemPackage {
                family: PackageFamily::Pacman,
                managed: true,
            }
            .automatic_check()
        );
        assert!(
            !InstallKind::WindowsPerMachine {
                install_dir: "C:\\Program Files\\VRC PhotoN".into()
            }
            .automatic_check()
        );
        assert!(!InstallKind::WindowsOther.automatic_check());
        assert!(!InstallKind::Development.automatic_check());
    }

    #[test]
    fn package_family_manifest_keys_round_trip() {
        for family in [PackageFamily::Pacman, PackageFamily::Deb] {
            let key = family.manifest_key().unwrap();
            assert_eq!(PackageFamily::from_manifest_key(key), Some(family));
            let json = serde_json::to_string(&family).unwrap();
            assert_eq!(json, format!("\"{key}\""));
            assert_eq!(
                serde_json::from_str::<PackageFamily>(&json).unwrap(),
                family
            );
        }
        assert_eq!(PackageFamily::Unknown.manifest_key(), None);
        assert_eq!(PackageFamily::from_manifest_key("rpm"), None);
    }

    #[test]
    fn selects_the_asset_for_the_install_kind() {
        let json = br#"{
          "schema": 1,
          "version": "0.4.0",
          "platforms": {
            "linux-x86_64": {
              "url": "https://example.com/vrc-photon_0.4.0_x86_64.AppImage",
              "format": "appimage",
              "size": 10,
              "sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
              "signature": "__SIGNATURE__"
            },
            "windows-x86_64": {
              "url": "https://example.com/vrc-photon_0.4.0_x64-setup.exe",
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
            install_dir: "C:\\Users\\me\\AppData\\Local\\VRC PhotoN".into(),
        };
        assert_eq!(
            manifest.asset_for(&windows).unwrap().format,
            UpdateFormat::Nsis
        );

        assert!(
            manifest
                .asset_for(&InstallKind::SystemPackage {
                    family: PackageFamily::Deb,
                    managed: true,
                })
                .is_none()
        );
    }

    #[test]
    fn update_paths_create_private_directories() {
        let dir = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        }
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
            assert_eq!(
                std::fs::metadata(dir.path()).unwrap().permissions().mode() & 0o777,
                0o700
            );
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
            working_dir: dir.to_path_buf(),
            database: None,
            health_token: "abc-123".to_string(),
            data_dir: dir.to_path_buf(),
            created_at: "2026-10-24T12:00:00Z".to_string(),
            signed_manifest: None,
            manifest_signature: None,
        }
    }

    fn system_job(dir: &Path) -> ApplyJob {
        let data_dir = dir.join("data");
        let staged = data_dir
            .join("updates/downloads/0.4.0")
            .join("vrc-photon-0.4.0-1-x86_64.pkg.tar.zst");
        std::fs::create_dir_all(staged.parent().unwrap()).unwrap();
        std::fs::write(&staged, b"package").unwrap();
        ApplyJob {
            schema: APPLY_SCHEMA,
            kind: ApplyKind::SystemPackage {
                family: PackageFamily::Pacman,
            },
            from_version: "0.3.0".to_string(),
            to_version: "0.4.0".to_string(),
            target: dir.join("vrc-photon"),
            staged,
            signature: "c2ln".to_string(),
            sha256: "a".repeat(64),
            ui_pid: 1,
            daemon_pid: Some(2),
            restart_args: Vec::new(),
            working_dir: dir.to_path_buf(),
            database: None,
            health_token: "abc-123".to_string(),
            data_dir,
            created_at: "2026-10-24T12:00:00Z".to_string(),
            signed_manifest: Some("{\"schema\":1}".to_string()),
            manifest_signature: Some("signature".to_string()),
        }
    }

    #[test]
    fn apply_kind_serializes_appimage_windows_and_system_package() {
        assert_eq!(
            serde_json::to_string(&ApplyKind::AppImage).unwrap(),
            "\"appimage\""
        );
        assert_eq!(
            serde_json::to_string(&ApplyKind::Windows).unwrap(),
            "\"windows\""
        );
        assert_eq!(
            serde_json::to_string(&ApplyKind::SystemPackage {
                family: PackageFamily::Pacman
            })
            .unwrap(),
            "{\"system_package\":{\"family\":\"pacman\"}}"
        );
    }

    #[test]
    fn apply_kind_accepts_the_legacy_app_image_alias() {
        assert_eq!(
            serde_json::from_str::<ApplyKind>("\"app_image\"").unwrap(),
            ApplyKind::AppImage
        );
    }

    #[test]
    fn system_package_job_requires_manifest_and_signature() {
        let dir = tempfile::tempdir().unwrap();
        let job = system_job(dir.path());
        job.validate().unwrap();

        let mut missing = job.clone();
        missing.signed_manifest = None;
        assert!(missing.validate().is_err());

        let mut empty_signature = job.clone();
        empty_signature.manifest_signature = Some(String::new());
        assert!(empty_signature.validate().is_err());
    }

    #[test]
    fn system_package_job_rejects_staging_outside_downloads() {
        let dir = tempfile::tempdir().unwrap();
        let mut job = system_job(dir.path());
        job.staged = dir.path().join("elsewhere.pkg.tar.zst");
        assert!(job.validate().is_err());
    }

    #[test]
    fn system_package_job_rejects_nested_staging_paths() {
        let dir = tempfile::tempdir().unwrap();
        let mut job = system_job(dir.path());
        let nested = job.staged.parent().unwrap().join("sub");
        std::fs::create_dir_all(&nested).unwrap();
        let nested_file = nested.join(job.staged.file_name().unwrap());
        std::fs::write(&nested_file, b"package").unwrap();
        job.staged = nested_file;
        assert!(job.validate().is_err());
    }

    #[test]
    fn system_package_job_rejects_parent_dir_components() {
        let dir = tempfile::tempdir().unwrap();
        let mut job = system_job(dir.path());
        let version_dir = job.staged.parent().unwrap().to_path_buf();
        let file_name = job.staged.file_name().unwrap().to_os_string();
        job.staged = version_dir.join("..").join("0.4.0").join(file_name);
        assert!(job.validate().is_err());
    }

    #[test]
    fn system_package_job_rejects_wrong_extension() {
        let dir = tempfile::tempdir().unwrap();
        let mut job = system_job(dir.path());
        job.staged.set_extension("deb");
        assert!(job.validate().is_err());
    }

    #[test]
    fn system_package_apply_requires_pkexec_helper_and_policy() {
        let install = InstallKind::SystemPackage {
            family: PackageFamily::Pacman,
            managed: true,
        };
        assert!(install.system_package_apply(true, true, true));
        assert!(!install.system_package_apply(false, true, true));
        assert!(!install.system_package_apply(true, false, true));
        assert!(!install.system_package_apply(true, true, false));
        assert!(!InstallKind::Development.system_package_apply(true, true, true));
        assert!(
            !InstallKind::SystemPackage {
                family: PackageFamily::Unknown,
                managed: true,
            }
            .system_package_apply(true, true, true)
        );
        assert!(
            !InstallKind::SystemPackage {
                family: PackageFamily::Pacman,
                managed: false,
            }
            .system_package_apply(true, true, true)
        );
    }

    #[test]
    fn can_apply_automatically_accepts_appimage_and_capable_system_packages() {
        assert!(
            InstallKind::AppImage {
                path: "/tmp/VRC.AppImage".into()
            }
            .can_apply_automatically(false, false, false)
        );
        let install = InstallKind::SystemPackage {
            family: PackageFamily::Deb,
            managed: true,
        };
        assert!(install.can_apply_automatically(true, true, true));
        assert!(!install.can_apply_automatically(true, true, false));
        assert!(!InstallKind::Development.can_apply_automatically(true, true, true));
    }

    #[cfg(unix)]
    #[test]
    fn finds_pkexec_in_a_path_directory() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let pkexec = dir.path().join("pkexec");
        std::fs::write(&pkexec, b"#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&pkexec, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(find_executable_in(&[dir.path().to_path_buf()], "pkexec"));
        std::fs::set_permissions(&pkexec, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(!find_executable_in(&[dir.path().to_path_buf()], "pkexec"));
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

        let mut relative_working_dir = job.clone();
        relative_working_dir.working_dir = "relative".into();
        assert!(relative_working_dir.validate().is_err());

        let mut relative_database = job.clone();
        relative_database.database = Some(NativeString::new("relative.db"));
        assert!(relative_database.validate().is_err());

        #[cfg(unix)]
        {
            use std::ffi::OsString;
            use std::os::unix::ffi::OsStringExt;
            let mut non_utf8 = job.clone();
            non_utf8.restart_args = vec![NativeString::from(OsString::from_vec(vec![b'x', 0x80]))];
            let json = serde_json::to_string(&non_utf8).unwrap();
            let decoded: ApplyJob = serde_json::from_str(&json).unwrap();
            assert_eq!(decoded.restart_args, non_utf8.restart_args);
            decoded.validate().unwrap();
        }
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
            "vrc-photon-updater.exe"
        } else {
            "vrc-photon-updater"
        };
        let current_exe = install.join(if cfg!(windows) {
            "vrc-photon.exe"
        } else {
            "vrc-photon"
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
        let exe = Path::new(r"C:\Users\me\AppData\Local\VRC PhotoN\vrc-photon.exe");
        assert_eq!(
            classify_windows(
                exe,
                Some(Path::new(r"C:\Users\me\AppData\Local\VRC PhotoN")),
                None
            ),
            InstallKind::WindowsCurrentUser {
                install_dir: exe.parent().unwrap().into()
            }
        );
        let machine_exe = Path::new(r"C:\Program Files\VRC PhotoN\vrc-photon.exe");
        assert_eq!(
            classify_windows(
                machine_exe,
                None,
                Some(Path::new(r"C:\Program Files\VRC PhotoN"))
            ),
            InstallKind::WindowsPerMachine {
                install_dir: machine_exe.parent().unwrap().into()
            }
        );
        assert_eq!(classify_windows(exe, None, None), InstallKind::WindowsOther);
    }
}
