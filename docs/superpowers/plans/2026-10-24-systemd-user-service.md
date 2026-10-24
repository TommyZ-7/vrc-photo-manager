# systemd User Service Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** deb / pacman 利用者が任意で systemd ユーザーサービスとしてデーモンを常駐できるようにする。

**Architecture:** `vrc-photo-daemon.service` をパッケージの `/usr/lib/systemd/user/` に同梱し、`graphical-session.target` に紐付ける。既存の XDG 自動起動は残し、`single-instance` で重複を防ぐ。デーモンは `INVOCATION_ID` で systemd 管理下を判定して DB に記録し、UI は稼働状態と一緒に表示する。

**Tech Stack:** Rust (edition 2024), gpui-kit 0.8, SQLite, cargo-packager 0.11.8, systemd user units

**Spec:** `docs/superpowers/specs/2026-10-24-systemd-user-service-design.md`

## Global Constraints

- systemd を必須にしない。既存の XDG 自動起動と AppImage の `$APPIMAGE --background` を残す。
- インストール時にサービスを自動 enable しない。
- unit の配置先は `usr/lib/systemd/user/vrc-photo-daemon.service`。
- unit は `/usr/bin/vrc-photo-daemon` を実行し、トレイを無効化しない。
- 二重起動防止は既存の `single-instance` を変更しない。
- supervisor 判定のために新しいランタイム依存クレートを追加しない。
- ドキュメントは日本語で書く。
- コミット接頭辞は `feat:`, `fix:`, `docs:`, `ci:`, `chore:` のいずれか。

## Review Focus

- **既存デーモンが動いている状態で `enable --now` する:** サービスは単一インスタンス判定で即終了し inactive になる。利用者には「次のログインから有効」と案内し、テストで重複起動が失敗しないことを確認する。
- **有効化したままアンインストールする:** unit ファイルはパッケージが削除するが、`~/.config/systemd/user/graphical-session.target.wants/` のリンクは残る。先に `systemctl --user disable --now` する手順をドキュメントに固定する。
- **`graphical-session.target` を管理しないデスクトップ環境:** ログイン時に起動しない。troubleshooting に確認方法と `default.target` への代替を書く。
- **デーモン停止後に古い `daemon_supervisor` が残る:** UI はハートビートが古ければ「デーモン停止」を優先し、systemd 表示を出さない。`daemon_sync_label` のテストで固定する。
- **パッケージ内の unit パスがずれる:** `cargo-packager` の `files` はパッケージルート基準。TOML の値をテストし、Release CI で deb / pacman の両方を検査する。

---

### Task 1: Daemon supervisor detection

**Files:**
- Modify: `crates/daemon/src/lib.rs`
- Test: `crates/daemon/src/lib.rs` (`mod tests`)

**Interfaces:**
- Consumes: `std::env::var_os("INVOCATION_ID")`
- Produces:
  - `fn supervisor_label(invocation_id: Option<&std::ffi::OsStr>) -> &'static str`
  - `fn Daemon::record_supervisor(&self)`
  - `app_settings.daemon_supervisor` values `"systemd"` / `"app"`

- [ ] **Step 1: Write the failing test**

Add this test at the end of the existing `mod tests` in `crates/daemon/src/lib.rs`:

```rust
#[test]
fn supervisor_label_detects_systemd() {
    assert_eq!(
        supervisor_label(Some(std::ffi::OsStr::new("invocation-id"))),
        "systemd"
    );
    assert_eq!(supervisor_label(None), "app");
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p vrc-photo-daemon supervisor_label_detects_systemd`

Expected: compile error `cannot find function supervisor_label in this scope`.

- [ ] **Step 3: Implement detection and record it on startup**

Add this free function near `error_signature` in `crates/daemon/src/lib.rs`:

```rust
fn supervisor_label(invocation_id: Option<&std::ffi::OsStr>) -> &'static str {
    if invocation_id.is_some() {
        "systemd"
    } else {
        "app"
    }
}
```

Add this method next to `write_heartbeat` in the `impl Daemon` block:

```rust
fn record_supervisor(&self) {
    let supervisor = supervisor_label(std::env::var_os("INVOCATION_ID").as_deref());
    let db = self.db.lock().unwrap();
    if let Err(error) = db.set_setting("daemon_supervisor", supervisor) {
        tracing::warn!(%error, "cannot record daemon supervisor");
    }
}
```

Call `self.record_supervisor();` as the first statement of `pub fn run(&mut self) -> Result<()>`.

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p vrc-photo-daemon supervisor_label_detects_systemd`

Expected: `test result: ok. 1 passed`.

- [ ] **Step 5: Commit**

```bash
git add crates/daemon/src/lib.rs
git commit -m "feat: record daemon supervisor"
```

---

### Task 2: UI shows the systemd supervisor

**Files:**
- Modify: `crates/ui/src/main.rs`
- Test: `crates/ui/src/main.rs` (`mod tests`)

**Interfaces:**
- Consumes: `app_settings.daemon_supervisor` (`"systemd"` / `"app"`)
- Produces:
  - `fn daemon_sync_label(age_secs: Option<i64>, supervisor: Option<&str>) -> String`
  - `fn autostart_description(daemon_active: bool, supervisor: Option<&str>) -> &'static str`
  - `App.daemon_supervisor: Option<String>`

- [ ] **Step 1: Write the failing tests**

Add these tests to the existing `mod tests` in `crates/ui/src/main.rs`, and add `autostart_description`, `daemon_sync_label` to the `use super::{...}` list:

```rust
#[test]
fn daemon_sync_label_includes_systemd_supervisor() {
    assert_eq!(
        daemon_sync_label(Some(5), Some("systemd")),
        "最終同期 5秒前 · systemd"
    );
    assert_eq!(daemon_sync_label(Some(5), Some("app")), "最終同期 5秒前");
    assert_eq!(daemon_sync_label(Some(30), Some("systemd")), "デーモン停止");
    assert_eq!(daemon_sync_label(None, None), "デーモン停止");
}

#[test]
fn autostart_description_mentions_systemd_only_while_active() {
    assert_eq!(
        autostart_description(true, Some("systemd")),
        "systemd 管理で起動中。サービス利用時はOFF推奨"
    );
    assert_eq!(
        autostart_description(false, Some("systemd")),
        "OSログイン時にデーモンを起動"
    );
    assert_eq!(
        autostart_description(true, Some("app")),
        "OSログイン時にデーモンを起動"
    );
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run:

```bash
cargo test -p vrc-photo-ui daemon_sync_label
cargo test -p vrc-photo-ui autostart_description
```

Expected: compile errors for the missing functions.

- [ ] **Step 3: Implement the helpers, field, read, and display**

Add these helpers next to the other free helper functions near the bottom of `crates/ui/src/main.rs`:

```rust
fn daemon_sync_label(age_secs: Option<i64>, supervisor: Option<&str>) -> String {
    match age_secs.filter(|age| *age < 20) {
        Some(age) if supervisor == Some("systemd") => {
            format!("最終同期 {age}秒前 · systemd")
        }
        Some(age) => format!("最終同期 {age}秒前"),
        None => "デーモン停止".to_string(),
    }
}

fn autostart_description(daemon_active: bool, supervisor: Option<&str>) -> &'static str {
    if daemon_active && supervisor == Some("systemd") {
        "systemd 管理で起動中。サービス利用時はOFF推奨"
    } else {
        "OSログイン時にデーモンを起動"
    }
}
```

At the start of `settings_screen`, add:

```rust
let autostart_hint = autostart_description(self.daemon_active, self.daemon_supervisor.as_deref());
```

Pass `autostart_hint` instead of the literal `"OSログイン時にデーモンを起動"` in the `setting_switch` call.

Add the field next to `daemon_age_secs` in `struct App`:

```rust
daemon_supervisor: Option<String>,
```

Initialize it to `None` where `daemon_age_secs: None` is initialized.

In `refresh_monitor`, after reading `daemon_heartbeat`, add:

```rust
self.daemon_supervisor = self.db.get_setting("daemon_supervisor").ok().flatten();
```

In `photos_header`, replace the existing `sync` branch with:

```rust
let sync = if self.demo_assets.is_some() {
    "デモデータ".to_string()
} else {
    daemon_sync_label(self.daemon_age_secs, self.daemon_supervisor.as_deref())
};
```

- [ ] **Step 4: Run the tests to verify they pass**

Run:

```bash
cargo test -p vrc-photo-ui daemon_sync_label
cargo test -p vrc-photo-ui autostart_description
```

Expected: both tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/ui/src/main.rs
git commit -m "feat: show systemd supervisor in UI"
```

---

### Task 3: Package the systemd user unit

**Files:**
- Create: `packaging/systemd/vrc-photo-daemon.service`
- Modify: `Packager.toml`

**Interfaces:**
- Produces: package path `usr/lib/systemd/user/vrc-photo-daemon.service` in deb and pacman.

- [ ] **Step 1: Write the failing packaging check**

Run:

```bash
python3 - <<'PY'
import tomllib
with open("Packager.toml", "rb") as f:
    config = tomllib.load(f)
target = "usr/lib/systemd/user/vrc-photo-daemon.service"
source = "packaging/systemd/vrc-photo-daemon.service"
assert config.get("deb", {}).get("files", {}).get(source) == target
assert config.get("pacman", {}).get("files", {}).get(source) == target
print("Packager.toml ok")
PY
```

Expected: `AssertionError` because the mappings do not exist yet.

- [ ] **Step 2: Create the unit file**

Create `packaging/systemd/vrc-photo-daemon.service` with exactly:

```ini
[Unit]
Description=VRC Photo Manager background daemon
Documentation=https://github.com/TommyZ-7/vrc-photo-manager/blob/main/docs/release.md
PartOf=graphical-session.target
After=graphical-session.target

[Service]
Type=simple
ExecStart=/usr/bin/vrc-photo-daemon
Restart=on-failure
RestartSec=5s

[Install]
WantedBy=graphical-session.target
```

- [ ] **Step 3: Add the package mappings**

Add after the `[deb]` block in `Packager.toml`:

```toml
[deb.files]
"packaging/systemd/vrc-photo-daemon.service" = "usr/lib/systemd/user/vrc-photo-daemon.service"
```

Add after the `[pacman]` block:

```toml
[pacman.files]
"packaging/systemd/vrc-photo-daemon.service" = "usr/lib/systemd/user/vrc-photo-daemon.service"
```

- [ ] **Step 4: Verify the TOML and unit syntax**

Run the packaging check from Step 1 again.

Expected: `Packager.toml ok`.

Run:

```bash
mkdir -p /tmp/opencode
sed 's#^ExecStart=.*#ExecStart=/bin/true#' \
  packaging/systemd/vrc-photo-daemon.service > /tmp/opencode/vrc-photo-daemon.service
systemd-analyze --user verify /tmp/opencode/vrc-photo-daemon.service
```

Expected: exit code 0 (warnings about the substitute binary are acceptable).

- [ ] **Step 5: Commit**

```bash
git add Packager.toml packaging/systemd/vrc-photo-daemon.service
git commit -m "feat: package systemd user unit"
```

---

### Task 4: ADR and user documentation

**Files:**
- Create: `docs/adr/0010-systemd-user-service.md`
- Modify: `docs/adr/README.md`
- Modify: `docs/release.md`
- Modify: `docs/roadmap.md`
- Modify: `docs/troubleshooting.md`
- Modify: `docs/features.md`
- Modify: `docs/architecture.md`
- Modify: `docs/data-model.md`
- Modify: `packaging/README.md`
- Modify: `README.md`
- Modify: `docs/HANDOFF.md`
- Include: `docs/superpowers/specs/2026-10-24-systemd-user-service-design.md`
- Include: `docs/superpowers/plans/2026-10-24-systemd-user-service.md`

**Interfaces:**
- Consumes: unit path, service name, supervisor setting from Tasks 1-3.
- Produces: documented enable, logs, overlap behavior, and uninstall procedure.

- [ ] **Step 1: Write ADR-0010**

Create `docs/adr/0010-systemd-user-service.md` with:

- Status: `Accepted`, Date: `2026-10-24`
- Context: deb/pacman はアプリ管理の XDG 自動起動のみで、再起動制御と journald がない。
- Decision:
  - `vrc-photo-daemon.service` を `/usr/lib/systemd/user/` に同梱する。
  - opt-in とし、インストール時に enable しない。
  - `graphical-session.target` に紐付ける。
  - 既存の XDG 自動起動を残し、`single-instance` で重複を防ぐ。
  - `INVOCATION_ID` で systemd 管理下を判定し、`daemon_supervisor` に記録する。
  - アンインストール前に `systemctl --user disable --now` する。
- Consequences: systemd 利用者は再起動制御と journald を得る。非 systemd 環境と AppImage は従来どおり。
- Alternatives: systemd を既定化して XDG を廃止する案、提供しない案。

Add a row to the table in `docs/adr/README.md`:

```markdown
| [0010](0010-systemd-user-service.md) | deb/pacmanはsystemdユーザーサービスを任意提供する | Accepted |
```

- [ ] **Step 2: Update release and packaging docs**

In `docs/release.md`:

- Add `/usr/lib/systemd/user/vrc-photo-daemon.service` to the Debian/pacman contents table.
- Add `[deb.files]` / `[pacman.files]` to the `Packager.toml` settings list.
- In section 4, state that deb/pacman ship the optional unit, that it is not enabled by install, and that the in-app autostart remains.
- Add a subsection with these exact commands:

```bash
systemctl --user daemon-reload
systemctl --user enable --now vrc-photo-daemon.service
systemctl --user status vrc-photo-daemon.service
journalctl --user -u vrc-photo-daemon.service -f
systemctl --user disable --now vrc-photo-daemon.service
```

- Explain that the package removes the unit, but the enable symlink lives in the user's home, so disable before uninstalling.
- Add a release checklist item for the unit in deb and pacman.
- Remove `systemdユーザーサービス` from section 10 `未対応`.

In `packaging/README.md`:

- Add a `systemd user unit` section with the install path, enable command, journalctl command, and the note that AppImage does not use it.

- [ ] **Step 3: Update roadmap, feature, architecture, and data model docs**

In `docs/roadmap.md`:

- Add systemd user service to the implemented list.
- Remove the `systemdユーザーサービス` row from P2.
- Replace unresolved item 2 with the accepted decision (opt-in service, app autostart remains).

In `docs/features.md` section 8:

- Add that deb/pacman ship an optional systemd user service and that in-app autostart still works.

In `docs/architecture.md` section 6:

- Add the unit path, `graphical-session.target`, and `INVOCATION_ID` detection.

In `docs/data-model.md` `app_settings` table:

- Add `daemon_supervisor` with value `"systemd"` or `"app"`.
- State that it is overwritten on each daemon start and ignored when the heartbeat is stale.

In `README.md`:

- Add a bullet for the optional systemd user service.

In `docs/HANDOFF.md`:

- Add the systemd service to the implemented list.
- Update the last-updated date to 2026-10-24.

- [ ] **Step 4: Verify the documentation**

Run:

```bash
grep -R "systemd" README.md docs/adr/README.md docs/release.md docs/roadmap.md \
  docs/troubleshooting.md docs/features.md docs/architecture.md \
  docs/data-model.md packaging/README.md docs/HANDOFF.md
```

Expected: every file appears, and `docs/release.md` no longer lists systemd under `未対応`.

Run:

```bash
grep -n "systemctl --user disable --now vrc-photo-daemon.service" docs/release.md docs/troubleshooting.md
```

Expected: the uninstall command appears in both files.

- [ ] **Step 5: Commit**

```bash
git add docs/adr/0010-systemd-user-service.md docs/adr/README.md \
  docs/release.md docs/roadmap.md docs/troubleshooting.md docs/features.md \
  docs/architecture.md docs/data-model.md packaging/README.md README.md \
  docs/HANDOFF.md docs/superpowers/specs/2026-10-24-systemd-user-service-design.md \
  docs/superpowers/plans/2026-10-24-systemd-user-service.md
git commit -m "docs: document systemd user service"
```

---

### Task 5: Release CI verifies the unit

**Files:**
- Modify: `.github/workflows/release.yml`

**Interfaces:**
- Consumes: package path `usr/lib/systemd/user/vrc-photo-daemon.service`.
- Produces: CI failure when the unit is missing or does not point at `/usr/bin/vrc-photo-daemon`.

- [ ] **Step 1: Add the verification step**

After `Verify third-party notices`, add:

```yaml
      - name: Verify systemd user unit
        run: |
          set -euo pipefail
          unit="usr/lib/systemd/user/vrc-photo-daemon.service"
          deb=$(ls dist/*.deb)
          pacman=$(ls dist/*.tar.gz)
          tmp=$(mktemp -d)
          mkdir -p "$tmp/deb" "$tmp/pacman"
          dpkg-deb --extract "$deb" "$tmp/deb"
          tar -xzf "$pacman" -C "$tmp/pacman"
          test -f "$tmp/deb/$unit" \
            || { echo "missing $unit in deb"; exit 1; }
          test -f "$tmp/pacman/$unit" \
            || { echo "missing $unit in pacman package"; exit 1; }
          grep -qx 'ExecStart=/usr/bin/vrc-photo-daemon' "$tmp/deb/$unit" \
            || { echo "unexpected ExecStart in deb unit"; exit 1; }
          grep -qx 'ExecStart=/usr/bin/vrc-photo-daemon' "$tmp/pacman/$unit" \
            || { echo "unexpected ExecStart in pacman unit"; exit 1; }
          echo "systemd user unit verified"
```

- [ ] **Step 2: Verify the workflow file structure**

Run:

```bash
grep -n "Verify systemd user unit" .github/workflows/release.yml
grep -n "ExecStart=/usr/bin/vrc-photo-daemon" .github/workflows/release.yml
git diff --check
```

Expected: both grep commands find the new step, and `git diff --check` is silent.

- [ ] **Step 3: Commit**

```bash
git add .github/workflows/release.yml
git commit -m "ci: verify systemd unit in packages"
```

---

### Task 6: Integration and full verification

**Files:**
- No source changes expected.
- Temporary test unit: `~/.config/systemd/user/vrc-photo-daemon-test.service` (removed after the test).

**Interfaces:**
- Consumes: all previous tasks.

- [ ] **Step 1: Run formatting, lint, and workspace tests**

Run:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Expected: all commands exit 0. If a test fails, fix it before continuing.

- [ ] **Step 2: Build the debug daemon for the integration test**

Run: `cargo build -p vrc-photo-daemon`

Expected: `target/debug/vrc-photo-daemon` exists.

- [ ] **Step 3: Run the daemon under a temporary systemd user unit**

Run:

```bash
set -euo pipefail
work=/tmp/opencode/systemd-supervisor-test
unit="$HOME/.config/systemd/user/vrc-photo-daemon-test.service"
rm -rf "$work"
mkdir -p "$work" "$(dirname "$unit")"
cat > "$unit" <<EOF
[Unit]
Description=VRC Photo Manager supervisor test

[Service]
Type=simple
Environment=XDG_DATA_HOME=$work
ExecStart=$PWD/target/debug/vrc-photo-daemon --no-tray --db $work/library.db

[Install]
WantedBy=default.target
EOF
systemctl --user daemon-reload
systemctl --user start vrc-photo-daemon-test.service
for _ in $(seq 1 20); do
  if [ -f "$work/library.db" ] && \
     [ "$(sqlite3 "$work/library.db" "select value from app_settings where key='daemon_supervisor';" 2>/dev/null)" = "systemd" ]; then
    break
  fi
  sleep 0.5
done
systemctl --user stop vrc-photo-daemon-test.service
test "$(sqlite3 "$work/library.db" "select value from app_settings where key='daemon_supervisor';")" = "systemd"
echo "supervisor integration test passed"
```

Expected: `supervisor integration test passed`.

- [ ] **Step 4: Verify the service does not duplicate a running daemon**

Run the same daemon manually, then try the temporary unit again while it is running:

```bash
XDG_DATA_HOME=/tmp/opencode/systemd-supervisor-test \
  target/debug/vrc-photo-daemon --no-tray --db /tmp/opencode/systemd-supervisor-test/library.db &
manual_pid=$!
sleep 1
systemctl --user start vrc-photo-daemon-test.service
systemctl --user is-active vrc-photo-daemon-test.service || true
kill "$manual_pid"
wait "$manual_pid" 2>/dev/null || true
systemctl --user stop vrc-photo-daemon-test.service || true
rm -f "$HOME/.config/systemd/user/vrc-photo-daemon-test.service"
systemctl --user daemon-reload
```

Expected: the manual daemon keeps running and the service exits without leaving two daemon processes. The service may report `inactive` or `failed`; that is the documented single-instance behavior. The temporary unit is removed at the end of this step.

- [ ] **Step 5: Review the final diff and status**

Run:

```bash
git status --short
git log --oneline -6
```

Expected: the worktree is clean and the branch contains the design, plan, code, packaging, docs, and CI commits.

- [ ] **Step 6: Push and open a PR**

```bash
git push -u origin feat/systemd-user-service
gh pr create --title "feat: add opt-in systemd user service" --body "$(cat <<'EOF'
## Summary
- add an opt-in `vrc-photo-daemon.service` for deb/pacman
- record whether the daemon runs under systemd and show it in the UI
- document enable, journalctl, uninstall, and overlap with app-managed autostart

Closes #23

## Verification
- `cargo fmt --all -- --check`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo test --workspace`
- temporary systemd user unit reports `daemon_supervisor=systemd`
EOF
)"
```

Expected: PR created. Monitor `CI` and `Release` per AGENTS.md section 6, and do not merge until both are green.

- [ ] **Step 7: Commit any fixes from CI**

If CI fails, fix the cause on this branch and push. Use `fix:` for code or packaging behavior, `ci:` for workflow-only fixes, and `docs:` for documentation corrections.
