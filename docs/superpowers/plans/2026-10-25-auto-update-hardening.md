# Auto-Update Review Hardening Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close the six auto-update review findings in issues #69–#74 without changing the release flow.

**Architecture:** Keep the existing check → download → helper → health-check pipeline. Harden each boundary in place: validate artifact signatures at manifest time, carry the check-time trust keys into download, tag I/O failures with their operation, fsync the AppImage directory, preserve restart arguments/working directory/database identity across the helper, and fix the update card's auto-check, label, and notice colors. Work in one branch with one commit per issue, ordered to minimize edits to the same files.

**Tech Stack:** Rust stable (edition 2024), `minisign-verify` / `minisign` (dev), serde/serde_json, base64, gpui-kit 0.8, existing `vrc-photo-update` and `vrc-photo-ui` crates.

**Spec:** GitHub issues [#69](https://github.com/TommyZ-7/vrc-photo-manager/issues/69), [#70](https://github.com/TommyZ-7/vrc-photo-manager/issues/70), [#71](https://github.com/TommyZ-7/vrc-photo-manager/issues/71), [#72](https://github.com/TommyZ-7/vrc-photo-manager/issues/72), [#73](https://github.com/TommyZ-7/vrc-photo-manager/issues/73), [#74](https://github.com/TommyZ-7/vrc-photo-manager/issues/74), plus the Decisions table below.

## Decisions

| Area | Decision |
|---|---|
| Delivery | One code PR, branch `fix/auto-update-review`, one commit per issue plus test/review fixes. Plan file stays untracked in the code worktree and goes to a separate docs PR after the code PR merges (AGENTS.md §4). |
| Commit order | #70 → #73 → #74 → #71 → #72 → #69. This puts shared-file edits (`manifest.rs`, `download.rs`, `error.rs`, `helper.rs`, `update_view.rs`) in dependency order and avoids rework. |
| #72 argument encoding | `args_os()` is preserved losslessly in the job as base64 of the platform's native bytes (Unix bytes / Windows UTF-16LE). A non-UTF-8 argument must not panic and must be restored exactly. |
| #72 working directory | `ApplyJob.working_dir` stores the UI's original current directory. The helper launches both the new and the old UI with it. A missing working directory is a job error. |
| #72 database identity | `ApplyJob.database` and `HealthMarker.database` store the canonical path of the opened SQLite file. The helper accepts the marker only when it matches. If the job has no database identity (legacy job), the check is skipped. |
| #73 key source | `AvailableUpdate.public_keys` carries the exact `FeedConfig.public_keys` used by `check`. `download` uses that list; it must not re-read the environment. |
| #74 error model | Add `IoOperation { Download, Helper, Job, Apply, Spawn, Other }` to `UpdateError::Io`, plus a dedicated `HelperMissing` variant. Keep the existing user-facing text for signature/network/integrity errors. |
| #69 notice colors | `success` = GREEN, `rolled_back` = AMBER (recovered), `failed` = RED. `last_checked_label` returns only the timestamp; the card keeps the “最終確認” label. |
| #69 auto-check | `Available` is eligible for the 24-hour automatic check, but an automatic check failure restores the previously available update instead of dropping to `Idle`. |

## Global Constraints

- Code worktree: `../vrc-photo-manager-auto-update-fixes`, branch `fix/auto-update-review`, cut from updated `main`.
- Do not push to `main`; do not create tags, publish releases, or run the Release workflow.
- One code PR for all six issues. Do not include `docs/**` changes in it.
- Japanese UI copy, matching the existing Settings card style.
- No new runtime dependencies. `minisign` stays a dev-dependency.
- `NativeString` and the new job/marker fields must compile on Windows; guard Unix-only conversions with `#[cfg(unix)]` / `#[cfg(windows)]`.
- `HealthMarker.database` uses `#[serde(default)]` so an older marker without the field still parses.
- Keep existing messages for `Network`, `Signature`, `Integrity`, `InvalidManifest`, `UnsupportedPlatform`, `Cancelled`, `NotFound`, and `InvalidJob`.
- Pre-push verification: `cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`.
- Targeted checks while working: `cargo test -p vrc-photo-update` and `cargo test -p vrc-photo-ui`, then clippy for those packages.
- Every subagent runs on the parent session model; never pass a `model` argument.

## Review Focus

1. An automatic re-check that starts while an update is available and then fails must leave the existing “更新あり” card visible. Test in Task 6.
2. A non-UTF-8 restart argument must not panic and must round-trip exactly through the job file. Test in Task 5.
3. A relaunched UI must start in the original working directory and must only satisfy the health check when it opened the same canonical database. Tests in Task 5.
4. A parent-directory fsync failure after the AppImage copy must restore the `.previous` file instead of leaving a missing or partial target. Test in Task 4.
5. A malformed base64 or minisign artifact signature must fail `check` before any download starts. Test in Task 1.
6. A custom `FeedConfig` key set must be the same set used by `download`; a different key must fail. Test in Task 2.
7. Helper-missing, job-write, AppImage-replace, spawn, and download I/O failures must produce different user-facing messages. Test in Task 3.

---

### Task 0: Create the isolated worktree

**Files:**
- Create: `../vrc-photo-manager-auto-update-fixes` (git worktree)
- Copy: this plan into the worktree as an untracked file

**Interfaces:**
- Consumes: updated `main` with no open auto-update PR.
- Produces: working directory `../vrc-photo-manager-auto-update-fixes` on branch `fix/auto-update-review`.

- [ ] **Step 1: Confirm `main` is current and the only local changes are known user files**

Run:
```bash
git fetch origin
git status --short --branch
git rev-list --left-right --count main...origin/main
```
Expected: `0 0`; `opencode.json` may be modified and `.opencode/skills/gnome-wayland-visual-qa/` may be untracked. Do not touch either.

- [ ] **Step 2: Create the worktree from `main`**

Run:
```bash
git worktree add ../vrc-photo-manager-auto-update-fixes -b fix/auto-update-review main
```
Expected: new worktree on branch `fix/auto-update-review` pointing at the same commit as `main`.

- [ ] **Step 3: Move this session to the worktree**

Use the harness session-move tool with the new directory. Then run:
```bash
git status --short --branch
```
Expected: `## fix/auto-update-review`.

---

### Task 1: Reject malformed artifact signatures during check (#70)

**Files:**
- Modify: `crates/update/src/verify.rs` (test-only signature helper)
- Modify: `crates/update/src/manifest.rs` (`PlatformAsset::validate`, tests)
- Modify: `crates/update/src/check.rs` (test fixtures)
- Modify: `crates/update/src/install.rs` (test fixture)

**Interfaces:**
- Produces: `#[cfg(test)] pub(crate) fn test_signature() -> String` in `verify.rs` — base64 of a parseable minisign signature over `b"test"`.
- Consumes: `crate::verify::decode_signature_base64`, `minisign_verify::Signature::decode`.
- Public API unchanged.

- [ ] **Step 1: Add failing tests for malformed signatures**

In `manifest.rs` tests, add:
```rust
#[test]
fn rejects_a_signature_that_is_not_base64() {
    let mut value = valid_manifest();
    value["platforms"]["linux-x86_64"]["signature"] = serde_json::json!("not base64!");
    assert!(matches!(parse(&value, false), Err(UpdateError::InvalidManifest(_))));
}

#[test]
fn rejects_a_signature_that_is_not_minisign() {
    let mut value = valid_manifest();
    // base64("nope")
    value["platforms"]["linux-x86_64"]["signature"] = serde_json::json!("bm9wZQ==");
    assert!(matches!(parse(&value, false), Err(UpdateError::InvalidManifest(_))));
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run:
```bash
cargo test -p vrc-photo-update --lib manifest::tests::rejects_a_signature
```
Expected: both tests FAIL because `validate` only checks for an empty string.

- [ ] **Step 3: Add the test signature helper and update every test manifest**

In `verify.rs`, outside `mod tests`:
```rust
#[cfg(test)]
pub(crate) fn test_signature() -> String {
    use base64::Engine as _;
    use std::io::Cursor;
    let keypair = minisign::KeyPair::generate_unencrypted_keypair().unwrap();
    let signature = minisign::sign(None, &keypair.sk, Cursor::new(b"test"), None, None).unwrap();
    base64::engine::general_purpose::STANDARD.encode(signature.into_string().as_bytes())
}
```

Replace every test-only dummy artifact signature with `crate::verify::test_signature()`:
- `manifest.rs` `valid_manifest()`
- `check.rs` `linux_platform()` and the inline Windows platform in `appimage_without_a_linux_asset_still_reports_the_version`
- `install.rs` `selects_the_asset_for_the_install_kind` (convert the raw JSON string to `serde_json::json!` + `serde_json::to_vec` so the helper can be called)

- [ ] **Step 4: Validate the signature format in `PlatformAsset::validate`**

After the empty-string check, add:
```rust
let signature = crate::verify::decode_signature_base64(&self.signature).map_err(|_| {
    UpdateError::InvalidManifest(format!("{platform}: signature is not valid base64"))
})?;
minisign_verify::Signature::decode(&signature).map_err(|_| {
    UpdateError::InvalidManifest(format!("{platform}: signature is not a minisign signature"))
})?;
```

- [ ] **Step 5: Run the update tests**

Run:
```bash
cargo test -p vrc-photo-update
cargo clippy -p vrc-photo-update --all-targets -- -D warnings
```
Expected: PASS, including `validates_a_complete_manifest`, the two new rejection tests, and the existing `rejects_empty_signature`.

- [ ] **Step 6: Commit**

```bash
git add crates/update/src/verify.rs crates/update/src/manifest.rs crates/update/src/check.rs crates/update/src/install.rs
git commit -m "fix(update): reject malformed artifact signatures during check (#70)"
```

---

### Task 2: Use the check-time keys for download (#73)

**Files:**
- Modify: `crates/update/src/manifest.rs` (`AvailableUpdate`)
- Modify: `crates/update/src/check.rs` (populate + end-to-end test)
- Modify: `crates/update/src/download.rs` (remove environment re-read)

**Interfaces:**
- Produces: `AvailableUpdate.public_keys: Vec<String>`; `check` fills it from `feed.public_keys`.
- Changes: `download(client, update, dest, progress, cancel)` keeps its signature but uses `update.public_keys`.
- Removes: `download_with_keys` (crate-private).
- Later tasks rely on `AvailableUpdate.public_keys` being present in every constructor.

- [ ] **Step 1: Add failing tests**

In `check.rs` tests, add a helper `fn public_key(keypair: &KeyPair) -> String` extracted from the existing fixture logic, then add:
```rust
#[test]
fn custom_feed_keys_are_used_for_check_and_download() {
    let keypair = KeyPair::generate_unencrypted_keypair().unwrap();
    let body = b"artifact bytes";
    let artifact_signature = minisign::sign(None, &keypair.sk, Cursor::new(body), None, None)
        .unwrap()
        .into_string();
    let platforms = serde_json::json!({
        "linux-x86_64": {
            "url": "https://example.com/vrc-photo-manager_0.4.0_x86_64.AppImage",
            "format": "appimage",
            "size": body.len(),
            "sha256": hex::encode(sha2::Sha256::digest(body)),
            "signature": base64::engine::general_purpose::STANDARD.encode(artifact_signature.as_bytes())
        }
    });
    let fixture = fixture_with_keypair(&keypair, "0.4.0", platforms);
    let client = FakeHttpClient::new(HashMap::from([
        (fixture.manifest_url.clone(), Ok(fixture.manifest.clone())),
        (fixture.signature_url.clone(), Ok(fixture.signature.clone().into_bytes())),
    ]))
    .with_artifact(body.to_vec());
    let feed = FeedConfig::for_tests(
        fixture.manifest_url.clone(),
        vec![fixture.public_key.clone()],
        false,
    );

    let update = check(&client, &feed, &Version::new(0, 3, 0), &appimage())
        .unwrap()
        .unwrap();
    assert_eq!(update.public_keys, feed.public_keys);

    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("artifact.AppImage");
    crate::download::download(
        &client,
        &update,
        &dest,
        &mut |_, _| {},
        &std::sync::atomic::AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(std::fs::read(&dest).unwrap(), body);

    let other = KeyPair::generate_unencrypted_keypair().unwrap();
    let mut wrong = update;
    wrong.public_keys = vec![public_key(&other)];
    let error = crate::download::download(
        &client,
        &wrong,
        &dir.path().join("wrong.AppImage"),
        &mut |_, _| {},
        &std::sync::atomic::AtomicBool::new(false),
    )
    .unwrap_err();
    assert!(matches!(error, UpdateError::Signature));
}
```
Also assert in `finds_a_newer_release_and_selects_the_asset` that `update.public_keys == feed.public_keys`. Put this assertion before `update.asset.unwrap()`, because unwrapping the asset partially moves `update`.

- [ ] **Step 2: Run the tests to verify they fail**

Run:
```bash
cargo test -p vrc-photo-update --lib check::tests::custom_feed_keys
```
Expected: FAIL to compile because `AvailableUpdate` has no `public_keys` field.

- [ ] **Step 3: Carry the keys on `AvailableUpdate`**

In `manifest.rs`:
```rust
pub struct AvailableUpdate {
    pub version: Version,
    pub notes_url: Option<String>,
    pub asset: Option<PlatformAsset>,
    /// Keys used to verify the manifest that produced this update.
    pub public_keys: Vec<String>,
}
```
In `check.rs`, fill `public_keys: feed.public_keys.clone()`.

- [ ] **Step 4: Make `download` use the carried keys**

In `download.rs`, delete `download_with_keys` and the `effective_public_keys` import. In `download`, replace `let keys = effective_public_keys();` with `let keys = &update.public_keys;` and use `keys` directly in the closure.

- [ ] **Step 5: Update download tests and add the test artifact support**

In `download.rs` tests:
- Change `available(asset)` to `available(asset, public_keys: Vec<String>)` and set the new field.
- Replace every `download_with_keys(...)` call with `download(...)`, dropping the explicit key slice.
- In `missing_asset_is_rejected`, set `public_keys: Vec::new()`.
- Keep the signature/hash/cancellation tests unchanged in behavior.

In `check.rs` tests:
- Add `artifact: Vec<u8>` to `FakeHttpClient`, `fn with_artifact(mut self, artifact: Vec<u8>) -> Self`, and implement `download` by streaming `self.artifact.chunks(4)` through `on_chunk`.
- Add `fixture_with_keypair(&KeyPair, ...)` and make the existing `fixture` call it.
- Add imports: `base64::Engine as _`, `sha2::Digest as _`, `std::sync::atomic::AtomicBool`.

- [ ] **Step 6: Run the tests**

Run:
```bash
cargo test -p vrc-photo-update
cargo clippy -p vrc-photo-update --all-targets -- -D warnings
```
Expected: PASS. The custom-key test proves `check` → `download` succeeds with a non-embedded key set and that a different key fails.

- [ ] **Step 7: Commit**

```bash
git add crates/update/src/manifest.rs crates/update/src/check.rs crates/update/src/download.rs
git commit -m "fix(update): use the feed keys carried by AvailableUpdate for downloads (#73)"
```

---

### Task 3: Report I/O failures with their operation (#74)

**Files:**
- Modify: `crates/update/src/error.rs`
- Modify: `crates/update/src/lib.rs` (re-export `IoOperation`)
- Modify: `crates/update/src/download.rs`
- Modify: `crates/update/src/install.rs`
- Modify: `crates/update/src/helper.rs`
- Modify: `crates/ui/src/update_view.rs` (`update_error_message`, tests)

**Interfaces:**
- Produces: `pub enum IoOperation { Download, Helper, Job, Apply, Spawn, Other }`
- Produces: `UpdateError::HelperMissing`
- Changes: `UpdateError::Io { operation: IoOperation, path: PathBuf, source: std::io::Error }`
- Produces: `UpdateError::io_at(operation, path, source)` and `UpdateError::with_io_operation(self, operation)`
- Consumes: existing `UpdateError::io(path, source)` — it now uses `IoOperation::Other`.

- [ ] **Step 1: Add failing UI message tests**

In `update_view.rs` tests, add:
```rust
#[test]
fn explains_update_io_failures_by_operation() {
    use vrc_photo_update::IoOperation;
    let cases = [
        (UpdateError::HelperMissing, "更新ヘルパーが見つかりません"),
        (
            UpdateError::io_at(IoOperation::Job, "/tmp/job.json", std::io::ErrorKind::PermissionDenied.into()),
            "更新ジョブを書き込めません",
        ),
        (
            UpdateError::io_at(IoOperation::Apply, "/tmp/app.AppImage", std::io::ErrorKind::PermissionDenied.into()),
            "AppImageを置き換えられません",
        ),
        (
            UpdateError::io_at(IoOperation::Spawn, "/tmp/helper", std::io::ErrorKind::NotFound.into()),
            "更新ヘルパーを起動できません",
        ),
        (
            UpdateError::io_at(IoOperation::Download, "/tmp/artifact", std::io::ErrorKind::NotFound.into()),
            "更新ファイルを保存できません",
        ),
    ];
    let messages: std::collections::HashSet<_> = cases
        .iter()
        .map(|(error, expected)| {
            let message = update_error_message(error);
            assert!(message.contains(*expected), "{message}");
            message
        })
        .collect();
    assert_eq!(messages.len(), cases.len());
}

#[test]
fn keeps_existing_update_error_messages() {
    assert!(update_error_message(&UpdateError::Signature).contains("署名"));
    assert!(update_error_message(&UpdateError::Integrity("x".into())).contains("検証"));
    assert!(update_error_message(&UpdateError::Network("x".into())).contains("ネットワーク"));
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run:
```bash
cargo test -p vrc-photo-ui --lib update_view::tests::explains_update_io_failures_by_operation
```
Expected: FAIL to compile because `IoOperation`, `HelperMissing`, and `io_at` do not exist.

- [ ] **Step 3: Extend `UpdateError`**

In `error.rs`:
```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IoOperation {
    Download,
    Helper,
    Job,
    Apply,
    Spawn,
    Other,
}

pub enum UpdateError {
    // ... existing variants ...
    #[error("update helper is missing")]
    HelperMissing,
    #[error("{operation:?} failed at {path}: {source}")]
    Io {
        operation: IoOperation,
        path: PathBuf,
        source: std::io::Error,
    },
    // ...
}

impl UpdateError {
    pub fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::io_at(IoOperation::Other, path, source)
    }

    pub fn io_at(
        operation: IoOperation,
        path: impl Into<PathBuf>,
        source: std::io::Error,
    ) -> Self {
        Self::Io {
            operation,
            path: path.into(),
            source,
        }
    }

    pub fn with_io_operation(self, operation: IoOperation) -> Self {
        match self {
            Self::Io { path, source, .. } => Self::Io {
                operation,
                path,
                source,
            },
            other => other,
        }
    }
}
```
Export `IoOperation` from `lib.rs`.

- [ ] **Step 4: Tag the I/O call sites**

- `download.rs`: all `UpdateError::io(...)` → `UpdateError::io_at(IoOperation::Download, ...)`.
- `install.rs`:
  - missing helper source → `UpdateError::HelperMissing`
  - helper copy/permissions → `IoOperation::Helper`
  - log open/clone → `IoOperation::Job`
  - helper spawn → `IoOperation::Spawn`
  - `write_atomic(...)` for the job → `.map_err(|error| error.with_io_operation(IoOperation::Job))?`
- `helper.rs`:
  - job read → `IoOperation::Job`
  - staged file checks / SHA-256 / replacement / restore / Windows apply → `IoOperation::Apply`
  - `launch` spawn → `IoOperation::Spawn`
  - lock/result/health writes may stay `Other`.
- `verify.rs`: file open stays `Other`.
- Do not change any error message for the variants listed in the constraints.

- [ ] **Step 5: Map the operations to Japanese messages**

In `update_error_message`, replace the single `Io { .. }` arm with:
```rust
UpdateError::HelperMissing => {
    "更新ヘルパーが見つかりません。アプリを再インストールしてください".to_string()
}
UpdateError::Io { operation, .. } => match operation {
    IoOperation::Helper => "更新ヘルパーを準備できませんでした".to_string(),
    IoOperation::Job => "更新ジョブを書き込めませんでした".to_string(),
    IoOperation::Apply => "AppImageを置き換えられませんでした".to_string(),
    IoOperation::Spawn => "更新ヘルパーを起動できませんでした".to_string(),
    IoOperation::Download | IoOperation::Other => {
        "更新ファイルを保存できませんでした".to_string()
    }
},
```
Add `IoOperation` to the `vrc_photo_update::...` import in `update_view.rs`.

- [ ] **Step 6: Run the tests**

Run:
```bash
cargo test -p vrc-photo-update
cargo test -p vrc-photo-ui --lib update_view::tests
cargo clippy -p vrc-photo-update -p vrc-photo-ui --all-targets -- -D warnings
```
Expected: PASS. All five I/O messages are distinct, and the existing network/signature/integrity messages are unchanged.

- [ ] **Step 7: Commit**

```bash
git add crates/update/src/error.rs crates/update/src/lib.rs crates/update/src/download.rs crates/update/src/install.rs crates/update/src/helper.rs crates/ui/src/update_view.rs
git commit -m "fix(update): report I/O failures with their operation (#74)"
```

---

### Task 4: fsync the AppImage parent directory after replacement (#71)

**Files:**
- Modify: `crates/update/src/helper.rs` (`replace_appimage`, tests)

**Interfaces:**
- Produces: `fn sync_parent_directory(path: &Path) -> Result<()>` (private; Unix opens the directory and calls `sync_all`, non-Unix is a no-op).
- Produces: `fn replace_appimage_impl(target: &Path, staged: &Path, sync_parent: &dyn Fn(&Path) -> Result<()>) -> Result<PathBuf>` (private; test injection point).
- Public signature `replace_appimage(target, staged)` unchanged.

- [ ] **Step 1: Add the failing rollback test**

In `helper.rs` tests:
```rust
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
```

- [ ] **Step 2: Run the test to verify it fails**

Run:
```bash
cargo test -p vrc-photo-update --lib helper::tests::a_parent_directory_sync_failure
```
Expected: FAIL to compile because `replace_appimage_impl` does not exist.

- [ ] **Step 3: Implement directory fsync with an injection point**

Add:
```rust
#[cfg(unix)]
fn sync_parent_directory(path: &Path) -> Result<()> {
    let dir = File::open(path).map_err(|error| UpdateError::io_at(IoOperation::Apply, path, error))?;
    dir.sync_all()
        .map_err(|error| UpdateError::io_at(IoOperation::Apply, path, error))
}

#[cfg(not(unix))]
fn sync_parent_directory(_path: &Path) -> Result<()> {
    Ok(())
}
```
Refactor `replace_appimage` into `replace_appimage_impl`; after the new file's `sync_all()` and before returning `Ok(())`, call:
```rust
let parent = target
    .parent()
    .ok_or_else(|| UpdateError::InvalidJob("target has no parent".to_string()))?;
sync_parent(parent)?;
```
`replace_appimage` calls `replace_appimage_impl(target, staged, &sync_parent_directory)`.

- [ ] **Step 4: Run the tests**

Run:
```bash
cargo test -p vrc-photo-update --lib helper::tests
cargo clippy -p vrc-photo-update --all-targets -- -D warnings
```
Expected: PASS. `replaces_and_restores_an_appimage` still passes; the injected failure restores the old target and keeps `.previous`.

- [ ] **Step 5: Commit**

```bash
git add crates/update/src/helper.rs
git commit -m "fix(update): fsync the AppImage parent directory after replacement (#71)"
```

---

### Task 5: Preserve restart arguments, working directory, and database identity (#72)

**Files:**
- Create: `crates/update/src/native.rs`
- Modify: `crates/update/src/lib.rs` (module + exports)
- Modify: `crates/update/src/install.rs` (`ApplyJob` fields, validation, tests)
- Modify: `crates/update/src/helper.rs` (`launch`, health marker, tests)
- Modify: `crates/ui/src/update_view.rs` (`UpdateState` database path, `apply_update`, health marker, current directory)
- Modify: `crates/ui/src/main.rs` (pass the opened DB path to `UpdateState::new`)

**Interfaces:**
- Produces: `pub struct NativeString(OsString)` with `new`, `as_os_str`, `as_path`, `to_path_buf`, `contains_nul`, `From<OsString>`, and lossless `Serialize`/`Deserialize` as base64 of native bytes.
- Changes: `ApplyJob.restart_args: Vec<NativeString>`; add `pub working_dir: PathBuf`; add `pub database: Option<NativeString>`.
- Changes: `HealthMarker` gains `#[serde(default)] pub database: Option<NativeString>`.
- Changes: `write_health_marker(paths, token, version, database: Option<&Path>)`.
- Changes: `wait_for_health(paths, token, expected, expected_database: Option<&Path>, child, timeout)`.
- Produces: `fn launch_command(target, args, health_token, working_dir) -> Command` (private; testable).
- Changes: `UpdateState::new(health_token, database_ok, demo, db_path: Option<PathBuf>)`.

- [ ] **Step 1: Add failing native round-trip tests**

In `native.rs` tests:
```rust
#[test]
fn round_trips_plain_and_empty_strings() {
    for value in ["", "--demo", "写真"] {
        let native = NativeString::new(value);
        let json = serde_json::to_string(&native).unwrap();
        let decoded: NativeString = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, native);
    }
}

#[cfg(unix)]
#[test]
fn round_trips_non_utf8_bytes() {
    use std::os::unix::ffi::OsStringExt;
    let native = NativeString::from(OsString::from_vec(vec![b'a', 0x80, b'b']));
    let json = serde_json::to_string(&native).unwrap();
    let decoded: NativeString = serde_json::from_str(&json).unwrap();
    assert_eq!(decoded, native);
}

#[test]
fn rejects_invalid_base64() {
    assert!(serde_json::from_str::<NativeString>("\"not base64!\"").is_err());
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run:
```bash
cargo test -p vrc-photo-update --lib native
```
Expected: FAIL to compile because the module and type do not exist.

- [ ] **Step 3: Implement `NativeString`**

Add `native.rs`:
- Unix: bytes = `OsStrExt::as_bytes()`; decode with `OsStringExt::from_vec`.
- Windows: bytes = `encode_wide().flat_map(u16::to_le_bytes)`; decode requires even length and uses `OsString::from_wide`.
- Other platforms: fall back to lossy UTF-8 bytes.
- `contains_nul`: Unix bytes contain `0`; Windows `encode_wide()` contains `0`.
- `Serialize` writes a base64 string; `Deserialize` reads a string, decodes base64, and maps malformed input to `serde::de::Error::custom`.
- Add `impl From<OsString> for NativeString` and `impl From<&OsStr> for NativeString`.
- Re-export `NativeString` from `lib.rs`.

- [ ] **Step 4: Add the job fields and validation tests**

In `install.rs`, add to `ApplyJob`:
```rust
pub restart_args: Vec<NativeString>,
pub working_dir: PathBuf,
pub database: Option<NativeString>,
```
Validation:
- `working_dir` must be absolute.
- If `database` is `Some`, its path must be absolute.
- `restart_args` must not contain NUL (`arg.contains_nul()`).
Add to `apply_job_round_trips_and_validates`:
```rust
let mut relative_working_dir = job.clone();
relative_working_dir.working_dir = "relative".into();
assert!(relative_working_dir.validate().is_err());

#[cfg(unix)]
{
    use std::os::unix::ffi::OsStringExt;
    let mut non_utf8 = job.clone();
    non_utf8.restart_args = vec![NativeString::from(OsString::from_vec(vec![b'x', 0x80]))];
    let json = serde_json::to_string(&non_utf8).unwrap();
    let decoded: ApplyJob = serde_json::from_str(&json).unwrap();
    assert_eq!(decoded.restart_args, non_utf8.restart_args);
    decoded.validate().unwrap();
}
```
Update every `ApplyJob` literal in `install.rs` and `helper.rs` tests with `working_dir: dir.to_path_buf()` and `database: None`.

- [ ] **Step 5: Launch with the original working directory and arguments**

In `helper.rs`, replace `launch` with:
```rust
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
```
Pass `&job.working_dir` in the new-version launch and `relaunch_old`.

Add a test:
```rust
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
```

- [ ] **Step 6: Add database identity to the health marker**

`HealthMarker`:
```rust
pub struct HealthMarker {
    pub version: String,
    pub mode: String,
    #[serde(default)]
    pub database: Option<NativeString>,
}
```
`write_health_marker(paths, token, version, database: Option<&Path>)` sets:
```rust
database: database.map(|path| NativeString::new(path.as_os_str())),
```
`wait_for_health` takes `expected_database: Option<&Path>` and requires:
```rust
let database_matches = expected_database
    .is_none_or(|expected| {
        marker.database.as_ref().map(NativeString::as_path) == Some(expected)
    });
```
Accept the marker only when version, mode, and `database_matches` all hold. If the version/mode match but the database does not, return an error mentioning the different database. In `run_helper_with_options`, pass `job.database.as_ref().map(NativeString::as_path)` as `expected_database`.
Update all `write_health_marker` / `wait_for_health` calls in tests. Add:
```rust
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
```
The successful call waits for the 1-second health grace period; the 100 ms timeout only bounds marker discovery, so this test takes about one second.

- [ ] **Step 7: Capture and pass the UI's working directory, arguments, and database path**

In `main.rs` `App::new`, before constructing `Self`:
```rust
let db_path = db
    .path()
    .and_then(|path| std::fs::canonicalize(path).ok());
```
Pass it as the fourth argument to `UpdateState::new`.

In `main.rs`, make the startup parser non-UTF-8 safe:
- Change `parse_args` to be generic over `Into<OsString>` and compare the known flags through `to_str()`; ignore unknown or non-UTF-8 arguments.
- Call `parse_args(std::env::args_os())` instead of `std::env::args()`.
- Add `launch_options_ignore_non_utf8_arguments`, which passes an invalid `OsString` and asserts the known flags still parse without a panic.

In `update_view.rs`:
- Add `db_path: Option<PathBuf>` to `UpdateState`; store the constructor argument.
- `write_health_marker` passes `self.db_path.as_deref()`.
- In `apply_update`, replace `std::env::args()` with:
```rust
let working_dir = match std::env::current_dir() {
    Ok(dir) => dir,
    Err(error) => {
        let message = format!("現在の作業ディレクトリを特定できません: {error}");
        self.update.phase = UpdatePhase::Error(message.clone());
        self.message = Some(message);
        cx.notify();
        return;
    }
};
let restart_args: Vec<NativeString> = std::env::args_os()
    .skip(1)
    .map(NativeString::new)
    .collect();
```
This is an early return before the job is created, matching the existing `current_exe()` failure path.
- Add to the job:
```rust
working_dir: working_dir.clone(),
database: self.update.db_path.as_ref().map(|path| NativeString::new(path.as_os_str())),
restart_args,
```

- [ ] **Step 8: Run the tests**

Run:
```bash
cargo test -p vrc-photo-update
cargo test -p vrc-photo-ui --lib update_view::tests
cargo clippy -p vrc-photo-update -p vrc-photo-ui --all-targets -- -D warnings
```
Expected: PASS. The non-UTF-8 round-trip, working-directory launch, database-mismatch rollback, and job validation tests all pass. On Windows CI, `cargo test -p vrc-photo-update` must also pass.

- [ ] **Step 9: Commit**

```bash
git add crates/update/src/native.rs crates/update/src/lib.rs crates/update/src/install.rs crates/update/src/helper.rs crates/ui/src/update_view.rs crates/ui/src/main.rs
git commit -m "fix(update): preserve restart arguments and working directory (#72)"
```

---

### Task 6: Fix the update card's auto-check, label, and notice colors (#69)

**Files:**
- Modify: `crates/ui/src/update_view.rs`
- Modify: `crates/ui/src/main.rs` (adapt the notice accessor)

**Interfaces:**
- Produces: `pub(crate) struct UpdateNotice { pub(crate) message: String, status: LastStatus }` with `pub(crate) fn color(&self) -> u32`.
- Changes: `UpdateState.notice: Option<UpdateNotice>`; `UpdateState::notice()` becomes `pub(crate)`; add `resume_available: Option<AvailableUpdate>`.
- Changes: `result_notice` returns `Option<UpdateNotice>`.
- Produces: `fn phase_allows_auto_check(phase: &UpdatePhase) -> bool`.
- Changes: `last_checked_label` returns only `YYYY/MM/DD HH:MM`.

- [ ] **Step 1: Update the failing UI tests**

In `update_view.rs` tests:
- Change the success/rollback tests to read `notice.message` and assert `notice.status`.
- Add:
```rust
#[test]
fn labels_only_the_time_for_a_checked_state() {
    let label = last_checked_label(Some(1_700_000_000));
    assert!(!label.contains("最終確認"));
    assert!(label.contains(':'));
}

#[test]
fn colors_rollback_and_failure_differently_from_success() {
    let success = UpdateNotice { message: "ok".into(), status: LastStatus::Success };
    let rolled_back = UpdateNotice { message: "back".into(), status: LastStatus::RolledBack };
    let failed = UpdateNotice { message: "failed".into(), status: LastStatus::Failed };
    assert_eq!(success.color(), GREEN);
    assert_eq!(rolled_back.color(), AMBER);
    assert_eq!(failed.color(), RED);
}

#[test]
fn auto_check_runs_while_an_update_is_available() {
    let update = AvailableUpdate {
        version: Version::new(0, 4, 0),
        notes_url: None,
        asset: None,
        public_keys: Vec::new(),
    };
    assert!(phase_allows_auto_check(&UpdatePhase::Available(update)));
    assert!(!phase_allows_auto_check(&UpdatePhase::Applying));
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run:
```bash
cargo test -p vrc-photo-ui --lib update_view::tests
```
Expected: FAIL to compile because `UpdateNotice` and `phase_allows_auto_check` do not exist.

- [ ] **Step 3: Implement the notice type and label**

```rust
#[derive(Debug, Clone)]
pub(crate) struct UpdateNotice {
    pub(crate) message: String,
    status: LastStatus,
}

impl UpdateNotice {
    pub(crate) fn color(&self) -> u32 {
        match self.status {
            LastStatus::Success => GREEN,
            LastStatus::RolledBack => AMBER,
            LastStatus::Failed => RED,
        }
    }
}
```
Change `result_notice` to return `Option<UpdateNotice>` and wrap each existing message. Change `UpdateState.notice` to `Option<UpdateNotice>` and make `notice()` `pub(crate)`. In `main.rs`, use `notice.message` when seeding the transient message. The visibility changes keep the `pub` `UpdateState` from exposing a crate-private type.
In `last_checked_label`, remove the `"最終確認: "` prefix.

- [ ] **Step 4: Allow automatic checks from `Available` and preserve the update on failure**

Add:
```rust
fn phase_allows_auto_check(phase: &UpdatePhase) -> bool {
    matches!(
        phase,
        UpdatePhase::Idle
            | UpdatePhase::UpToDate
            | UpdatePhase::Available(_)
            | UpdatePhase::Error(_)
    )
}
```
Use it in `maybe_auto_update_check` in place of the inline `matches!`.

In `start_update_check`, before setting `Checking`:
```rust
self.update.resume_available = match &self.update.phase {
    UpdatePhase::Available(update) => Some(update.clone()),
    _ => None,
};
```
Extract the result transition so the behavior is testable:
```rust
fn resolve_checked_phase(
    manual: bool,
    result: &std::result::Result<Option<AvailableUpdate>, String>,
    resume: Option<AvailableUpdate>,
) -> UpdatePhase {
    match result {
        Ok(Some(update)) => UpdatePhase::Available(update.clone()),
        Ok(None) => UpdatePhase::UpToDate,
        Err(message) if manual => UpdatePhase::Error(message.clone()),
        Err(_) => resume.map(UpdatePhase::Available).unwrap_or(UpdatePhase::Idle),
    }
}
```
`handle_update_message` takes `resume_available`, keeps the message updates, then sets the phase from `resolve_checked_phase`. Add `automatic_check_failure_restores_an_available_update`: automatic failure with a resume value stays `Available`, manual failure becomes `Error`, no resume becomes `Idle`, and `Ok(None)` becomes `UpToDate`.
In the thread-spawn failure path, restore `Available` for an automatic check instead of replacing it with an error.

- [ ] **Step 5: Use the notice color in the card**

In `update_card`, change the notice block to:
```rust
if let Some(notice) = self.update.notice() {
    card = card.child(
        div()
            .p(px(10.))
            .rounded(px(8.))
            .bg(rgb(PANEL_2))
            .text_size(px(11.))
            .text_color(rgb(notice.color()))
            .child(notice.message.clone()),
    );
}
```

- [ ] **Step 6: Run the tests**

Run:
```bash
cargo test -p vrc-photo-ui --lib update_view::tests
cargo clippy -p vrc-photo-ui --all-targets -- -D warnings
```
Expected: PASS. The label has no duplicate prefix, rollback/failed notices use different colors, and `Available` is eligible for auto-check.

- [ ] **Step 7: Commit**

```bash
git add crates/ui/src/update_view.rs crates/ui/src/main.rs
git commit -m "fix(ui): refine update card status and notices (#69)"
```

---

### Post-review fixes

The whole-branch review found two issues that the task plan did not cover:

- **Critical:** `main()` still called `std::env::args()`, so a non-UTF-8 argument panicked before the updater could run. Fixed by making `parse_args` accept `Into<OsString>`, calling it with `args_os()`, and adding the non-UTF-8 parser test described in Task 5.
- **Important:** the auto-check failure restoration had no behavior test. Fixed by extracting `resolve_checked_phase` and adding `automatic_check_failure_restores_an_available_update` as described in Task 6.

The full-workspace run also showed that the release-tool test fixture's fake `.minisig` is rejected by the new #70 validation. The fixture now signs with a generated minisign key. These review fixes are separate commits after the six issue commits.

### Task 7: Full verification, PR, and merge

**Files:**
- No source changes expected.
- The plan file remains untracked and is not part of the code PR.

**Interfaces:**
- Consumes: all six commits.
- Produces: green CI on PR `fix/auto-update-review` and a merged `main`.

- [ ] **Step 1: Run the full local verification**

Run:
```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```
Expected: all three commands exit 0.

- [ ] **Step 2: Review the diff against `main`**

Run:
```bash
git log --oneline main..HEAD
git diff --stat main...HEAD
git status --short
```
Expected: the six issue commits plus the release-tool fixture and review-fix commits; only `crates/update/**`, `crates/ui/src/update_view.rs`, `crates/ui/src/main.rs`, and `crates/release-tool/src/lib.rs` changed; the plan file is untracked.

- [ ] **Step 3: Push and open the PR**

```bash
git push -u origin fix/auto-update-review
gh pr create --base main --head fix/auto-update-review \
  --title "fix: address auto-update review findings (#69–#74)" \
  --body "Closes #69
Closes #70
Closes #71
Closes #72
Closes #73
Closes #74

## Summary
- reject malformed artifact signatures during check and carry the check-time keys into download
- fsync the AppImage parent directory and preserve restart args, working directory, and database identity
- tag I/O errors by operation and fix the update card's auto-check, label, and notice colors

## Verification
- cargo fmt --all -- --check
- cargo clippy --workspace --all-targets -- -D warnings
- cargo test --workspace
- Windows CI updater tests"
```

- [ ] **Step 4: Watch CI and merge**

Follow the `ci-watch` skill in the background. On success, merge:
```bash
gh pr merge <number> --merge
git switch main
git pull --ff-only
git branch -d fix/auto-update-review
git push origin --delete fix/auto-update-review
```
On failure, analyze the failing job and push a fix on the branch. Do not merge red.

- [ ] **Step 5: Open the separate docs PR for this plan**

After the code PR merges, copy this plan into a docs worktree/branch and open a docs-only PR. Do not mix it into the code PR.
