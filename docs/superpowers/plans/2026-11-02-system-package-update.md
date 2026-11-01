# System Package Update Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** pacman / deb の既存インストールを、署名済みパッケージと polkit を使ってアプリ内から更新できるようにする。

**Architecture:** `latest.json` に後方互換な `system_packages` を追加し、既存の download / 署名検証パイプラインで `.pkg.tar.zst` / `.deb` を staging する。UI はユーザー権限の `vrc-photo-updater --apply` を起動し、そのヘルパーが `pkexec /usr/bin/vrc-photo-updater --apply-system-package` を呼ぶ。特権側は manifest・staging・パッケージメタデータを再検証し、固定コマンドでインストールするだけに留める。

**Tech Stack:** Rust stable (edition 2024), serde/serde_json, minisign-verify, semver, sha2, gpui-kit 0.8, polkit/pkexec, pacman, dpkg, cargo-packager 0.11.8, Arch `base-devel` container, GitHub Actions.

**Spec:** `docs/superpowers/specs/2026-11-02-system-package-update-design.md`

## Global Constraints

- `latest.json` の `schema` は 1 のまま。`system_packages` は任意フィールドで、既存クライアントの `platforms` 検証を変えない。
- pacman の更新用成果物は `vrc-photo-manager-<version>-1-x86_64.pkg.tar.zst`。`.tar.gz` + `PKGBUILD` は手動 / AUR 用に残す。
- deb の更新用成果物は `vrc-photo-manager_<version>_amd64.deb`。
- 特権側が実行するインストールコマンドは `pacman -U --noconfirm <file>` と `dpkg -i <file>` に固定する。シェルを使わない。
- 特権側は `signed_manifest` の minisign 署名、`manifest.version == job.to_version`、`manifest.version > CURRENT_VERSION`、パッケージ名 `vrc-photo-manager`、パッケージバージョン一致を必須にする。
- 特権側は `/var/tmp/vrc-photo-update-<uuid>.pkg` に root 所有でコピーし、コピー先で SHA-256 と署名を再検証してからインストールする。
- `--apply` は euid 0 で拒否する。`--apply-system-package` は euid 0 かつ `PKEXEC_UID` 必須。
- polkit policy は `usr/share/polkit-1/actions/com.tommyz.vrc-photo-manager.update.policy` に同梱し、`org.freedesktop.policykit.exec.path` を `/usr/bin/vrc-photo-updater` にする。
- システムパッケージの自動ロールバックは行わない。失敗時は `last-result.json` と `updater.log` に理由を残す。
- 認証キャンセル / DB ロック / 検証失敗では job と staged を掃除する。`PackageManagerFailed` / health 失敗では job / staged / log を残す。
- UI は `pkexec`、`/usr/bin/vrc-photo-updater`、policy の3つが揃う場合だけ自動適用を出す。欠ける場合は既存のパッケージマネージャコマンドを表示する。
- systemd 管理のデーモンは `systemctl --user` で start / restart する。`daemon-reload` は失敗しても続行する。
- DB スキーマは変更しない。設定は既存の `AppSettings` に追加しない。
- ローカル検証は `cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`。

## Review Focus

- 署名済み manifest のパッケージ名・バージョンが実際の `.pkg.tar.zst` / `.deb` と違う場合、インストール前に拒否されること。Task 5 の `system_package_rejects_a_package_name_mismatch` / `system_package_rejects_a_package_version_mismatch` で固定する。
- job の `staged` が staging 外、symlink、別ユーザー所有の場合に root のファイル操作が始まらないこと。Task 4 の `system_package_rejects_a_staged_file_outside_the_download_dir` / `system_package_rejects_a_symlink` で固定する。
- pacman / dpkg の DB ロック時に部分インストールせず、再試行可能なエラーになること。Task 5 の lock テストで固定する。
- polkit 認証キャンセル / エージェントなしでシステムに変更が入らず、UI が戻ってコマンド表示にフォールバックできること。Task 6 の `maps_pkexec_exit_codes` と Task 7 の `system_package_without_auto_apply_shows_the_package_command` で固定する。
- systemd 管理のデーモンが更新後・失敗後の再起動で unit 外のプロセスにならないこと。Task 8 の `systemd_supervisor_uses_systemctl` / `start_daemon_uses_spawn_for_app_supervisor` で固定する。

---

### Task 1: Manifest と release tool に system package asset を追加

**Files:**
- Modify: `crates/update/src/manifest.rs`
- Modify: `crates/update/src/install.rs` (`PackageFamily` の serde と `manifest_key`)
- Modify: `crates/release-tool/src/lib.rs`
- Test: 上記ファイル内の `#[cfg(test)]`

**Interfaces:**
- Produces: `UpdateFormat::{Pacman, Deb}`
- Produces: `Manifest.system_packages: BTreeMap<String, PlatformAsset>`
- Produces: `PackageFamily::manifest_key(self) -> Option<&'static str>`
- Produces: `PackageFamily::from_manifest_key(key: &str) -> Option<Self>`
- Produces: `Manifest::system_package_for(&self, family: PackageFamily) -> Option<&PlatformAsset>`
- Produces: `build_manifest` が `system_packages` を含む `Manifest` を返す

- [ ] **Step 1: manifest の失敗テストを書く**

`crates/update/src/manifest.rs` の tests に追加する。`test_signature()` は既存のものを使う。

```rust
fn system_packages() -> Value {
    serde_json::json!({
        "pacman": {
            "url": "https://example.com/vrc-photo-manager-0.4.0-1-x86_64.pkg.tar.zst",
            "format": "pacman",
            "size": 1024,
            "sha256": "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
            "signature": crate::verify::test_signature()
        },
        "deb": {
            "url": "https://example.com/vrc-photo-manager_0.4.0_amd64.deb",
            "format": "deb",
            "size": 1024,
            "sha256": "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd",
            "signature": crate::verify::test_signature()
        }
    })
}

#[test]
fn validates_pacman_and_deb_system_packages() {
    let mut value = valid_manifest();
    value["system_packages"] = system_packages();
    let (manifest, _) = parse(&value, false).unwrap();
    assert_eq!(manifest.system_packages.len(), 2);
    assert_eq!(
        manifest.system_packages["pacman"].format,
        UpdateFormat::Pacman
    );
    assert_eq!(manifest.system_packages["deb"].format, UpdateFormat::Deb);
}

#[test]
fn accepts_a_manifest_without_system_packages() {
    let value = valid_manifest();
    assert!(parse(&value, false).is_ok());
}

#[test]
fn rejects_unknown_system_package_keys() {
    let mut value = valid_manifest();
    let mut packages = system_packages();
    packages["rpm"] = packages["deb"].clone();
    value["system_packages"] = packages;
    assert!(matches!(
        parse(&value, false),
        Err(UpdateError::InvalidManifest(_))
    ));
}

#[test]
fn rejects_a_system_package_with_the_wrong_format() {
    let mut value = valid_manifest();
    let mut packages = system_packages();
    packages["pacman"]["format"] = serde_json::json!("deb");
    value["system_packages"] = packages;
    assert!(matches!(
        parse(&value, false),
        Err(UpdateError::InvalidManifest(_))
    ));
}

#[test]
fn rejects_a_system_package_with_the_wrong_extension() {
    let mut value = valid_manifest();
    let mut packages = system_packages();
    packages["pacman"]["url"] =
        serde_json::json!("https://example.com/vrc-photo-manager_0.4.0_x86_64.tar.gz");
    value["system_packages"] = packages;
    assert!(matches!(
        parse(&value, false),
        Err(UpdateError::InvalidManifest(_))
    ));
}
```

- [ ] **Step 2: manifest テストを失敗させる**

Run: `cargo test -p vrc-photo-update manifest::tests::validates_pacman_and_deb_system_packages`
Expected: FAIL（`system_packages` フィールドがない、または `UpdateFormat::Pacman` がない）

- [ ] **Step 3: `PackageFamily` に serde と manifest key を追加する**

`crates/update/src/install.rs`:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PackageFamily {
    Deb,
    Pacman,
    Unknown,
}

impl PackageFamily {
    pub fn manifest_key(self) -> Option<&'static str> {
        match self {
            Self::Pacman => Some("pacman"),
            Self::Deb => Some("deb"),
            Self::Unknown => None,
        }
    }

    pub fn from_manifest_key(key: &str) -> Option<Self> {
        match key {
            "pacman" => Some(Self::Pacman),
            "deb" => Some(Self::Deb),
            _ => None,
        }
    }
}
```

- [ ] **Step 4: `Manifest` / `PlatformAsset` を実装する**

`crates/update/src/manifest.rs`:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum UpdateFormat {
    #[serde(rename = "appimage")]
    AppImage,
    #[serde(rename = "nsis")]
    Nsis,
    #[serde(rename = "pacman")]
    Pacman,
    #[serde(rename = "deb")]
    Deb,
}

pub struct Manifest {
    // 既存フィールド ...
    #[serde(default)]
    pub system_packages: BTreeMap<String, PlatformAsset>,
}

impl Manifest {
    pub fn system_package_for(&self, family: PackageFamily) -> Option<&PlatformAsset> {
        self.system_packages.get(family.manifest_key()?)
    }
}
```

`parse_and_validate` で `system_packages` を検証する。`PlatformAsset` の共通検証（size / sha256 / signature / URL / file name）を
`validate_common(&self, label: &str, allow_http: bool) -> Result<(Url, String)>` に切り出し、既存の
`validate` と新しい `validate_system_package(family, allow_http)` の両方から呼ぶ。

```rust
fn validate_system_package(&self, family: PackageFamily, allow_http: bool) -> Result<()> {
    let (expected_format, suffix) = match family {
        PackageFamily::Pacman => (UpdateFormat::Pacman, ".pkg.tar.zst"),
        PackageFamily::Deb => (UpdateFormat::Deb, ".deb"),
        PackageFamily::Unknown => {
            return Err(UpdateError::InvalidManifest(
                "unknown system package family".to_string(),
            ));
        }
    };
    let (_, file_name) = self.validate_common("system package", allow_http)?;
    if self.format != expected_format {
        return Err(UpdateError::InvalidManifest(format!(
            "system package format {:?} does not match {family:?}",
            self.format
        )));
    }
    if !file_name.to_ascii_lowercase().ends_with(suffix) {
        return Err(UpdateError::InvalidManifest(format!(
            "unexpected system package file name {file_name}"
        )));
    }
    Ok(())
}
```

`parse_and_validate` の `platforms` ループの後に:

```rust
for (key, asset) in &manifest.system_packages {
    let family = PackageFamily::from_manifest_key(key).ok_or_else(|| {
        UpdateError::InvalidManifest(format!("unknown system package key {key}"))
    })?;
    asset.validate_system_package(family, allow_http)?;
}
```

`asset_for` の `_ => return None` の前に:

```rust
InstallKind::SystemPackage { family } => self.system_package_for(*family),
```

- [ ] **Step 5: manifest テストを通す**

Run: `cargo test -p vrc-photo-update manifest::tests`
Expected: PASS

- [ ] **Step 6: release-tool の失敗テストを書く**

`crates/release-tool/src/lib.rs` の tests に追加する。既存の `write_artifact` を使う。

```rust
#[test]
fn builds_a_manifest_with_system_packages() {
    let dir = tempfile::tempdir().unwrap();
    for (name, body) in [
        ("vrc-photo-manager_0.4.0_x86_64.AppImage", b"appimage".as_slice()),
        ("vrc-photo-manager_0.4.0_x64-setup.exe", b"installer".as_slice()),
        (
            "vrc-photo-manager-0.4.0-1-x86_64.pkg.tar.zst",
            b"pacman".as_slice(),
        ),
        ("vrc-photo-manager_0.4.0_amd64.deb", b"deb".as_slice()),
    ] {
        write_artifact(dir.path(), name, body);
    }
    let manifest = build_manifest(
        dir.path(),
        "v0.4.0",
        "TommyZ-7/vrc-photo-manager",
        "https://example.com/notes",
    )
    .unwrap();
    assert_eq!(manifest.platforms.len(), 2);
    assert_eq!(manifest.system_packages.len(), 2);
    assert_eq!(
        manifest.system_packages["pacman"].format,
        UpdateFormat::Pacman
    );
    assert_eq!(manifest.system_packages["deb"].format, UpdateFormat::Deb);
    assert!(
        manifest.system_packages["pacman"]
            .url
            .ends_with("vrc-photo-manager-0.4.0-1-x86_64.pkg.tar.zst")
    );

    let json = serde_json::to_vec(&manifest).unwrap();
    let (parsed, version) = Manifest::parse_and_validate(&json, false).unwrap();
    assert_eq!(version.to_string(), "0.4.0");
    assert_eq!(parsed.system_packages.len(), 2);
}
```

既存の `builds_a_manifest_for_both_platforms` と拒否テストの fixture にも `.pkg.tar.zst` と `.deb` を追加する
（追加しないと `build_manifest` が system package 欠落で失敗する）。

- [ ] **Step 7: release-tool テストを失敗させる**

Run: `cargo test -p vrc-photo-release-tool builds_a_manifest_with_system_packages`
Expected: FAIL（`system_packages` がない、または `.pkg.tar.zst` が見つからない）

- [ ] **Step 8: release-tool を実装する**

`build_manifest`:

```rust
let pacman = unique_artifact(
    dist,
    |name| name.ends_with(".pkg.tar.zst"),
    "pacman package",
)?;
let deb = unique_artifact(dist, |name| name.ends_with(".deb"), "Debian package")?;
for path in [&appimage, &nsis, &pacman, &deb] {
    let name = file_name(path)?;
    if !name.contains(version) {
        bail!("artifact name {name} does not contain the tag version {version}");
    }
}
let mut system_packages = BTreeMap::new();
system_packages.insert(
    "pacman".to_string(),
    asset(dist, &pacman, tag, repo, UpdateFormat::Pacman)?,
);
system_packages.insert(
    "deb".to_string(),
    asset(dist, &deb, tag, repo, UpdateFormat::Deb)?,
);
Ok(Manifest {
    schema: 1,
    version: version.to_string(),
    pub_date: Some(...),
    notes_url: Some(notes_url.to_string()),
    platforms,
    system_packages,
})
```

- [ ] **Step 9: release-tool テストを通す**

Run: `cargo test -p vrc-photo-release-tool`
Expected: PASS

- [ ] **Step 10: commit**

```bash
git add crates/update/src/manifest.rs crates/update/src/install.rs crates/release-tool/src/lib.rs
git commit -m "feat(update): add system package assets to the signed manifest"
```

---

### Task 2: Apply job と pkexec / helper / policy 判定

**Files:**
- Modify: `crates/update/src/install.rs`
- Test: `crates/update/src/install.rs` の `#[cfg(test)]`

**Interfaces:**
- Consumes: Task 1 の `PackageFamily`
- Produces: `ApplyKind::SystemPackage { family: PackageFamily }`
- Produces: `ApplyJob.signed_manifest: Option<String>`
- Produces: `ApplyJob.manifest_signature: Option<String>`
- Produces: `InstallKind::system_package_apply(&self, pkexec: bool, helper: bool, policy: bool) -> bool`
- Produces: `InstallKind::can_apply_automatically(&self, pkexec: bool, helper: bool, policy: bool) -> bool`
- Produces: `pkexec_available() -> bool`
- Produces: `system_helper_available() -> bool`
- Produces: `system_policy_available() -> bool`
- Produces: `find_executable_in(dirs: &[PathBuf], name: &str) -> bool`

- [ ] **Step 1: 失敗テストを書く**

`crates/update/src/install.rs` tests に追加する。`test_job` は AppImage 用なので、system package 用のヘルパーを作る。

```rust
fn system_job(dir: &Path) -> ApplyJob {
    let data_dir = dir.join("data");
    let staged = data_dir
        .join("updates/downloads/0.4.0")
        .join("vrc-photo-manager-0.4.0-1-x86_64.pkg.tar.zst");
    std::fs::create_dir_all(staged.parent().unwrap()).unwrap();
    std::fs::write(&staged, b"package").unwrap();
    ApplyJob {
        schema: APPLY_SCHEMA,
        kind: ApplyKind::SystemPackage {
            family: PackageFamily::Pacman,
        },
        from_version: "0.3.0".to_string(),
        to_version: "0.4.0".to_string(),
        target: "/usr/bin/vrc-photo-manager".into(),
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
fn apply_kind_keeps_existing_json_and_adds_system_package() {
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
    };
    assert!(install.system_package_apply(true, true, true));
    assert!(!install.system_package_apply(false, true, true));
    assert!(!install.system_package_apply(true, false, true));
    assert!(!install.system_package_apply(true, true, false));
    assert!(!InstallKind::Development.system_package_apply(true, true, true));
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
```

- [ ] **Step 2: テストを失敗させる**

Run: `cargo test -p vrc-photo-update install::tests::apply_kind_keeps_existing_json_and_adds_system_package`
Expected: FAIL（`SystemPackage` variant がない）

- [ ] **Step 3: `ApplyKind` / `ApplyJob` を実装する**

`ApplyKind` に `SystemPackage { family: PackageFamily }` を追加する。`ApplyJob` に:

```rust
#[serde(default)]
pub signed_manifest: Option<String>,
#[serde(default)]
pub manifest_signature: Option<String>,
```

`validate()` に system package 分岐を追加する。`to_version` は `parse_job_version` で正規化し、
`UpdatePaths::new(&self.data_dir).downloads_dir().join(version.to_string())` を期待ディレクトリにする。

```rust
if let ApplyKind::SystemPackage { family } = self.kind {
    if !matches!(family, PackageFamily::Pacman | PackageFamily::Deb) {
        return Err(UpdateError::InvalidJob(
            "unsupported system package family".to_string(),
        ));
    }
    let manifest = self.signed_manifest.as_deref().ok_or_else(|| {
        UpdateError::InvalidJob("signed manifest is missing".to_string())
    })?;
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
    if !self.staged.starts_with(&expected_dir)
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
        .ok_or_else(|| UpdateError::InvalidJob("staged package has no file name".to_string()))?;
    if !file_name.ends_with(suffix) {
        return Err(UpdateError::InvalidJob(format!(
            "staged package must end with {suffix}"
        )));
    }
}
```

`install.rs` の先頭で `use crate::manifest::{MAX_MANIFEST_BYTES, MAX_SIGNATURE_BYTES};` を追加する。

- [ ] **Step 4: capability 判定を実装する**

`crates/update/src/install.rs`:

```rust
pub const SYSTEM_HELPER_PATH: &str = "/usr/bin/vrc-photo-updater";
pub const SYSTEM_POLICY_PATH: &str =
    "/usr/share/polkit-1/actions/com.tommyz.vrc-photo-manager.update.policy";

impl InstallKind {
    pub fn system_package_apply(
        &self,
        pkexec_available: bool,
        helper_available: bool,
        policy_available: bool,
    ) -> bool {
        matches!(
            self,
            Self::SystemPackage {
                family: PackageFamily::Pacman | PackageFamily::Deb
            }
        ) && pkexec_available
            && helper_available
            && policy_available
    }

    pub fn can_apply_automatically(
        &self,
        pkexec_available: bool,
        helper_available: bool,
        policy_available: bool,
    ) -> bool {
        self.automatic_apply()
            || self.system_package_apply(
                pkexec_available,
                helper_available,
                policy_available,
            )
    }
}

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

pub fn system_helper_available() -> bool {
    std::path::Path::new(SYSTEM_HELPER_PATH).is_file()
}

pub fn system_policy_available() -> bool {
    std::path::Path::new(SYSTEM_POLICY_PATH).is_file()
}
```

`crates/update/src/lib.rs` の re-export に新しい関数と定数を追加する。

- [ ] **Step 5: テストを通す**

Run: `cargo test -p vrc-photo-update install::tests`
Expected: PASS

- [ ] **Step 6: commit**

```bash
git add crates/update/src/install.rs crates/update/src/lib.rs
git commit -m "feat(update): add system package apply jobs and capability checks"
```

---

### Task 3: Check / AvailableUpdate に署名済み manifest を保持する

**Files:**
- Modify: `crates/update/src/manifest.rs`
- Modify: `crates/update/src/check.rs`
- Modify: `crates/update/src/download.rs` (test literals)
- Modify: `crates/ui/src/update_view.rs` (test literals only)
- Test: 上記ファイル内の `#[cfg(test)]`

**Interfaces:**
- Consumes: Task 1 の `system_packages`
- Produces: `SignedManifest { bytes: Vec<u8>, signature: String }`
- Produces: `AvailableUpdate.signed_manifest: SignedManifest`

- [ ] **Step 1: 失敗テストを書く**

`crates/update/src/manifest.rs`:

```rust
#[test]
fn available_update_carries_the_signed_manifest() {
    let update = AvailableUpdate {
        version: semver::Version::new(0, 4, 0),
        notes_url: None,
        asset: None,
        public_keys: Vec::new(),
        signed_manifest: SignedManifest {
            bytes: b"{\"schema\":1}".to_vec(),
            signature: "signature".to_string(),
        },
    };
    assert_eq!(update.signed_manifest.bytes, b"{\"schema\":1}");
    assert_eq!(update.signed_manifest.signature, "signature");
}
```

`crates/update/src/check.rs` の tests に、system_packages を含む fixture を作るヘルパーを追加し:

```rust
fn system_platforms() -> serde_json::Value {
    serde_json::json!({
        "linux-x86_64": {
            "url": "https://example.com/vrc-photo-manager_0.4.0_x86_64.AppImage",
            "format": "appimage",
            "size": 1024,
            "sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "signature": crate::verify::test_signature()
        },
        "system_packages": {
            "pacman": {
                "url": "https://example.com/vrc-photo-manager-0.4.0-1-x86_64.pkg.tar.zst",
                "format": "pacman",
                "size": 1024,
                "sha256": "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
                "signature": crate::verify::test_signature()
            }
        }
    })
}

#[test]
fn system_package_check_selects_the_pacman_asset_and_keeps_the_manifest() {
    let fixture = fixture("0.4.0", system_platforms());
    let client = FakeHttpClient::new(HashMap::from([
        (fixture.manifest_url.clone(), Ok(fixture.manifest.clone())),
        (
            fixture.signature_url.clone(),
            Ok(fixture.signature.clone().into_bytes()),
        ),
    ]));
    let feed = FeedConfig::for_tests(
        fixture.manifest_url.clone(),
        vec![fixture.public_key.clone()],
        false,
    );
    let update = check(
        &client,
        &feed,
        &Version::new(0, 3, 0),
        &InstallKind::SystemPackage {
            family: PackageFamily::Pacman,
        },
    )
    .unwrap()
    .unwrap();
    assert_eq!(update.asset.unwrap().format, UpdateFormat::Pacman);
    assert_eq!(update.signed_manifest.bytes, fixture.manifest);
    assert_eq!(update.signed_manifest.signature, fixture.signature);
}
```

既存の `system_packages_get_the_version_without_an_asset` は、`system_packages` を含む manifest では
asset が `Some` になるよう名前と期待値を変更する。`system_packages` のない旧 manifest 用に
`old_manifest_without_system_packages_has_no_asset` を追加する。

- [ ] **Step 2: テストを失敗させる**

Run: `cargo test -p vrc-photo-update check::tests::system_package_check_selects_the_pacman_asset_and_keeps_the_manifest`
Expected: FAIL（`signed_manifest` がない）

- [ ] **Step 3: `SignedManifest` / `AvailableUpdate` を実装する**

`crates/update/src/manifest.rs`:

```rust
#[derive(Debug, Clone)]
pub struct SignedManifest {
    pub bytes: Vec<u8>,
    pub signature: String,
}

pub struct AvailableUpdate {
    pub version: Version,
    pub notes_url: Option<String>,
    pub asset: Option<PlatformAsset>,
    pub public_keys: Vec<String>,
    pub signed_manifest: SignedManifest,
}
```

`crates/update/src/check.rs` の `Ok(Some(AvailableUpdate { ... }))` に:

```rust
signed_manifest: SignedManifest {
    bytes: manifest_bytes,
    signature: signature_text,
},
```

`crates/update/src/lib.rs` の re-export に `SignedManifest` を追加する。

- [ ] **Step 4: 既存の `AvailableUpdate` リテラルを更新する**

`crates/update/src/download.rs` の `available()` と、`crates/ui/src/update_view.rs` の tests にある
`AvailableUpdate { ... }` に以下を追加する。

```rust
signed_manifest: vrc_photo_update::SignedManifest {
    bytes: Vec::new(),
    signature: String::new(),
},
```

- [ ] **Step 5: テストを通す**

Run: `cargo test -p vrc-photo-update`
Expected: PASS

Run: `cargo test -p vrc-photo-ui update_view::tests`
Expected: PASS

- [ ] **Step 6: commit**

```bash
git add crates/update/src/manifest.rs crates/update/src/check.rs crates/update/src/download.rs crates/update/src/lib.rs crates/ui/src/update_view.rs
git commit -m "feat(update): carry the signed manifest with available updates"
```

---

### Task 4: 特権側の staging / manifest 検証と root temp copy

**Files:**
- Modify: `crates/update/src/error.rs`
- Modify: `crates/update/src/helper.rs`
- Test: `crates/update/src/helper.rs` の `#[cfg(test)]`

**Interfaces:**
- Consumes: Task 1 の `system_packages`、Task 3 の `SignedManifest`
- Produces: `LastErrorCode`（`AuthCancelled`, `AuthUnavailable`, `PackageManagerLocked`, `PackageManagerFailed`, `PackageVerificationFailed`, `PackageMetadataMismatch`, `Downgrade`, `HealthCheckFailed`, `HelperFailed`）
- Produces: `LastResult.code: Option<LastErrorCode>`
- Produces: `UpdateError::{NotPrivileged, SystemHelperMissing, StagingRejected, PackageManagerLocked, PackageManagerUnavailable, PackageMetadataMismatch, Downgrade, PackageManagerFailed}`
- Produces: `UpdateError::helper_exit_code() -> i32`
- Produces: `TempPackage`
- Produces: `verify_system_package(job: &ApplyJob, options: &HelperOptions) -> Result<TempPackage>`

- [ ] **Step 1: 失敗テストを書く**

`crates/update/src/helper.rs` の tests に、system package 用の fixture を追加する。
`minisign::KeyPair` と `sha2` は既存の dev-dependency を使う。

```rust
#[cfg(unix)]
fn next_version() -> String {
    let mut version = semver::Version::parse(crate::CURRENT_VERSION).unwrap();
    version.patch += 1;
    version.to_string()
}

#[cfg(unix)]
fn system_package_fixture(dir: &Path, version: &str) -> (ApplyJob, HelperOptions) {
    use base64::Engine as _;
    use minisign::KeyPair;
    use sha2::{Digest as _, Sha256};
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
    let package = b"fake pacman package";
    let asset_signature = minisign::sign(
        None,
        &keypair.sk,
        std::io::Cursor::new(package),
        None,
        None,
    )
    .unwrap()
    .into_string();
    let manifest = serde_json::to_vec(&serde_json::json!({
        "schema": 1,
        "version": version,
        "system_packages": {
            "pacman": {
                "url": format!(
                    "https://example.com/vrc-photo-manager-{version}-1-x86_64.pkg.tar.zst"
                ),
                "format": "pacman",
                "size": package.len(),
                "sha256": hex::encode(Sha256::digest(package)),
                "signature": base64::engine::general_purpose::STANDARD
                    .encode(asset_signature.as_bytes())
            }
        }
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
        .join(format!(
            "vrc-photo-manager-{version}-1-x86_64.pkg.tar.zst"
        ));
    std::fs::create_dir_all(staged.parent().unwrap()).unwrap();
    std::fs::write(&staged, package).unwrap();
    let job = ApplyJob {
        schema: APPLY_SCHEMA,
        kind: ApplyKind::SystemPackage {
            family: PackageFamily::Pacman,
        },
        from_version: "0.3.0".to_string(),
        to_version: version.to_string(),
        target: "/usr/bin/vrc-photo-manager".into(),
        staged,
        signature: base64::engine::general_purpose::STANDARD
            .encode(asset_signature.as_bytes()),
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
        pkexec_uid: Some(std::fs::metadata(dir.path()).unwrap().uid()),
        root_temp_dir: Some(dir.join("root-temp")),
        ..HelperOptions::default()
    };
    std::fs::create_dir_all(options.root_temp_dir.as_ref().unwrap()).unwrap();
    (job, options)
}

#[test]
fn system_package_rejects_a_bad_manifest_signature() {
    let dir = tempfile::tempdir().unwrap();
    let (job, mut options) = system_package_fixture(dir.path(), &next_version());
    options.public_keys = vec![
        "RWQf6LRCGA9i53mlYecO4IzT51TGPpvWucNSCh1CBM0QTaLn73Y7GFO3".to_string(),
    ];
    assert!(matches!(
        verify_system_package(&job, &options),
        Err(UpdateError::Signature)
    ));
}

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

#[test]
fn system_package_rejects_a_downgrade() {
    let dir = tempfile::tempdir().unwrap();
    let (job, options) =
        system_package_fixture(dir.path(), crate::CURRENT_VERSION);
    assert!(matches!(
        verify_system_package(&job, &options),
        Err(UpdateError::Downgrade { .. })
    ));
}

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
```

注意: fixture の version は `CURRENT_VERSION` より大きい `next_version()` を使う。
ダウングレードテストだけ `CURRENT_VERSION` を渡す。
`public_key` の取得方法は既存の `verify.rs` テストの `public_box` パターンに合わせる。

- [ ] **Step 2: テストを失敗させる**

Run: `cargo test -p vrc-photo-update helper::tests::system_package_rejects_a_bad_manifest_signature`
Expected: FAIL（`LastErrorCode` / `verify_system_package` がない）

- [ ] **Step 3: error / LastResult の型を追加する**

`crates/update/src/error.rs` に `LastErrorCode` と新しい `UpdateError` variants を追加する。

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LastErrorCode {
    AuthCancelled,
    AuthUnavailable,
    PackageManagerLocked,
    PackageManagerFailed,
    PackageVerificationFailed,
    PackageMetadataMismatch,
    Downgrade,
    HealthCheckFailed,
    HelperFailed,
}

pub enum UpdateError {
    // 既存 ...
    #[error("this operation must run as an unprivileged user")]
    NotPrivileged,
    #[error("system update helper is missing")]
    SystemHelperMissing,
    #[error("staging rejected: {0}")]
    StagingRejected(String),
    #[error("the package manager database is locked")]
    PackageManagerLocked,
    #[error("the package manager is unavailable")]
    PackageManagerUnavailable,
    #[error("package metadata mismatch: {0}")]
    PackageMetadataMismatch(String),
    #[error("package verification failed: {0}")]
    PackageVerificationFailed(String),
    #[error("downgrade refused: current {current}, target {target}")]
    Downgrade { current: String, target: String },
    #[error("package manager failed: {family:?} {status}: {output}")]
    PackageManagerFailed {
        family: PackageFamily,
        status: String,
        output: String,
    },
}

impl UpdateError {
    pub fn helper_exit_code(&self) -> i32 {
        match self {
            Self::InvalidJob(_) | Self::NotPrivileged | Self::StagingRejected(_) => 10,
            Self::Signature | Self::PackageVerificationFailed(_) => 11,
            Self::PackageMetadataMismatch(_) => 12,
            Self::Downgrade { .. } => 13,
            Self::PackageManagerLocked => 14,
            Self::PackageManagerFailed { .. } | Self::PackageManagerUnavailable => 15,
            _ => 16,
        }
    }
}
```

`error.rs` は `PackageFamily` を使うため `use crate::install::PackageFamily;` を追加する。
`crates/update/src/lib.rs` の re-export に `LastErrorCode` を追加する。
`LastResult` に:

```rust
#[serde(default)]
pub code: Option<LastErrorCode>,
```

既存の `LastResult` リテラル（helper.rs / update_view.rs tests）に `code: None` を追加する。

- [ ] **Step 4: `HelperOptions` と `TempPackage` を追加する**

`helper.rs`:

```rust
pub struct HelperOptions {
    // 既存 ...
    #[cfg(test)]
    pub pkexec_uid: Option<u32>,
    #[cfg(test)]
    pub root_temp_dir: Option<PathBuf>,
    #[cfg(test)]
    pub pacman_path: Option<PathBuf>,
    #[cfg(test)]
    pub dpkg_path: Option<PathBuf>,
    #[cfg(test)]
    pub dpkg_deb_path: Option<PathBuf>,
}
```

`Default` で `None` を設定する。

```rust
struct TempPackage {
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
```

- [ ] **Step 5: staging / manifest / copy 検証を実装する**

`helper.rs` の先頭に `use semver::Version;` と
`use crate::manifest::{Manifest, PlatformAsset};` を追加する。

```rust
fn pkexec_uid(options: &HelperOptions) -> Result<u32> {
    #[cfg(test)]
    if let Some(uid) = options.pkexec_uid {
        return Ok(uid);
    }
    std::env::var("PKEXEC_UID")
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
        .filter(|uid| *uid > 0)
        .ok_or(UpdateError::NotPrivileged)
}

#[cfg(unix)]
fn verify_staging(job: &ApplyJob, uid: u32) -> Result<()> {
    use std::os::unix::fs::MetadataExt;

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
    let expected = data_dir
        .join("updates/downloads")
        .join(version.to_string());
    let expected = std::fs::canonicalize(&expected)
        .map_err(|error| UpdateError::StagingRejected(error.to_string()))?;
    if !expected.starts_with(&data_dir) {
        return Err(UpdateError::StagingRejected(
            "downloads directory escapes the data directory".to_string(),
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
    let keys = if options.public_keys.is_empty() {
        effective_public_keys()
    } else {
        options.public_keys.clone()
    };
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
        .ok_or_else(|| UpdateError::InvalidManifest("system package asset is missing".to_string()))?;
    if asset.file_name()? != job.staged.file_name().and_then(|name| name.to_str()).unwrap_or_default()
    {
        return Err(UpdateError::InvalidJob(
            "manifest asset does not match the staged file name".to_string(),
        ));
    }
    if asset.size != std::fs::metadata(&job.staged).map_err(|error| UpdateError::io(&job.staged, error))?.len()
    {
        return Err(UpdateError::Integrity(
            "staged package size does not match the manifest".to_string(),
        ));
    }
    Ok((manifest, version, asset))
}

fn copy_and_verify(
    job: &ApplyJob,
    asset: &PlatformAsset,
    options: &HelperOptions,
) -> Result<TempPackage> {
    let dir = options
        .root_temp_dir
        .clone()
        .unwrap_or_else(|| PathBuf::from("/var/tmp"));
    let temp = TempPackage::create(&dir)?;
    let mut source = std::fs::File::open(&job.staged)
        .map_err(|error| UpdateError::io(&job.staged, error))?;
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
    target.sync_all().map_err(|error| UpdateError::io(temp.path(), error))?;
    let digest = hex::encode(hasher.finalize());
    if digest != asset.sha256 {
        return Err(UpdateError::Integrity(
            "staged package SHA-256 does not match the manifest".to_string(),
        ));
    }
    let signature = decode_signature_base64(&asset.signature)?;
    let keys = if options.public_keys.is_empty() {
        effective_public_keys()
    } else {
        options.public_keys.clone()
    };
    verify_file(temp.path(), &signature, &keys)?;
    Ok(temp)
}

pub(crate) fn verify_system_package(
    job: &ApplyJob,
    options: &HelperOptions,
) -> Result<TempPackage> {
    let uid = pkexec_uid(options)?;
    verify_staging(job, uid)?;
    let (_, _, asset) = verify_manifest(job, options)?;
    copy_and_verify(job, &asset, options)
}
```

`verify_manifest` の `asset.file_name()` は `Result<String>` なので `?` で文字列にし、
`job.staged.file_name()` と比較する。job とのバージョン比較には、manifest の生文字列ではなく
`parse_and_validate` が返す `Version` を使う。

- [ ] **Step 6: テストを通す**

Run: `cargo test -p vrc-photo-update helper::tests::system_package`
Expected: PASS（ダウングレード / 署名 / staging / symlink）

- [ ] **Step 7: `LastResult` の code を既存テストに反映する**

`helper.rs` の既存 `LastResult` リテラルと `crates/ui/src/update_view.rs` tests の
`LastResult` リテラルに `code: None` を追加する。

Run: `cargo test -p vrc-photo-update helper::tests`
Expected: PASS

- [ ] **Step 8: commit**

```bash
git add crates/update/src/error.rs crates/update/src/helper.rs crates/ui/src/update_view.rs
git commit -m "feat(update): verify system package staging and signed manifests"
```

---

### Task 5: パッケージメタデータ検証と固定インストールコマンド

**Files:**
- Modify: `crates/update/src/helper.rs`
- Test: `crates/update/src/helper.rs` の `#[cfg(test)]`

**Interfaces:**
- Consumes: Task 4 の `TempPackage` / `verify_system_package`
- Produces: `PackageMetadata { name: String, version: String }`
- Produces: `read_package_metadata(family: PackageFamily, path: &Path, options: &HelperOptions) -> Result<PackageMetadata>`
- Produces: `install_package(family: PackageFamily, path: &Path, options: &HelperOptions) -> Result<()>`
- Produces: `run_system_package(job_path: &Path, options: &HelperOptions) -> Result<()>`

- [ ] **Step 1: 失敗テストを書く**

`crates/update/src/helper.rs` の tests に fake command scripts を追加する。

```rust
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
            "#!/bin/sh\nif [ \"$1\" = \"-Qip\" ]; then\ncat <<'EOF'\n{metadata}\nEOF\nexit 0\nfi\nif [ \"$1\" = \"-U\" ]; then\necho \"$@\" >> \"{log}\"\necho \"{install_stderr}\" >&2\nexit {install_exit}\nfi\nexit 2\n",
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
        &format!("#!/bin/sh\ncat <<'EOF'\n{metadata}\nEOF\n"),
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

#[test]
fn package_version_normalization_accepts_revisions() {
    assert!(package_version_matches("0.4.0", "0.4.0"));
    assert!(package_version_matches("0.4.0-1", "0.4.0"));
    assert!(package_version_matches("1:0.4.0-1ubuntu1", "0.4.0"));
    assert!(!package_version_matches("0.4.1-1", "0.4.0"));
}

#[cfg(unix)]
#[test]
fn system_package_rejects_a_package_name_mismatch() {
    let dir = tempfile::tempdir().unwrap();
    let (job, mut options) = system_package_fixture(dir.path(), &next_version());
    options.pacman_path = Some(fake_pacman(
        dir.path(),
        "Name            : evil\nVersion         : 0.4.0-1",
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
    let metadata = format!("Name            : vrc-photo-manager\nVersion         : {version}-1");
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
fn system_package_runs_the_fixed_pacman_command() {
    let dir = tempfile::tempdir().unwrap();
    let version = next_version();
    let (job, mut options) = system_package_fixture(dir.path(), &version);
    let metadata = format!("Name            : vrc-photo-manager\nVersion         : {version}-1");
    options.pacman_path = Some(fake_pacman(
        dir.path(),
        &metadata,
        0,
        "",
    ));
    run_system_package_from_job(&job, &options).unwrap();
    let args = std::fs::read_to_string(dir.path().join("pacman-args.log")).unwrap();
    assert!(args.contains("-U --noconfirm"));
}
```

`system_package_fixture` は Task 4 のものを使う。`run_system_package_from_job` は
`run_system_package` の内部処理を job 直接受けでテストするための `pub(crate)` 関数にする。

- [ ] **Step 2: テストを失敗させる**

Run: `cargo test -p vrc-photo-update helper::tests::package_version_normalization_accepts_revisions`
Expected: FAIL（`package_version_matches` がない）

- [ ] **Step 3: メタデータとコマンドを実装する**

`helper.rs`:

```rust
pub struct PackageMetadata {
    pub name: String,
    pub version: String,
}

fn package_version_matches(raw: &str, expected: &str) -> bool {
    let raw = raw.split_once(':').map(|(_, value)| value).unwrap_or(raw);
    raw == expected
        || raw
            .strip_prefix(expected)
            .is_some_and(|rest| rest.starts_with('-'))
}

fn read_package_metadata(
    family: PackageFamily,
    path: &Path,
    options: &HelperOptions,
) -> Result<PackageMetadata> {
    let (program, args) = match family {
        PackageFamily::Pacman => (
            options
                .pacman_path
                .clone()
                .unwrap_or_else(|| PathBuf::from("/usr/bin/pacman")),
            vec!["-Qip".to_string(), path.display().to_string()],
        ),
        PackageFamily::Deb => (
            options
                .dpkg_deb_path
                .clone()
                .unwrap_or_else(|| PathBuf::from("/usr/bin/dpkg-deb")),
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
    let name = parse_field(&text, name_key)
        .ok_or_else(|| UpdateError::PackageMetadataMismatch("package name is missing".to_string()))?;
    let version = parse_field(&text, "Version")
        .ok_or_else(|| UpdateError::PackageMetadataMismatch("package version is missing".to_string()))?;
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

fn install_package(
    family: PackageFamily,
    path: &Path,
    options: &HelperOptions,
) -> Result<()> {
    let (program, args) = match family {
        PackageFamily::Pacman => (
            options
                .pacman_path
                .clone()
                .unwrap_or_else(|| PathBuf::from("/usr/bin/pacman")),
            vec!["-U".to_string(), "--noconfirm".to_string(), path.display().to_string()],
        ),
        PackageFamily::Deb => (
            options
                .dpkg_path
                .clone()
                .unwrap_or_else(|| PathBuf::from("/usr/bin/dpkg")),
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
```

`run_system_package`:

```rust
pub fn run_system_package(job_path: &Path, options: &HelperOptions) -> Result<()> {
    let bytes = std::fs::read(job_path)
        .map_err(|error| UpdateError::io_at(IoOperation::Job, job_path, error))?;
    let job: ApplyJob = serde_json::from_slice(&bytes)
        .map_err(|error| UpdateError::InvalidJob(format!("cannot parse apply job: {error}")))?;
    job.validate()?;
    run_system_package_from_job(&job, options)
}

pub(crate) fn run_system_package_from_job(
    job: &ApplyJob,
    options: &HelperOptions,
) -> Result<()> {
    let temp = verify_system_package(job, options)?;
    let family = match job.kind {
        ApplyKind::SystemPackage { family } => family,
        _ => return Err(UpdateError::InvalidJob("not a system package job".to_string())),
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
```

- [ ] **Step 4: テストを通す**

Run: `cargo test -p vrc-photo-update helper::tests::system_package`
Expected: PASS

- [ ] **Step 5: commit**

```bash
git add crates/update/src/helper.rs
git commit -m "feat(update): verify package metadata and run fixed install commands"
```

---

### Task 6: ユーザー権限ヘルパーの pkexec フローとバイナリモード

**Files:**
- Modify: `crates/update/src/helper.rs`
- Modify: `crates/update/src/bin/vrc-photo-updater.rs`
- Modify: `crates/update/Cargo.toml` (`libc` on unix)
- Modify: `Cargo.toml` (workspace `libc`)
- Test: `crates/update/src/helper.rs` の `#[cfg(test)]`

**Interfaces:**
- Consumes: Task 5 の `run_system_package`
- Produces: `run_helper_with_options` が `ApplyKind::SystemPackage` を専用フローへ分岐
- Produces: `run_system_package_apply(paths, job, job_path, options) -> Result<()>`
- Produces: `user_mode_allowed(euid: u32) -> bool`
- Produces: `system_mode_allowed(euid: u32, pkexec_uid: Option<u32>) -> bool`
- Produces: `map_pkexec_status(status: ExitStatus, stderr: &str) -> (LastErrorCode, String)`
- Produces: `is_preinstall_failure(code: LastErrorCode) -> bool`

- [ ] **Step 1: 失敗テストを書く**

`crates/update/src/helper.rs` tests:

```rust
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
        let status = std::process::ExitStatus::from_raw(code);
        assert_eq!(map_pkexec_status(status, "reason").0, expected);
    }
}

#[test]
fn only_preinstall_failures_cleanup_the_job() {
    assert!(is_preinstall_failure(LastErrorCode::AuthCancelled));
    assert!(is_preinstall_failure(LastErrorCode::AuthUnavailable));
    assert!(is_preinstall_failure(LastErrorCode::PackageManagerLocked));
    assert!(is_preinstall_failure(LastErrorCode::PackageVerificationFailed));
    assert!(is_preinstall_failure(LastErrorCode::PackageMetadataMismatch));
    assert!(is_preinstall_failure(LastErrorCode::Downgrade));
    assert!(!is_preinstall_failure(LastErrorCode::PackageManagerFailed));
    assert!(!is_preinstall_failure(LastErrorCode::HealthCheckFailed));
}
```

- [ ] **Step 2: テストを失敗させる**

Run: `cargo test -p vrc-photo-update helper::tests::maps_pkexec_exit_codes`
Expected: FAIL（`map_pkexec_status` がない）

- [ ] **Step 3: libc と root ガードを追加する**

`Cargo.toml` の `[workspace.dependencies]` に `libc = "0.2"` を追加する。
`crates/update/Cargo.toml` の `[target.'cfg(unix)'.dependencies]` に `libc.workspace = true` を追加する。

`crates/update/src/helper.rs`:

```rust
pub fn user_mode_allowed(euid: u32) -> bool {
    euid != 0
}

pub fn system_mode_allowed(euid: u32, pkexec_uid: Option<u32>) -> bool {
    euid == 0 && pkexec_uid.is_some_and(|uid| uid > 0)
}

#[cfg(unix)]
pub fn effective_uid() -> u32 {
    unsafe { libc::geteuid() }
}

#[cfg(not(unix))]
pub fn effective_uid() -> u32 {
    1
}
```

`crates/update/src/bin/vrc-photo-updater.rs`:

```rust
fn main() {
    let mut args = std::env::args_os().skip(1);
    let command = args.next().and_then(|value| value.to_str().map(str::to_string));
    let path = args.next();
    match (command.as_deref(), path) {
        (Some("--apply"), Some(path)) => {
            if !vrc_photo_update::helper::user_mode_allowed(vrc_photo_update::helper::effective_uid()) {
                eprintln!("vrc-photo-updater: --apply must not run as root");
                std::process::exit(10);
            }
            if let Err(error) = vrc_photo_update::helper::run_helper(Path::new(&path)) {
                eprintln!("vrc-photo-updater: {error}");
                std::process::exit(error.helper_exit_code());
            }
        }
        (Some("--apply-system-package"), Some(path)) => {
            let pkexec_uid = std::env::var("PKEXEC_UID")
                .ok()
                .and_then(|value| value.parse::<u32>().ok());
            if !vrc_photo_update::helper::system_mode_allowed(
                vrc_photo_update::helper::effective_uid(),
                pkexec_uid,
            ) {
                eprintln!("vrc-photo-updater: --apply-system-package requires pkexec");
                std::process::exit(10);
            }
            let options = vrc_photo_update::helper::HelperOptions::default();
            if let Err(error) =
                vrc_photo_update::helper::run_system_package(Path::new(&path), &options)
            {
                eprintln!("vrc-photo-updater: {error}");
                std::process::exit(error.helper_exit_code());
            }
        }
        _ => {
            eprintln!("usage: vrc-photo-updater --apply <job.json>");
            eprintln!("       vrc-photo-updater --apply-system-package <job.json>");
            std::process::exit(2);
        }
    }
}
```

- [ ] **Step 4: pkexec フローを実装する**

`helper.rs` に追加する。

```rust
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

fn run_system_package_apply(
    paths: &UpdatePaths,
    job: &ApplyJob,
    job_path: &Path,
    options: &HelperOptions,
) -> Result<()> {
    if let Err(error) = verify_staged(job, options) {
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

    let output = Command::new("pkexec")
        .arg("--disable-internal-agent")
        .arg(crate::install::SYSTEM_HELPER_PATH)
        .arg("--apply-system-package")
        .arg(job_path)
        .env("LC_ALL", "C")
        .output()
        .map_err(|error| UpdateError::io("pkexec", error))?;
    log(
        paths,
        &format!(
            "system package helper exit: {:?}\n{}",
            output.status.code(),
            truncate_output(&output.stderr)
        ),
    );
    if !output.status.success() {
        let (code, reason) = map_pkexec_status(output.status, &String::from_utf8_lossy(&output.stderr));
        return fail_system_package(paths, job, job_path, code, reason, is_preinstall_failure(code));
    }

    let mut child = launch(
        &job.target,
        &job.restart_args,
        Some(&job.health_token),
        &job.working_dir,
    )?;
    match wait_for_health(
        paths,
        &job.health_token,
        &job.to_version,
        job.database.as_ref().map(NativeString::as_path),
        &mut child,
        options.health_timeout,
    ) {
        Ok(()) => {
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
            let _ = child.kill();
            let _ = child.wait();
            let _ = LastResult::write(
                paths,
                &LastResult {
                    status: LastStatus::Failed,
                    from: job.from_version.clone(),
                    to: job.to_version.clone(),
                    reason: Some(error.to_string()),
                    code: Some(LastErrorCode::HealthCheckFailed),
                },
            );
            relaunch_target(paths, job);
            Err(error)
        }
    }
}

fn fail_system_package(
    paths: &UpdatePaths,
    job: &ApplyJob,
    job_path: &Path,
    code: LastErrorCode,
    reason: String,
    cleanup_files: bool,
) -> Result<()> {
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

fn relaunch_target(paths: &UpdatePaths, job: &ApplyJob) {
    if let Err(error) = launch(&job.target, &job.restart_args, None, &job.working_dir) {
        log(paths, &format!("cannot relaunch the target: {error}"));
    }
}
```

`launch` 自体が失敗した場合も `LastResult::Failed`（`HealthCheckFailed`）を書いて
`relaunch_target` し、job / staged / log を残す。

`run_helper_with_options` の UI / daemon 終了待ちの後、`verify_staged` の前に:

```rust
if matches!(job.kind, ApplyKind::SystemPackage { .. }) {
    return run_system_package_apply(&paths, &job, job_path, &options);
}
```

- [ ] **Step 5: テストを通す**

Run: `cargo test -p vrc-photo-update helper::tests`
Expected: PASS

- [ ] **Step 6: commit**

```bash
git add Cargo.toml crates/update/Cargo.toml crates/update/src/helper.rs crates/update/src/bin/vrc-photo-updater.rs
git commit -m "feat(update): orchestrate system package updates through pkexec"
```

---

### Task 7: UI の自動更新表示・フォールバック・通知

**Files:**
- Modify: `crates/ui/src/update_view.rs`
- Modify: `crates/i18n/locales/ja.yml`
- Modify: `crates/i18n/locales/en.yml`
- Test: `crates/ui/src/update_view.rs` の `#[cfg(test)]`

**Interfaces:**
- Consumes: Task 2 の `InstallKind::system_package_apply` / `can_apply_automatically`
- Consumes: Task 3 の `AvailableUpdate.signed_manifest`
- Consumes: Task 4 の `LastErrorCode` / `LastResult.code`
- Produces: `UpdateState.system_apply_available: bool`
- Produces: `can_apply(install, has_asset, daemon_active, ipc_connected, system_apply_available) -> bool`

- [ ] **Step 1: 失敗テストを書く**

`crates/ui/src/update_view.rs` tests:

```rust
#[test]
fn system_package_can_apply_only_when_ready() {
    let install = InstallKind::SystemPackage {
        family: PackageFamily::Pacman,
    };
    assert!(can_apply(&install, true, false, false, true));
    assert!(!can_apply(&install, true, false, false, false));
    assert!(can_apply(&appimage(), true, false, false, false));
}

#[test]
fn system_package_without_auto_apply_shows_the_package_command() {
    assert!(should_show_package_command(
        &InstallKind::SystemPackage {
            family: PackageFamily::Pacman
        },
        false,
        true,
    ));
    assert!(!should_show_package_command(
        &InstallKind::SystemPackage {
            family: PackageFamily::Pacman
        },
        true,
        true,
    ));
    assert!(should_show_package_command(
        &InstallKind::SystemPackage {
            family: PackageFamily::Pacman
        },
        true,
        false,
    ));
}

#[test]
fn result_notice_maps_auth_and_lock_codes() {
    let cancelled = LastResult {
        status: LastStatus::Failed,
        from: "0.9.2".to_string(),
        to: "0.10.0".to_string(),
        reason: None,
        code: Some(LastErrorCode::AuthCancelled),
    };
    let notice = result_notice(&cancelled, "0.9.2").unwrap();
    assert!(notice.message_in("ja").contains("キャンセル"));
    assert!(notice.message_in("en").contains("cancel"));

    let locked = LastResult {
        status: LastStatus::Failed,
        from: "0.9.2".to_string(),
        to: "0.10.0".to_string(),
        reason: None,
        code: Some(LastErrorCode::PackageManagerLocked),
    };
    let notice = result_notice(&locked, "0.9.2").unwrap();
    assert!(notice.message_in("ja").contains("ロック"));
}

#[test]
fn failed_notice_is_shown_on_the_new_version() {
    let result = LastResult {
        status: LastStatus::Failed,
        from: "0.9.2".to_string(),
        to: "0.10.0".to_string(),
        reason: Some("health".to_string()),
        code: Some(LastErrorCode::HealthCheckFailed),
    };
    assert!(result_notice(&result, "0.10.0").is_some());
    assert!(result_notice(&result, "0.9.2").is_some());
    assert!(result_notice(&result, "0.8.0").is_none());
}
```

- [ ] **Step 2: テストを失敗させる**

Run: `cargo test -p vrc-photo-ui update_view::tests::system_package_can_apply_only_when_ready`
Expected: FAIL（`can_apply` の引数が足りない）

- [ ] **Step 3: UpdateState と can_apply を実装する**

`UpdateState` に `system_apply_available: bool` を追加する。`new` で:

```rust
let last_result = data_dir
    .as_deref()
    .and_then(|dir| LastResult::take(&UpdatePaths::new(dir)));
let auth_unavailable = last_result
    .as_ref()
    .is_some_and(|result| result.code == Some(LastErrorCode::AuthUnavailable));
let system_apply_available = install.system_package_apply(
    vrc_photo_update::pkexec_available(),
    vrc_photo_update::system_helper_available(),
    vrc_photo_update::system_policy_available(),
) && !auth_unavailable;
let notice = last_result.and_then(|result| result_notice(&result, vrc_photo_update::CURRENT_VERSION));
```

`can_apply`:

```rust
fn can_apply(
    install: &InstallKind,
    has_asset: bool,
    daemon_active: bool,
    ipc_connected: bool,
    system_apply_available: bool,
) -> bool {
    let automatic = install.automatic_apply()
        || (matches!(install, InstallKind::SystemPackage { .. }) && system_apply_available);
    automatic && has_asset && (!daemon_active || ipc_connected)
}
```

`should_show_package_command`:

```rust
fn should_show_package_command(
    install: &InstallKind,
    system_apply_available: bool,
    has_asset: bool,
) -> bool {
    matches!(install, InstallKind::SystemPackage { .. })
        && (!system_apply_available || !has_asset)
}
```

- [ ] **Step 4: UI の表示と job を更新する**

- `update_available_view`: ダウンロード可能条件を次にする。

```rust
let can_download = update.asset.is_some()
    && match &self.update.install {
        InstallKind::AppImage { .. } => appimage_writable(&self.update.install),
        InstallKind::SystemPackage { .. } => self.update.system_apply_available,
        InstallKind::WindowsCurrentUser { .. } => true,
        _ => false,
    };
```

  SystemPackage で自動適用不可なら `update.package_manager_fallback` を表示する。
- `update_ready_view`: `can_apply(..., self.update.system_apply_available)` に変更。
- `update_card`: `let has_asset = match &phase { UpdatePhase::Available(update) | UpdatePhase::Downloading { update, .. } | UpdatePhase::Ready { update, .. } => update.asset.is_some(), _ => false };` を計算する。
  `should_show_package_command(&install, self.update.system_apply_available, has_asset)` が true なら
  既存のコマンド表示を出す。false かつ `SystemPackage` なら `update.system_update_hint` を表示する。
- `apply_update_confirmed`:
  - `SystemPackage { family }` を `ApplyKind::SystemPackage { family: *family }` にする。
  - `signed_manifest: Some(String::from_utf8(update.signed_manifest.bytes.clone()).map_err(...)?)`。
    UI スレッドで失敗したら `UpdatePhase::Error` にする。
  - `manifest_signature: Some(update.signed_manifest.signature.clone())`。
  - 非 system package は `None`。
  - `self.update.system_apply_available` が false なら適用しない。
- `update_error_message` に新しい `UpdateError` variants のマッピングを追加する。
- `result_notice`:
  - `UpdateNotice` に `code: Option<LastErrorCode>` を追加する。既存の `UpdateNotice` リテラル
    （`colors_rollback_and_failure_differently_from_success`）にも `code: None` を追加する。
  - `message_in` で code ごとのキーを返す。
  - `Failed` の条件を `result.from == current_version || result.to == current_version` にする。

追加する i18n キー（ja/en 両方）:

```yaml
update.error.polkit_missing: "polkit（pkexec）が利用できないため、アプリ内更新を利用できません。パッケージマネージャで更新してください"
update.error.polkit_agent: "polkit 認証エージェントが見つかりません。パッケージマネージャで更新してください"
update.error.auth_cancelled: "認証がキャンセルされました。もう一度「更新して再起動」を押してください"
update.error.package_locked: "pacman のデータベースがロックされています。他のパッケージ操作が終わってから再試行してください"
update.error.package_failed: "パッケージのインストールに失敗しました"
update.error.package_verify: "パッケージの検証に失敗しました"
update.error.package_mismatch: "パッケージ名またはバージョンが署名済み情報と一致しません"
update.error.downgrade: "このバージョンへのダウングレードはできません"
update.error.system_helper_missing: "システム更新ヘルパーが見つかりません。アプリを再インストールしてください"
update.package_manager_fallback: "この環境ではアプリ内更新を利用できません。以下のコマンドで更新してください"
update.system_update_hint: "polkit の認証後にシステムパッケージを更新します"
update.notice.auth_cancelled: "更新の認証がキャンセルされました。再試行できます"
update.notice.auth_unavailable: "polkit 認証エージェントが見つかりません。パッケージマネージャのコマンドで更新してください"
update.notice.package_locked: "pacman のデータベースがロックされています。ロック解除後に再試行してください"
update.notice.package_failed: "パッケージのインストールに失敗しました"
update.notice.package_mismatch: "パッケージ名またはバージョンが署名済み情報と一致しません"
update.notice.downgrade: "このバージョンへのダウングレードはできません"
update.notice.health_failed: "インストール後の起動確認に失敗しました。ログを確認してください"
```

英語側も同じ意味で追加する。`update.notice.package_failed` は reason を出さない（既存の
`update.notice.failed` を使うなら不要）。`UpdateNotice.message_in` の code 分岐で使う。

- [ ] **Step 5: テストを通す**

Run: `cargo test -p vrc-photo-ui update_view::tests`
Expected: PASS

Run: `cargo test -p vrc-photo-i18n`
Expected: PASS（ja/en のキーが揃う）

- [ ] **Step 6: commit**

```bash
git add crates/ui/src/update_view.rs crates/i18n/locales/ja.yml crates/i18n/locales/en.yml
git commit -m "feat(ui): offer in-app updates for system packages"
```

---

### Task 8: デーモン再起動を systemd 対応にする

**Files:**
- Modify: `crates/ui/src/app/daemon.rs`
- Modify: `crates/ui/src/app.rs`
- Modify: `crates/ui/src/update_view.rs` (`complete_update_startup`)
- Test: `crates/ui/src/app/daemon.rs` の `#[cfg(test)]`

**Interfaces:**
- Produces: `should_use_systemd(supervisor: Option<&str>) -> bool`
- Produces: `start_daemon(supervisor: Option<&str>) -> Result<(), String>`
- Produces: `restart_daemon_after_update(supervisor: Option<&str>) -> Result<(), String>`

- [ ] **Step 1: 失敗テストを書く**

`crates/ui/src/app/daemon.rs` tests:

```rust
#[test]
fn systemd_supervisor_uses_systemctl() {
    assert!(should_use_systemd(Some("systemd")));
    assert!(!should_use_systemd(Some("app")));
    assert!(!should_use_systemd(None));
}

#[test]
fn daemon_start_uses_spawn_for_app_supervisor() {
    assert!(!should_use_systemd(Some("app")));
}
```

- [ ] **Step 2: テストを失敗させる**

Run: `cargo test -p vrc-photo-ui app::daemon::tests::systemd_supervisor_uses_systemctl`
Expected: FAIL（`should_use_systemd` がない）

- [ ] **Step 3: `start_daemon` / `restart_daemon_after_update` を実装する**

`daemon.rs`:

```rust
pub(crate) fn should_use_systemd(supervisor: Option<&str>) -> bool {
    supervisor == Some("systemd")
}

fn systemctl(args: &[&str]) -> Result<(), String> {
    let status = Command::new("systemctl")
        .args(args)
        .status()
        .map_err(|error| error.to_string())?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("systemctl {} failed: {status}", args.join(" ")))
    }
}

fn reload_user_units() {
    let _ = systemctl(&["--user", "daemon-reload"]);
}

pub(crate) fn start_daemon(supervisor: Option<&str>) -> Result<(), String> {
    if should_use_systemd(supervisor) {
        reload_user_units();
        if systemctl(&["--user", "start", "vrc-photo-daemon.service"]).is_ok() {
            return Ok(());
        }
    }
    spawn_daemon_process()
}

pub(crate) fn restart_daemon_after_update(supervisor: Option<&str>) -> Result<(), String> {
    if should_use_systemd(supervisor) {
        reload_user_units();
        if systemctl(&["--user", "restart", "vrc-photo-daemon.service"]).is_ok() {
            return Ok(());
        }
    }
    spawn_daemon_process()
}
```

- [ ] **Step 4: 起動経路を差し替える**

`crates/ui/src/app.rs`:

```rust
use crate::app::daemon::start_daemon;
// ...
let supervisor = app.db.get_setting("daemon_supervisor").ok().flatten();
if !app.demo
    && !app.daemon_active
    && !updating
    && let Err(error) = start_daemon(supervisor.as_deref())
{
    app.toast_error(tr_args!("app.daemon_start_error", error = error), cx);
}
```

`crates/ui/src/update_view.rs` の `complete_update_startup`:

```rust
let supervisor = self
    .daemon_supervisor
    .clone()
    .or_else(|| self.db.get_setting("daemon_supervisor").ok().flatten());
if !self.demo
    && let Err(error) = restart_daemon_after_update(supervisor.as_deref())
{
    self.toast_error(tr_args!("update.error.daemon_start", error = error), cx);
}
```

`start_apply` 失敗時の `spawn_daemon_process()` も `start_daemon(supervisor.as_deref())` に置き換える。
`app.rs` と `update_view.rs` で不要になった `spawn_daemon_process` の import を削除する。

- [ ] **Step 5: テストを通す**

Run: `cargo test -p vrc-photo-ui app::daemon::tests`
Expected: PASS

- [ ] **Step 6: commit**

```bash
git add crates/ui/src/app/daemon.rs crates/ui/src/app.rs crates/ui/src/update_view.rs
git commit -m "fix(ui): restart systemd-managed daemons after updates"
```

---

### Task 9: polkit policy、Packager、Release workflow

**Files:**
- Create: `packaging/polkit/com.tommyz.vrc-photo-manager.update.policy`
- Modify: `Packager.toml`
- Modify: `.github/workflows/release.yml`
- Test: release workflow / package inspection

**Interfaces:**
- Consumes: Task 1 の `build_manifest` と `.pkg.tar.zst`
- Produces: deb / pacman に同梱される polkit action
- Produces: CI で `.pkg.tar.zst` を生成・検証する

- [ ] **Step 1: policy ファイルを作る**

`packaging/polkit/com.tommyz.vrc-photo-manager.update.policy`:

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE policyconfig PUBLIC
 "-//freedesktop//DTD PolicyKit Policy Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/PolicyKit/1/policyconfig.dtd">
<policyconfig>
  <vendor>TommyZ7</vendor>
  <vendor_url>https://github.com/TommyZ-7/vrc-photo-manager</vendor_url>
  <action id="com.tommyz.vrc-photo-manager.update-package">
    <description>Update VRC Photo Manager</description>
    <message>Authentication is required to update VRC Photo Manager</message>
    <defaults>
      <allow_any>auth_admin</allow_any>
      <allow_inactive>auth_admin</allow_inactive>
      <allow_active>auth_admin</allow_active>
    </defaults>
    <annotations>
      <annotation key="org.freedesktop.policykit.exec.path">/usr/bin/vrc-photo-updater</annotation>
    </annotations>
  </action>
</policyconfig>
```

- [ ] **Step 2: Packager.toml に追加する**

`[deb.files]` と `[pacman.files]` に:

```toml
"packaging/polkit/com.tommyz.vrc-photo-manager.update.policy" = "usr/share/polkit-1/actions/com.tommyz.vrc-photo-manager.update.policy"
```

- [ ] **Step 3: Release workflow に makepkg ステップを追加する**

`cargo packager --release` の後、`List artifacts` の前に:

```yaml
      - name: Build pacman package
        run: |
          set -euo pipefail
          docker run --rm -v "$PWD/dist:/dist" -w /dist archlinux:base-devel bash -c '
            useradd -m builder
            chown -R builder:builder /dist
            su builder -c "makepkg -f --nodeps"
            pacman -Qip /dist/*.pkg.tar.zst
          '
          ls -lh dist/*.pkg.tar.zst
```

- [ ] **Step 4: 既存の検証ステップを更新する**

- `Verify third-party notices` の `pacman=$(ls dist/*.tar.gz)` はそのまま（ソース tar.gz 用）。
- `Verify systemd user unit` の後に policy 検証を追加する:

```yaml
      - name: Verify polkit policy
        run: |
          set -euo pipefail
          policy="usr/share/polkit-1/actions/com.tommyz.vrc-photo-manager.update.policy"
          deb=$(ls dist/*.deb)
          pkg=$(ls dist/*.pkg.tar.zst)
          tmp=$(mktemp -d)
          mkdir -p "$tmp/deb"
          dpkg-deb --extract "$deb" "$tmp/deb"
          test -f "$tmp/deb/$policy" \
            || { echo "missing $policy in deb"; exit 1; }
          grep -q 'org.freedesktop.policykit.exec.path' "$tmp/deb/$policy"
          docker run --rm -v "$PWD/dist:/dist" archlinux:base-devel bash -c "
            pacman -Qlp /dist/$(basename "$pkg") | grep -q '$policy$'
          "
          echo "polkit policy verified"
```

- [ ] **Step 5: latest.json の jq チェックを更新する**

`publish` ジョブの:

```yaml
jq -e '.schema == 1 and (.platforms | length == 2)' dist/latest.json
```

を:

```yaml
jq -e '.schema == 1 and (.platforms | length == 2) and (.system_packages | length == 2)' dist/latest.json
```

に変更する。

- [ ] **Step 6: ローカルで policy XML を検証する**

Run: `xmllint --noout packaging/polkit/com.tommyz.vrc-photo-manager.update.policy`
Expected: PASS（xmllint がない場合は `python3 -c "import xml.etree.ElementTree as ET; ET.parse('...')"` で代替）

- [ ] **Step 7: commit**

```bash
git add packaging/polkit/com.tommyz.vrc-photo-manager.update.policy Packager.toml .github/workflows/release.yml
git commit -m "feat(packaging): ship polkit policy and build a real pacman package"
```

---

### Task 10: ADR とユーザー向けドキュメント

**Files:**
- Create: `docs/adr/0016-system-package-update.md`
- Modify: `docs/adr/README.md`
- Modify: `docs/release.md`
- Modify: `docs/architecture.md`
- Modify: `docs/testing.md`
- Modify: `docs/features.md`
- Modify: `docs/troubleshooting.md`
- Modify: `docs/security-privacy.md`

**Interfaces:**
- Consumes: 実装済みの設計とコマンド
- Produces: 利用者・リリース担当向けの手順

- [x] **Step 1: ADR-0016 を書く**

含める内容:
- ADR-0012 の「deb/pacman は通知のみ」を置き換える。
- `system_packages` の後方互換な manifest 拡張。
- `pkexec` + 固定コマンド + root temp copy + 再検証。
- `.pkg.tar.zst` を `makepkg` で生成する理由。
- 自動ロールバックしない理由。
- 認証キャンセル / DB ロック / policy 欠落時のフォールバック。

- [x] **Step 2: ADR README に追記する**

`docs/adr/README.md` の表に:

```markdown
| [0016](0016-system-package-update.md) | pacman / deb も polkit 経由でアプリ内更新する | Accepted |
```

- [x] **Step 3: release / architecture / testing を更新する**

- `docs/release.md` 10章: `system_packages`、`.pkg.tar.zst` の生成、policy、署名、jq 検証。
- `docs/architecture.md` 4.7: ユーザーヘルパー → pkexec → 特権検証 → 固定コマンド → health → daemon restart。
- `docs/testing.md`: 手動 QA チェックリスト（認証キャンセル、DB ロック、署名不正、systemd、deb）。
- `docs/features.md`: pacman / deb のアプリ内更新を追記。
- `docs/troubleshooting.md`: pacman / deb の失敗時のログと手動復旧。
- `docs/security-privacy.md`: polkit によるローカル特権昇格と権限境界。

- [x] **Step 4: リンク切れを確認する**

Run: `rg -n "0016-system-package-update|system_packages|pkg.tar.zst" docs`
Expected: 追加したドキュメントと ADR の参照が見つかる

- [x] **Step 5: commit**

```bash
git add docs/adr docs/release.md docs/architecture.md docs/testing.md docs/features.md docs/troubleshooting.md docs/security-privacy.md
git commit -m "docs: document in-app system package updates"
```

---

### Task 11: 全体検証と手動 QA

**Files:**
- Test: workspace 全体
- 手動: pacman 環境

> **Status (2026-11-02):** ローカル検証（fmt / clippy / test）と Release workflow の
> 静的確認は完了。特権を伴う pacman / deb の手動 QA はユーザーの明示承認待ちのため
> 未実施。計画全体の完了マークは保留する。

- [x] **Step 1: fmt / clippy / test を実行する**

Run:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Expected: すべて PASS

結果: 3コマンドとも PASS（2026-11-02、774 passed / 1 ignored）。

- [x] **Step 2: release workflow の bundle を確認する**

PR に `bundle-check` ラベルを付けるか、packaging 変更を含むため Release workflow の
`build` ジョブが走ることを確認する。`build` で以下を確認:

- `dist/*.pkg.tar.zst` が存在する
- `pacman -Qip` が `vrc-photo-manager` / `<version>` を返す
- deb / pacman に policy が入る
- `latest.json` の `system_packages` が 2 件

結果: workflow の静的確認のみ実施（Docker 不在のため Release workflow は未実行）。
bundle 生成条件、`makepkg`、`pacman -Qip` の Name / Version / Architecture 検証、
deb / pacman の policy 検証、`system_packages` の件数チェックを確認した。

- [ ] **Step 3: pacman の手動 QA を行う**

> **Pending:** ユーザーの明示承認が必要なため未実施。

ユーザーの明示承認を得てから、以下を確認する。

1. ローカルの署名済みテストフィード（デバッグビルドの loopback HTTP または信頼済み HTTPS）を用意する。
2. アプリ内でダウンロード → 適用 → polkit 認証 → 新バージョン起動 → デーモン再接続。
3. 認証ダイアログをキャンセルし、システムに変更が入らず、UI が戻って再試行できる。
4. pacman DB をロックし、エラーが表示され、ロック解除後に再試行できる。
5. 署名不正のパッケージが通常側 / 特権側の両方で拒否される。
6. systemd ユーザーサービスでデーモンを動かし、更新後に
   `systemctl --user is-active vrc-photo-daemon.service` が active。
7. `pkexec` を隠した環境でコマンド表示にフォールバックする。

- [ ] **Step 4: deb の手動 QA を行う（可能な場合）**

> **Pending:** 特権を伴う deb 実機 QA は未実施。Task 5 の偽 `dpkg` / `dpkg-deb`
> テストと Release workflow のパッケージ検証で代替している。

Debian コンテナ / VM で同じ流れを確認する。難しい場合は、Task 5 の偽 `dpkg` /
`dpkg-deb` テストと Release workflow のパッケージ検証で代替し、未実施として記録する。

- [ ] **Step 5: 計画のチェックボックスを更新する**

> **Pending:** 手動 QA が未完了のため、計画全体の完了チェックは保留。
> 本検証で完了した Step 1 / Step 2 のみ `- [x]` にした。

実行完了時に `docs/superpowers/plans/2026-11-02-system-package-update.md` の
チェックボックスを `- [x]` に更新する（AGENTS.md §4）。

- [ ] **Step 6: 最終 commit**

> **Pending:** 手動 QA 完了後に計画完了マークの最終コミットを行う。
> 本検証時点の状態は `docs: record system package update verification status` で記録済み。

```bash
git add docs/superpowers/plans/2026-11-02-system-package-update.md
git commit -m "docs: mark the system package update plan complete"
```
