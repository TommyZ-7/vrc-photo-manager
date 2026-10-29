# Thumbnail cache LRU eviction — implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `thumbnail_cache_mb` a real disk cap by deleting the least recently used `thumbnails/<id>.webp` files at daemon startup and after thumbnail generation.

**Architecture:** `vrc-photo-core` owns size accounting, mtime-based eviction, and access-time refresh. The daemon thumbnail worker runs eviction on startup, after a batch of generated thumbnails, when its queue goes idle, and immediately when the live limit changes. The UI refreshes cache mtime on a hit and asks core to regenerate once if a file disappears between the cache check and decode.

**Tech Stack:** Rust 2024, `std::fs`, `AtomicU64`, existing SQLite settings, `tempfile` in tests.

**Spec:** `docs/superpowers/specs/2026-10-29-thumbnail-cache-lru-design.md`

## Global Constraints

- `thumbnail_cache_mb = 0` means unlimited: never delete a thumbnail for a zero limit.
- Eviction may touch only regular files named `<u64>.webp` directly inside the thumbnail cache directory. Never touch source photos, `.tmp` files, subdirectories, or non-numeric `.webp` files.
- Eviction runs on the daemon thumbnail worker, never on the UI thread.
- Do not add dependencies, DB migrations, IPC protocol changes, or settings-editor UI.
- A settings change must reach the worker without a daemon restart; the existing 2-second settings poll is the deadline.
- Product docs are a separate docs-only PR per `AGENTS.md` §4. Do not put `docs/features.md`, `docs/troubleshooting.md`, `docs/architecture.md`, or `docs/ui.md` changes in the code PR.
- Use `feat:` for code commits and `docs:` for the documentation PR.

## Review Focus

- **Cache directory missing or empty.** Expected: usage and pruning return zeroed results, not an error or a startup warning.
- **The limit is smaller than a single thumbnail.** Expected: pruning deletes entries until zero bytes remain, rather than stopping above the limit.
- **A non-numeric `.webp`, a `.tmp` file, or a subdirectory in the cache.** Expected: it is neither counted nor deleted.
- **`thumbnail_cache_mb = 0`.** Expected: no file is deleted, even when the cache is large.
- **A thumbnail disappears between the UI cache check and decode.** Expected: the UI regenerates it once from the source; demo mode never writes to the real cache.
- **A cached thumbnail is older than one hour when it is displayed.** Expected: its mtime is refreshed so the next prune treats it as recently used.

---

## Prerequisites

- Merge the design/plan docs-only PR first, then cut the implementation worktree from updated `main`:

```bash
REPO=$(basename "$(git rev-parse --show-toplevel)")
git worktree add ../${REPO}-thumbnail-cache-lru-feat -b feat/thumbnail-cache-lru
```

- Open a separate session in that worktree and implement Tasks 1–4 and 6 there. Task 5 is a separate docs-only PR after the code merge.

### Task 1: Core thumbnail cache accounting and size-based pruning

**Files:**
- Modify: `crates/core/src/thumbnail.rs`
- Test: inline `#[cfg(test)] mod tests` in `crates/core/src/thumbnail.rs`

**Interfaces:**
- Produces:
  - `pub struct ThumbnailCacheUsage { pub files: usize, pub bytes: u64 }`
  - `pub struct ThumbnailPruneReport { pub scanned_files: usize, pub scanned_bytes: u64, pub removed_files: usize, pub removed_bytes: u64, pub remaining_files: usize, pub remaining_bytes: u64 }`
  - `pub fn thumbnail_cache_usage_in(cache_dir: &Path) -> Result<ThumbnailCacheUsage>`
  - `pub fn prune_thumbnails_in(cache_dir: &Path, max_bytes: u64) -> Result<ThumbnailPruneReport>`
- Both structs derive `Debug, Clone, Copy, Default, PartialEq, Eq`.
- `prune_thumbnails_in` treats `max_bytes == 0` as unlimited and returns the scanned totals with no removals.
- A missing cache directory is an empty cache, not an error.

- [ ] **Step 1: Write the failing pruning tests**

Add this helper and tests to the existing `tests` module:

```rust
fn write_cache_entry(path: &Path, bytes: usize, modified: SystemTime) {
    fs::write(path, vec![0u8; bytes]).unwrap();
    fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(modified)
        .unwrap();
}

#[test]
fn prune_thumbnails_removes_oldest_until_under_limit() {
    let dir = tempdir().unwrap();
    let now = SystemTime::now();
    write_cache_entry(&dir.path().join("1.webp"), 1000, now - Duration::from_secs(30));
    write_cache_entry(&dir.path().join("2.webp"), 1000, now - Duration::from_secs(20));
    write_cache_entry(&dir.path().join("3.webp"), 1000, now - Duration::from_secs(10));
    write_cache_entry(&dir.path().join("4.webp"), 1000, now);

    let report = prune_thumbnails_in(dir.path(), 2500).unwrap();

    assert_eq!(report.scanned_files, 4);
    assert_eq!(report.scanned_bytes, 4000);
    assert_eq!(report.removed_files, 2);
    assert_eq!(report.removed_bytes, 2000);
    assert_eq!(report.remaining_files, 2);
    assert_eq!(report.remaining_bytes, 2000);
    assert!(!dir.path().join("1.webp").exists());
    assert!(!dir.path().join("2.webp").exists());
    assert!(dir.path().join("3.webp").exists());
    assert!(dir.path().join("4.webp").exists());
}

#[test]
fn thumbnail_cache_usage_counts_numeric_webp_only() {
    let dir = tempdir().unwrap();
    write_cache_entry(&dir.path().join("1.webp"), 1000, SystemTime::now());
    write_cache_entry(&dir.path().join("2.webp"), 2000, SystemTime::now());
    write_cache_entry(&dir.path().join("keep.webp"), 3000, SystemTime::now());
    write_cache_entry(&dir.path().join("3.webp.tmp"), 4000, SystemTime::now());

    let usage = thumbnail_cache_usage_in(dir.path()).unwrap();

    assert_eq!(usage.files, 2);
    assert_eq!(usage.bytes, 3000);
}

#[test]
fn prune_thumbnails_keeps_files_when_under_limit() {
    let dir = tempdir().unwrap();
    write_cache_entry(&dir.path().join("1.webp"), 1000, SystemTime::now());

    let report = prune_thumbnails_in(dir.path(), 2000).unwrap();

    assert_eq!(report.removed_files, 0);
    assert_eq!(report.remaining_bytes, 1000);
    assert!(dir.path().join("1.webp").exists());
}

#[test]
fn prune_thumbnails_zero_limit_is_unlimited() {
    let dir = tempdir().unwrap();
    write_cache_entry(&dir.path().join("1.webp"), 1000, SystemTime::now());

    let report = prune_thumbnails_in(dir.path(), 0).unwrap();

    assert_eq!(report.removed_files, 0);
    assert_eq!(report.remaining_files, 1);
    assert!(dir.path().join("1.webp").exists());
}

#[test]
fn prune_thumbnails_ignores_non_thumbnail_files() {
    let dir = tempdir().unwrap();
    let now = SystemTime::now();
    write_cache_entry(&dir.path().join("1.webp"), 5000, now - Duration::from_secs(10));
    write_cache_entry(&dir.path().join("keep.webp"), 5000, now - Duration::from_secs(20));
    write_cache_entry(&dir.path().join("2.webp.tmp"), 5000, now - Duration::from_secs(30));
    fs::write(dir.path().join("notes.txt"), vec![0u8; 5000]).unwrap();
    fs::create_dir(dir.path().join("sub")).unwrap();

    let report = prune_thumbnails_in(dir.path(), 1000).unwrap();

    assert_eq!(report.scanned_files, 1);
    assert_eq!(report.removed_files, 1);
    assert!(!dir.path().join("1.webp").exists());
    assert!(dir.path().join("keep.webp").exists());
    assert!(dir.path().join("2.webp.tmp").exists());
    assert!(dir.path().join("notes.txt").exists());
    assert!(dir.path().join("sub").is_dir());
}

#[test]
fn prune_thumbnails_missing_directory_is_empty() {
    let dir = tempdir().unwrap();

    let report = prune_thumbnails_in(&dir.path().join("missing"), 1024).unwrap();

    assert_eq!(report.scanned_files, 0);
    assert_eq!(report.removed_files, 0);
}

#[test]
fn prune_thumbnails_removes_every_entry_when_limit_is_below_one_file() {
    let dir = tempdir().unwrap();
    write_cache_entry(&dir.path().join("1.webp"), 1000, SystemTime::now());

    let report = prune_thumbnails_in(dir.path(), 1).unwrap();

    assert_eq!(report.removed_files, 1);
    assert_eq!(report.remaining_bytes, 0);
    assert!(!dir.path().join("1.webp").exists());
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p vrc-photo-core thumbnail`
Expected: compile error because `prune_thumbnails_in`, `thumbnail_cache_usage_in`, and the report types do not exist.

- [ ] **Step 3: Implement the cache scan and prune API**

In `crates/core/src/thumbnail.rs`, add `use std::time::{Duration, SystemTime};` and implement the two public structs and functions. Use one private entry type:

```rust
struct ThumbnailEntry {
    modified: SystemTime,
    bytes: u64,
    path: PathBuf,
}
```

Scan rules:

- `fs::read_dir(cache_dir)?`; map `ErrorKind::NotFound` to an empty vector.
- Match only `path.extension() == Some("webp")`.
- Require `path.file_stem().and_then(OsStr::to_str).and_then(|stem| stem.parse::<u64>().ok()).is_some()`.
- Require `entry.metadata()?.is_file()`; skip entries with unreadable metadata.
- Use `metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH)`.

`prune_thumbnails_in` fills the scanned totals, returns immediately when `max_bytes == 0` or the total is already within the limit, sorts by `(modified, path)` ascending, then removes until `remaining <= max_bytes`. Count only successful removals. Log failed removals at debug level and continue.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p vrc-photo-core thumbnail`
Expected: all existing thumbnail tests plus the six `prune_thumbnails_*` tests and the usage test pass.

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/thumbnail.rs
git commit -m "feat: add size-based thumbnail cache pruning"
```

---

### Task 2: Refresh cache recency on every hit

**Files:**
- Modify: `crates/core/src/thumbnail.rs`
- Test: inline `#[cfg(test)] mod tests` in `crates/core/src/thumbnail.rs`

**Interfaces:**
- Produces `pub fn refresh_thumbnail_access(path: &Path)`
- Produces private `const ACCESS_REFRESH_AFTER: Duration = Duration::from_secs(60 * 60)`.
- `ensure_thumbnail_in` does **not** call it: the daemon's startup pass over every cached thumbnail must not promote the whole cache. The UI calls it only when a thumbnail is actually displayed.

- [ ] **Step 1: Write the failing recency tests**

Add these tests to the same module:

```rust
#[test]
fn refresh_thumbnail_access_rewrites_a_stale_mtime() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("1.webp");
    let stale = SystemTime::now() - Duration::from_secs(2 * 60 * 60);
    write_cache_entry(&path, 10, stale);
    let before = fs::metadata(&path).unwrap().modified().unwrap();

    refresh_thumbnail_access(&path);

    let after = fs::metadata(&path).unwrap().modified().unwrap();
    assert!(after > before);
}

#[test]
fn refresh_thumbnail_access_leaves_a_recent_mtime_alone() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("1.webp");
    let recent = SystemTime::now() - Duration::from_secs(5);
    write_cache_entry(&path, 10, recent);
    let before = fs::metadata(&path).unwrap().modified().unwrap();

    refresh_thumbnail_access(&path);

    let after = fs::metadata(&path).unwrap().modified().unwrap();
    assert_eq!(after, before);
}

```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p vrc-photo-core thumbnail`
Expected: compile error because `refresh_thumbnail_access` does not exist.

- [ ] **Step 3: Implement `refresh_thumbnail_access`**

Open with `fs::OpenOptions::new().write(true).open(path)`, read `metadata.modified()`, and return without writing when the age is below `ACCESS_REFRESH_AFTER` or the mtime is in the future. On a stale hit, call `file.set_modified(SystemTime::now())`. Log failures at debug level with the path; never return an error. Leave `ensure_thumbnail_in` unchanged.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p vrc-photo-core thumbnail`
Expected: the two `refresh_thumbnail_access*` tests and all earlier thumbnail tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/thumbnail.rs
git commit -m "feat: refresh thumbnail access time for LRU"
```

---

### Task 3: Daemon startup, generation, and live-limit pruning

**Files:**
- Modify: `crates/daemon/src/lib.rs`
- Test: inline `#[cfg(test)] mod tests` in `crates/daemon/src/lib.rs`

**Interfaces:**
- Adds `Daemon.thumbnail_cache_limit: Arc<AtomicU64>`.
- Produces `fn thumbnail_cache_limit_bytes(megabytes: u64) -> u64`.
- Produces `fn prune_thumbnail_cache(cache_dir: &Path, limit_bytes: u64)`.
- Changes `fn thumbnail_loop(db: Arc<Mutex<Db>>, running: Arc<AtomicBool>, rx: Receiver<i64>, cache_dir: PathBuf, cache_limit: Arc<AtomicU64>)`.
- `refresh_compression_settings` now also stores the byte limit; update its doc comment to mention the thumbnail cache.
- `reload_settings` stores the limit before replacing `self.settings`.

- [ ] **Step 1: Write the failing daemon tests**

Add a `wait_for` helper next to `wait_until_idle`, then add the tests:

```rust
fn wait_for(mut condition: impl FnMut() -> bool, timeout: Duration) {
    let started = Instant::now();
    while !condition() {
        assert!(
            started.elapsed() < timeout,
            "condition was not met within {timeout:?}"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

fn write_thumbnail_cache_entry(path: &Path, bytes: usize, modified: SystemTime) {
    std::fs::write(path, vec![0u8; bytes]).unwrap();
    std::fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(modified)
        .unwrap();
}

#[test]
fn startup_prunes_thumbnail_cache_to_limit() {
    let dir = tempfile::tempdir().unwrap();
    let mut daemon = test_daemon(&dir);
    let cache = dir.path().join("thumbnails");
    std::fs::create_dir_all(&cache).unwrap();
    let now = SystemTime::now();
    for (id, age) in [(1, 30), (2, 20), (3, 10)] {
        write_thumbnail_cache_entry(
            &cache.join(format!("{id}.webp")),
            600_000,
            now - Duration::from_secs(age),
        );
    }
    save_daemon_settings(&daemon, |settings| settings.thumbnail_cache_mb = 1);
    daemon.refresh_compression_settings();
    daemon.set_thumbnail_dir(cache.clone());

    daemon.start_workers();
    wait_for(
        || !cache.join("1.webp").exists() && !cache.join("2.webp").exists(),
        Duration::from_secs(5),
    );
    assert!(cache.join("3.webp").exists());
    daemon.stop_workers();
}

#[test]
fn live_thumbnail_limit_change_prunes_cache() {
    let dir = tempfile::tempdir().unwrap();
    let mut daemon = test_daemon(&dir);
    let cache = dir.path().join("thumbnails");
    std::fs::create_dir_all(&cache).unwrap();
    let now = SystemTime::now();
    for (id, age) in [(1, 30), (2, 20), (3, 10)] {
        write_thumbnail_cache_entry(
            &cache.join(format!("{id}.webp")),
            600_000,
            now - Duration::from_secs(age),
        );
    }
    daemon.set_thumbnail_dir(cache.clone());
    daemon.start_workers();
    assert!(cache.join("1.webp").exists());

    save_daemon_settings(&daemon, |settings| settings.thumbnail_cache_mb = 1);
    daemon.refresh_compression_settings();
    wait_for(
        || !cache.join("1.webp").exists() && !cache.join("2.webp").exists(),
        Duration::from_secs(5),
    );
    assert!(cache.join("3.webp").exists());
    daemon.stop_workers();
}
```

Add `SystemTime` to the daemon test module imports if it is not already imported.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p vrc-photo-daemon thumbnail`
Expected: both tests compile but fail in `wait_for` because the daemon never prunes the cache.

- [ ] **Step 3: Implement the limit field and settings plumbing**

- Add `AtomicU64` to `use std::sync::atomic::{AtomicBool, Ordering};`.
- Add `thumbnail_cache_limit: Arc<AtomicU64>` to `Daemon`.
- In `open_inner`, compute `thumbnail_cache_limit_bytes(settings.thumbnail_cache_mb)` before constructing `Self` and store it with `Arc::new(AtomicU64::new(...))`.
- Add `fn thumbnail_cache_limit_bytes(megabytes: u64) -> u64 { megabytes.saturating_mul(1024 * 1024) }`.
- In `reload_settings`, after the autostart comparison, store the loaded value with `Ordering::Relaxed` before assigning `self.settings`.
- In `refresh_compression_settings`, store the loaded value alongside the compression fields.

- [ ] **Step 4: Implement the worker pruning trigger**

Use these constants near the other daemon timing constants:

```rust
const THUMBNAIL_PRUNE_BATCH: usize = 128;
const THUMBNAIL_PRUNE_IDLE: Duration = Duration::from_millis(500);
```

`prune_thumbnail_cache` calls `thumbnail::prune_thumbnails_in`, logs an info event only when `removed_files > 0` (include `removed_files`, `removed_bytes`, `remaining_bytes`, `limit_bytes`), and logs errors with the cache directory path.

In `thumbnail_loop`:

- Run one prune before entering the loop.
- Use `THUMBNAIL_PRUNE_IDLE` for the existing `recv_timeout` interval.
- Keep `last_limit` and `processed_since_prune`.
- At the top of every iteration, load the current limit. If it differs from `last_limit`, update `last_limit`, reset `processed_since_prune`, and prune immediately.
- Before `ensure_thumbnail_in`, compute `let cache_path = cache_dir.join(format!("{id}.webp")); let generated = !cache_path.is_file();`. Increment `processed_since_prune` only for `Ok(Some(_))` results with `generated == true`; when it reaches `THUMBNAIL_PRUNE_BATCH`, reset it and prune. Cache hits must not trigger a full directory scan.
- On `RecvTimeoutError::Timeout`, prune once if `processed_since_prune > 0`, then reset it. Do not prune on every idle timeout when nothing was generated.
- On `Disconnected`, break as today.
- Clone `self.thumbnail_cache_limit` in `start_workers` and pass it to the thread.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p vrc-photo-daemon thumbnail`
Expected: `startup_prunes_thumbnail_cache_to_limit` and `live_thumbnail_limit_change_prunes_cache` pass, along with existing thumbnail-related daemon tests.

- [ ] **Step 6: Commit**

```bash
git add crates/daemon/src/lib.rs
cargo test -p vrc-photo-daemon thumbnail
git commit -m "feat: prune thumbnail cache from the daemon worker"
```

---

### Task 4: UI access refresh, regeneration fallback, and cache label

**Files:**
- Modify: `crates/ui/src/state/thumbnail.rs`
- Modify: `crates/ui/src/app/photos.rs`
- Modify: `crates/ui/src/ui/settings.rs`
- Test: inline `#[cfg(test)] mod tests` in `crates/ui/src/state/thumbnail.rs`

**Interfaces:**
- Changes `pub(crate) fn decode_thumbnail(id: i64, cache_path: &Path, source: &Path, cache_dir: Option<&Path>) -> Option<(u32, image::RgbaImage)>`.
- `cache_dir = Some(dir)` means the real cache: core resolves the path or regenerates a miss through `ensure_thumbnail_in`; the UI refreshes a hit with `refresh_thumbnail_access`. If the resolved file disappears before decode, retry `ensure_thumbnail_in` once.
- `cache_dir = None` means demo or an explicit read-only path: decode `cache_path` if it exists and never write.
- `request_thumbnail` passes `thumbnail::thumbnails_dir()` only when `self.demo_assets.is_none()`.

- [ ] **Step 1: Update the existing test and add the failing UI tests**

Change the existing `decode_thumbnail_uses_the_cached_file_and_swaps_channels` call to pass `None` as the fourth argument.

Add:

```rust
#[test]
fn decode_thumbnail_regenerates_a_removed_cache_file() {
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("thumbnails");
    let source = dir.path().join("source.png");
    image::RgbaImage::from_pixel(128, 80, image::Rgba([200, 100, 50, 255]))
        .save(&source)
        .unwrap();
    let cache_path = cache.join("7.webp");

    let first = decode_thumbnail(7, &cache_path, &source, Some(&cache)).expect("generated");
    assert!(cache_path.is_file());
    std::fs::remove_file(&cache_path).unwrap();
    let second = decode_thumbnail(7, &cache_path, &source, Some(&cache)).expect("regenerated");
    assert!(cache_path.is_file());
    assert_eq!(first.0, second.0);
}

#[test]
fn decode_thumbnail_refreshes_cached_access_time() {
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("thumbnails");
    let source = dir.path().join("source.png");
    image::RgbaImage::from_pixel(32, 32, image::Rgba([10, 20, 30, 255]))
        .save(&source)
        .unwrap();
    let cache_path = cache.join("8.webp");
    decode_thumbnail(8, &cache_path, &source, Some(&cache)).expect("generated");
    let stale = std::time::SystemTime::now() - std::time::Duration::from_secs(2 * 60 * 60);
    std::fs::File::options()
        .write(true)
        .open(&cache_path)
        .unwrap()
        .set_modified(stale)
        .unwrap();
    let before = std::fs::metadata(&cache_path).unwrap().modified().unwrap();

    decode_thumbnail(8, &cache_path, &source, Some(&cache)).expect("cached");

    assert!(std::fs::metadata(&cache_path).unwrap().modified().unwrap() > before);
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p vrc-photo-ui decode_thumbnail`
Expected: compile error because `decode_thumbnail` takes three arguments.

- [ ] **Step 3: Implement the core-backed decode path**

Refactor `decode_thumbnail` into a resolver plus a file decoder:

- `fn decode_thumbnail_file(id: i64, path: &Path) -> Option<(u32, image::RgbaImage)>` keeps the existing `decode_image`, RGBA-to-BGRA, and average-colour logic.
- When `cache_dir` is `Some(dir)`, record `let had_cache = cache_path.is_file();`, call `thumbnail::ensure_thumbnail_in(id, source, dir)`, and decode the returned path. If `had_cache`, call `thumbnail::refresh_thumbnail_access(&path)` before decoding. If decoding fails and `path.is_file()` is now false, call `ensure_thumbnail_in` one more time and decode that result. Log `missing source` for `Ok(None)` and `cannot generate thumbnail` for `Err`.
- When `cache_dir` is `None`, return `None` unless `cache_path.is_file()`; otherwise decode `cache_path` directly. Do not refresh demo access times.
- Do not call `ensure_thumbnail` (the global path resolver) from this function.
- This makes display the only source of recency updates; the daemon's startup pass over cached files does not promote them.

In `request_thumbnail`, compute the real cache directory before spawning the background task:

```rust
let cache_dir = if self.demo_assets.is_none() {
    thumbnail::thumbnails_dir()
} else {
    None
};
```

Move `cache_dir` into the background closure and pass `cache_dir.as_deref()` as the fourth `decode_thumbnail` argument.

- [ ] **Step 4: Update the settings cache label**

In `crates/ui/src/ui/settings.rs`, replace the single `format!` with:

```rust
let cache_label = if self.settings.thumbnail_cache_mb == 0 {
    "無制限".to_string()
} else {
    format!("{} MB", self.settings.thumbnail_cache_mb)
};
```

and render:

```rust
.child(format!(
    "言語: {} · サムネイルキャッシュ上限: {}",
    self.settings.language, cache_label
))
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p vrc-photo-ui decode_thumbnail`
Expected: the existing channels test plus both new decode tests pass.

- [ ] **Step 6: Commit**

```bash
git add crates/ui/src/state/thumbnail.rs crates/ui/src/app/photos.rs crates/ui/src/ui/settings.rs
git commit -m "feat: refresh thumbnail recency and regenerate evicted files"
```

---

### Task 5: Product documentation (separate docs-only PR)

**Files:**
- Modify: `docs/features.md`
- Modify: `docs/troubleshooting.md`
- Modify: `docs/architecture.md`
- Modify: `docs/ui.md`

**Interfaces:**
- Consumes the behavior implemented in Tasks 1–4.
- This task must run after the code PR is merged so `main` never documents unimplemented behavior.

- [ ] **Step 1: Update the feature list**

In `docs/features.md`:

- Replace the line `- サムネイルキャッシュ上限（設定値のみ。ディスク上のLRU削除は未実装）` with a line stating that the daemon deletes the least recently used thumbnails at startup and after generation, the default is 2 GB, `0` means unlimited, and setting changes apply without a restart.
- Replace the line `- ディスク上のサムネイルキャッシュのLRU削除は未実装（メモリ上の表示サムネイルは192件LRU）。` with a line stating that disk thumbnails are pruned to `thumbnail_cache_mb` and memory thumbnails remain a 192-entry LRU.

- [ ] **Step 2: Update troubleshooting and architecture**

In `docs/troubleshooting.md`, replace `- 現状LRU削除は未実装。thumbnail_cache_mb は設定値のみ。` with the trigger, default, and `0 = unlimited` behavior.

In `docs/architecture.md` §4.5, add a bullet: the daemon checks total `thumbnails/*.webp` size at startup and after generation, deletes oldest-first until the `thumbnail_cache_mb` limit is met, treats `0` as unlimited, and refreshes mtime when a thumbnail is displayed from the cache. Background generation hits do not count as display access.

- [ ] **Step 3: Update the settings description**

In `docs/ui.md`, update the settings footer description to say the cache value is an upper limit and that `0` is shown as `無制限`.

- [ ] **Step 4: Verify docs-only scope**

Run: `git diff --name-only`
Expected: only the four files above. No `crates/` paths.

- [ ] **Step 5: Commit and open the docs PR**

```bash
git add docs/features.md docs/troubleshooting.md docs/architecture.md docs/ui.md
git commit -m "docs: document thumbnail cache LRU eviction"
```

Open a `docs/` PR with the `documentation` and `enhancement` labels, and wait for CI per the `ci-watch` skill before merging.

---

### Task 6: Full verification and code PR

**Files:**
- Verify only; no new edits.

- [ ] **Step 1: Run the full local gate**

Run:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Expected: all three commands exit 0. Investigate any warning or failure before pushing.

- [ ] **Step 2: Confirm the code commit contains no product docs**

Run:

```bash
git diff --name-only main...HEAD
```

Expected: only `crates/core/src/thumbnail.rs`, `crates/daemon/src/lib.rs`, `crates/ui/src/state/thumbnail.rs`, `crates/ui/src/app/photos.rs`, and `crates/ui/src/ui/settings.rs` in the code PR. The design/plan files belong to the docs PR.

- [ ] **Step 3: Push and open the code PR**

```bash
git push -u origin feat/thumbnail-cache-lru
gh pr create --title "feat: enforce thumbnail cache LRU limit" --label enhancement --body "Closes #15. Enforces thumbnail_cache_mb at daemon startup and after generation, with 0 meaning unlimited."
```

- [ ] **Step 4: Monitor CI and merge only when green**

Follow the `ci-watch` skill. On success, merge with `gh pr merge <number> --merge`, switch to `main`, and pull. On failure, fix on the branch and push again.

- [ ] **Step 5: Open the separate docs PR**

Run Task 5 from an updated `main` worktree after the code merge. Do not merge the docs PR before the code PR.

---

## Self-Review

- **Spec coverage:** startup prune (Task 3), post-generation prune (Task 3), LRU access refresh (Tasks 2 and 4), only `*.webp` thumbnails (Task 1), UI non-blocking (Tasks 3 and 4), live settings (Task 3), `0 = unlimited` (Tasks 1 and 3), regeneration (Task 4), docs (Task 5).
- **Type consistency:** `prune_thumbnails_in` and `ThumbnailPruneReport` are used unchanged by the daemon; `decode_thumbnail`'s fourth argument is `Option<&Path>` in both the implementation and tests; the daemon limit is bytes in both the field and worker.
- **Review Focus tests:** missing cache directory and limit below one file (Task 1), non-numeric/`.tmp`/subdirectory survival (Task 1), zero limit (Task 1), UI cache disappearance (Task 4), stale mtime refresh (Task 2). The live limit change is covered by Task 3's `live_thumbnail_limit_change_prunes_cache`; the worker also checks the limit at the top of every iteration so a busy backlog sees it immediately.
