# Async Preview Loading and Disk Cache Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make library selection paint immediately by decoding 1200 px previews on the background executor and caching them on disk.

**Architecture:** A new core `preview` module owns a bounded 1200 px WebP cache. The UI keeps one preview decode in flight, installs a result only when its photo and display variant are still selected, and warms newly imported photos after indexing. The 256 px thumbnail pipeline stays unchanged and remains the immediate placeholder.

**Tech Stack:** Rust 2024, gpui-kit 0.8, image 0.25, webp, SQLite (unchanged).

**Spec:** `docs/superpowers/specs/2026-10-25-async-preview-design.md`

## Global Constraints

- No preview decode runs on the UI thread.
- Cache path: `<data_dir>/previews/<photo_id>-<variant>.webp`, where
  `variant = photo.display_derivative_id.unwrap_or(0)`.
- Preview size: at most 1200 px on the long edge. WebP quality 85.
- Cache bound: 2048 files, pruned to 1536 oldest-first every 32 new entries.
- One preview decode in flight. The latest requested key is the only queued
  job; stale completions are dropped.
- No new database columns, migrations, or settings.
- Demo mode writes previews under the demo root, never the real data directory.
- Design/plan docs and user-facing docs land in separate docs PRs per
  `AGENTS.md`; code PRs contain code and tests only.

## Review Focus

- Rapid card clicks or viewer arrow navigation while a decode is running: the
  highlight for the newly selected photo must stay, and only the latest
  preview may replace the current one.
- Missing or unreadable source: selection must not stall; the thumbnail stays
  and the inspector shows the failure message only after the attempt returns.
- Corrupt or partial cache entry (for example a crash during write): the entry
  must be discarded and regenerated, never shown or allowed to block.
- A compression replacement changes the displayed derivative while the photo
  stays selected: the old preview may remain visible briefly, but the new
  variant must be requested and eventually replace it.
- Cache growth past the cap: pruning must delete only preview cache files,
  must not fail the preview, and must keep the newest entries.

---

### Task 1: Core 1200 px preview cache

**Files:**
- Create: `crates/core/src/preview.rs`
- Modify: `crates/core/src/lib.rs` (add `pub mod preview;` after `pub mod metadata;`)
- Test: `crates/core/src/preview.rs` (`#[cfg(test)] mod tests`)

**Interfaces:**
- Produces:
  - `pub const PREVIEW_MAX: u32 = 1200;`
  - `pub fn previews_dir() -> Option<PathBuf>`
  - `pub fn decode_preview(source: &Path) -> Option<image::RgbaImage>`
  - `pub fn ensure_preview(photo_id: i64, variant: i64, source: &Path) -> Option<image::RgbaImage>`
  - `pub fn ensure_preview_in(photo_id: i64, variant: i64, source: &Path, cache_dir: &Path) -> Option<image::RgbaImage>`

- [ ] **Step 1: Write the failing cache tests**

Add this test module at the bottom of the new `crates/core/src/preview.rs` (the
non-test module starts empty for now):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use image::{Rgba, RgbaImage};
    use std::fs;
    use std::time::{Duration, SystemTime};
    use tempfile::tempdir;

    fn write_png(path: &Path, width: u32, height: u32, color: [u8; 4]) {
        RgbaImage::from_pixel(width, height, Rgba(color))
            .save(path)
            .unwrap();
    }

    fn set_mtime(path: &Path, time: SystemTime) {
        fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(time)
            .unwrap();
    }

    #[test]
    fn creates_a_scaled_preview_and_reuses_it_without_the_source() {
        let dir = tempdir().unwrap();
        let source = dir.path().join("source.png");
        let cache = dir.path().join("previews");
        write_png(&source, 1600, 1000, [255, 0, 0, 255]);

        let first = ensure_preview_in(7, 0, &source, &cache).expect("preview");
        assert_eq!(first.dimensions(), (1200, 750));
        assert!(cache.join("7-0.webp").is_file());
        let pixel = first.get_pixel(0, 0).0;
        assert!(pixel[0] > 240 && pixel[1] < 16 && pixel[2] < 16);

        fs::remove_file(&source).unwrap();
        let second = ensure_preview_in(7, 0, &source, &cache).expect("cached preview");
        assert_eq!(second.dimensions(), (1200, 750));
        let pixel = second.get_pixel(0, 0).0;
        assert!(pixel[0] > 240 && pixel[1] < 16 && pixel[2] < 16);
    }

    #[test]
    fn keeps_display_variants_separate() {
        let dir = tempdir().unwrap();
        let source = dir.path().join("source.png");
        let cache = dir.path().join("previews");
        write_png(&source, 320, 200, [0, 255, 0, 255]);

        assert!(ensure_preview_in(8, 0, &source, &cache).is_some());
        assert!(ensure_preview_in(8, 42, &source, &cache).is_some());
        assert!(cache.join("8-0.webp").is_file());
        assert!(cache.join("8-42.webp").is_file());
    }

    #[test]
    fn missing_source_and_cache_returns_none() {
        let dir = tempdir().unwrap();
        let source = dir.path().join("missing.png");
        assert!(ensure_preview_in(9, 0, &source, &dir.path().join("previews")).is_none());
    }

    #[test]
    fn regenerates_when_the_source_is_newer_than_the_cache() {
        let dir = tempdir().unwrap();
        let source = dir.path().join("source.png");
        let cache = dir.path().join("previews");
        write_png(&source, 320, 200, [255, 0, 0, 255]);
        assert!(ensure_preview_in(10, 0, &source, &cache).is_some());

        let cache_path = cache.join("10-0.webp");
        let older = fs::metadata(&cache_path).unwrap().modified().unwrap()
            - Duration::from_secs(2);
        set_mtime(&cache_path, older);
        write_png(&source, 320, 200, [0, 0, 255, 255]);

        let refreshed = ensure_preview_in(10, 0, &source, &cache).expect("regenerated");
        let pixel = refreshed.get_pixel(0, 0).0;
        assert!(pixel[2] > 240 && pixel[0] < 16 && pixel[1] < 16);
    }

    #[test]
    fn repairs_a_corrupt_cache_file() {
        let dir = tempdir().unwrap();
        let source = dir.path().join("source.png");
        let cache = dir.path().join("previews");
        write_png(&source, 320, 200, [200, 100, 50, 255]);
        assert!(ensure_preview_in(11, 0, &source, &cache).is_some());

        fs::write(cache.join("11-0.webp"), b"not an image").unwrap();
        let repaired = ensure_preview_in(11, 0, &source, &cache).expect("repaired");
        let pixel = repaired.get_pixel(0, 0).0;
        assert!(pixel[0] > 180 && pixel[1] > 80 && pixel[2] > 30);
    }

    #[test]
    fn prunes_only_the_oldest_cache_files() {
        let dir = tempdir().unwrap();
        let cache = dir.path().join("previews");
        fs::create_dir_all(&cache).unwrap();
        let unrelated = cache.join("keep.txt");
        fs::write(&unrelated, b"keep").unwrap();
        for index in 0..6 {
            let path = cache.join(format!("{index}-0.webp"));
            fs::write(&path, b"preview").unwrap();
            set_mtime(
                &path,
                SystemTime::UNIX_EPOCH + Duration::from_secs(index as u64 + 1),
            );
        }

        let removed = prune_previews_in(&cache, 4, 2).unwrap();
        assert_eq!(removed, 4);
        assert!(!cache.join("0-0.webp").exists());
        assert!(!cache.join("3-0.webp").exists());
        assert!(cache.join("4-0.webp").exists());
        assert!(cache.join("5-0.webp").exists());
        assert!(unrelated.exists());
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p vrc-photo-core preview`
Expected: FAIL because `crates/core/src/preview.rs` has no functions yet
(`cannot find function ...`).

- [ ] **Step 3: Implement the preview module**

Write the non-test part of `crates/core/src/preview.rs` with these exact
signatures and behavior:

```rust
pub const PREVIEW_MAX: u32 = 1200;
const PREVIEW_QUALITY: f32 = 85.0;
const PREVIEW_CACHE_MAX_FILES: usize = 2048;
const PREVIEW_CACHE_PRUNE_TO: usize = 1536;
const PRUNE_EVERY: usize = 32;

static WRITES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

pub fn previews_dir() -> Option<std::path::PathBuf> {
    crate::data_dir().map(|dir| dir.join("previews"))
}

pub fn decode_preview(source: &Path) -> Option<image::RgbaImage> {
    let image = crate::image_decode::decode_image(source)?;
    Some(image.thumbnail(PREVIEW_MAX, PREVIEW_MAX).to_rgba8())
}

pub fn ensure_preview(
    photo_id: i64,
    variant: i64,
    source: &Path,
) -> Option<image::RgbaImage> {
    let dir = previews_dir()?;
    ensure_preview_in(photo_id, variant, source, &dir)
}

pub fn ensure_preview_in(
    photo_id: i64,
    variant: i64,
    source: &Path,
    cache_dir: &Path,
) -> Option<image::RgbaImage> { /* behavior below */ }
```

Behavior for `ensure_preview_in`:

1. Compute `cache = cache_dir.join(format!("{photo_id}-{variant}.webp"))`.
2. If the cache exists with non-zero length and the source is either missing
   or not newer than the cache, decode the cache. Return it on success. If the
   decode fails, remove the cache and continue.
3. Decode the source with `decode_preview`. On failure, log
   `tracing::warn!` with the photo id and path and return `None`.
4. Encode the RGBA buffer to lossy WebP at `PREVIEW_QUALITY` with
   `webp::Encoder::from_rgba`, create the cache directory, write to
   `cache.with_extension("webp.tmp")`, and rename it over the cache. Log a
   warning and still return the image if any write step fails.
5. On a successful rename, call `maybe_prune(cache_dir)` and return the image.

Also implement:

```rust
fn cache_is_fresh(cache: &Path, source: &Path) -> bool
fn maybe_prune(cache_dir: &Path)
fn prune_previews_in(
    cache_dir: &Path,
    max_files: usize,
    prune_to: usize,
) -> std::io::Result<usize>
```

- `cache_is_fresh`: false when the cache is missing or zero bytes. When the
  source is missing, return true. Otherwise compare `modified()` values and
  return true when the source is not newer (or either mtime is unavailable).
- `maybe_prune`: increment `WRITES`; only run when the new value is a multiple
  of `PRUNE_EVERY`. Call `prune_previews_in(cache_dir,
  PREVIEW_CACHE_MAX_FILES, PREVIEW_CACHE_PRUNE_TO)` and log warnings, never
  propagate.
- `prune_previews_in`: collect only entries whose extension is `webp` with
  their mtimes; skip entries whose metadata cannot be read and return `Ok(0)`
  when the count is at or below `max_files`. Otherwise sort oldest first and
  delete `entries.len() - prune_to` files, ignoring per-file delete errors but
  returning the number actually removed. Return `Err` only when the cache
  directory itself cannot be read.

Add `pub mod preview;` to `crates/core/src/lib.rs`.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p vrc-photo-core preview`
Expected: PASS (6 tests).

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/preview.rs crates/core/src/lib.rs
git commit -m "feat(core): add 1200px preview cache"
```

---

### Task 2: Async selection pipeline

**Files:**
- Modify: `crates/ui/src/main.rs`

**Interfaces:**
- Consumes: `vrc_photo_core::preview::{decode_preview, ensure_preview_in, previews_dir}` and `PREVIEW_MAX` from Task 1.
- Produces:
  - `#[derive(Debug, Clone, Copy, PartialEq, Eq)] struct PreviewKey { photo_id: i64, variant: i64 }`
  - `fn preview_key(photo: &Photo) -> PreviewKey`
  - `#[derive(Debug, Default)] struct PreviewQueue { in_flight: Option<PreviewKey>, wanted: Option<PreviewKey> }` with `want`, `begin`, `finish`, `cancel_wanted`, `pending_for`.
  - App fields `preview_key: Option<PreviewKey>` and `preview_queue: PreviewQueue`.
  - App methods `request_preview(&mut self, cx: &mut Context<Self>)`, `pump_preview(&mut self, cx: &mut Context<Self>)`, `refresh_stale_preview(&mut self, cx: &mut Context<Self>)`.
  - `DemoAssets.preview_dir: PathBuf` so previews stay out of the real cache in demo mode.

- [ ] **Step 1: Write the failing queue tests**

Add to the existing `#[cfg(test)] mod tests` in `crates/ui/src/main.rs`:

```rust
#[test]
fn preview_queue_runs_one_decode_and_keeps_the_latest_wanted() {
    let mut queue = PreviewQueue::default();
    assert!(queue.want(PreviewKey { photo_id: 1, variant: 0 }));
    assert_eq!(
        queue.begin(),
        Some(PreviewKey { photo_id: 1, variant: 0 })
    );
    assert!(!queue.want(PreviewKey { photo_id: 2, variant: 0 }));
    assert_eq!(queue.begin(), None);
    queue.finish(PreviewKey { photo_id: 1, variant: 0 });
    assert_eq!(
        queue.begin(),
        Some(PreviewKey { photo_id: 2, variant: 0 })
    );
}

#[test]
fn preview_queue_re_requesting_the_in_flight_key_drops_the_queued_key() {
    let mut queue = PreviewQueue::default();
    assert!(queue.want(PreviewKey { photo_id: 1, variant: 0 }));
    queue.begin();
    assert!(!queue.want(PreviewKey { photo_id: 2, variant: 0 }));
    assert!(!queue.want(PreviewKey { photo_id: 1, variant: 0 }));
    queue.finish(PreviewKey { photo_id: 1, variant: 0 });
    assert_eq!(queue.begin(), None);
}

#[test]
fn preview_queue_pending_for_covers_queued_and_in_flight() {
    let mut queue = PreviewQueue::default();
    queue.want(PreviewKey { photo_id: 1, variant: 0 });
    assert!(queue.pending_for(1));
    queue.begin();
    assert!(queue.pending_for(1));
    queue.finish(PreviewKey { photo_id: 1, variant: 0 });
    assert!(!queue.pending_for(1));
}

#[test]
fn preview_key_uses_the_display_derivative() {
    let mut photo = test_photo(7);
    assert_eq!(
        preview_key(&photo),
        PreviewKey { photo_id: 7, variant: 0 }
    );
    photo.display_derivative_id = Some(42);
    assert_eq!(
        preview_key(&photo),
        PreviewKey { photo_id: 7, variant: 42 }
    );
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p vrc-photo-ui preview_queue preview_key`
Expected: FAIL because `PreviewQueue`, `PreviewKey`, and `preview_key` are not
defined.

- [ ] **Step 3: Implement the queue and key helpers**

Place these near `ThumbnailCache`:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PreviewKey {
    photo_id: i64,
    variant: i64,
}

fn preview_key(photo: &Photo) -> PreviewKey {
    PreviewKey {
        photo_id: photo.id,
        variant: photo.display_derivative_id.unwrap_or(0),
    }
}

#[derive(Debug, Default)]
struct PreviewQueue {
    in_flight: Option<PreviewKey>,
    wanted: Option<PreviewKey>,
}

impl PreviewQueue {
    /// Records `key` as the latest request. Returns true when the caller
    /// should start a decode immediately.
    fn want(&mut self, key: PreviewKey) -> bool {
        if self.in_flight == Some(key) {
            self.wanted = None;
            return false;
        }
        self.wanted = Some(key);
        self.in_flight.is_none()
    }

    /// Takes the queued key and marks it in flight. Returns None while another
    /// decode is still running.
    fn begin(&mut self) -> Option<PreviewKey> {
        if self.in_flight.is_some() {
            return None;
        }
        let key = self.wanted.take()?;
        self.in_flight = Some(key);
        Some(key)
    }

    /// Clears `key` if it is still the in-flight decode.
    fn finish(&mut self, key: PreviewKey) {
        if self.in_flight == Some(key) {
            self.in_flight = None;
        }
    }

    fn cancel_wanted(&mut self) {
        self.wanted = None;
    }

    fn pending_for(&self, photo_id: i64) -> bool {
        self.in_flight.is_some_and(|key| key.photo_id == photo_id)
            || self.wanted.is_some_and(|key| key.photo_id == photo_id)
    }
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p vrc-photo-ui preview_queue preview_key`
Expected: PASS (4 tests).

- [ ] **Step 5: Replace the synchronous preview load**

In `crates/ui/src/main.rs`:

- Add `use vrc_photo_core::preview;` next to the existing `thumbnail` import.
- Add `preview_key: Option<PreviewKey>` and `preview_queue: PreviewQueue` to the
  `App` struct next to `preview`, and `preview_key: None` /
  `preview_queue: PreviewQueue::default()` to the initializer.
- Add `preview_dir: PathBuf` to `DemoAssets`. In `prepare_demo`, create
  `root.join("previews")` next to `photos` and `thumbnails`, and set the field.
  Update the `DemoAssets` literal in `demo_thumb_path_uses_png_in_demo_dir`
  with `preview_dir: "/tmp/vrc-demo/previews".into()`.
- Replace `set_primary_photo` with this behavior:

```rust
fn set_primary_photo(&mut self, id: i64, cx: &mut Context<Self>) {
    if self.delete_armed != Some(id) {
        self.delete_armed = None;
    }
    let changed = self.selected != Some(id) || self.selected_photo.is_none();
    if changed {
        let previous_color = self
            .selected_photo
            .as_ref()
            .map(|photo| self.color_for(photo));
        self.previous_preview = self.preview.take();
        self.previous_color = previous_color;
        self.preview_key = None;
        self.selection_epoch = self.selection_epoch.wrapping_add(1);
        self.selection_changed_at = Some(Instant::now());
        self.load_selected_meta(id);
        self.request_preview(cx);
    } else if self.preview_key.is_none() && !self.preview_queue.pending_for(id) {
        // A previous decode failed; a fresh click retries it.
        self.request_preview(cx);
    }
    self.thumbnails.pin(id);
    if self.thumbnails.get(id).is_none()
        && let Some(photo) = self.selected_photo.clone()
    {
        self.request_thumbnail(&photo, cx);
    }
}
```

- Remove `load_selected` and the free function `load_preview`.
- Add `request_preview`, `pump_preview`, and `refresh_stale_preview`:

```rust
fn request_preview(&mut self, cx: &mut Context<Self>) {
    let Some(photo) = self.selected_photo.as_ref() else {
        return;
    };
    let key = preview_key(photo);
    if self.preview_key == Some(key) {
        self.preview_queue.cancel_wanted();
        return;
    }
    if self.preview_queue.want(key) {
        self.pump_preview(cx);
    }
}

fn pump_preview(&mut self, cx: &mut Context<Self>) {
    loop {
        let Some(key) = self.preview_queue.begin() else {
            return;
        };
        let matches = self
            .selected_photo
            .as_ref()
            .is_some_and(|photo| preview_key(photo) == key);
        if !matches || self.preview_key == Some(key) {
            self.preview_queue.finish(key);
            continue;
        }
        let source = PathBuf::from(
            self.selected_photo
                .as_ref()
                .expect("selected photo")
                .effective_path(),
        );
        let cache_dir = self
            .demo_assets
            .as_ref()
            .map(|assets| assets.preview_dir.clone())
            .or_else(preview::previews_dir);
        cx.spawn(async move |this, cx| {
            let decoded = cx
                .background_executor()
                .spawn(async move {
                    let start = Instant::now();
                    let decoded = match cache_dir {
                        Some(dir) => {
                            preview::ensure_preview_in(key.photo_id, key.variant, &source, &dir)
                        }
                        None => preview::decode_preview(&source),
                    };
                    log_timing("preview decode", start);
                    decoded
                })
                .await;
            this.update(cx, |this, cx| {
                this.preview_queue.finish(key);
                let current = this
                    .selected_photo
                    .as_ref()
                    .is_some_and(|photo| preview_key(photo) == key);
                if current && let Some(rgba) = decoded {
                    this.preview = Some(preview_render_image(rgba));
                    this.preview_key = Some(key);
                }
                this.pump_preview(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
        return;
    }
}

fn refresh_stale_preview(&mut self, cx: &mut Context<Self>) {
    let Some(photo) = self.selected_photo.as_ref() else {
        return;
    };
    let key = preview_key(photo);
    if self.preview_key.is_some_and(|loaded| loaded != key) {
        self.request_preview(cx);
    }
}
```

- Add the channel-swap helper next to `make_render_image`:

```rust
fn preview_render_image(mut rgba: image::RgbaImage) -> Arc<RenderImage> {
    for pixel in rgba.pixels_mut() {
        pixel.0.swap(0, 2);
    }
    make_render_image(rgba)
}
```

- In `select_photo`, wrap the existing body with `let start = Instant::now();`
  and `log_timing("select_photo", start);` before `cx.notify()`.
- In `reload_aux`, replace `self.load_selected(id);` with
  `self.load_selected_meta(id); self.refresh_stale_preview(cx);`.
- In `clear_selection`, add `self.preview_key = None;` and
  `self.preview_queue.cancel_wanted();` next to the preview clears.

- [ ] **Step 6: Run the UI tests**

Run: `cargo test -p vrc-photo-ui`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add crates/ui/src/main.rs
git commit -m "feat(ui): load previews asynchronously"
```

---

### Task 3: Loading state, re-selection skip, and stale refresh

**Files:**
- Modify: `crates/ui/src/main.rs`

**Interfaces:**
- Consumes: `PreviewQueue::pending_for`, `refresh_stale_preview`, `preview_key` from Task 2.
- Produces: no new public interface; behavior only.

- [ ] **Step 1: Add the stale-refresh call to the live tick**

In `refresh_live`, after `self.load_selected_meta(id);` inside
`if let Some(id) = self.selected`, add:

```rust
self.refresh_stale_preview(cx);
```

- [ ] **Step 2: Show a loading label while the preview is pending**

In the inspector preview frame, replace the unconditional
`プレビューを読み込めません` block with:

```rust
if current_preview.is_none() {
    let label = if self.preview_queue.pending_for(id) {
        "プレビューを読み込み中…"
    } else {
        "プレビューを読み込めません"
    };
    preview_frame = preview_frame.child(
        div()
            .absolute()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .text_size(px(10.))
            .text_color(rgb(MUTED_2))
            .child(label),
    );
}
```

- [ ] **Step 3: Verify same-photo clicks skip the decode**

With the Task 2 `set_primary_photo` body, clicking the selected card keeps
`preview_key == Some(key)`, so `request_preview` cancels any queued key and
returns. Confirm this by reading the path; do not add a new test for the UI
handler.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p vrc-photo-ui`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/ui/src/main.rs
git commit -m "feat(ui): keep selection instant while previews load"
```

---

### Task 4: Import preview warm-up

**Files:**
- Modify: `crates/ui/src/main.rs`

**Interfaces:**
- Consumes: `preview::ensure_preview_in`, `PreviewKey`, `preview_key`, and `DemoAssets.preview_dir` from Tasks 1–2.
- Produces:
  - `fn warm_previews(&mut self, jobs: Vec<(PreviewKey, PathBuf)>, cx: &mut Context<Self>)`

- [ ] **Step 1: Warm previews after import**

In `index_import_destination`, while iterating `scan.new_ids` for thumbnails,
collect preview jobs:

```rust
let mut preview_jobs = Vec::new();
for id in &scan.new_ids {
    let Ok(Some(photo)) = self.db.photo_by_id(*id) else {
        continue;
    };
    preview_jobs.push((preview_key(&photo), PathBuf::from(photo.effective_path())));
    if let Err(error) =
        thumbnail::ensure_thumbnail(*id, Path::new(photo.effective_path()))
    {
        tracing::warn!(%error, %id, "cannot generate thumbnail after import");
    }
}
self.warm_previews(preview_jobs, cx);
```

Add:

```rust
fn warm_previews(&mut self, jobs: Vec<(PreviewKey, PathBuf)>, cx: &mut Context<Self>) {
    if jobs.is_empty() {
        return;
    }
    let cache_dir = self
        .demo_assets
        .as_ref()
        .map(|assets| assets.preview_dir.clone())
        .or_else(preview::previews_dir);
    let Some(cache_dir) = cache_dir else {
        return;
    };
    cx.background_executor()
        .spawn(async move {
            for (key, source) in jobs {
                let start = Instant::now();
                let _ = preview::ensure_preview_in(
                    key.photo_id,
                    key.variant,
                    &source,
                    &cache_dir,
                );
                log_timing("preview warm", start);
            }
        })
        .detach();
}
```

- [ ] **Step 2: Run the tests**

Run: `cargo test -p vrc-photo-ui`
Expected: PASS.

- [ ] **Step 3: Commit**

```bash
git add crates/ui/src/main.rs
git commit -m "feat(ui): warm previews for imported photos"
```

---

### Task 5: Verification

**Files:** none (verification only)

- [ ] **Step 1: Run formatting, clippy, and the full suite**

```bash
cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace
```

Expected: all commands exit 0.

- [ ] **Step 2: Measure the selection path**

Build the debug binary and run the demo with timing enabled:

```bash
cargo build -p vrc-photo-ui
DISPLAY=:0 VRC_PHOTO_TIMING=1 ./target/debug/vrc-photo-manager --demo --demo-reset --window-size 1280x820
```

Expected in the log:

- `select_photo` is at or below a few milliseconds.
- `preview decode` appears after selection, from the background worker.
- Selecting the same photo again produces no second `preview decode`.
- Clicking different photos shows `photo card click total` in the sub-millisecond
  range (the probe can be added temporarily if manual verification needs it;
  do not commit it).
- The inspector shows `プレビューを読み込み中…` only until the preview arrives.

- [ ] **Step 3: Update user-facing docs in a separate docs PR**

After the feature PR merges, create a `docs/async-preview` worktree/branch and
update:

- `docs/ui.md`: previews decode asynchronously with one in-flight job; the
  selection highlight is immediate; the inspector shows the loading label;
  preview cache path/format/cap; the 260 ms cross-fade may finish before a
  slow first decode.
- `docs/security-privacy.md`: add a `previews/` row (1200 px WebP, local cache,
  regenerable).
- `docs/data-model.md`: document `previews/<photo_id>-<variant>.webp` and that
  it is not part of backups.
- `docs/HANDOFF.md`: replace the `load_render` / `load_preview` note with the
  new `preview` module, `make_render_image`, and the background pipeline.

Expected: docs-only PR, no code changes.
