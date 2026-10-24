# Compression Dashboard and Waiting Queue Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Reorganize compression around a shared dashboard queue, make profile settings clear (including a working lossless toggle), move the global keep-original policy to Settings, and add an opt-in waiting queue for automatic compression.

**Architecture:** No database schema migration is needed: `compression_jobs.status` is free text, so a new `waiting` state can be added to the model. Two `AppSettings` fields (`compression_keep_original`, `auto_compress_immediate`) control the global policy and the automatic enqueue mode with backward-compatible defaults. The daemon enqueues automatic jobs as `waiting` or `pending` and stamps `keep`/`replace` from the setting; the UI moves the queue from Compression to Dashboard and releases waiting jobs with one explicit button.

**Tech Stack:** Rust stable (edition 2024), gpui-kit 0.8, SQLite via rusqlite, serde, chrono, existing core/daemon/UI crates.

**Spec:** https://github.com/TommyZ-7/vrc-photo-manager/issues/80

## Global Constraints

- Code worktree: `../vrc-photo-manager-compression-dashboard`, branch `feat/compression-dashboard`, cut from updated `main`.
- Documentation worktree: `../vrc-photo-manager-compression-dashboard-docs`, branch `docs/compression-dashboard`, created after the code PR merges. Do not mix code and product docs in the same PR (AGENTS §4). The plan file itself may stay untracked until the docs PR.
- No DB schema migration. `compression_jobs.status` accepts the new `waiting` text value.
- `compression_keep_original` defaults to `true`; `auto_compress_immediate` defaults to `true`. Existing settings JSON must load unchanged and preserve the old safe behavior.
- `replace` still moves the original to the OS trash only after the compressed output exists and is non-empty. A failure before that point leaves the original untouched.
- Automatic compression uses the global keep/replace setting. When automatic compression is ON and keep-original is OFF, Settings shows a warning that automatic compression will trash originals. This plan assumes the warning belongs to the dangerous OFF (replace) state; the copy changes if the setting is OFF while automatic compression is OFF.
- Waiting jobs are never selected by `next_pending_job` or the worker loop. Only the explicit Dashboard action promotes them to `pending`.
- Japanese UI copy, matching the existing Settings/Dashboard card style.
- Do not create a tag, publish a release, or bump the version.
- Keep `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, and `cargo test --workspace` green.
- Commit once per task. Do not squash unrelated tasks into one commit.

## Review Focus

1. An upgraded install whose `app_settings` JSON lacks the new fields must default to keeping originals; otherwise automatic compression would start trashing photos after an upgrade.
2. A `waiting` job must never be picked by `next_pending_job` or a worker until the user presses `待機をすべて圧縮`; neither a daemon restart nor a settings toggle may release it implicitly.
3. Automatic `replace` must leave the original in place when compression or output validation fails, including when the output directory cannot be created.
4. Changing a profile to JPEG while lossless is ON must clear `lossless`; otherwise the profile claims lossless while the JPEG encoder ignores it.
5. Shared-queue counts and the batch action must cover every profile and every waiting job, not just the newest 20 rows loaded for display.

---

### Task 1: Add global compression policy settings

**Files:**
- Modify: `crates/core/src/settings.rs:10-64`
- Test: inline `#[cfg(test)] mod tests` in `crates/core/src/settings.rs`

**Interfaces:**
- Produces: `AppSettings::compression_keep_original: bool` (default `true`)
- Produces: `AppSettings::auto_compress_immediate: bool` (default `true`)
- Consumes: nothing from earlier tasks

- [ ] **Step 1: Write the failing settings tests**

Add these tests to the existing `tests` module in `settings.rs`:

```rust
#[test]
fn old_settings_default_to_keep_originals_and_immediate_auto_compress() {
    let json = r#"{
        "photo_roots": [],
        "log_dir": null,
        "managed_dir": null,
        "language": "ja",
        "autostart": true,
        "tray": true,
        "auto_compress": true,
        "auto_compress_backlog": false,
        "estimated_link": true,
        "thumbnail_cache_mb": 2048,
        "compression_workers": 1
    }"#;
    let settings: AppSettings = serde_json::from_str(json).unwrap();
    assert!(settings.compression_keep_original);
    assert!(settings.auto_compress_immediate);
}

#[test]
fn compression_policy_settings_roundtrip() {
    let db = Db::open_in_memory().unwrap();
    let settings = AppSettings {
        compression_keep_original: false,
        auto_compress_immediate: false,
        ..Default::default()
    };
    settings.save(&db).unwrap();
    let loaded = AppSettings::load(&db).unwrap();
    assert!(!loaded.compression_keep_original);
    assert!(!loaded.auto_compress_immediate);
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p vrc-photo-core old_settings_default_to_keep_originals_and_immediate_auto_compress`
Expected: FAIL to compile because `compression_keep_original` and `auto_compress_immediate` do not exist.

- [ ] **Step 3: Implement the settings fields**

In `AppSettings`, add:

```rust
#[serde(default = "default_true")]
pub compression_keep_original: bool,
#[serde(default = "default_true")]
pub auto_compress_immediate: bool,
```

Add `fn default_true() -> bool { true }` next to `default_auto_update_check`. Set both fields to `true` in `Default::default()`.

- [ ] **Step 4: Run the settings tests to verify they pass**

Run: `cargo test -p vrc-photo-core settings`
Expected: PASS. The old JSON test proves upgraded installs keep the safe default.

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/settings.rs
git commit -m "feat: add global compression policy settings"
```

---

### Task 2: Add the waiting queue state and DB operations

**Files:**
- Modify: `crates/core/src/model.rs:146-176`
- Modify: `crates/core/src/db.rs:1323-1505`
- Test: inline module in `crates/core/src/model.rs`
- Test: inline `#[cfg(test)] mod tests` in `crates/core/src/db.rs`

**Interfaces:**
- Produces: `QueueStatus::Waiting`, `QueueStatus::is_active(&self) -> bool`
- Produces: `Db::enqueue_job_with_status(photo_id: i64, profile_id: &str, policy: &str, status: QueueStatus) -> Result<String>`
- Produces: `Db::enqueue_job_for_existing_profile_with_status(photo_id: i64, profile_id: &str, policy: &str, status: QueueStatus) -> Result<bool>`
- Produces: `Db::release_waiting_jobs() -> Result<usize>`
- Produces: `Db::count_active_jobs() -> Result<i64>`
- Consumes: nothing from Task 1

- [ ] **Step 1: Write the failing model test**

Append a `#[cfg(test)] mod tests` block to `model.rs` (or add to an existing one) with:

```rust
#[test]
fn queue_status_roundtrips_waiting() {
    assert_eq!(QueueStatus::Waiting.as_str(), "waiting");
    assert_eq!(QueueStatus::parse("waiting"), QueueStatus::Waiting);
    assert!(QueueStatus::Waiting.is_active());
    assert!(QueueStatus::Pending.is_active());
    assert!(QueueStatus::Processing.is_active());
    assert!(!QueueStatus::Completed.is_active());
    assert!(!QueueStatus::Failed.is_active());
    assert!(!QueueStatus::Cancelled.is_active());
}
```

- [ ] **Step 2: Run the model test to verify it fails**

Run: `cargo test -p vrc-photo-core queue_status_roundtrips_waiting`
Expected: FAIL to compile because `QueueStatus::Waiting` and `is_active` do not exist.

- [ ] **Step 3: Implement the waiting status**

Add `Waiting` to `QueueStatus`. Map it to `"waiting"` in `as_str` and parse `"waiting"` in `parse`. Add:

```rust
pub fn is_active(&self) -> bool {
    matches!(self, Self::Waiting | Self::Pending | Self::Processing)
}
```

Do not change the `update_job` finished-at rule: `waiting` is not a terminal state.

- [ ] **Step 4: Write the failing DB tests**

Add to the DB tests:

```rust
#[test]
fn waiting_jobs_are_not_dequeued_until_released() {
    let db = Db::open_in_memory().unwrap();
    let root = db.add_root("/photos", "reference").unwrap();
    let photo = db
        .upsert_photo(&sample_photo(root, "/photos/a.png", 100))
        .unwrap();
    let job = db
        .enqueue_job_with_status(photo, "p", "replace", QueueStatus::Waiting)
        .unwrap();

    assert_eq!(db.count_jobs(QueueStatus::Waiting).unwrap(), 1);
    assert_eq!(db.count_active_jobs().unwrap(), 1);
    assert!(db.next_pending_job().unwrap().is_none());

    assert_eq!(db.release_waiting_jobs().unwrap(), 1);
    assert_eq!(db.count_jobs(QueueStatus::Waiting).unwrap(), 0);
    assert_eq!(db.count_jobs(QueueStatus::Pending).unwrap(), 1);
    let released = db.next_pending_job().unwrap().unwrap();
    assert_eq!(released.id, job);
    assert_eq!(released.policy, "replace");
    assert_eq!(db.release_waiting_jobs().unwrap(), 0);
}

#[test]
fn waiting_job_blocks_profile_deletion() {
    let db = Db::open_in_memory().unwrap();
    db.save_profile(&profile("p", "P")).unwrap();
    db.save_profile(&profile("q", "Q")).unwrap();
    let root = db.add_root("/photos", "reference").unwrap();
    let photo = db
        .upsert_photo(&sample_photo(root, "/photos/a.png", 100))
        .unwrap();
    let job = db
        .enqueue_job_with_status(photo, "p", "keep", QueueStatus::Waiting)
        .unwrap();

    assert_eq!(db.active_job_count_for_profile("p").unwrap(), 1);
    assert!(matches!(db.delete_profile("p"), Err(CoreError::Invalid(_))));

    db.update_job(&job, QueueStatus::Completed, 1.0, None, None)
        .unwrap();
    db.delete_profile("p").unwrap();
}
```

- [ ] **Step 5: Run the DB tests to verify they fail**

Run: `cargo test -p vrc-photo-core waiting_jobs_are_not_dequeued_until_released`
Expected: FAIL to compile because the status-aware enqueue methods, `release_waiting_jobs`, and `count_active_jobs` do not exist.

- [ ] **Step 6: Implement the DB operations**

- Keep the existing `enqueue_job` and `enqueue_job_for_existing_profile` signatures, delegating to the new `*_with_status` methods with `QueueStatus::Pending`.
- `enqueue_job_with_status` inserts `status.as_str()` instead of a literal `'pending'`.
- `enqueue_job_for_existing_profile_with_status` keeps the `WHERE EXISTS (SELECT 1 FROM compression_profiles WHERE id = ?3)` guard.
- `release_waiting_jobs` runs one UPDATE:

```sql
UPDATE compression_jobs
SET status = 'pending', progress = 0, error = NULL,
    started_at = NULL, finished_at = NULL
WHERE status = 'waiting'
```

Return the changed row count.

- `count_active_jobs` returns `COUNT(*)` where `status IN ('waiting', 'pending', 'processing')`.
- Update both profile-active queries (`active_job_count_for_profile` and the duplicate query inside `delete_profile`) to include `'waiting'`.

- [ ] **Step 7: Run the queue tests to verify they pass**

Run: `cargo test -p vrc-photo-core waiting_`
Expected: PASS, including the profile-deletion guard.
Run: `cargo test -p vrc-photo-core queue`
Expected: PASS; the existing queue roundtrip and retry tests still pass.

- [ ] **Step 8: Commit**

```bash
git add crates/core/src/model.rs crates/core/src/db.rs
git commit -m "feat: add a waiting compression queue state"
```

---

### Task 3: Apply the global policy and immediate/waiting mode in the daemon

**Files:**
- Modify: `crates/daemon/src/lib.rs:716-719`
- Modify: `crates/daemon/src/lib.rs:882-888`
- Modify: `crates/daemon/src/lib.rs:1509-1524`
- Test: inline `#[cfg(test)] mod tests` in `crates/daemon/src/lib.rs`

**Interfaces:**
- Consumes: `AppSettings::compression_keep_original`, `AppSettings::auto_compress_immediate` from Task 1
- Consumes: `QueueStatus::Waiting`, `Db::enqueue_job_for_existing_profile_with_status`, `Db::release_waiting_jobs` from Task 2
- Produces: `enqueue_auto_jobs(db: &Db, new_ids: &[i64], keep_original: bool, immediate: bool) -> Result<()>`

- [ ] **Step 1: Update existing tests and add the new failing tests**

Change the existing call in `enqueue_auto_jobs` order test to:

```rust
enqueue_auto_jobs(&db, &[photo_id], true, true).unwrap();
```

Add:

```rust
#[test]
fn auto_jobs_use_replace_policy_and_waiting_status() {
    let dir = tempfile::tempdir().unwrap();
    let photo_dir = dir.path().join("photos");
    let log_dir = dir.path().join("logs");
    std::fs::create_dir_all(&photo_dir).unwrap();
    std::fs::create_dir_all(&log_dir).unwrap();
    let original = photo_dir.join("VRChat_2026-10-21_21-24-00.000_4x3.png");
    write_photo(&original);

    let db_path = dir.path().join("library.db");
    add_auto_profile(&db_path);
    let mut daemon = Daemon::open(&db_path, Some(photo_dir), Some(log_dir)).unwrap();
    daemon.set_thumbnail_dir(dir.path().join("thumbnails"));
    daemon.run_once().unwrap();

    let db = Db::open(&db_path).unwrap();
    let photo_id = db.photo_by_path(original.to_str().unwrap()).unwrap().unwrap().id;
    enqueue_auto_jobs(&db, &[photo_id], false, false).unwrap();

    let job = db.list_jobs(1).unwrap().remove(0);
    assert_eq!(job.policy, "replace");
    assert_eq!(job.status, QueueStatus::Waiting);
    assert!(db.next_pending_job().unwrap().is_none());
}

#[test]
fn auto_compress_holds_waiting_jobs_until_released() {
    let dir = tempfile::tempdir().unwrap();
    let photo_dir = dir.path().join("photos");
    let log_dir = dir.path().join("logs");
    std::fs::create_dir_all(&photo_dir).unwrap();
    std::fs::create_dir_all(&log_dir).unwrap();
    let original = photo_dir.join("VRChat_2026-10-21_21-24-00.000_4x3.png");
    write_photo(&original);

    let db_path = dir.path().join("library.db");
    add_auto_profile(&db_path);
    let mut daemon = Daemon::open(&db_path, Some(photo_dir), Some(log_dir)).unwrap();
    daemon.set_thumbnail_dir(dir.path().join("thumbnails"));
    daemon.settings.auto_compress_backlog = true;
    daemon.settings.auto_compress_immediate = false;
    daemon.run_once().unwrap();

    let db = Db::open(&db_path).unwrap();
    let photo = db.photo_by_path(original.to_str().unwrap()).unwrap().unwrap();
    assert_eq!(db.count_jobs(QueueStatus::Waiting).unwrap(), 1);
    daemon.compress_pending().unwrap();
    assert!(db.derivatives_for_photo(photo.id).unwrap().is_empty());

    db.release_waiting_jobs().unwrap();
    daemon.compress_pending().unwrap();
    assert_eq!(db.derivatives_for_photo(photo.id).unwrap().len(), 1);
}

#[test]
fn auto_replace_failure_leaves_the_original_in_place() {
    let dir = tempfile::tempdir().unwrap();
    let photo_dir = dir.path().join("photos");
    let log_dir = dir.path().join("logs");
    std::fs::create_dir_all(&photo_dir).unwrap();
    std::fs::create_dir_all(&log_dir).unwrap();
    let original = photo_dir.join("VRChat_2026-10-21_21-24-00.000_4x3.png");
    write_photo(&original);
    // Make the default output directory impossible to create.
    std::fs::write(photo_dir.join("compressed"), b"not a directory").unwrap();

    let db_path = dir.path().join("library.db");
    add_auto_profile(&db_path);
    let mut daemon = Daemon::open(&db_path, Some(photo_dir), Some(log_dir)).unwrap();
    daemon.set_thumbnail_dir(dir.path().join("thumbnails"));
    daemon.settings.auto_compress_backlog = true;
    daemon.settings.compression_keep_original = false;
    daemon.run_once().unwrap();
    daemon.compress_pending().unwrap();

    let db = Db::open(&db_path).unwrap();
    assert!(original.exists());
    let photo = db.photo_by_path(original.to_str().unwrap()).unwrap().unwrap();
    assert_eq!(photo.status, PhotoStatus::Present);
    assert!(db.derivatives_for_photo(photo.id).unwrap().is_empty());
    assert_eq!(db.list_jobs(1).unwrap()[0].status, QueueStatus::Failed);
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p vrc-photo-daemon auto_jobs_use_replace_policy_and_waiting_status`
Expected: FAIL to compile because `enqueue_auto_jobs` still takes two arguments and hardcodes `pending`/`keep`.

- [ ] **Step 3: Implement the daemon changes**

Change `enqueue_auto_jobs` to:

```rust
fn enqueue_auto_jobs(
    db: &Db,
    new_ids: &[i64],
    keep_original: bool,
    immediate: bool,
) -> Result<()> {
    // ... existing profile filter ...
    let policy = if keep_original { "keep" } else { "replace" };
    let status = if immediate {
        QueueStatus::Pending
    } else {
        QueueStatus::Waiting
    };
    for photo_id in new_ids {
        for profile in &auto_profiles {
            db.enqueue_job_for_existing_profile_with_status(
                *photo_id,
                &profile.id,
                policy,
                status,
            )?;
        }
    }
    Ok(())
}
```

At both call sites, pass `self.settings.compression_keep_original` and `self.settings.auto_compress_immediate`. Keep the existing `auto_compress` and initial-scan `auto_compress_backlog` guards unchanged.

- [ ] **Step 4: Run the daemon auto-compression tests to verify they pass**

Run: `cargo test -p vrc-photo-daemon auto_`
Expected: PASS. The waiting job is not processed until released; the replace failure test proves the original survives.
Run: `cargo test -p vrc-photo-daemon auto_jobs_follow_profile_sort_order`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/daemon/src/lib.rs
git commit -m "feat: apply compression policy to automatic jobs"
```

---

### Task 4: Move the keep-original policy into Settings and add the warning

**Files:**
- Modify: `crates/ui/src/main.rs:1148-1176` (App fields)
- Modify: `crates/ui/src/main.rs:1280-1330` (App initialization)
- Modify: `crates/ui/src/main.rs:2438-2466` (`enqueue_selected`)
- Modify: `crates/ui/src/main.rs:4919-4965` (Compression header and policy buttons)
- Modify: `crates/ui/src/main.rs:6234-6256` (`policy_button`)
- Modify: `crates/ui/src/main.rs:6920-6940` (Settings "起動と圧縮" card)
- Test: inline `#[cfg(test)] mod tests` in `crates/ui/src/main.rs`

**Interfaces:**
- Consumes: `AppSettings::compression_keep_original`, `AppSettings::auto_compress_immediate` from Task 1
- Produces: `compression_policy(keep_original: bool) -> &'static str`
- Produces: `compression_policy_warning(keep_original: bool, auto_compress: bool) -> Option<&'static str>`

- [ ] **Step 1: Write the failing helper tests**

Add to the UI tests:

```rust
#[test]
fn compression_policy_maps_to_stored_values() {
    assert_eq!(compression_policy(true), "keep");
    assert_eq!(compression_policy(false), "replace");
}

#[test]
fn compression_policy_warning_mentions_automatic_trash_only_when_enabled() {
    assert!(compression_policy_warning(true, true).is_none());
    assert!(compression_policy_warning(true, false).is_none());

    let automatic = compression_policy_warning(false, true).unwrap();
    assert!(automatic.contains("自動圧縮"));
    assert!(automatic.contains("OSゴミ箱"));

    let manual = compression_policy_warning(false, false).unwrap();
    assert!(!manual.contains("自動圧縮"));
    assert!(manual.contains("OSゴミ箱"));
}
```

Add the helpers to the test module's `use super::{...}` import list.

- [ ] **Step 2: Run the helper tests to verify they fail**

Run: `cargo test -p vrc-photo-ui compression_policy_`
Expected: FAIL to compile because the helpers do not exist.

- [ ] **Step 3: Implement the helpers and Settings UI**

Add near `format_label`:

```rust
fn compression_policy(keep_original: bool) -> &'static str {
    if keep_original { "keep" } else { "replace" }
}

fn compression_policy_warning(
    keep_original: bool,
    auto_compress: bool,
) -> Option<&'static str> {
    if keep_original {
        return None;
    }
    if auto_compress {
        Some("自動圧縮でも、圧縮に成功すると元画像をOSゴミ箱へ移動します。取り消すにはゴミ箱から復元してください。")
    } else {
        Some("圧縮に成功すると元画像をOSゴミ箱へ移動します。取り消すにはゴミ箱から復元してください。")
    }
}
```

In the Settings screen's "起動と圧縮" card, after the existing `自動圧縮` switch, add:

- `setting_switch("新規写真をすぐに圧縮", "OFFにすると未実行キューに追加し、ダッシュボードからまとめて実行できます", self.settings.auto_compress_immediate, cx, |settings, value| settings.auto_compress_immediate = value)`, rendered only when `self.settings.auto_compress` is true.
- `setting_switch("圧縮後に元画像を保持", "OFFにすると圧縮成功後、元画像をOSゴミ箱へ移動します", self.settings.compression_keep_original, cx, |settings, value| settings.compression_keep_original = value)`.
- When `compression_policy_warning(self.settings.compression_keep_original, self.settings.auto_compress)` returns `Some(text)`, render a warning card: `p(12)`, `rounded(8)`, `bg(rgb(0x2a1414))`, `border_1`, `border_color(rgba(0xfb718533))`, `text_size(10)`, `text_color(rgb(RED))`, child text.

In `enqueue_selected`, replace `self.compress_policy.clone()` with `compression_policy(self.settings.compression_keep_original).to_string()`.

Remove `compress_policy` from `App`, its initialization, the two policy buttons from the Compression header, and the `policy_button` method. Change the Compression subtitle to `"プロファイルと画質を管理します。元画像の扱いは設定タブで変更できます。"`.

- [ ] **Step 4: Run the UI helper tests to verify they pass**

Run: `cargo test -p vrc-photo-ui compression_policy_`
Expected: PASS.

- [ ] **Step 5: Verify the old in-memory policy is gone**

Run: `rg -n "compress_policy|policy_button" crates/ui/src/main.rs`
Expected: no matches (the new helper is named `compression_policy`, which does not match `compress_policy`).

- [ ] **Step 6: Commit**

```bash
git add crates/ui/src/main.rs
git commit -m "feat(ui): move compression policy to settings"
```

---

### Task 5: Make profile settings clear and fix the lossless toggle

**Files:**
- Modify: `crates/ui/src/main.rs:4980-5138` (profile cards)
- Modify: `crates/ui/src/main.rs:5277-5535` (`profile_editor`)
- Test: inline `#[cfg(test)] mod tests` in `crates/ui/src/main.rs`

**Interfaces:**
- Produces: `lossless_supported(format: &CompressionFormat) -> bool`
- Produces: `lossless_hint(format: &CompressionFormat) -> &'static str`
- Produces: `profile_summary(profile: &CompressionProfile) -> String`
- Produces: `apply_profile_format(profile: &mut CompressionProfile, format: CompressionFormat)`
- Consumes: nothing from earlier tasks

- [ ] **Step 1: Write the failing profile helper tests**

Add to the UI tests:

```rust
#[test]
fn lossless_is_unsupported_for_jpeg_and_resets_on_format_change() {
    assert!(!lossless_supported(&CompressionFormat::Jpeg));
    assert!(lossless_supported(&CompressionFormat::Webp));
    assert!(lossless_supported(&CompressionFormat::Avif));
    assert!(lossless_supported(&CompressionFormat::JpegXl));

    let mut profile = CompressionProfile {
        format: CompressionFormat::Webp,
        lossless: true,
        ..Default::default()
    };
    apply_profile_format(&mut profile, CompressionFormat::Jpeg);
    assert_eq!(profile.format, CompressionFormat::Jpeg);
    assert!(!profile.lossless);

    profile.lossless = true;
    apply_profile_format(&mut profile, CompressionFormat::Webp);
    assert!(profile.lossless);
}

#[test]
fn lossless_hints_cover_every_format() {
    assert!(lossless_hint(&CompressionFormat::Jpeg).contains("非対応"));
    assert!(lossless_hint(&CompressionFormat::Avif).contains("近似"));
    assert!(lossless_hint(&CompressionFormat::Webp).contains("可逆"));
    assert!(lossless_hint(&CompressionFormat::JpegXl).contains("可逆"));
}

#[test]
fn profile_summary_hides_quality_when_lossless() {
    let lossless = CompressionProfile {
        format: CompressionFormat::JpegXl,
        lossless: true,
        ..Default::default()
    };
    assert_eq!(profile_summary(&lossless), "JPEG XL · Lossless");

    let lossy = CompressionProfile {
        format: CompressionFormat::Webp,
        quality: 82,
        lossless: false,
        ..Default::default()
    };
    assert_eq!(profile_summary(&lossy), "WebP · Q82 · Lossy");
}
```

Add `CompressionFormat` and `CompressionProfile` to the test module imports.

- [ ] **Step 2: Run the helper tests to verify they fail**

Run: `cargo test -p vrc-photo-ui lossless_`
Expected: FAIL to compile because the helpers do not exist.

- [ ] **Step 3: Implement the helpers**

```rust
fn lossless_supported(format: &CompressionFormat) -> bool {
    !matches!(format, CompressionFormat::Jpeg)
}

fn lossless_hint(format: &CompressionFormat) -> &'static str {
    match format {
        CompressionFormat::Jpeg => "JPEGはロスレス非対応",
        CompressionFormat::Avif => "AVIFは近似ロスレス（品質100）",
        CompressionFormat::Webp => "WebPの可逆圧縮",
        CompressionFormat::JpegXl => "JPEG XLの可逆圧縮",
    }
}

fn profile_summary(profile: &CompressionProfile) -> String {
    if profile.lossless {
        format!("{} · Lossless", format_label(&profile.format))
    } else {
        format!(
            "{} · Q{} · Lossy",
            format_label(&profile.format),
            profile.quality
        )
    }
}

fn apply_profile_format(profile: &mut CompressionProfile, format: CompressionFormat) {
    if !lossless_supported(&format) {
        profile.lossless = false;
    }
    profile.format = format;
}
```

- [ ] **Step 4: Restructure the profile editor**

- The left profile card uses `profile_summary(profile)` instead of the inline `format_label/quality/lossless` formatting.
- The editor header keeps the name, format badge, duplicate, and delete. Move the auto-apply switch out of the header into its own row:
  - Label: `"自動圧縮に使う"`.
  - Description: `"新規写真にこのプロファイルを適用します"`.
  - `Switch::new("profile-auto").checked(profile.auto_enabled)`.
- Add section labels using `self.section_label`:
  - `"形式と画質"`: format chips, lossless row, quality row.
  - `"リサイズ"`: existing resize controls.
  - `"出力"`: output directory and suffix meta rows.
- Format chip click uses `apply_profile_format(profile, selected_format.clone())` inside `edit_profile`.
- Add a lossless row:
  - Label `"ロスレス"`.
  - `Switch::new("profile-lossless").checked(profile.lossless).disabled(!lossless_supported(&profile.format))`.
  - Hint uses `lossless_hint(&profile.format)`, text size 10, muted.
  - On click, `edit_profile(|profile| profile.lossless = *checked, cx)`.
- Quality row:
  - Disable both `quality-minus` and `quality-plus` with `.disabled(profile.lossless)`.
  - When lossless, show `"ロスレスでは品質指定を使用しません"`; otherwise `"品質を調整"`.
- Replace the green note `"自動圧縮では元画像を必ず保持します。"` with `"変更は即時保存されます。元画像の扱いは設定タブで変更できます。"` in a neutral `PANEL_2`/`BORDER` note.

- [ ] **Step 5: Run the profile helper tests to verify they pass**

Run: `cargo test -p vrc-photo-ui lossless_`
Expected: PASS.
Run: `cargo test -p vrc-photo-ui profile_summary_hides_quality_when_lossless`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/ui/src/main.rs
git commit -m "feat(ui): expose lossless and clarify profile settings"
```

---

### Task 6: Rename Monitor to Dashboard and move the shared queue

**Files:**
- Modify: `crates/ui/src/main.rs:112-130` (`Screen`)
- Modify: `crates/ui/src/main.rs:1148-1176` and `1280-1330` (job count state)
- Modify: `crates/ui/src/main.rs:2699-2704` (sidebar badge)
- Modify: `crates/ui/src/main.rs:2775-2795` (nav item/icon)
- Modify: `crates/ui/src/main.rs:3000-3010` (nav tooltip index)
- Modify: `crates/ui/src/main.rs:3135-3160` (alerts)
- Modify: `crates/ui/src/main.rs:3190-3200` (screen dispatch)
- Modify: `crates/ui/src/main.rs:4919-5138` (remove queue from Compression)
- Modify: `crates/ui/src/main.rs:5537-5713` (`queue_panel`)
- Modify: `crates/ui/src/main.rs:6286-6435` (`monitor_screen`)
- Modify: `crates/ui/src/main.rs:7680-7705` (`alert_summaries`)
- Test: inline `#[cfg(test)] mod tests` in `crates/ui/src/main.rs`

**Interfaces:**
- Consumes: `QueueStatus::Waiting`, `Db::list_jobs_by_status`, `Db::count_jobs`, `Db::count_active_jobs`, `Db::release_waiting_jobs` from Task 2
- Produces: `Screen::Dashboard`
- Produces: `queue_status_badge(status: &QueueStatus) -> (&'static str, u32, u32)`
- Produces: `App::release_waiting_jobs(&mut self, cx: &mut Context<Self>)`

- [ ] **Step 1: Write the failing screen and queue tests**

Update the existing `alert_summaries_follow_priority_order` assertion from `Screen::Monitor` to `Screen::Dashboard`, then add:

```rust
#[test]
fn dashboard_replaces_the_monitor_screen() {
    assert_eq!(Screen::Dashboard.title(), "ダッシュボード");
    assert_eq!(alert_summaries(0, 1, 0)[0].0, Screen::Dashboard);
}

#[test]
fn queue_status_badges_distinguish_waiting_and_pending() {
    assert_eq!(queue_status_badge(&QueueStatus::Waiting).0, "未実行");
    assert_eq!(queue_status_badge(&QueueStatus::Pending).0, "実行待ち");
    assert_eq!(queue_status_badge(&QueueStatus::Processing).0, "処理中");
    assert_eq!(queue_status_badge(&QueueStatus::Completed).0, "完了");
    assert_eq!(queue_status_badge(&QueueStatus::Failed).0, "失敗");
    assert_eq!(queue_status_badge(&QueueStatus::Cancelled).0, "取消");
}
```

Add `queue_status_badge` to the test module imports.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p vrc-photo-ui dashboard_replaces_the_monitor_screen`
Expected: FAIL to compile because `Screen::Dashboard` and `queue_status_badge` do not exist.

- [ ] **Step 3: Rename the screen**

- `Screen::Monitor` becomes `Screen::Dashboard`.
- `title()` returns `"ダッシュボード"`.
- Nav label becomes `"ダッシュボード"` and icon becomes `IconName::LayoutDashboard`.
- `nav_tooltip` match uses `Screen::Dashboard`.
- `global_alerts` target uses `Screen::Dashboard` with action `"ダッシュボードを開く"`.
- `content` dispatch calls `self.dashboard_screen(cx)`.
- `alert_summaries` uses `Screen::Dashboard`.
- Rename `monitor_screen` to `dashboard_screen`; keep the existing daemon/session/path/log sections.
- Dashboard header: title `"ダッシュボード"`, subtitle `"ライブラリ、圧縮キュー、ログ監視の状況をまとめて確認できます。"`.

- [ ] **Step 4: Add job count state and the batch action**

In `App`, add:

```rust
active_job_count: i64,
waiting_job_count: i64,
waiting_jobs: Vec<CompressionJob>,
```

Initialize them to `0`/`Vec::new()`. In `reload_job_state`:

- Keep the previous value when a count query fails instead of resetting to zero.
- Set `active_job_count` with `db.count_active_jobs()`.
- Set `waiting_job_count` with `db.count_jobs(QueueStatus::Waiting)`.
- Set `waiting_jobs` with `db.list_jobs_by_status(QueueStatus::Waiting, 100)`.

Sidebar: replace the local `pending_jobs` calculation over `self.jobs` with `self.active_job_count`.

Add:

```rust
fn release_waiting_jobs(&mut self, cx: &mut Context<Self>) {
    match self.db.release_waiting_jobs() {
        Ok(0) => self.message = Some("未実行のジョブはありません".to_string()),
        Ok(count) => {
            self.message = Some(format!("{count} 件を圧縮キューに戻しました"));
            self.reload_job_state();
            self.send_wake_jobs(cx);
        }
        Err(error) => {
            tracing::warn!(%error, "cannot release waiting compression jobs");
            self.message = Some(format!("待機ジョブを開始できません: {error}"));
        }
    }
    cx.notify();
}
```

- [ ] **Step 5: Move the queue to Dashboard and update it**

- Remove `.child(self.queue_panel(cx))` from `compression_screen`.
- In `dashboard_screen`, render the stats row, then `self.queue_panel(cx)`, then the daemon card, errors, session/paths, and log card.
- Stats row: `写真`, `未完了ジョブ` (`active_job_count`), `未実行` (`waiting_job_count`), `デーモン`.
- `queue_status_badge` returns:
  - `Waiting`: `("未実行", AMBER, 0x2a2412)`
  - `Pending`: `("実行待ち", MUTED, PANEL_2)`
  - `Processing`: `("処理中", BLUE, 0x14283d)`
  - `Completed`: `("完了", GREEN, 0x112a22)`
  - `Failed`: `("失敗", RED, 0x2a1414)`
  - `Cancelled`: `("取消", MUTED, PANEL_2)`
- Queue header:
  - Title `"圧縮キュー"`.
  - Badge `"未完了 {active_job_count}"` when non-zero.
  - Badge `"未実行 {waiting_job_count}"` when non-zero.
  - Badge `"失敗 {failed_job_count}"` when non-zero.
  - Button `"待機をすべて圧縮"` (small, primary) when `waiting_job_count > 0`; calls `release_waiting_jobs`.
  - Keep `"失敗をすべて再試行"` and the 100-failure notice.
- Rows:
  - Render `waiting_jobs` first (up to 20) with the waiting badge, `photo #<id>`, profile name, and progress.
  - Render recent non-failed, non-waiting `self.jobs` next (up to 6) with the same profile-name treatment.
  - Keep the existing failed rows and retry buttons.
  - Empty state only when `waiting_jobs`, `jobs`, and `failed_jobs` are all empty.
- Profile names come from `self.profiles` by `job.profile_id`; fall back to `"不明なプロファイル"`.

- [ ] **Step 6: Run the UI tests and build check**

Run: `cargo test -p vrc-photo-ui dashboard_`
Expected: PASS.
Run: `cargo test -p vrc-photo-ui queue_status_badges_`
Expected: PASS.
Run: `cargo check -p vrc-photo-ui`
Expected: PASS; no remaining `Screen::Monitor` references.

- [ ] **Step 7: Commit**

```bash
git add crates/ui/src/main.rs
git commit -m "feat(ui): move the shared compression queue to dashboard"
```

---

### Task 7: Update product docs and run final verification

**Files:**
- Create: `docs/adr/0013-auto-compression-policy.md`
- Modify: `docs/adr/README.md`
- Modify: `docs/adr/0004-compression-policy.md`
- Modify: `docs/features.md` (3.2–3.5)
- Modify: `README.md`
- Modify: `docs/HANDOFF.md`
- Modify: `docs/security-privacy.md`
- Modify: `docs/data-model.md`

**Interfaces:**
- Consumes: the behavior implemented by Tasks 1–6
- Produces: documentation only

- [ ] **Step 1: Write ADR-0013 and update the ADR index**

Create `docs/adr/0013-auto-compression-policy.md`:

- Status: `Accepted`, date `2026-10-25`, supersedes ADR-0004.
- Context: one global keep-original setting now applies to manual, batch, and automatic compression; automatic replacement can run unattended.
- Decision: `compression_keep_original` defaults to `true`; automatic jobs use `keep`/`replace` from it; Settings shows a warning when automatic compression is ON and keep-original is OFF; `replace` still validates the output and uses OS trash; `auto_compress_immediate=false` enqueues `waiting` jobs released only by the Dashboard button.
- Consequences: automatic disk savings are possible; the default and warning protect against accidental loss; originals are recoverable from OS trash.
- Alternatives: keep automatic compression fixed to `keep` (rejected by the user), per-profile policy (rejected for simplicity), confirmation on every automatic compression (rejected because it interrupts photo capture).

In `docs/adr/README.md`, add ADR-0013 and mark ADR-0004 as `Superseded by ADR-0013`. In `docs/adr/0004-compression-policy.md`, change the status line and add the same superseded note used by ADR-0007.

- [ ] **Step 2: Update the product documentation**

- `docs/features.md` 3.2: document the lossless toggle and format constraints (JPEG disabled, AVIF approximate, WebP/JXL supported); document the labeled auto-apply switch and grouped profile editor.
- `docs/features.md` 3.3: replace "元画像は必ず保持" with the global `compression_keep_original` behavior, the warning condition, and `auto_compress_immediate` / `waiting`.
- `docs/features.md` 3.4: document that manual compression uses the same global policy; replace is selected in Settings, not per run in the Compression header.
- `docs/features.md` 3.5: add `waiting`, the shared Dashboard queue, the `待機をすべて圧縮` action, profile names per row, and counts across all profiles.
- `README.md`: update the compression feature bullets and the "元画像を保持する自動圧縮" description.
- `docs/HANDOFF.md`: update the UI screen list (Dashboard, queue location) and the automatic compression policy.
- `docs/security-privacy.md` section 5: automatic replacement happens only when the user turns keep-original OFF; output validation and OS trash remain mandatory.
- `docs/data-model.md`: add `compression_keep_original`, `auto_compress_immediate` to the `AppSettings` field list, document `waiting`, and update the queue invariant.

- [ ] **Step 3: Run the full local verification**

Run:

```bash
cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace
```

Expected: all three commands pass.

- [ ] **Step 4: Perform the visual QA**

Use the `gnome-wayland-visual-qa` skill to launch the app, enqueue at least one compression job so the queue is populated, and capture:

- Dashboard: queue is visible, waiting badge and `待機をすべて圧縮` button render without clipping.
- Settings: `新規写真をすぐに圧縮` and `圧縮後に元画像を保持` switches, plus the red warning when keep-original is OFF.
- Compression: profile editor's lossless row, grouped sections, and disabled JPEG lossless control.

Record any text clipping or layout overflow and fix it before committing.

- [ ] **Step 5: Commit**

```bash
git add docs/adr/0013-auto-compression-policy.md docs/adr/README.md docs/adr/0004-compression-policy.md docs/features.md README.md docs/HANDOFF.md docs/security-privacy.md docs/data-model.md
git commit -m "docs: document the compression dashboard and policy"
```
