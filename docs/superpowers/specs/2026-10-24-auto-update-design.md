# Automatic application updates: Design

- Date: 2026-10-24
- Issue: [#24 feat: 自動アップデート](https://github.com/TommyZ-7/vrc-photo-manager/issues/24)
- Status: Proposed
- Target: new `crates/update`, `crates/ui`, `crates/ipc` (PID plumbing),
  `Packager.toml`, `.github/workflows/release.yml`, docs

## 1. Intent

Updates are currently manual downloads from GitHub Releases. As the number of
package formats grows, asking every user to find, download, and install the
right artifact is both a support burden and a security problem: there is no
single, verifiable update path.

Issue #24 asks for update checking, download, signature verification,
replacement, restart, a rollback path, and an auto-check setting. It also asks
for the design to be recorded in an ADR first.

This design implements the update path in the UI, because applying an update
requires restarting the UI and the daemon, and because the update experience
(version, progress, release notes, restart confirmation) is inherently a UI
concern. The daemon continues to do background work; it is only stopped and
restarted as part of the update.

## 2. Goals

- Check a signed update feed and tell the user when a newer version exists.
- Download the correct artifact, verify its minisign signature and integrity,
  and never apply an artifact that fails verification.
- Replace the running installation on AppImage and Windows NSIS, then restart.
- Keep a previous-version backup and restore it automatically when the new
  version fails to start.
- Make automatic checks opt-out, visible in Settings, and never block the UI.
- Keep update state in the existing settings store; no DB migration.
- Produce signed artifacts and an update manifest from the tag release
  workflow.
- Document the trust model, key custody, and manual recovery.

## 3. Non-goals

- Self-updating deb and pacman installs. They are owned by the system package
  manager and would require root; the UI only tells the user how to update.
- macOS support (the app does not ship a macOS bundle).
- Delta/zsync updates, staged rollouts, beta channels, or a custom update
  server. The feed is a static manifest on GitHub Releases.
- Authenticode (Windows code signing). The installer may still trigger
  SmartScreen; that is a separate release-signing project.
- Checking for updates from the daemon while the UI is closed.
  `docs/roadmap.md` can add that later without changing the manifest or the
  apply helper.
- Downgrading to an arbitrary older version from the UI. The automatic
  rollback is a safety net for a failed update, not a version picker.
- Changes to photo, compression, or log behavior.

## 4. Current behavior and constraints

- Distributions: Linux AppImage, deb, pacman; Windows NSIS. All artifacts are
  built by `cargo packager` in `.github/workflows/release.yml`.
- There is no code signing and no network code in the application today.
  `docs/security-privacy.md` explicitly says the current implementation does
  not communicate over the network. This feature changes that statement.
- The workspace version (`[workspace.package] version`) is the single source of
  truth. `Packager.toml` duplicates it. The tag is `vX.Y.Z`.
- The UI and daemon share SQLite. The daemon is a child of the UI when the UI
  starts it, but can also run independently from autostart/tray. IPC exposes a
  `shutdown` request and the daemon PID in its `hello` response.
- `AppSettings` is a JSON blob in `app_settings`; new fields can be added with
  serde defaults, so no DB migration is needed.
- The app is a gpui application. Background work is already done on threads
  with `async-channel` handoff to GPUI tasks (IPC bridge). The update checker
  follows the same pattern.
- The AppImage runtime sets `APPIMAGE` to the path of the running AppImage.
  Replacing that file is possible while it is mounted; the old inode remains
  available to running processes.
- The Windows NSIS installer defaults to a current-user install
  (`NSISInstallerMode::CurrentUser`), which does not require administrator
  access. Per-machine installs are out of scope for automatic replacement.
- GitHub Release assets can be fetched through
  `https://github.com/<owner>/<repo>/releases/latest/download/<asset>` without
  using the API, so update checks do not consume the API rate limit.

## 5. Approaches considered

### 5.1 Scope of self-update

**A. AppImage and Windows NSIS only; notify for deb/pacman (chosen).**
These two formats are single, replaceable artifacts owned by the user. deb and
pacman are owned by the distribution package manager; silently overwriting
their files would fight the package database and require root. The UI shows a
package-manager command and a release-page link instead.

**B. All formats.** Superficially uniform, but a deb/pacman self-update would
need privilege escalation and would break package ownership. Rejected.

**C. AppImage only.** Smallest, but leaves Windows users without the update
path and weakens the point of the feature. Rejected.

### 5.2 Where update checks run

**A. UI-driven (chosen).** The UI checks on startup and then every 24 hours
while it is open, downloads in a background thread, and applies on an explicit
user action. No DB schema or IPC protocol changes. The daemon only needs to be
stopped through the existing `shutdown` request.

**B. Daemon-driven.** The daemon checks while the UI is closed, stores update
state in SQLite, and pushes IPC events. This adds a table, IPC requests/events,
and process-restart coordination, and the user still has to open the UI to
apply the update. Deferred to a later issue.

**C. External updater crate.** `cargo-packager-updater` already verifies
minisign signatures and can install AppImage/NSIS. It was evaluated. It loads
whole artifacts into memory, exits the process on Windows, and has no health
check or previous-version restore. Its rollback only covers a failed file
write. We keep the compatible `platforms` manifest shape but implement our own
smaller client so the acceptance criteria can be met.

### 5.3 Feed and signature

**A. Static signed manifest on GitHub Releases (chosen).** A `latest.json`
asset at `releases/latest/download/latest.json`, plus a detached
`latest.json.minisig`. The manifest lists one artifact per platform with its
URL, size, SHA-256, and minisign signature. Both the manifest and the artifact
are verified with the same long-lived minisign key. No API token, no API rate
limit, and the file can be cached or mirrored.

**B. GitHub Releases API.** Convenient metadata, but unauthenticated API calls
are rate-limited per IP, responses are not signed, and we would still need a
signature for the artifact. Rejected.

**C. No signature, TLS only.** Simpler, but a compromised GitHub account or
release asset could ship arbitrary code. The issue explicitly requires
signature verification. Rejected.

## 6. Design

### 6.1 Install kinds

`crates/update` classifies the running install:

| Kind | Detection | Automatic apply |
|---|---|---|
| `AppImage { path }` | `APPIMAGE` is set and points to a file | Yes |
| `WindowsCurrentUser` | Windows, the NSIS per-user uninstall key points at the running executable's directory | Yes |
| `WindowsPerMachine` | Windows, the NSIS per-machine uninstall key points at the running directory | No; manual installer guidance |
| `WindowsOther` | Windows, no matching uninstall key (development or portable copy) | No; manual check only |
| `SystemPackage { family }` | Linux, no `APPIMAGE`, executable under `/usr/bin` or `/usr/local/bin` | No; package-manager guidance |
| `Development` | Any other install (e.g. `target/debug`, extracted AppImage) | No; manual check only |

The family is `Deb` or `Pacman`, detected from `/etc/os-release`
(`ID`/`ID_LIKE`). Unknown Linux families get generic guidance.

Windows detection reads the NSIS uninstall key
`Software\Microsoft\Windows\CurrentVersion\Uninstall\VRC Photo Manager` under
`HKCU` and `HKLM` (via `winreg`), strips the quotes from `InstallLocation`, and
compares its canonicalized path with the running executable's parent directory.
This avoids offering self-update to a `cargo run` binary or a portable copy
just because it runs on Windows, and it detects per-machine installs so the UI
can explain that they need the installer instead of silently creating a second
current-user installation. If the registry is unreadable, the kind is
`WindowsOther` (fail safe, no apply).

Rules:

- `Development`, `WindowsOther`, and `WindowsPerMachine` skip automatic checks
  so contributors and portable users do not generate network traffic. A manual
  check is still allowed and shows the available version, but the apply button
  is disabled with an explanation.
- `SystemPackage` auto-checks and notifies; the card shows
  `sudo apt install --only-upgrade vrc-photo-manager` or
  `sudo pacman -Syu vrc-photo-manager` and a release-page button.
- `AppImage` checks that the target file is writable before offering apply.
  If the directory is read-only, the card explains how to move the AppImage or
  update manually.

### 6.2 Update feed and manifest

Default feed:

```text
https://github.com/TommyZ-7/vrc-photo-manager/releases/latest/download/latest.json
```

Signature: the same URL with `.minisig` appended. `VRC_PHOTO_UPDATE_FEED`
overrides the feed URL (useful for mirrors and QA). Production builds accept
only `https` URLs; debug builds additionally allow `http` for loopback
testing.

Manifest schema (version 1):

```json
{
  "schema": 1,
  "version": "0.4.0",
  "pub_date": "2026-10-24T12:00:00Z",
  "notes_url": "https://github.com/TommyZ-7/vrc-photo-manager/releases/tag/v0.4.0",
  "platforms": {
    "linux-x86_64": {
      "url": "https://github.com/.../releases/download/v0.4.0/vrc-photo-manager_0.4.0_x86_64.AppImage",
      "format": "appimage",
      "size": 12345678,
      "sha256": "64 lowercase hex characters",
      "signature": "base64 of the complete .minisig file"
    },
    "windows-x86_64": {
      "url": "https://github.com/.../releases/download/v0.4.0/vrc-photo-manager_0.4.0_x64-setup.exe",
      "format": "nsis",
      "size": 12345678,
      "sha256": "64 lowercase hex characters",
      "signature": "base64 of the complete .minisig file"
    }
  }
}
```

Validation before the version comparison:

- The manifest bytes are capped at 512 KiB and the signature at 16 KiB.
- The detached manifest signature is verified over the exact manifest bytes
  **before** JSON parsing.
- `schema` must be 1; unknown values fail closed with a clear error.
- `version` must parse as semver (leading `v` is accepted).
- Platform keys are `linux-x86_64` and `windows-x86_64`.
- `format` must match the entry (`appimage` / `nsis`).
- `size` must be greater than 0 and at most 300 MiB.
- `sha256` must be 64 hex characters.
- `signature` must decode as base64 and then as a minisign signature.
- `url` must use https (http only for debug loopback), and the file name taken
  from the URL must be a safe single path component.
- `notes_url` is optional and must be https (or debug http).

A missing manifest (HTTP 404) means "this release predates the update
feature"; it is treated as up to date, not as an error.

The client does not try to read a version out of the AppImage or NSIS binary;
that is platform-specific and would require running or parsing the artifact
before verification. Instead, the release manifest generator requires the
selected artifact's file name to contain the manifest version. The signature
then binds the exact bytes to the signed manifest entry. A release with a
mismatched artifact name fails generation; there is no runtime guess.

### 6.3 Signing and key management

- minisign (Ed25519, prehashed). The manifest and every published artifact get
  a detached `.minisig` signature.
- Public keys live in `packaging/update/keys.txt`, one base64 key per line,
  with `#` comments. `crates/update` embeds the file with `include_str!` and
  accepts a signature from any listed key. This supports rotation without a
  flag day.
- The secret key is a passwordless minisign secret key stored only as the
  GitHub Actions repository secret `MINISIGN_SECRET_KEY`, used only by the
  tag-only `publish` job. It is never committed and never exported by the app.
- The release workflow fails closed if the secret is missing; it never
  publishes an unsigned update.
- Rotation procedure (documented in `docs/release.md`):
  1. generate a new key and add its public key to `keys.txt`;
  2. release that version signed by the old key;
  3. switch the secret to the new key in the next release;
  4. remove the old public key after one release cycle.
  If the old key is compromised, skip steps 2–4 and announce a manual download.
- The test suite uses a key pair generated at test time with the `minisign`
  crate (dev-dependency) and verifies it with `minisign-verify`. The production
  key never appears in test code.

### 6.4 Crate and module layout

Add `crates/update` (`vrc-photo-update`), which depends on `ureq` (default
rustls), `minisign-verify`, `semver`, `serde`, `serde_json`, `sha2`, `base64`,
`thiserror`, `tracing`, and `chrono`. It does not depend on `core`, `ui`, or
`daemon`.

```text
crates/update/src/
  lib.rs          public API, version constant, key list
  manifest.rs     schema types, validation, platform asset selection
  fetch.rs        ureq client behind an HttpClient trait
  verify.rs       minisign verification (manifest and streaming artifacts)
  download.rs     .part download, size/hash/signature checks, progress
  install.rs      install-kind detection, update directory, apply job model
  helper.rs       apply/rollback engine used by the helper binary
  bin/vrc-photo-updater.rs           apply helper
```

The UI depends on the library; the helper binary is not included in the UI
binary. `Packager.toml` adds `[[binaries]] path = "vrc-photo-updater"` and the
packaging command builds `-p vrc-photo-update` so the helper ships in every
package.

The release-time manifest generator and signer live in a separate
`crates/release-tool` (`vrc-photo-release-tool`, binary
`vrc-photo-update-manifest`). It depends on the update crate for the manifest
types and on the `minisign` crate for signing, is used only by CI, and is not
packaged. Keeping it out of `crates/update` keeps clap and the signing key
code out of the shipped binaries.

The public library API:

```rust
pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

pub fn check(client: &dyn HttpClient, feed: &FeedConfig, current: &Version)
    -> Result<Option<AvailableUpdate>>;

pub fn download(
    client: &dyn HttpClient,
    update: &AvailableUpdate,
    dest: &Path,
    progress: &mut dyn FnMut(u64, Option<u64>),
    cancel: &AtomicBool,
) -> Result<PathBuf>;

pub fn start_apply(job: &ApplyJob) -> Result<()>; // copies the helper, writes the job, spawns it
pub fn run_helper(job_path: &Path) -> Result<()>;
```

`HttpClient` is a small trait with `get_bytes` (capped) and `download_to`
(streaming); tests use a fake implementation. The production implementation
sets a 15 s connect timeout, a 60 s global timeout for small responses, and a
10 min body budget for artifact downloads.

### 6.5 Check, download, and verification flow

1. Fetch `latest.json` (≤ 512 KiB) and `latest.json.minisig` (≤ 16 KiB).
2. Verify the detached signature over the manifest bytes with any embedded
   public key. Abort on failure.
3. Parse and validate the manifest.
4. Compare `version > CURRENT_VERSION` using semver. If not newer, report
   "up to date".
5. Select the platform entry for AppImage/Windows. For SystemPackage, the
   version is enough; the card only needs release metadata.
6. Return `AvailableUpdate { version, notes_url, asset }`.
7. On user confirmation, download the asset into
   `<data_dir>/updates/downloads/<version>/<asset-name>.part`:
   - stop at `size + 1` bytes and fail if the manifest understated the size;
   - compute SHA-256 while streaming;
   - stream the minisign signature verification with `minisign-verify`'s
     `StreamVerifier` (prehashed signatures);
   - compare the final size and SHA-256 and finalize the signature;
   - fsync and rename to the final `.part`-free name.
8. On any failure, delete the partial file and keep the current version. The
   UI shows the error; automatic checks only log it.

Cancellation is a flag checked per chunk; the UI may expose a cancel action
later. The download thread never touches the UI thread.

### 6.6 Apply helper and rollback

The UI cannot replace itself reliably on Windows (the executable is locked)
and cannot guarantee a rollback if the new AppImage crashes. A separate helper
process handles both:

1. Before exiting, the UI copies `vrc-photo-updater` from the app directory to
   `<data_dir>/updates/bin/` (mode 0700) and writes an atomic job file.
2. The helper waits for the UI PID and the daemon PID (if any) to exit, up to
   30 s. If they do not exit, it aborts, relaunches the UI, and records that
   the update was cancelled.
3. The helper re-verifies the staged artifact's signature and SHA-256 from the
   job file. A tampered or incomplete download is never applied.
4. A lock file `<data_dir>/updates/apply.lock` prevents concurrent applies.
   A stale lock is removed only when its PID is no longer alive.

Job file (mode 0600, JSON):

```json
{
  "schema": 1,
  "kind": "appimage",
  "from_version": "0.3.0",
  "to_version": "0.4.0",
  "target": "/home/me/Applications/VRC Photo Manager.AppImage",
  "staged": "/home/me/.local/share/vrc-photo-manager/updates/downloads/0.4.0/vrc-photo-manager_0.4.0_x86_64.AppImage",
  "signature": "base64 minisign signature",
  "sha256": "…",
  "ui_pid": 1234,
  "daemon_pid": 5678,
  "restart_args": [],
  "health_token": "1729…-1234",
  "created_at": "2026-10-24T12:00:00Z"
}
```

AppImage apply:

- Resolve the target (canonicalize symlinks) and make sure the parent directory
  is writable.
- Rename the current target to `<target>.previous` (same directory, same
  filesystem).
- Copy the staged file to `<target>`, preserve mode and set the executable bit,
  fsync the file and directory.
- On any error, rename `.previous` back.
- On success, keep `.previous` for manual recovery.

Windows apply:

- Copy the current `vrc-photo-manager.exe` and `vrc-photo-daemon.exe` into
  `<data_dir>/updates/backup/<from_version>/`.
- Run the staged NSIS installer with `/S` (current-user install, no admin
  prompt). Wait for the installer process, with a 10 minute timeout.
- On a non-zero exit or a missing executable, restore the two backed-up
  executables.
- Keep the backup directory after a successful update.

Restart and health check (both kinds):

- Launch the new target with the original UI arguments plus
  `VRC_PHOTO_HEALTH_TOKEN=<token>`. `restart_args` is
  `std::env::args_os().skip(1)` captured before exiting; `--demo` is never
  updateable, so it cannot appear here in a real apply.
- On the first frame, the new UI writes
  `<data_dir>/updates/health-<token>.json` containing its own
  `CURRENT_VERSION` and `"mode": "normal"`. The marker is written only when the
  real database opened successfully and the UI is **not** in the demo fallback
  that `open_database` uses after a DB/migration error. A DB failure suppresses
  the marker so the helper rolls back.
- The helper waits up to 45 s for the marker and compares the version. If the
  child exits early, the marker is missing, `mode` is not `normal`, or the
  version is wrong, it kills the child if needed, restores the backup, and
  relaunches the old version.
- Immediately before launching the new version, the helper writes
  `<data_dir>/updates/last-result.json` atomically
  (`{"status":"success","from":…,"to":…}`). If the health check fails, it
  overwrites the file with `{"status":"rolled_back","from":…,"to":…,"reason":…}`
  before relaunching the old version. The UI reads it once on startup and
  displays the result. A success result is only shown when the running version
  matches `to`; a rollback result is only shown when it matches `from`.
- The helper appends to `<data_dir>/updates/updater.log` (rotated at 1 MiB).
  On a rollback it deletes the staged file and job; on success it does the
  same. If even the restore fails, it leaves the backup, job, and log in place
  and writes a `failed` result with the paths.

The UI sends the existing IPC `shutdown` request before spawning the helper. If
the daemon is active but IPC is disconnected, apply is disabled with
"デーモンに接続できないため更新できません" so the helper is not left waiting
for a process it cannot stop.

#### Daemon lifecycle during apply

The daemon must not hold old binaries or mount points while the helper is
replacing them, and a failed update must not leave a new daemon running against
an old UI. The ordering is:

1. The UI requires either "daemon stopped" or "daemon connected". In the
   connected case it sends `shutdown` and includes the daemon PID from the IPC
   `hello` response in the job.
2. The helper waits for the UI and that daemon PID to exit before touching any
   files. If either is still alive after 30 s, the helper aborts, relaunches
   the old UI, and records a cancellation. It never overwrites files held by a
   running process.
3. When the new UI starts with `VRC_PHOTO_HEALTH_TOKEN`, it **defers the
   daemon auto-spawn** until after it has written the health marker. The
   existing 3 s refresh and IPC reconnect loop still start normally. This means
   a health failure cannot leave a daemon started by the new version holding
   files that the helper needs to restore.
4. On success, the helper returns, the new UI spawns the daemon with the normal
   existing logic, and IPC reconnects. On rollback, the old UI is relaunched
   and follows the normal startup path; no new daemon was created before the
   failure.
5. Health is a UI-level check (real DB open + first frame), not a daemon
   liveness check. A daemon that fails to start after a successful UI start is
   surfaced by the existing daemon cards and does not trigger a rollback. This
   avoids rolling back a working application because of an unrelated daemon or
   IPC problem.
6. A systemd-managed daemon only exists on deb/pacman, where automatic apply is
   disabled. Autostart during the short apply window is an accepted edge case;
   the helper's PID wait and backup guard still prevent file corruption.

### 6.7 UI and settings

`crates/ui/src/update_view.rs` owns the update UI and background handoff;
`main.rs` only wires it into the app. It follows the existing IPC pattern: a
worker thread sends `UpdateMessage`s over `async-channel` to a GPUI task.

Settings card "アップデート" (below "起動と圧縮"):

- Current version and install kind (`AppImage` / `Windows` / `deb` / `pacman` /
  development build).
- Switch "起動中に24時間ごとに確認" bound to `auto_update_check`
  (default on).
- "今すぐ確認" button, disabled while checking or downloading.
- Last checked time and last result.
- When a newer version exists: version, "リリースノート" link, and either
  "ダウンロード" (AppImage/Windows) or package-manager guidance.
- During download: a progress bar with received/total bytes and percent.
- After verification: "ダウンロードと署名検証が完了しました" and a two-click
  "更新して再起動" action (first click arms, second applies).
- While applying: "再起動しています…"; the UI exits immediately after the
  helper starts.
- Errors: verification failure, network failure, unwritable AppImage, missing
  helper, daemon cannot be stopped, and rollback notice.

Additional UI:

- The Settings navigation item shows a "更新" badge while an update is known.
- An automatic check that finds a newer version sets a non-destructive header
  message ("アップデート vX があります。設定 → アップデートから更新できます").
- Manual checks show errors; automatic checks fail silently in the UI and only
  log, so an offline machine is never nagged.
- `daemon_pid` is added to the UI's IPC bridge. `Subscription` already receives
  the daemon PID in the `hello` response; the bridge exposes it to the app.
  This is a Rust API change, not a protocol change.
- `AppSettings` gains:
  - `auto_update_check: bool` (default `true`);
  - `update_last_checked_at: Option<i64>` (Unix seconds, set after an attempt).
  Both use serde defaults, so existing settings load unchanged.

Timing:

- The first automatic check runs 15 s after startup if
  `now - update_last_checked_at >= 24 h`.
- A GPUI timer wakes hourly and performs the same due check, so a long-running
  app still checks daily.
- A check is `Development`-skipped; manual checks are always allowed.
- The update thread is detached; dropping the receiver ends it. The UI never
  waits on the network.

### 6.8 Release workflow and manifest generation

The Windows and Linux build jobs keep building unsigned packages on PRs. The
tag-only `publish` job:

1. Downloads both artifact sets and builds `vrc-photo-release-tool`.
2. Verifies that the tag version (`v` stripped) matches the workspace version
   and `Packager.toml`; fails otherwise.
3. Requires `MINISIGN_SECRET_KEY`; writes it to a `0700` temp file with a
   cleanup trap.
4. Signs every release artifact with the release tool's minisign signer
   (prehashed), producing `<asset>.minisig`; verifies each with the same
   tool's `minisign-verify` path.
5. Runs `vrc-photo-update-manifest generate` to build `dist/latest.json` from
   the AppImage and NSIS artifacts, their `.minisig` files, sizes, and SHA-256
   values. The generator rejects missing/duplicate artifacts and artifact file
   names that do not contain the tag version.
6. Signs `latest.json` and verifies the signature.
7. Creates or updates the GitHub Release with all artifacts, `.minisig` files,
   and `latest.json`.
8. Fails the job if any step fails; never publishes an unsigned update.

The manifest generator reuses `manifest.rs`, so the client and release tool
cannot drift. It is not shipped in the app.

### 6.9 Network and privacy

- The only outbound traffic is an HTTPS GET of the manifest, its signature, and
  (after the user starts a download) the artifact. No telemetry, no unique
  identifiers, no query parameters with user data.
- The User-Agent is `vrc-photo-manager/<version>`.
- GitHub sees the user's IP address and the requested release asset, which is
  inherent to downloading over HTTPS. This is documented in
  `docs/security-privacy.md`.
- Automatic checking can be disabled in Settings; manual checks still work.
- Downloads are accepted only from the signed manifest; there is no
  "download from an arbitrary URL" UI.
- The feed URL may be overridden with `VRC_PHOTO_UPDATE_FEED` (mirrors/QA).
  The signature and hash requirements are unchanged.
- Debug builds (`cfg!(debug_assertions)`) accept
  `VRC_PHOTO_UPDATE_PUBLIC_KEY`, which replaces the embedded key list for
  check/download UI testing. Release builds ignore the variable entirely. An
  end-to-end AppImage test that needs to apply an update instead adds the test
  public key to `packaging/update/keys.txt` in a scratch build; that change is
  never committed.

### 6.10 Error and edge behavior

| Case | Behavior |
|---|---|
| Manifest 404 | Treat as up to date (release predates the feature); log |
| Manifest/signature invalid | Abort; manual check shows a verification error; automatic check logs |
| Version equal/older | "最新です" |
| No artifact for this platform | Version can still be shown for system packages; AppImage/Windows cannot apply |
| Offline/timeout | Bounded timeout; UI never blocks; automatic check silent, manual shows an error |
| Disk full / write error | Delete partial file; show error |
| AppImage target unwritable | Disable apply with guidance; no download started |
| Missing helper | Refuse to apply; show an error (development or broken package) |
| Daemon active, IPC disconnected | Refuse to apply; ask the user to stop the daemon |
| Daemon does not exit | Helper aborts, relaunches the UI, records the cancellation |
| New version fails to start | Helper restores the backup and relaunches the old version |
| Helper crashes mid-apply | RAII guard restores the backup on normal errors; a hard kill leaves `.previous` and the job/log for manual recovery (documented) |
| DB migration in the new version | A demo fallback or DB-open failure suppresses the health marker, so the helper rolls back; downgrade is still best-effort because the old binary may not understand a newer schema (already documented in `docs/release.md`) |
| Concurrent apply | `apply.lock` with a live PID prevents it; a stale lock is reclaimed |

## 7. Testing

Unit tests in `crates/update`:

- Manifest parsing: valid schema, missing fields, unknown schema, bad version,
  bad size, bad hash, unsafe file name, missing platform.
- Platform selection for AppImage, Windows, and system packages.
- Semver comparison including prerelease behavior.
- Signature verification: a key pair generated with the `minisign` crate signs
  a manifest and an artifact; tampering either fails; trying the wrong key
  fails; legacy signatures are rejected by the streaming path.
- Download: size cap, SHA-256 mismatch, signature mismatch, cancellation,
  partial-file cleanup, progress callbacks.
- Install-kind detection with injected paths/environment, including
  table-driven Windows current-user / per-machine / no-registry cases.
- Manifest-generator artifact selection: versioned file names are accepted,
  missing or duplicate artifacts and unversioned names are rejected.
- Job serialization and path validation.
- Helper backup/restore with temp directories: successful AppImage swap, copy
  failure restores the old file, missing staged file leaves the target intact,
  rollback result file contents.
- Release tool: manifest generation for both platforms, missing/duplicate/
  unversioned artifact rejection, signing a file and verifying it with the
  app's verification path.
- HTTP client: a local `TcpListener` server serves a manifest and an artifact
  over loopback; a server that accepts but never responds times out.

UI tests (pure helpers): progress text/percentage, status labels, result
message selection, and settings defaults.

Manual smoke tests (documented in `docs/testing.md`):

1. Build two AppImages with different versions, add a test public key to
   `packaging/update/keys.txt` in the scratch build, and sign a local test feed
   with the matching test secret key. A debug build can check/download the same
   feed with `VRC_PHOTO_UPDATE_PUBLIC_KEY`, but apply is disabled outside a real
   AppImage.
2. Check for updates, confirm the release notes link, download, verify the
   signature, apply, and confirm the new version starts and the daemon
   reconnects.
3. Repeat with a deliberately corrupted artifact and manifest; confirm the
   update is rejected and the current version still runs.
4. Simulate a new version that exits immediately; confirm the helper restores
   the previous AppImage and the UI comes back.
5. Disconnect the network; confirm the UI does not freeze and manual check
   shows an error.
6. On Windows, install vN, update to vN+1 over the current-user NSIS install,
   confirm the daemon stops/restarts and the app starts; repeat with the
   installer replaced by a file that exits without installing, and confirm the
   rollback.
7. deb/pacman: confirm the card shows the package-manager command and no apply
   button.

Pre-push verification remains:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

The `Release` workflow on the PR proves the new helper and manifest tool are
packaged and the bundle stays buildable.

## 8. Documentation

- Add `docs/adr/0012-auto-update.md` with the decision, consequences,
  alternatives, trust model, and rollback limits.
- Update `docs/release.md`: key generation, `MINISIGN_SECRET_KEY`, manifest
  schema, signing steps, verification commands, key rotation, publication
  checklist, and manual recovery.
- Update `docs/security-privacy.md`: the network section changes from "no
  communication" to "update checks only", with data sent, opt-out, and the
  signature model.
- Update `docs/features.md` and `docs/ui.md` (avoid the existing "自動更新"
  term for the 3 s refresh; call this "アプリのアップデート").
- Update `docs/development.md` (crate, helper, env vars), `docs/testing.md`
  (manual update checks), `docs/troubleshooting.md` (rollback/manual restore),
  `docs/roadmap.md` (feature status and remaining limits), `README.md`, and
  `docs/HANDOFF.md`.
- Add `packaging/update/keys.txt` with the production public key(s), and
  `.gitignore` entries for secret keys and local update artifacts.

## 9. Acceptance mapping

| Issue acceptance criterion | Implementation |
|---|---|
| 旧バージョンから新バージョンへ更新できる | Signed `latest.json` + AppImage/NSIS apply helper; manual smoke test with two versions |
| 署名不正時は適用されない | Manifest signature checked before parsing; artifact signature + SHA-256 checked before download completes and again before apply |
| 更新失敗時に起動不能にならない | Previous-version backup, health marker, automatic restore and relaunch; `.previous`/backup documented for manual recovery |
| オフライン時にエラーで固まらない | Background thread, bounded connect/global/body timeouts, silent automatic failures, explicit manual error |
| 設定で自動確認のON/OFF | `auto_update_check` setting and Settings switch |
| 失敗時に現行バージョンへ戻せる | Helper restores the previous AppImage or Windows executables on launch failure |

## 10. Risks and open items

- **Key custody is the trust root.** A leaked `MINISIGN_SECRET_KEY` allows
  arbitrary signed updates. The secret is restricted to the tag-only publish
  job, never printed, and removed with a trap. A protected GitHub environment
  with required reviewers is a recommended follow-up.
- **Windows code signing is still absent.** The minisign signature protects the
  update path, but SmartScreen may warn about the downloaded installer.
  Authenticode remains a separate item in `docs/roadmap.md`.
- **Per-machine Windows installs** are not supported for automatic apply;
  current-user installs (the cargo-packager default) are. The Windows rollback
  restores the previous executables, not installer registry metadata; a later
  update or repair normalizes that metadata.
- **AppImage must be writable.** Users who keep it in a read-only location get
  guidance instead of a broken update.
- **Rollback after a DB migration** is best-effort; the existing release
  policy already says downgrades are not guaranteed.
- **The first signed release** must be produced after this feature merges;
  users on older releases cannot be auto-updated because those versions have
  no updater. The release notes should say so.
- **Version duplication** in `Packager.toml` is a known hazard; the release
  workflow checks it against the tag before publishing.
- **UI-only checks** mean a user who never opens the UI is never notified.
  Daemon-side checks are a future enhancement.
- **GitHub outage/rate limiting**: the static download path is not the API,
  but GitHub availability is still a dependency; failures are non-fatal.

## 11. Decisions made while writing this (please check)

- Automatic checks default **on** (`auto_update_check = true`), because a
  security update that users never see is not useful. The switch is in
  Settings, and the privacy document explains the request.
- The update UI lives in **Settings**, with a navigation badge and a header
  message rather than a modal.
- The helper is shipped as a **separate binary** (`vrc-photo-updater`) rather
  than a hidden mode of the UI, so it can survive the AppImage mount and
  Windows file locks.
- **deb/pacman are notify-only.** Their files are owned by the package manager.
- The signing key is **generated once during implementation and registered as
  the repository secret `MINISIGN_SECRET_KEY`** (with the user's approval),
  while only the public key is committed.
