# Automatic Application Updates Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a signed, UI-driven update path for the AppImage and Windows NSIS builds, with a previous-version rollback, a manual-update notice for deb/pacman, and an auto-check setting.

**Architecture:** A new `vrc-photo-update` crate checks a signed `latest.json` on GitHub Releases, verifies minisign signatures, streams downloads to a staging directory, and hands the replace/restart work to a small `vrc-photo-updater` helper. The helper waits for the UI and daemon to exit, backs up the current install, applies the update, waits for a health marker written by the new UI, and restores the previous version if the new one does not start. The UI checks on startup and every 24 hours, shows progress in Settings, and never blocks on the network. No DB migration; two settings fields.

**Tech Stack:** Rust stable (edition 2024), `ureq` 3.5 (rustls), `minisign-verify` 0.3, `semver`, `sha2`, `base64`, `serde`, `chrono`, gpui-kit 0.8, `async-channel`, GitHub Actions, minisign CLI.

**Spec:** `docs/superpowers/specs/2026-10-24-auto-update-design.md`

## Global Constraints

- Rust stable, edition 2024; keep `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, and `cargo test --workspace` green.
- No DB schema migration. Add only `auto_update_check` (default `true`) and `update_last_checked_at` (default `None`) to `AppSettings` with serde defaults.
- Self-update only for AppImage and current-user Windows NSIS. deb/pacman and development/portable/per-machine Windows installs are notify-only.
- Verify the manifest signature **before** parsing it, and re-verify the artifact signature and SHA-256 **before** replacing anything.
- Release builds accept only HTTPS; debug builds may use HTTP only on loopback. Never trust an unsigned update.
- Limits: manifest 512 KiB, signature 16 KiB, artifact 300 MiB.
- All network and file work runs off the UI thread; bounded timeouts; automatic-check failures are silent in the UI.
- Japanese UI copy, following the existing Settings card style.
- Public keys live in `packaging/update/keys.txt`; the secret key exists only in the GitHub Actions secret `MINISIGN_SECRET_KEY` and a maintainer-held offline backup. Never print it.
- Do not bump the app version or create a tag/release in this work. The first signed release is a separate user-authorized release step.
- Worktree: `../vrc-photo-manager-auto-update`, branch `feat/auto-update`.

## Review Focus

1. A tampered or unsigned manifest/artifact must never be applied, including after the UI hands off to the helper.
2. A failed update (DB fallback, new binary exits, installer failure, daemon won't stop) must leave a launchable previous version.
3. Offline or slow networking must not freeze the UI or nag on automatic checks.
4. The daemon must be stopped before replacement and restarted by the new UI after the health marker; no orphan process or locked file.
5. Existing settings and installs must load unchanged; deb/pacman and unsupported Windows installs must never self-replace.

---

### Task 1: ADR, workspace crate, and manifest validation

**Files:**
- Create: `docs/adr/0012-auto-update.md`
- Create: `crates/update/Cargo.toml`
- Create: `crates/update/src/lib.rs`
- Create: `crates/update/src/error.rs`
- Create: `crates/update/src/manifest.rs`
- Modify: `Cargo.toml` (workspace members and dependencies)
- Modify: `.gitignore` (secret-key patterns)
- Test: inline `#[cfg(test)] mod tests` in `manifest.rs`

**Interfaces:**
- Produces: `vrc_photo_update::manifest::{Manifest, PlatformAsset, UpdateFormat, AvailableUpdate, MANIFEST_SCHEMA, MAX_MANIFEST_BYTES, MAX_SIGNATURE_BYTES, MAX_ARTIFACT_BYTES}`
- Produces: `Manifest::parse_and_validate(bytes: &[u8], allow_http: bool) -> Result<(Manifest, semver::Version)>`
- Produces: `vrc_photo_update::error::{UpdateError, Result}`, including
  `UpdateError::Integrity(String)` for size/hash mismatches

- [ ] **Step 1: Write the ADR**

Create `docs/adr/0012-auto-update.md` with:
- Status `Accepted`, date `2026-10-24`.
- Context: manual downloads, five install formats, no signatures, no network code, issue #24.
- Decision: static signed `latest.json` on GitHub Releases; minisign over the manifest and artifacts; AppImage/Windows current-user self-update; deb/pacman notify-only; UI-driven 24-hour checks; `vrc-photo-updater` helper with backup/health/rollback; per-platform install detection.
- Consequences and limits from spec §10.
- Alternatives: GitHub API, unsigned TLS-only, all-format self-update, daemon-side checks, `cargo-packager-updater`.
- References: issue #24, this spec, `docs/release.md`.

- [ ] **Step 2: Add the workspace crate and dependencies**

Add `"crates/update"` to `[workspace] members`. Add to `[workspace.dependencies]`:

```toml
base64 = "0.22"
hex = "0.4"
minisign-verify = "0.3"
semver = "1"
sha2 = "0.10"
ureq = { version = "3.5", default-features = false, features = ["rustls", "gzip"] }
url = "2"
```

`crates/update/Cargo.toml` declares `vrc-photo-update`, inherits the workspace
version/edition/license/repository/authors, and has a library plus two binaries
(`vrc-photo-updater`, `vrc-photo-update-manifest`) created in later tasks. Add
`tempfile` as a dev-dependency. Add `winreg = "0.56"` under
`[target.'cfg(windows)'.dependencies]` now so later tasks do not touch the
manifest again.

Add `.gitignore` entries:

```text
/packaging/update/*.key
/update.key
```

- [ ] **Step 3: Write the failing manifest tests**

In `manifest.rs`, test:

```rust
#[test]
fn validates_a_complete_manifest() {
    let bytes = br#"{
      "schema": 1,
      "version": "0.4.0",
      "pub_date": "2026-10-24T12:00:00Z",
      "notes_url": "https://github.com/TommyZ-7/vrc-photo-manager/releases/tag/v0.4.0",
      "platforms": {
        "linux-x86_64": {
          "url": "https://github.com/x/y/releases/download/v0.4.0/vrc-photo-manager_0.4.0_x86_64.AppImage",
          "format": "appimage",
          "size": 1024,
          "sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
          "signature": "untrusted comment: x\nRUQ=\ntrusted comment: x\nAAAA"
        }
      }
    }"#;
    let (manifest, version) = Manifest::parse_and_validate(bytes, false).unwrap();
    assert_eq!(version, semver::Version::new(0, 4, 0));
    assert_eq!(manifest.platforms.len(), 1);
}
```

Add failure tests for: schema 2, unparsable version, zero/oversized `size`,
sha256 of 63 chars, `http://` URL with `allow_http=false`, loopback http with
`allow_http=true` accepted, unknown platform key, unsafe file name
(`../evil`), and `notes_url` with `javascript:`.

- [ ] **Step 4: Run the tests to verify they fail**

Run: `cargo test -p vrc-photo-update manifest`
Expected: FAIL; `Manifest` is not defined.

- [ ] **Step 5: Implement the manifest types and validation**

Implement `Manifest`, `PlatformAsset`, `UpdateFormat` (`appimage`, `nsis`), and
`AvailableUpdate`. `parse_and_validate`:

1. `serde_json::from_slice` with errors mapped to `InvalidManifest`.
2. Require `schema == 1`.
3. Strip one leading `v` from `version`; parse with `semver`.
4. Require only `linux-x86_64` and `windows-x86_64` keys.
5. For each asset: `url::Url::parse`, scheme `https` (or `http` on a loopback
   host when `allow_http`), safe single-component file name, `size` in
   `1..=MAX_ARTIFACT_BYTES`, 64 lowercase hex chars, non-empty signature.
6. `notes_url` must be `https` (or loopback http), if present.
7. `pub_date`, if present, must parse as RFC 3339; store as `String`.

Use `UpdateError::InvalidManifest(String)` with a specific reason.

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test -p vrc-photo-update manifest`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add Cargo.toml Cargo.lock .gitignore crates/update docs/adr/0012-auto-update.md
git commit -m "feat: add update manifest schema and validation"
```

---

### Task 2: minisign verification and the production key

**Files:**
- Create: `crates/update/src/verify.rs`
- Create: `crates/update/examples/generate_update_key.rs`
- Create: `packaging/update/keys.txt`
- Modify: `crates/update/src/lib.rs`
- Modify: `crates/update/Cargo.toml` (`minisign` dev-dependency)
- Test: inline tests in `verify.rs`

**Interfaces:**
- Consumes: `UpdateError`.
- Produces: `verify::verify_bytes(data: &[u8], signature_text: &str, keys: &[String]) -> Result<()>`
- Produces: `verify::verify_file(path: &Path, signature_text: &str, keys: &[String]) -> Result<()>`
- Produces: `verify::decode_signature_base64(encoded: &str) -> Result<String>`
- Produces: `lib::embedded_public_keys() -> Vec<String>`
- Produces: `lib::effective_public_keys() -> Vec<String>` (debug-only env override)

- [ ] **Step 1: Generate the production key and register the secret**

Add `minisign = "0.10"` as a dev-dependency. Create
`examples/generate_update_key.rs` that:
- takes one output path argument,
- calls `minisign::KeyPair::generate_unencrypted_keypair()`,
- writes the secret key box string with mode 0600,
- prints only the public key base64 line.

Run:

```bash
mkdir -p /tmp/opencode/vrcpm-update-key
cargo run -p vrc-photo-update --example generate_update_key -- /tmp/opencode/vrcpm-update-key/minisign.key
```

Append the printed `RW...` line to `packaging/update/keys.txt` under a
`# Production update key (generated 2026-10-24)` comment. Register the secret:

```bash
gh secret set MINISIGN_SECRET_KEY < /tmp/opencode/vrcpm-update-key/minisign.key
gh secret list | grep MINISIGN_SECRET_KEY
```

Do not print the secret. Keep the secret file for a maintainer offline backup
and tell the user its path in the task report; never `git add` it. If
`gh secret set` fails (insufficient permission), stop and ask the user to set
`MINISIGN_SECRET_KEY` manually; do not proceed to a release workflow that would
depend on it.

- [ ] **Step 2: Write the failing verification tests**

Generate a fresh test key pair inside the test (not the production key), sign
`b"test"` with `minisign::sign(None, &sk, Cursor::new(data), None, None)`, and
assert:

- `verify_bytes(b"test", &signature.to_string(), &[pk_base64])` is `Ok`.
- Tampered data returns `UpdateError::Signature`.
- A different key returns `UpdateError::Signature`.
- A legacy (non-prehashed) signature returns `UpdateError::Signature`.
- `embedded_public_keys()` is non-empty and every key parses with
  `minisign_verify::PublicKey::from_base64`.

Use the legacy fixture from the `minisign-verify` crate documentation for the
legacy case.

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test -p vrc-photo-update verify`
Expected: FAIL; `verify_bytes` is not defined.

- [ ] **Step 4: Implement verification**

- Parse the signature with `Signature::decode`.
- Parse all keys with `PublicKey::from_base64`; invalid keys are skipped.
- For byte slices and files, use `PublicKey::verify_stream` + `update` +
  `finalize` (prehashed only). Try each key; the key-ID mismatch is an
  expected per-key failure.
- `verify_file` streams the file in 64 KiB chunks; no whole-file allocation.
- `decode_signature_base64` uses `base64::STANDARD`.
- `embedded_public_keys` parses `include_str!("../../../packaging/update/keys.txt")`,
  ignoring blank lines and `#` comments.
- `effective_public_keys` returns the debug-only
  `VRC_PHOTO_UPDATE_PUBLIC_KEY` value when `cfg!(debug_assertions)` and the
  variable is non-empty; otherwise the embedded list.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p vrc-photo-update verify`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/update .gitignore packaging/update/keys.txt
git commit -m "feat: verify update signatures with minisign"
```

---

### Task 3: Install-kind detection and update paths

**Files:**
- Create: `crates/update/src/install.rs`
- Modify: `crates/update/src/lib.rs`
- Test: inline tests in `install.rs`

**Interfaces:**
- Produces: `InstallKind`, `PackageFamily`, `classify_linux`, `classify_windows`, `install_kind`
- Produces: `UpdatePaths` and its path accessors
- Produces: `Manifest::asset_for(&self, kind: &InstallKind) -> Option<&PlatformAsset>`

- [ ] **Step 1: Write the failing tests**

Table-driven tests:

```rust
assert_eq!(classify_linux(Path::new("/usr/bin/vrc-photo-manager"), None,
    "ID=ubuntu\nID_LIKE=debian\n"), InstallKind::SystemPackage { family: PackageFamily::Deb });
assert!(matches!(classify_linux(Path::new("/home/me/Apps/VRC.AppImage"),
    Some(Path::new("/home/me/Apps/VRC.AppImage")), ""), InstallKind::AppImage { .. }));
assert_eq!(classify_linux(Path::new("/home/me/project/target/debug/vrc-photo-manager"),
    None, ""), InstallKind::Development);
assert_eq!(PackageFamily::from_os_release("ID=arch\n"), PackageFamily::Pacman);
```

Windows classification with injected registry paths:

```rust
let exe = Path::new(r"C:\Users\me\AppData\Local\VRC Photo Manager\vrc-photo-manager.exe");
assert_eq!(classify_windows(exe, Some(Path::new(r"C:\Users\me\AppData\Local\VRC Photo Manager")), None),
    InstallKind::WindowsCurrentUser { install_dir: exe.parent().unwrap().into() });
assert_eq!(classify_windows(exe, None, Some(Path::new(r"C:\Program Files\VRC Photo Manager"))),
    InstallKind::WindowsPerMachine { .. });
assert_eq!(classify_windows(exe, None, None), InstallKind::WindowsOther);
```

Add `automatic_apply()` and `automatic_check()` assertions, `asset_for` tests
(AppImage → linux appimage; WindowsCurrentUser → windows nsis; SystemPackage →
None), and `UpdatePaths::ensure()` creating `updates/`, `downloads/`, `bin/`
with mode 0700 on Unix.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p vrc-photo-update install`
Expected: FAIL; types are not defined.

- [ ] **Step 3: Implement classification**

- `PackageFamily::from_os_release`: lowercase; `debian`/`ubuntu` or
  `ID_LIKE` containing debian → `Deb`; `arch`/`manjaro` or `ID_LIKE` containing
  arch → `Pacman`; otherwise `Unknown`.
- `classify_linux`: AppImage if the path exists; otherwise `/usr/bin` or
  `/usr/local/bin` prefix → system package; otherwise development.
- `classify_windows`: canonicalize and compare case-insensitively (strip
  surrounding quotes) with the HKCU/HKLM `InstallLocation`; HKCU match →
  current user, HKLM match → per machine, no match → other.
- `install_kind()`: real `APPIMAGE`, `/etc/os-release`, current exe, and (on
  Windows) `winreg` reads of
  `Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\VRC Photo Manager`
  under HKCU/HKLM. Any registry error → `WindowsOther`.
- `automatic_apply`: true only for AppImage and WindowsCurrentUser.
- `automatic_check`: true for AppImage, WindowsCurrentUser, and SystemPackage;
  false for Development, WindowsOther, WindowsPerMachine.
- `UpdatePaths` stores `root`; `ensure()` creates the root, `downloads`, `bin`
  and sets 0700 on Unix. `health_file(token)` returns
  `root/health-{token}.json`; `backup_dir(version)` returns `root/backup/{version}`.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p vrc-photo-update install`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/update
git commit -m "feat: detect update-capable installations"
```

---

### Task 4: HTTP client and signed update check

**Files:**
- Create: `crates/update/src/fetch.rs`
- Create: `crates/update/src/check.rs`
- Modify: `crates/update/src/lib.rs`
- Test: inline tests in `fetch.rs` and `check.rs`

**Interfaces:**
- Consumes: `Manifest`, `InstallKind`, `verify_bytes`.
- Produces: `HttpClient` with `get_bytes` and `download` as specified in spec §6.4.
- Produces: `UreqClient::new(allow_http: bool)` and `UreqClient::with_timeouts(connect, small_global)`.
- Produces: `FeedConfig::from_env()`, `FeedConfig::for_tests`, and
  `check(client, feed, current, kind) -> Result<Option<AvailableUpdate>>`.

- [ ] **Step 1: Write the failing check tests**

Use a `FakeHttpClient` map of URL → bytes. Build a signed manifest helper that
signs the bytes with a generated test key. Assert:

- Newer version with an AppImage asset returns `Some` with the asset.
- Equal/older version returns `None`.
- Manifest 404 returns `None`.
- Signature 404 returns `Signature`.
- Tampered manifest bytes return `Signature`.
- A manifest over 512 KiB returns `InvalidManifest` before parsing.
- `SystemPackage` returns the newer version with `asset: None`.
- AppImage manifest without `linux-x86_64` returns the version with
  `asset: None`.
- `FeedConfig::for_tests` with an `http://127.0.0.1` URL works; a non-loopback
  http URL fails even when `allow_http` is false.
- `check` requests the manifest URL and `manifest_url + ".minisig"`.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p vrc-photo-update check`
Expected: FAIL; `check` is not defined.

- [ ] **Step 3: Implement the client and check flow**

- `UreqClient` holds `allow_http`; build ureq agents per request with
  connect 15 s / global 60 s for small responses and connect 15 s /
  receive-body 10 min for downloads.
- `get_bytes` rejects `Content-Length > max`, reads at most `max + 1` bytes,
  maps 404 to `NotFound`, other statuses to `Network`.
- `download` reads fixed 64 KiB chunks, enforces `max_bytes`, and invokes the
  callback; callback errors abort the read.
- `FeedConfig::from_env` uses `VRC_PHOTO_UPDATE_FEED` or the production URL,
  `effective_public_keys()`, and `cfg!(debug_assertions)` for `allow_http`.
- `check`: fetch manifest + signature, verify, parse/validate, compare semver,
  select `asset_for(kind)`, return `AvailableUpdate`.
- A manifest 404 is `Ok(None)`; a signature 404 is an error.

- [ ] **Step 4: Add a real loopback HTTP test**

Bind `127.0.0.1:0`, serve the manifest and signature once, and assert
`check` succeeds with `UreqClient`. Add a second listener that accepts and never
responds; with `with_timeouts(200ms, 300ms)`, assert the call returns an error
within 2 seconds.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p vrc-photo-update check`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/update
git commit -m "feat: check signed update feed over HTTPS"
```

---

### Task 5: Streaming download with signature and hash verification

**Files:**
- Create: `crates/update/src/download.rs`
- Modify: `crates/update/src/lib.rs`
- Test: inline tests in `download.rs`

**Interfaces:**
- Consumes: `HttpClient`, `AvailableUpdate`, `verify` internals.
- Produces: `download(client, update, dest, progress, cancel) -> Result<PathBuf>`

- [ ] **Step 1: Write the failing download tests**

With a fake client and a generated test key:

- A valid artifact writes `dest`, returns it, and reports progress ending at
  the total size.
- A tampered artifact returns `Signature` and leaves no `.part` file.
- A SHA-256 mismatch returns `Integrity` and leaves no file.
- An artifact larger than the manifest `size` returns `Integrity` and leaves no
  file.
- Setting the cancel flag before/while downloading returns `Cancelled` and
  leaves no file.
- An `AvailableUpdate` without an asset returns `UnsupportedPlatform`.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p vrc-photo-update download`
Expected: FAIL.

- [ ] **Step 3: Implement the download**

- Create the destination parent.
- Write to `dest + ".part"`.
- Decode the asset's base64 signature, parse the signature, and create one
  `StreamVerifier` for the matching key (skip keys with `UnexpectedKeyId`).
- For each received chunk: check cancellation, write, update SHA-256 and the
  verifier, enforce `size + 1`, and call progress with `(received, Some(size))`.
- Flush/fsync, finalize the verifier, compare the hash, and rename `.part` to
  `dest`.
- On any error, remove `.part`.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p vrc-photo-update download`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/update
git commit -m "feat: download and verify update artifacts"
```

---

### Task 6: Apply job, paths, and the helper file engine

**Files:**
- Modify: `crates/update/src/install.rs` (add `ApplyJob`, `ApplyKind`, `start_apply`, `prepare_apply`)
- Modify: `crates/update/Cargo.toml` (`uuid.workspace = true`)
- Create: `crates/update/src/helper.rs`
- Modify: `crates/update/src/lib.rs`
- Test: inline tests in `install.rs` and `helper.rs`

**Interfaces:**
- Produces: `ApplyKind`, `ApplyJob`, `ApplyJob::validate`, `ApplyJob::new_health_token() -> String`
- Produces: `prepare_apply(job: &ApplyJob, current_exe: &Path, data_dir: &Path) -> Result<PathBuf>`
- Produces: `start_apply(job: &ApplyJob, current_exe: &Path, data_dir: &Path) -> Result<()>`
- Produces: `helper::{LastResult, LastStatus, HealthMarker, read_health_marker, write_health_marker, replace_appimage, restore_appimage, apply_in_progress}`

- [ ] **Step 1: Write the failing job and file tests**

- `ApplyJob` round-trips through JSON with schema 1.
- `validate` rejects a relative path, an empty health token, a token with `/`,
  a non-semver version, and schema 2.
- `prepare_apply` copies a fake `vrc-photo-updater` from a temp “install dir”
  to `updates/bin/`, chmods it 0700, writes the job file atomically, and does
  not spawn anything.
- `replace_appimage`: target `app.AppImage` with content `old`, staged `new`
  with content `new` → target content `new`, `app.AppImage.previous` content
  `old`, returned backup path; missing staged leaves the target untouched.
- `restore_appimage`: restores the backup content.
- `LastResult::write` then `take` returns the result and removes the file; a
  second `take` returns `None`.
- `write_health_marker`/`read_health_marker` round-trip `{version, mode}`.
- `apply_in_progress`: a lock file with the current PID → true; with a
  definitely-dead PID (a just-reaped child) → false and the file is removed.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p vrc-photo-update`
Expected: FAIL.

- [ ] **Step 3: Implement paths, job, and file engine**

- `ApplyJob` fields exactly as spec §6.6 plus `data_dir`. Validate absolute
  paths, schema, versions, token charset, and `kind`.
  `new_health_token()` returns `uuid::Uuid::new_v4().to_string()`.
- `prepare_apply`: `UpdatePaths::ensure`, locate
  `current_exe.parent()/vrc-photo-updater[.exe]`, copy to `paths.bin_dir()`,
  mode 0700, write `job.json` via a `.tmp` + rename, return the copied helper
  path.
- `start_apply`: `prepare_apply`, then spawn with `--apply <job>`, stdin null,
  stdout/stderr appended to `updater.log`, current dir `paths.root`.
- `replace_appimage`: verify the target is a file; rename target to
  `<target>.previous`; copy staged to target; set mode to old mode | 0o111;
  fsync; on any error rename the backup back and return the error.
- `restore_appimage`: rename backup over target.
- `LastResult` and `HealthMarker` serialize with serde, write atomically, and
  use 0600 on Unix.
- `apply_in_progress`: read the lock PID; if alive return true; otherwise
  remove the stale lock and return false.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p vrc-photo-update`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/update
git commit -m "feat: add update apply job and backup engine"
```

---

### Task 7: Helper binary, process wait, health check, rollback

**Files:**
- Create: `crates/update/src/bin/vrc-photo-updater.rs`
- Modify: `crates/update/src/helper.rs`
- Modify: `crates/update/src/lib.rs`
- Test: inline tests in `helper.rs` plus a Unix integration test in the same module

**Interfaces:**
- Consumes: `ApplyJob`, `UpdatePaths`, `verify_file`, `replace_appimage`, `HealthMarker`, `LastResult`.
- Produces: `HelperOptions { exit_timeout: Duration, health_timeout: Duration }`
  with `Default = 30s/45s`; under `#[cfg(test)]` it also has
  `skip_signature_check: bool` so the process integration test can exercise
  launch/rollback without the production key.
- Produces: `run_helper(job_path: &Path) -> Result<()>` and
  `run_helper_with_options(job_path: &Path, options: HelperOptions) -> Result<()>`
- Produces: `wait_for_process_exit(pid: u32, timeout: Duration) -> bool` and
  `wait_for_health(paths: &UpdatePaths, token: &str, expected: &str, child: &mut Child, timeout: Duration) -> Result<()>`
- Produces: the `vrc-photo-updater --apply <job.json>` binary.

- [ ] **Step 1: Write the failing process tests**

- `wait_for_process_exit(pid, 2s)` returns true for a child that already
  exited and false for the current process with a 100 ms timeout.
- `wait_for_health` returns `Ok` when a marker with the expected version
  appears, `Err` for the wrong version, and `Err` on timeout.
- On Unix, a helper integration test:
  - Build a temp data dir, a fake target script `old.sh`, a staged `new.sh`,
    a job with a dead `ui_pid`, and
    `HelperOptions { health_timeout: 2s, skip_signature_check: true, ..Default::default() }`.
  - Staged writes `health-<token>.json` with `{"version":"9.9.9","mode":"normal"}`
    then sleeps; assert `run_helper_with_options` returns Ok, the target content
    is the new script, `last-result.json` is `success`, and
    `<target>.previous` exists.
  - Repeat with a staged script that exits without writing the marker; assert
    the target is restored to `old.sh`, the result is `rolled_back`, and the
    old target was launched (have the old script write a sentinel file).

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p vrc-photo-update helper`
Expected: FAIL.

- [ ] **Step 3: Implement the helper**

`run_helper_with_options`:
1. Read and validate the job; create `UpdatePaths`.
2. Acquire `apply.lock` with `create_new`; if it exists, read the PID, remove it
   only if dead, and retry once.
3. Wait for `ui_pid` and `daemon_pid` with the configured timeout. On timeout,
   write a `rolled_back` result with reason `processes did not exit`, relaunch
   the target with `restart_args` (no health token), and return `Err`.
4. Re-verify the staged file signature and SHA-256 from the job.
5. Apply:
   - AppImage: `replace_appimage`; launch the new target with
     `VRC_PHOTO_HEALTH_TOKEN`.
   - Windows (`cfg(windows)`): copy `vrc-photo-manager.exe` and
     `vrc-photo-daemon.exe` to `backup_dir`, run the staged installer `/S` with
     a 10-minute timeout, then launch the new target.
6. Write the `success` result, wait for the health marker; on failure kill the
   child, restore the backup, write `rolled_back`, and launch the old target.
7. Delete the staged file, job, and lock on exit. Keep `.previous`/backup.
8. Append every step to `updater.log`; rotate at 1 MiB.

`wait_for_process_exit` uses `/proc/<pid>` on Linux, `kill(pid, 0)` on other
Unix, and `OpenProcess`/`WaitForSingleObject` on Windows. `wait_for_health`
polls every 250 ms and also checks `Child::try_wait`.

- [ ] **Step 4: Implement the binary**

```rust
#![cfg_attr(windows, windows_subsystem = "windows")]

fn main() {
    let mut args = std::env::args_os().skip(1);
    match (args.next().as_deref(), args.next()) {
        (Some("--apply"), Some(path)) => {
            if let Err(error) = vrc_photo_update::helper::run_helper(Path::new(&path)) {
                // log already written by run_helper where possible
                std::process::exit(1);
            }
        }
        _ => std::process::exit(2),
    }
}
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p vrc-photo-update helper`
Expected: PASS (the Unix integration test may be skipped on non-Unix).

- [ ] **Step 6: Commit**

```bash
git add crates/update
git commit -m "feat: add update helper with health check and rollback"
```

---

### Task 8: IPC daemon PID plumbing

**Files:**
- Modify: `crates/ipc/src/client.rs`
- Modify: `crates/ipc/src/lib.rs` if needed for exports
- Modify: `crates/ui/src/ipc_bridge.rs`
- Test: existing client/bridge tests plus new assertions

**Interfaces:**
- Produces: `client::SubscriptionInfo { status: StatusSnapshot, daemon_pid: u32, daemon_version: String }`
- Produces: `Subscription::open(...) -> Result<(Subscription, SubscriptionInfo), IpcError>`
- Produces: `ipc_bridge::Incoming::Connected { status: StatusSnapshot, daemon_pid: u32 }`

- [ ] **Step 1: Write the failing tests**

- Client test: a server hello with `pid: 4242` → `Subscription::open` returns
  `daemon_pid == 4242`.
- Bridge test: the first `Incoming` is `Connected { daemon_pid: 1, .. }`.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p vrc-photo-ipc subscription`
Expected: FAIL; the type/field is missing.

- [ ] **Step 3: Implement the API change**

Make `handshake` return the parsed `Hello` fields (`protocol_version`, `pid`,
`daemon_version`) or a `SubscriptionInfo`. `Subscription::open` returns the new
struct. Update the bridge to pass the PID. Do not change the wire protocol or
`PROTOCOL_VERSION`.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p vrc-photo-ipc`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/ipc crates/ui/src/ipc_bridge.rs
git commit -m "feat: expose the daemon PID to the UI"
```

---

### Task 9: UI settings, update card, background check/download/apply

**Files:**
- Create: `crates/ui/src/update_view.rs`
- Modify: `crates/ui/src/main.rs`
- Modify: `crates/core/src/settings.rs`
- Modify: `crates/ui/Cargo.toml` (add `vrc-photo-update`)
- Test: inline tests in `settings.rs` and `update_view.rs`

**Interfaces:**
- Consumes: `check`, `download`, `install_kind`, `start_apply`, `ApplyJob`, `LastResult`, `UpdatePaths`.
- Produces: `AppSettings::{auto_update_check, update_last_checked_at}`
- Produces: `App::{start_update_timers, start_update_check, start_update_download, apply_update, update_card, complete_update_startup, handle_update_message}`
- Produces: `UpdateState` with the status used by the card and nav badge.

- [ ] **Step 1: Write the failing settings tests**

In `settings.rs`:

```rust
#[test]
fn defaults_auto_update_check_to_on() {
    let settings = AppSettings::default();
    assert!(settings.auto_update_check);
    assert_eq!(settings.update_last_checked_at, None);
}

#[test]
fn old_settings_json_defaults_update_fields() {
    let json = r#"{"photo_roots":[],"log_dir":null,"managed_dir":null,
      "language":"ja","autostart":true,"tray":true,"auto_compress":true,
      "auto_compress_backlog":false,"estimated_link":true,
      "thumbnail_cache_mb":2048,"compression_workers":1}"#;
    let settings: AppSettings = serde_json::from_str(json).unwrap();
    assert!(settings.auto_update_check);
    assert_eq!(settings.update_last_checked_at, None);
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p vrc-photo-core settings`
Expected: FAIL; fields are missing.

- [ ] **Step 3: Add the settings fields and run the tests**

Add `#[serde(default = "default_auto_update_check")] pub auto_update_check: bool`,
`#[serde(default)] pub update_last_checked_at: Option<i64>`, and the default
`true`. Update `Default`. Run:
`cargo test -p vrc-photo-core settings` → PASS.

- [ ] **Step 4: Write the failing update-view helper tests**

In `update_view.rs`:

- `format_bytes(1536)` → `"1.5 KB"`.
- `last_checked_label(None)` → `"未確認"`.
- `result_notice(&LastResult { status: Success, from: "0.3.0", to: "0.4.0", .. }, "0.4.0")`
  → `Some("v0.4.0 に更新しました")`.
- A `rolled_back` result for the running `from` version includes the reason.
- `can_apply(&InstallKind::SystemPackage { .. })` → false; AppImage with an
  asset → true; AppImage with `daemon_active && !ipc_connected` → false.

- [ ] **Step 5: Run the tests to verify they fail**

Run: `cargo test -p vrc-photo-ui update_view`
Expected: FAIL.

- [ ] **Step 6: Implement `UpdateState` and the background flow**

`update_view.rs` (child module of `main`; import the GPUI prelude and widgets
explicitly, then `use super::*` for `App`, colors, `glass_panel`, and
`open_path`):

```rust
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::switch::Switch;
use gpui_kit::prelude::*;
use gpui_kit::*;
use super::*;
```

- `UpdatePhase` (`Idle`, `Checking`, `UpToDate`, `Available`, `Downloading`,
  `Ready`, `Applying`, `Error`) with the exact Japanese labels from the spec.
- `UpdateState { phase, install, last_checked, armed, notice, manual_check, tx, rx, health_token, database_ok, data_dir }`.
- `UpdateState::new(health_token, database_ok, demo)` calls `install_kind()` and
  `vrc_photo_core::data_dir()`.
- `take_last_result_notice()` uses `LastResult::take` and the helper above.
- `start_channel(cx)` spawns a GPUI task reading `async_channel`.
- `start_update_check(manual, cx)` spawns a `std::thread` with
  `UreqClient::new(cfg!(debug_assertions))`, `FeedConfig::from_env()`,
  `check`, and sends `UpdateMessage::Checked { manual, result }`.
- `start_update_download(cx)` downloads to
  `UpdatePaths::downloads_dir()/version/asset filename`, sends
  `Progress { received, total }` and `Downloaded`.
- `apply_update(cx)`: require the two-click arm; reject
  `daemon_active && !ipc_connected`; build `ApplyJob` with
  `std::process::id()`, the stored `daemon_pid`, `restart_args` from
  `std::env::args().skip(1)`, `ApplyJob::new_health_token()`, the asset
  signature/hash, and `current_exe`; send IPC `Shutdown` if connected via
  `cx.background_executor()`; call `start_apply`; on success set `Applying`
  and `cx.quit()`; on error show the message and stay alive.
- `start_update_timers(cx)`: return early in demo; after 15 s, run
  `maybe_auto_check`; then repeat hourly. Due when
  `now - update_last_checked_at >= 86_400`, `settings.auto_update_check`, and
  `install.automatic_check()`. On any completed attempt, set
  `update_last_checked_at` and save settings.
- On `Checked(Err)` with `manual == false`, log and return to `Idle`; do not set
  a user message. With `manual == true`, set `Error`.
- `update_card(cx)`: the Settings card per spec §6.7, using `glass_panel`,
  `Button`, `Switch`, `badge`, `meta_row`, and the existing colors. Include
  the current version (`vrc_photo_update::CURRENT_VERSION`), install label,
  auto-check switch, "今すぐ確認", progress bar, two-click apply, release-notes
  button via `open_path(Path::new(url))`, package-manager command for
  deb/pacman, and error/rollback text.
- `nav_badge()` returns `Some("更新")` only when an update is available.
- `write_health_marker()` writes `{version, mode:"normal"}` only when
  `database_ok && !demo` and a token is present.

- [ ] **Step 7: Wire the UI**

In `main.rs`:
- `mod update_view;`
- Add `daemon_pid: Option<u32>` and `update: UpdateState`.
- Change `open_database` to return `(db, demo, database_ok, message, demo_assets)`;
  `database_ok` is true only for the real `Db::open` success.
- In `App::new`, read `VRC_PHOTO_HEALTH_TOKEN`, construct `UpdateState`, set the
  last-result notice, and skip the initial daemon spawn when the token is
  present.
- On the first frame (when a token is present), call
  `complete_update_startup`: write the health marker if `database_ok`, then
  spawn the daemon unconditionally (single-instance handles a duplicate) and
  set a message on failure.
- Set `daemon_pid` in `handle_ipc_incoming` for `Connected { status, pid }`
  and clear it on disconnect.
- Add the update card after the "起動と圧縮" panel in `settings_screen`.
- Compute `let update_badge = self.update.nav_badge();` and pass it to the
  Settings `nav_item`.
- Call `start_update_timers(cx)` after IPC startup.
- Disable update checks in demo (`start_update_timers` returns early; the card
  shows "デモでは利用できません").

- [ ] **Step 8: Run formatting, checks, and tests**

Run:

```bash
cargo fmt --all
cargo test -p vrc-photo-core -p vrc-photo-ui -p vrc-photo-update
cargo check -p vrc-photo-ui
```

Expected: PASS. Fix warnings before committing.

- [ ] **Step 9: Commit**

```bash
git add crates/core/src/settings.rs crates/ui
git commit -m "feat: add update settings and UI"
```

---

### Task 10: Manifest generator, packaging, and release workflow

> Implementation note (2026-10-24): the generator and signer live in
> `crates/release-tool` and sign with the `minisign` crate instead of the
> distro CLI, so CI is self-contained and the key format is exercised by the
> same tests. See the design §6.4/§6.8 and the execution ledger ruling.

**Files:**
- Create: `crates/update/src/bin/vrc-photo-update-manifest.rs`
- Modify: `crates/update/Cargo.toml` (`clap`, `chrono` dependencies; `[[bin]]`)
- Modify: `Packager.toml`
- Modify: `.github/workflows/release.yml`
- Modify: `.github/workflows/ci.yml`
- Test: inline tests in the manifest binary (or a shared module)

**Interfaces:**
- Consumes: `Manifest`, `PlatformAsset`.
- Produces: `build_manifest(dist: &Path, tag: &str, repo: &str, notes_url: &str) -> Result<Manifest>`
- Produces: `vrc-photo-update-manifest generate --dist DIR --tag TAG --repo OWNER/REPO --notes-url URL --output PATH`

- [ ] **Step 1: Write the failing generator tests**

Create a temp dist with:
- `vrc-photo-manager_0.4.0_x86_64.AppImage` + `.minisig`,
- `vrc-photo-manager_0.4.0_x64-setup.exe` + `.minisig`.

Assert:
- Two platform entries; `linux-x86_64` format `appimage`; `windows-x86_64`
  format `nsis`; sizes and SHA-256 match; signatures are the base64 of the
  `.minisig` contents; version is `0.4.0`; URLs use the tag.
- Missing `.minisig` is an error.
- An unversioned artifact name is an error (name must contain `0.4.0`).
- Two AppImages are an error.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p vrc-photo-update --bin vrc-photo-update-manifest`
Expected: FAIL.

- [ ] **Step 3: Implement the generator**

Use `clap` derive. `build_manifest`:
- Find exactly one `*.AppImage` and one `*-setup.exe`, excluding `.minisig`.
- Require the tag version string (without `v`) in each file name.
- Read `<asset>.minisig`; base64 encode the text.
- Compute SHA-256 with `sha2` and `hex`.
- Build URLs with `url::Url::parse` on
  `https://github.com/{repo}/releases/download/{tag}/`, then
  `path_segments_mut().push(file_name)` so the file name is percent-encoded.
- `pub_date` = `chrono::Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)`.
- Write pretty JSON to `--output`.

- [ ] **Step 4: Package the helper**

In `Packager.toml`:
- `beforePackagingCommand = "cargo build --release -p vrc-photo-ui -p vrc-photo-daemon -p vrc-photo-update"`
- Add `[[binaries]] path = "vrc-photo-updater"`.

Verify locally that `cargo build --release -p vrc-photo-ui -p vrc-photo-daemon -p
vrc-photo-update` puts `vrc-photo-updater` next to the UI in `target/release`.

- [ ] **Step 5: Update the release workflow**

In the tag-only `publish` job:
- Install Rust and cache; `apt-get install -y minisign`.
- Verify the tag version against the workspace and `Packager.toml`; fail on
  mismatch.
- Require `MINISIGN_SECRET_KEY`; write it to `$RUNNER_TEMP/minisign.key` with
  `umask 077` and a cleanup trap.
- Sign every artifact (skip `*.minisig` and `latest.json*`), verify each with
  `minisign -V -H -P "$PUB"`, where `PUB` is the first `^RW` line of
  `packaging/update/keys.txt`.
- Build and run the generator to produce `dist/latest.json`.
- Sign and verify `dist/latest.json`.
- Create/upload the release with `dist/*` as today.
- Never publish if signing or verification fails.

Add `cargo test -p vrc-photo-update` and
`cargo clippy -p vrc-photo-update --all-targets -- -D warnings` to the existing
Windows CI job (keep the job id/name so branch protection stays intact).

- [ ] **Step 6: Test the generator locally**

Run:

```bash
mkdir -p /tmp/opencode/vrcpm-dist
# create dummy versioned AppImage/NSIS files and .minisig files
cargo run -p vrc-photo-update --bin vrc-photo-update-manifest -- generate \
  --dist /tmp/opencode/vrcpm-dist --tag v0.4.0 \
  --repo TommyZ-7/vrc-photo-manager \
  --notes-url https://github.com/TommyZ-7/vrc-photo-manager/releases/tag/v0.4.0 \
  --output /tmp/opencode/vrcpm-dist/latest.json
jq . /tmp/opencode/vrcpm-dist/latest.json
```

Expected: two platform entries with the dummy sizes/hashes.

- [ ] **Step 7: Commit**

```bash
git add crates/update/Cargo.toml crates/update/src/bin Packager.toml .github/workflows
git commit -m "ci: build and publish signed update manifests"
```

---

### Task 11: Documentation, manual QA, and PR

**Files:**
- Modify: `docs/release.md`, `docs/security-privacy.md`, `docs/features.md`,
  `docs/ui.md`, `docs/development.md`, `docs/testing.md`,
  `docs/troubleshooting.md`, `docs/roadmap.md`, `README.md`,
  `docs/HANDOFF.md`, `docs/architecture.md`
- Verify: `docs/adr/0012-auto-update.md` matches the final implementation

- [ ] **Step 1: Update the release documentation**

In `docs/release.md` add:
- Key generation and `MINISIGN_SECRET_KEY` registration (no secret in the repo).
- The manifest schema and where `latest.json`/`.minisig` live.
- The publish steps and the "never unsigned" rule.
- Key rotation procedure from spec §6.3.
- Manual recovery: restore `<AppImage>.previous`; restore Windows exes from
  `<data_dir>/updates/backup/<version>/`; read `updater.log` and
  `last-result.json`.
- The release checklist items for the helper binary, signatures, manifest, and
  the tag/version match.

- [ ] **Step 2: Update privacy/security, features, UI, development**

- `docs/security-privacy.md`: replace "no network" with the update-check
  exception, the exact requests, the opt-out, the signature model, and the fact
  that GitHub sees the IP/asset.
- `docs/features.md`: add an update section (check, download, verify, apply,
  rollback, deb/pacman guidance, dev/per-machine limits).
- `docs/ui.md`: add the Settings update card and nav badge; keep the existing
  §5 "自動更新" (3 s refresh) distinct by calling this "アプリのアップデート".
- `docs/development.md`: new crate, helper, `VRC_PHOTO_UPDATE_FEED`,
  `VRC_PHOTO_UPDATE_PUBLIC_KEY` (debug only), key/secret setup, and the new
  verification commands.
- `docs/architecture.md`: add the update flow (UI → feed → helper → restart).

- [ ] **Step 3: Update testing, troubleshooting, roadmap, README, HANDOFF**

- `docs/testing.md`: unit tests plus the manual smoke checklist from spec §7.
- `docs/troubleshooting.md`: update errors, rollback, manual restore, offline
  behavior, per-machine Windows and deb/pacman limits.
- `docs/roadmap.md`: mark automatic updates implemented; note the remaining
  limits (UI-only checks, no Authenticode, no deb/pacman self-update).
- `README.md`: feature bullet and the update check privacy note.
- `docs/HANDOFF.md`: current status and known limitations.

- [ ] **Step 4: Run the full local verification**

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Expected: all pass.

- [ ] **Step 5: Manual QA on Linux**

1. Build the release binaries and confirm the helper is next to the UI:
   `cargo build --release -p vrc-photo-ui -p vrc-photo-daemon -p vrc-photo-update`
   and `ls target/release/vrc-photo-*`.
2. Start the UI, open Settings, confirm the update card shows the version,
   install kind, auto-check switch, and "今すぐ確認".
3. In a debug UI, set
   `VRC_PHOTO_UPDATE_FEED=http://127.0.0.1:9/latest.json` and press
   "今すぐ確認"; the UI must stay responsive and show an error.
4. Serve a locally signed manifest with a test key and press "今すぐ確認";
   confirm "最新です" / "更新があります", the release-notes link, and that
   a tampered artifact is rejected in the unit tests.
5. If a local AppImage can be built, run the AppImage, apply a signed test
   update, and confirm the new version starts and the daemon reconnects. If the
   environment cannot build an AppImage, report that the AppImage apply test is
   pending on the PR's release artifact / a machine with `cargo-packager`.
6. Confirm no real GitHub release was created and no tag was pushed.

- [ ] **Step 6: Commit docs**

```bash
git add docs README.md
git commit -m "docs: document signed automatic updates"
```

- [ ] **Step 7: Write the PR body, push, and open the PR**

Write `/tmp/opencode/vrcpm-auto-update-pr.md` with the issue link, spec/plan
links, security model (signed manifest/artifact, key custody), test commands,
manual QA results, and known limits (Windows manual test, deb/pacman
notify-only, first signed release required, no tag created).

```bash
git push -u origin feat/auto-update
gh pr create --title "feat: 自動アップデート" --body-file /tmp/opencode/vrcpm-auto-update-pr.md
```

Monitor CI and the Release bundle with the `ci-watch` skill; merge only when
green, then update `main` and remove the worktree/branch per `AGENTS.md`.

---

## Self-review notes

- Spec §6.1 install kinds → Task 3.
- Spec §6.2 manifest → Tasks 1 and 4.
- Spec §6.3 signing/keys → Task 2.
- Spec §6.4 crate/API → Tasks 1–7.
- Spec §6.5 check/download → Tasks 4 and 5.
- Spec §6.6 helper/rollback/daemon ordering → Tasks 6, 7, and 9.
- Spec §6.7 UI/settings → Task 9.
- Spec §6.8 release workflow → Task 10.
- Spec §6.9 privacy/network → Tasks 4, 9, and 11.
- Spec §6.10 edge behavior → Tasks 5, 7, and 9 tests.
- Spec §7 testing → each task plus Task 11 manual QA.
- Spec §8 docs → Tasks 1 and 11.
- Spec §9 acceptance → Tasks 2, 5, 7, 9, and 11.
