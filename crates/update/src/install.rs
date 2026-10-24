use std::path::{Path, PathBuf};

use crate::error::{Result, UpdateError};

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
            path: path.to_path_buf(),
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
        let bytes = br#"{
          "schema": 1,
          "version": "0.4.0",
          "platforms": {
            "linux-x86_64": {
              "url": "https://example.com/vrc-photo-manager_0.4.0_x86_64.AppImage",
              "format": "appimage",
              "size": 10,
              "sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
              "signature": "sig"
            },
            "windows-x86_64": {
              "url": "https://example.com/vrc-photo-manager_0.4.0_x64-setup.exe",
              "format": "nsis",
              "size": 10,
              "sha256": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
              "signature": "sig"
            }
          }
        }"#;
        let (manifest, _) = Manifest::parse_and_validate(bytes, false).unwrap();

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
        assert!(matches!(
            classify_windows(
                exe,
                None,
                Some(Path::new(r"C:\Program Files\VRC Photo Manager"))
            ),
            InstallKind::WindowsPerMachine { .. }
        ));
        assert_eq!(classify_windows(exe, None, None), InstallKind::WindowsOther);
    }
}
