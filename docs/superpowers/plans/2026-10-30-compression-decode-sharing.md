# Compression memory peak and per-photo decode sharing — implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Remove redundant full-image copies from the compression encoders and decode each photo once per compression batch, without losing AVIF alpha or changing replace-policy input semantics.

**Architecture:** `vrc-photo-core` borrows `&DynamicImage` through resize and encode, using raw-buffer reinterpretation for AVIF RGBA and direct buffers for WebP/JPEG/JXL. The daemon claims all pending jobs for the oldest pending photo in one DB batch, decodes once, and processes profiles sequentially. `replace` input paths are frozen at batch start; forced deletion cancels the remaining claimed jobs between encodes.

**Tech Stack:** Rust 2024, `image` 0.25, `ravif` 0.13 (`rgb` + `bytemuck`), `webp` 0.3, `jpegxl-rs` 0.14, SQLite (`rusqlite`), `tempfile` in tests.

**Spec:** `docs/superpowers/specs/2026-10-30-compression-decode-sharing-design.md`

## Global Constraints

- AVIF must always use `ravif::Encoder::encode_rgba`; never switch RGB inputs to `encode_rgb`. Transparency must survive.
- AVIF RGBA input must not allocate a second full-size pixel buffer. RGB input may allocate one RGBA conversion, but must not collect a `Vec<rgb::RGBA8>`.
- `compress_decoded` must borrow the decoded image; callers must be able to compress the same image with two profiles.
- A batch is all pending jobs for the oldest pending photo. Jobs enqueued after the claim are a separate batch and may decode again.
- The source path is resolved once per batch using the existing rule: `photo.abs_path` if it is a file, otherwise `photo.effective_path()`. Every job in the batch uses that frozen path.
- A failure in one job must not fail the other jobs in the batch. A batch setup failure (missing photo, decode failure) fails every job in the batch.
- Forced deletion (`deletion_requests.force = 1`, status `pending`/`blocked`) cancels the remaining claimed jobs between encodes; the job currently being processed finishes.
- `compress_pending` must not start compression workers; only the calling thread claims jobs.
- No DB schema change, no IPC protocol change, no new product settings.
- Product docs are a separate docs-only PR per `AGENTS.md` §4. Do not include `docs/` changes in the code PR.
- Use `feat:` for code commits and `docs:` for the documentation PR.

## Review Focus

- **AVIF input with transparent pixels.** Expected: the output keeps an alpha channel; transparent pixels stay transparent and opaque pixels stay opaque.
- **A photo with a `replace` job followed by another profile job in the same batch.** Expected: the second job still reads the original file, not the first job's derivative.
- **One profile fails while another succeeds for the same photo.** Expected: the successful derivative is recorded and the failed job alone reports the error.
- **A forced delete arrives after the batch is claimed.** Expected: the current encode finishes, remaining jobs are cancelled, and deletion is no longer blocked by them.
- **An RGB8 input to AVIF/WebP/JPEG/JXL.** Expected: the encoder receives the correct channel order and produces a decodable image, even though one conversion may be required.

---

## Prerequisites

- Merge the design/plan docs-only PR first, then cut the implementation worktree from updated `main`:

```bash
REPO=$(basename "$(git rev-parse --show-toplevel)")
git worktree add ../${REPO}-compression-decode-sharing-feat -b feat/compression-decode-sharing
```

- Open a separate session in that worktree and implement Tasks 1–4 there. Task 5 is a separate docs-only PR after the code merge.
- Baseline local test command for the worktree: `cargo test -p vrc-photo-core && cargo test -p vrc-photo-daemon`.

---

### Task 1: Borrow decoded images and remove encoder copies

**Files:**
- Modify: `Cargo.toml` (workspace `rgb` feature)
- Modify: `crates/core/src/compressor.rs`
- Test: inline `#[cfg(test)] mod tests` in `crates/core/src/compressor.rs`

**Interfaces:**
- Changes `pub fn compress_decoded(image: &DynamicImage, source: &Path, profile: &CompressionProfile) -> Result<CompressionResult>` (was `image: DynamicImage`).
- Produces private `fn resize_for_profile<'a>(image: &'a DynamicImage, profile: &CompressionProfile) -> Cow<'a, DynamicImage>`.
- `encode` and all four `encode_*` functions keep their names and take `&DynamicImage`.
- No public encoder API is added.

- [ ] **Step 1: Write the failing borrow test**

Add to the existing `compressor` tests:

```rust
#[test]
fn compress_decoded_can_use_the_same_image_twice() {
    let dir = tempdir().unwrap();
    let source = dir.path().join("VRChat_2026-10-21_21-14-03.221_16x10.png");
    sample(&source);
    let image = crate::image_decode::decode_image(&source).unwrap();

    let first = compress_decoded(&image, &source, &profile(CompressionFormat::Webp)).unwrap();
    let second = compress_decoded(&image, &source, &profile(CompressionFormat::Webp)).unwrap();

    assert!(first.output_path.is_file());
    assert!(second.output_path.is_file());
    assert_ne!(first.output_path, second.output_path);
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p vrc-photo-core compress_decoded_can_use_the_same_image_twice`

Expected: compile error because `compress_decoded` takes `DynamicImage`, not `&DynamicImage`.

- [ ] **Step 3: Change the API to borrow and preserve resize behavior**

In `crates/core/src/compressor.rs`:

- Import `std::borrow::Cow`.
- Change `compress_decoded` to take `&DynamicImage` and call `resize_for_profile(image, profile)`.
- `resize_for_profile` returns `Cow::Borrowed(image)` when resize is disabled, the profile bounds are zero, or `DownscaleOnly` already fits; otherwise returns `Cow::Owned(image.resize(...))` / `resize_exact(...)` using the existing Lanczos3 calls.
- `compress_image` decodes, then calls `compress_decoded(&image, source, profile)`.
- `render_clipboard_png` passes `&image` to `resize_for_profile` and writes the resulting `Cow` as before.
- Width/height and `encode(&image, profile)` read through the `Cow`.

Do not change output paths, suffix sanitizing, or atomic writes.

- [ ] **Step 4: Run the core compressor tests to verify they pass**

Run: `cargo test -p vrc-photo-core compressor`

Expected: the new borrow test and all existing compressor tests pass.

- [ ] **Step 5: Write encoder regression tests before refactoring the encoders**

Add these tests (they should pass on the current implementation and must stay green after the refactor):

```rust
#[test]
fn avif_keeps_transparency() {
    let dir = tempdir().unwrap();
    let source = dir.path().join("alpha.png");
    let image = RgbaImage::from_fn(8, 8, |x, _| {
        if x < 4 {
            Rgba([255, 0, 0, 0])
        } else {
            Rgba([0, 0, 255, 255])
        }
    });
    image.save(&source).unwrap();
    let mut profile = profile(CompressionFormat::Avif);
    profile.lossless = true;

    let result = compress_image(&source, &profile).unwrap();
    let decoded = crate::image_decode::decode_image(&result.output_path)
        .unwrap()
        .to_rgba8();

    assert!(decoded.get_pixel(0, 0)[3] < 32, "transparent pixel lost alpha");
    assert!(decoded.get_pixel(7, 0)[3] > 223, "opaque pixel lost alpha");
    assert!(decoded.get_pixel(0, 0)[0] > 200, "channel order changed");
    assert!(decoded.get_pixel(7, 0)[2] > 200, "channel order changed");
}

#[test]
fn jpeg_rgba_fast_path_matches_rgb_conversion() {
    let rgba = DynamicImage::ImageRgba8(RgbaImage::from_fn(8, 8, |x, y| {
        Rgba([(x * 31) as u8, (y * 31) as u8, ((x + y) * 15) as u8, 255])
    }));
    let rgb = DynamicImage::ImageRgb8(rgba.to_rgb8());

    assert_eq!(encode_jpeg(&rgba, 85).unwrap(), encode_jpeg(&rgb, 85).unwrap());
}

#[test]
fn webp_rgb_fast_path_matches_rgba_conversion() {
    let rgb = DynamicImage::ImageRgb8(RgbImage::from_fn(8, 8, |x, y| {
        Rgb([(x * 31) as u8, (y * 31) as u8, ((x + y) * 15) as u8])
    }));
    let rgba = DynamicImage::ImageRgba8(rgb.to_rgba8());

    assert_eq!(
        encode_webp(&rgb, 80, false).unwrap(),
        encode_webp(&rgba, 80, false).unwrap()
    );
}

#[test]
fn jxl_rgb_fast_path_matches_rgba_conversion() {
    let rgb = DynamicImage::ImageRgb8(RgbImage::from_fn(8, 8, |x, y| {
        Rgb([(x * 31) as u8, (y * 31) as u8, ((x + y) * 15) as u8])
    }));
    let rgba = DynamicImage::ImageRgba8(rgb.to_rgba8());

    assert_eq!(encode_jxl(&rgb, 80, false, 8).unwrap(), encode_jxl(&rgba, 80, false, 8).unwrap());
}

#[test]
fn avif_rgba_fast_path_matches_rgb_conversion() {
    let rgba = DynamicImage::ImageRgba8(RgbaImage::from_fn(8, 8, |x, y| {
        Rgba([(x * 31) as u8, (y * 31) as u8, ((x + y) * 15) as u8, 255])
    }));
    let rgb = DynamicImage::ImageRgb8(rgba.to_rgb8());

    assert_eq!(
        encode_avif(&rgba, 60, false, 8).unwrap(),
        encode_avif(&rgb, 60, false, 8).unwrap()
    );
}
```

Update the test imports to include `Rgb` and `RgbImage`.

- [ ] **Step 6: Run the new encoder tests on the current implementation**

Run: `cargo test -p vrc-photo-core avif_keeps_transparency jpeg_rgba_fast_path webp_rgb_fast_path jxl_rgb_fast_path avif_rgba_fast_path`

Expected: all pass. These pin the output behavior before the copy-removal refactor.

- [ ] **Step 7: Enable `rgb`'s bytemuck support**

In the workspace `Cargo.toml`, change:

```toml
rgb = { version = "0.8", features = ["bytemuck"] }
```

Use `rgb::bytemuck::cast_slice`; do not add a direct `bytemuck` dependency.

- [ ] **Step 8: Rewrite the four encoders to use borrowed buffers**

In `crates/core/src/compressor.rs`:

- `encode_avif`:
  - Build a `Cow<'_, [u8]>`: `Cow::Borrowed(rgba.as_raw())` when `image.as_rgba8()` is `Some`, otherwise `Cow::Owned(image.to_rgba8().into_raw())`.
  - `let pixels: &[rgb::RGBA<u8>] = rgb::bytemuck::cast_slice(&rgba);`
  - Build `imgref::Img::new(pixels, image.width() as usize, image.height() as usize)`.
  - Keep the existing quality/lossless logic and `encode_rgba`.
  - Delete the `Vec<rgb::RGBA8>` collect.
- `encode_webp`:
  - First try `webp::Encoder::from_image(image)`; if `Ok`, encode lossless or lossy.
  - Otherwise fall back to the existing `to_rgba8()` + `from_rgba` path.
- `encode_jpeg`:
  - Match `DynamicImage::ImageRgb8(rgb)` and `DynamicImage::ImageRgba8(rgba)` and call `encoder.encode_image(rgb)` / `encoder.encode_image(rgba)`.
  - Otherwise use the existing `to_rgb8()` and `encode_image(&rgb)`.
- `encode_jxl`:
  - Use `Cow<'_, [u8]>`: `image.as_rgb8()` raw bytes when available, otherwise `image.to_rgb8().into_raw()`.
  - Pass `rgb.as_ref()` to `encoder.encode`.
  - Keep speed mapping and lossless handling unchanged.

- [ ] **Step 9: Run the core compressor tests to verify they pass**

Run: `cargo test -p vrc-photo-core compressor`

Expected: all compressor tests, including the five regression tests and the borrow test, pass.

- [ ] **Step 10: Commit**

```bash
git add Cargo.toml crates/core/src/compressor.rs
git commit -m "feat: avoid redundant copies when compressing images"
```

---

### Task 2: Claim all pending jobs for the oldest pending photo

**Files:**
- Modify: `crates/core/src/db.rs`
- Test: inline `#[cfg(test)] mod tests` in `crates/core/src/db.rs`

**Interfaces:**
- Produces `pub fn next_pending_photo_jobs(&self) -> Result<Vec<CompressionJob>>`
  - Returns all pending jobs for the photo owning the oldest pending job, in enqueue (`rowid`) order.
  - Returns an empty `Vec` when no pending jobs exist.
  - Marks the returned jobs `processing` and sets `started_at`.
- Produces `pub fn photo_has_forced_deletion(&self, photo_id: i64) -> Result<bool>`
  - True when a `deletion_requests` row exists for the photo with `force = 1` and status `pending` or `blocked`.
- Keeps `next_pending_job()` unchanged for existing tests and other callers.

- [ ] **Step 1: Write the failing DB tests**

Add near the existing queue tests:

```rust
#[test]
fn next_pending_photo_jobs_claims_the_oldest_photos_jobs() {
    let db = Db::open_in_memory().unwrap();
    let root = db.add_root("/photos", "reference").unwrap();
    let first = db
        .upsert_photo(&sample_photo(root, "/photos/a.png", 100))
        .unwrap();
    let second = db
        .upsert_photo(&sample_photo(root, "/photos/b.png", 200))
        .unwrap();
    db.save_profile(&profile("p", "P")).unwrap();
    let first_a = db.enqueue_job(first, "p", "keep").unwrap();
    let first_b = db.enqueue_job(first, "p", "keep").unwrap();
    let waiting = db
        .enqueue_job_with_status(first, "p", "keep", QueueStatus::Waiting)
        .unwrap();
    let second_a = db.enqueue_job(second, "p", "keep").unwrap();

    let batch = db.next_pending_photo_jobs().unwrap();
    assert_eq!(
        batch.iter().map(|job| job.id.as_str()).collect::<Vec<_>>(),
        vec![first_a.as_str(), first_b.as_str()]
    );
    assert!(batch.iter().all(|job| job.status == QueueStatus::Processing));
    assert!(batch.iter().all(|job| job.started_at.is_some()));

    let next = db.next_pending_photo_jobs().unwrap();
    assert_eq!(next.len(), 1);
    assert_eq!(next[0].id, second_a);
    assert!(db.next_pending_photo_jobs().unwrap().is_empty());

    let waiting = db
        .list_jobs(10)
        .unwrap()
        .into_iter()
        .find(|job| job.id == waiting)
        .unwrap();
    assert_eq!(waiting.status, QueueStatus::Waiting);
}

#[test]
fn photo_has_forced_deletion_tracks_the_force_flag() {
    let db = Db::open_in_memory().unwrap();
    let id = seed_photo(&db);
    let request = db
        .request_deletion(&[id], DeletionReason::Manual, false)
        .unwrap()[0];

    assert!(!db.photo_has_forced_deletion(id).unwrap());
    assert!(db.force_deletion(request).unwrap());
    assert!(db.photo_has_forced_deletion(id).unwrap());
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p vrc-photo-core next_pending_photo_jobs photo_has_forced_deletion`

Expected: compile errors because both methods do not exist.

- [ ] **Step 3: Implement the batch claim**

In `crates/core/src/db.rs`:

- Read the oldest pending `photo_id`:

```sql
SELECT photo_id FROM compression_jobs
WHERE status = 'pending'
ORDER BY created_at, rowid
LIMIT 1
```

- If it is absent, return `Ok(Vec::new())`.
- Otherwise claim every pending job for that photo with one `UPDATE ... RETURNING`:

```sql
UPDATE compression_jobs
SET status = 'processing', started_at = ?1
WHERE status = 'pending' AND photo_id = ?2
RETURNING id, photo_id, profile_id, status, progress, error, output_path, policy,
  output_dir, retry_count, created_at, started_at, finished_at, rowid
```

- Map rows with `row_to_job` (which ignores the extra trailing `rowid` column), collect `(rowid, job)`, sort by `rowid`, and return the jobs.
- The daemon stores `Db` behind a single mutex, so the SELECT/UPDATE pair cannot interleave with another worker. A job enqueued for the same photo between the two statements is included; a job for another photo waits for the next batch.

- [ ] **Step 4: Implement the forced-deletion query**

Add:

```rust
pub fn photo_has_forced_deletion(&self, photo_id: i64) -> Result<bool> {
    let exists: i64 = self.conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM deletion_requests \
         WHERE photo_id = ?1 AND force = 1 AND status IN ('pending', 'blocked'))",
        params![photo_id],
        |row| row.get(0),
    )?;
    Ok(exists != 0)
}
```

- [ ] **Step 5: Run the DB tests to verify they pass**

Run: `cargo test -p vrc-photo-core next_pending_photo_jobs photo_has_forced_deletion`

Expected: both new tests pass. Then run the full core suite:

Run: `cargo test -p vrc-photo-core`

Expected: all core tests pass, including the unchanged `next_pending_job` tests.

- [ ] **Step 6: Commit**

```bash
git add crates/core/src/db.rs
git commit -m "feat: claim compression jobs per photo"
```

---

### Task 3: Process one photo batch and share the decoded image

**Files:**
- Modify: `crates/daemon/Cargo.toml`
- Modify: `crates/daemon/src/lib.rs`
- Test: inline `#[cfg(test)] mod tests` in `crates/daemon/src/lib.rs`

**Interfaces:**
- Adds `image = { version = "0.25", default-features = false, features = ["png"] }` to `[dependencies]`; remove the duplicate from `[dev-dependencies]`.
- Adds private:

```rust
struct JobOutcome {
    job: CompressionJob,
    status: QueueStatus,
    error: Option<String>,
}

fn process_photo_batch(
    db: &Arc<Mutex<Db>>,
    jobs: &[CompressionJob],
) -> Result<Vec<JobOutcome>>
```

- `process_photo_batch` records each job's terminal status (`Completed`, `Failed`, `Cancelled`) itself and returns one outcome per input job.
- Keeps `fn process_job(db: &Arc<Mutex<Db>>, job: &CompressionJob) -> Result<()>` as a single-job wrapper over `process_photo_batch` for existing tests.
- `worker_loop` publishes `Processing` for every claimed job, then `Completed` / `Failed` / `Cancelled` per outcome.
- `compress_pending` no longer calls `start_workers` / `stop_workers`.

- [ ] **Step 1: Write the failing daemon tests**

Add these helpers/tests in the daemon test module:

```rust
fn save_batch_profile(
    db: &Db,
    id: &str,
    format: vrc_photo_core::model::CompressionFormat,
    suffix: &str,
    resize: Option<(u32, u32)>,
) {
    db.save_profile(&CompressionProfile {
        id: id.to_string(),
        name: id.to_string(),
        format,
        suffix: suffix.to_string(),
        resize_enabled: resize.is_some(),
        width: resize.map(|(w, _)| w).unwrap_or(3840),
        height: resize.map(|(_, h)| h).unwrap_or(2160),
        ..CompressionProfile::default()
    })
    .unwrap();
}

#[test]
fn compress_pending_processes_every_profile_for_one_photo() {
    let dir = tempfile::tempdir().unwrap();
    let mut daemon = manual_test_daemon(&dir);
    daemon.run_once().unwrap();
    let db = Db::open(dir.path().join("library.db")).unwrap();
    let photo = db
        .photo_by_path(original_path(&dir).to_str().unwrap())
        .unwrap()
        .unwrap();
    save_batch_profile(&db, "webp", vrc_photo_core::model::CompressionFormat::Webp, "_webp", None);
    save_batch_profile(&db, "jpeg", vrc_photo_core::model::CompressionFormat::Jpeg, "_jpeg", None);
    db.enqueue_job(photo.id, "webp", "keep").unwrap();
    db.enqueue_job(photo.id, "jpeg", "keep").unwrap();

    daemon.compress_pending().unwrap();

    assert!(
        daemon.running.load(Ordering::Relaxed),
        "compress_pending must not start and stop the worker set"
    );
    assert!(daemon.workers.is_empty());
    let db = Db::open(dir.path().join("library.db")).unwrap();
    let jobs = db.list_jobs(10).unwrap();
    assert_eq!(jobs.len(), 2);
    assert!(jobs.iter().all(|job| job.status == QueueStatus::Completed));
    assert_eq!(db.derivatives_for_photo(photo.id).unwrap().len(), 2);
}

#[test]
fn batch_keeps_the_original_source_after_a_replace_job() {
    let dir = tempfile::tempdir().unwrap();
    let mut daemon = manual_test_daemon(&dir);
    daemon.run_once().unwrap();
    let db = Db::open(dir.path().join("library.db")).unwrap();
    let photo = db
        .photo_by_path(original_path(&dir).to_str().unwrap())
        .unwrap()
        .unwrap();
    save_batch_profile(&db, "small", vrc_photo_core::model::CompressionFormat::Webp, "_small", Some((8, 8)));
    save_batch_profile(&db, "full", vrc_photo_core::model::CompressionFormat::Webp, "_full", None);
    db.enqueue_job(photo.id, "small", "replace").unwrap();
    db.enqueue_job(photo.id, "full", "keep").unwrap();

    daemon.compress_pending().unwrap();

    let db = Db::open(dir.path().join("library.db")).unwrap();
    let derivatives = db.derivatives_for_photo(photo.id).unwrap();
    let small = derivatives.iter().find(|d| d.profile_id == "small").unwrap();
    let full = derivatives.iter().find(|d| d.profile_id == "full").unwrap();
    assert_eq!((small.width, small.height), (8, 5));
    assert_eq!((full.width, full.height), (16, 10), "second job read the derivative");
}

#[test]
fn batch_continues_after_one_profile_fails() {
    let dir = tempfile::tempdir().unwrap();
    let mut daemon = manual_test_daemon(&dir);
    daemon.run_once().unwrap();
    let blocked = dir.path().join("photos/not-a-directory");
    std::fs::write(&blocked, b"x").unwrap();
    let db = Db::open(dir.path().join("library.db")).unwrap();
    let photo = db
        .photo_by_path(original_path(&dir).to_str().unwrap())
        .unwrap()
        .unwrap();
    save_batch_profile(&db, "good", vrc_photo_core::model::CompressionFormat::Webp, "_good", None);
    save_batch_profile(&db, "bad", vrc_photo_core::model::CompressionFormat::Webp, "_bad", None);
    db.enqueue_job(photo.id, "good", "keep").unwrap();
    db.enqueue_job_with_output_dir(photo.id, "bad", "keep", Some(blocked.to_str().unwrap()))
        .unwrap();

    daemon.compress_pending().unwrap();

    let db = Db::open(dir.path().join("library.db")).unwrap();
    let jobs = db.list_jobs(10).unwrap();
    let good = jobs.iter().find(|job| job.profile_id == "good").unwrap();
    let bad = jobs.iter().find(|job| job.profile_id == "bad").unwrap();
    assert_eq!(good.status, QueueStatus::Completed);
    assert_eq!(bad.status, QueueStatus::Failed);
    assert_eq!(db.derivatives_for_photo(photo.id).unwrap().len(), 1);
}

#[test]
fn forced_deletion_cancels_the_rest_of_the_batch() {
    let dir = tempfile::tempdir().unwrap();
    let mut daemon = manual_test_daemon(&dir);
    daemon.run_once().unwrap();
    let db = Db::open(dir.path().join("library.db")).unwrap();
    let photo = db
        .photo_by_path(original_path(&dir).to_str().unwrap())
        .unwrap()
        .unwrap();
    save_batch_profile(&db, "first", vrc_photo_core::model::CompressionFormat::Webp, "_first", None);
    save_batch_profile(&db, "second", vrc_photo_core::model::CompressionFormat::Webp, "_second", None);
    db.enqueue_job(photo.id, "first", "keep").unwrap();
    db.enqueue_job(photo.id, "second", "keep").unwrap();
    let jobs = db.next_pending_photo_jobs().unwrap();
    assert_eq!(jobs.len(), 2);
    let request = db
        .request_deletion(&[photo.id], DeletionReason::Manual, false)
        .unwrap()[0];
    assert!(db.force_deletion(request).unwrap());

    let outcomes = process_photo_batch(&daemon.db, &jobs).unwrap();

    assert_eq!(outcomes[0].status, QueueStatus::Completed);
    assert_eq!(outcomes[1].status, QueueStatus::Cancelled);
    assert!(db.claim_deletion(request).unwrap().is_some());
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p vrc-photo-daemon compress_pending_processes_every_profile batch_keeps_the_original_source batch_continues_after_one_profile forced_deletion_cancels_the_rest`

Expected: compile errors because `next_pending_photo_jobs` exists in core but `process_photo_batch` and `JobOutcome` do not, and `compress_pending` still starts workers.

- [ ] **Step 3: Add the `image` dependency**

In `crates/daemon/Cargo.toml`, move the `image` dependency from `[dev-dependencies]` to `[dependencies]` with the same version/features. Tests still use `image::RgbaImage` and `.save`.

- [ ] **Step 4: Implement `process_photo_batch`**

In `crates/daemon/src/lib.rs`, add `use image::DynamicImage;` to the imports, then add the `JobOutcome` struct and the batch function:

- Return an empty `Vec` for an empty input.
- Load the photo once with `db.photo_by_id(first.photo_id)`. If missing, mark every job `failed` with `"photo no longer exists"` and return outcomes.
- Resolve the frozen source path once:
  - `photo.abs_path` if `Path::new(&photo.abs_path).is_file()`
  - otherwise `photo.effective_path()`
- Fill `photos.blake3` once when it is `None`, the source is `abs_path`, and `hashing::file_blake3` succeeds (same as the old `process_job`).
- Decode once with `vrc_photo_core::image_decode::decode_image(&source_path)`. If it returns `None`, mark every job `failed` with `"cannot decode <path>"` and return outcomes.
- Loop over the jobs:
  - Before jobs 2..N (not before the first), call `db.photo_has_forced_deletion(photo.id)`. If true, mark every remaining job `cancelled` with `db.update_job(..., QueueStatus::Cancelled, 0.0, None, None)`, push `Cancelled` outcomes, and break.
  - Read the profile from `db.list_profiles()` by `job.profile_id`. Missing profile is a per-job failure.
  - Apply `job.output_dir` to the profile when it is non-empty.
  - Call `compressor::compress_decoded(&image, &source_path, &profile)`.
  - Keep the existing output validation (`is_file` and non-zero size), `db.add_derivative`, `replace` handling, `db.set_replaced`, `db.request_deletion`, `db.update_job(Completed)`, `resume_blocked_deletions_for_photo`, and `bump_library_revision` logic.
  - On a per-job error, record `Failed` with the error string, push the outcome, log `tracing::error!`, and continue with the next job.
- Log the same `compressed` info line per successful job (source, output, saved).

Keep the DB lock scoped around each read/write; never hold it during decode or encode.

- [ ] **Step 5: Rewrite `process_job` as the single-job wrapper**

```rust
fn process_job(db: &Arc<Mutex<Db>>, job: &CompressionJob) -> Result<()> {
    let mut outcomes = process_photo_batch(db, std::slice::from_ref(job))?;
    let outcome = outcomes.pop().context("compression produced no outcome")?;
    match outcome.status {
        QueueStatus::Completed => Ok(()),
        _ => Err(anyhow::anyhow!(
            outcome.error.unwrap_or_else(|| "compression failed".to_string())
        )),
    }
}
```

- [ ] **Step 6: Switch the worker and `compress_pending` to batches**

- `worker_loop`:
  - Replace `next_pending_job` with `next_pending_photo_jobs`, treating an empty `Vec` as no work.
  - Publish `Processing` / 0.05 for every job before calling `process_photo_batch`.
  - For each outcome publish `Completed` / `Failed` / `Cancelled` with the matching `JobStatus` and error string.
  - If `process_photo_batch` returns `Err`, log it, mark every job in the batch `failed`, and publish `Failed` for each. This mirrors the old single-job error path.
  - Keep the 400ms `jobs.wait` idle wait.
- `compress_pending`:
  - Remove `self.start_workers()` and `self.stop_workers()`.
  - Loop over `self.db.lock().unwrap().next_pending_photo_jobs()?` until empty.
  - Call `process_photo_batch(&self.db, &jobs)?` and ignore the returned outcomes (statuses are already recorded).
  - Call `self.process_deletions()?` at the end.

- [ ] **Step 7: Run the daemon tests to verify they pass**

Run: `cargo test -p vrc-photo-daemon compress_pending_processes_every_profile batch_keeps_the_original_source batch_continues_after_one_profile forced_deletion_cancels_the_rest`

Expected: all four pass. Then run the existing batch/replace tests:

Run: `cargo test -p vrc-photo-daemon multi_profile_replace_is_worker_count_independent failed_job_blocks_deletion compression_workers_process_a_batch`

Expected: all pass.

- [ ] **Step 8: Run the full daemon suite**

Run: `cargo test -p vrc-photo-daemon`

Expected: all daemon tests pass.

- [ ] **Step 9: Commit**

```bash
git add crates/daemon/Cargo.toml crates/daemon/src/lib.rs
git commit -m "feat: share decoded images across profile jobs"
```

---

### Task 4: Full verification, measurements, and code PR

**Files:**
- Verify only; no committed changes from measurements.

**Interfaces:**
- Consumes Tasks 1–3.
- Produces the #192 peak-RSS table and the #194 two-profile timing table in the PR body and issue comments.

- [ ] **Step 1: Run the full local gate**

Run:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Expected: all three commands exit 0. Investigate any warning or failure before pushing.

- [ ] **Step 2: Confirm the code PR contains no docs**

Run: `git diff --name-only main...HEAD`

Expected: `Cargo.toml`, `crates/core/src/compressor.rs`, `crates/core/src/db.rs`, `crates/daemon/Cargo.toml`, `crates/daemon/src/lib.rs`. No `docs/` paths.

- [ ] **Step 3: Measure #192 peak memory**

In both the baseline `main` worktree and the feature worktree, create a temporary `crates/core/examples/compression_peak.rs` (do not commit) with:

- A `generate <path> <width> <height> <rgb|rgba>` mode that writes a deterministic gradient PNG.
- A `compress <path> <jpeg|webp|avif|jxl>` mode that calls `compress_image` with quality 60, speed 8, suffix `_bench`, resize disabled, and prints the output size.

Build with:

```bash
cargo build -p vrc-photo-core --release --example compression_peak
```

Generate the shared inputs outside the measured process:

```bash
mkdir -p /tmp/opencode/compression-bench
target/release/examples/compression_peak generate /tmp/opencode/compression-bench/4k-rgb.png 3840 2160 rgb
target/release/examples/compression_peak generate /tmp/opencode/compression-bench/4k-rgba.png 3840 2160 rgba
target/release/examples/compression_peak generate /tmp/opencode/compression-bench/8k-rgb.png 7680 4320 rgb
target/release/examples/compression_peak generate /tmp/opencode/compression-bench/8k-rgba.png 7680 4320 rgba
```

Run each format and size in a fresh process, on `main` and on the feature branch:

```bash
/usr/bin/time -v target/release/examples/compression_peak compress <input> <format>
```

Record `Maximum resident set size (kbytes)` for JPEG/WebP/AVIF/JXL × 4K/8K × RGB/RGBA. Keep the same input files for both branches. Delete the temporary example before committing.

- [ ] **Step 4: Measure #194 two-profile time**

In both the baseline `main` worktree and the feature worktree, add a temporary ignored test to `crates/daemon/src/lib.rs` (do not commit):

```rust
#[test]
#[ignore]
fn measure_two_profile_compression() {
    let dir = tempfile::tempdir().unwrap();
    let mut daemon = manual_test_daemon(&dir);
    let photo_path = original_path(&dir);
    // Replace the 16x10 sample with a 4K PNG before run_once.
    let image = image::RgbImage::from_fn(3840, 2160, |x, y| {
        image::Rgb([(x % 256) as u8, (y % 256) as u8, ((x + y) % 256) as u8])
    });
    image.save(&photo_path).unwrap();
    daemon.run_once().unwrap();
    let db = Db::open(dir.path().join("library.db")).unwrap();
    let photo = db.photo_by_path(photo_path.to_str().unwrap()).unwrap().unwrap();
    for (id, format, suffix) in [
        ("webp", vrc_photo_core::model::CompressionFormat::Webp, "_webp"),
        ("avif", vrc_photo_core::model::CompressionFormat::Avif, "_avif"),
    ] {
        db.save_profile(&CompressionProfile {
            id: id.to_string(),
            name: id.to_string(),
            format,
            suffix: suffix.to_string(),
            ..CompressionProfile::default()
        })
        .unwrap();
    }
    db.enqueue_job(photo.id, "webp", "keep").unwrap();
    db.enqueue_job(photo.id, "avif", "keep").unwrap();
    daemon.settings.compression_workers = 1;

    let started = Instant::now();
    daemon.start_workers();
    wait_until_idle(&daemon.db, Duration::from_secs(180));
    daemon.stop_workers();
    println!("two-profile compress elapsed: {:?}", started.elapsed());
}
```

Build once, then run three times on each branch:

```bash
cargo test -p vrc-photo-daemon measure_two_profile_compression --release --no-run
cargo test -p vrc-photo-daemon measure_two_profile_compression --release -- --ignored --nocapture
```

Record the median wall time. Also run the same command under `/usr/bin/time -v` and record user+sys. Use the same 4K input and machine for both branches. Delete the temporary test before committing.

- [ ] **Step 5: Push and open the code PR**

```bash
git push -u origin feat/compression-decode-sharing
gh pr create --title "feat: reduce compression memory peak and share decodes" --label enhancement --body "$(cat <<'EOF'
Closes #192
Closes #194

- Borrow decoded images through resize/encode; remove the AVIF RGBA collect and redundant WebP/JPEG/JXL copies. AVIF stays on the RGBA path and preserves alpha.
- Claim all pending jobs for the oldest pending photo and decode once per batch. Freeze the source path so a `replace` job cannot feed a derivative into the next job.
- Isolate per-job failures and cancel the remaining batch on forced deletion.
- `compress_pending` now uses a single claimer instead of racing a worker.

## Measurements

(peak RSS and two-profile timing tables go here)
EOF
)"
```

- [ ] **Step 6: Monitor CI and merge only when green**

Follow the `ci-watch` skill. On success, merge with `gh pr merge <number> --merge`, switch to `main`, and pull. On failure, fix on the branch and push again.

- [ ] **Step 7: Record the measurements on the issues**

Post the #192 peak-RSS table and the #194 timing table as issue comments, including the machine/commit and the note that `compression_workers >= 2` may trade wall time for lower peak memory.

---

### Task 5: Product documentation (separate docs-only PR)

**Files:**
- Modify: `docs/architecture.md`
- Modify: `docs/features.md`
- Modify: `docs/development.md`
- Modify: `docs/testing.md`

**Interfaces:**
- Consumes the behavior and measurements from Tasks 1–4.
- Must run after the code PR is merged so `main` never documents unimplemented behavior.

- [ ] **Step 1: Update the architecture**

In `docs/architecture.md` §4.4:

- Change the worker bullet from “1件を原子的に取得” to “最も古い pending ジョブの写真について pending ジョブをまとめて原子的に取得”.
- Add: the worker decodes once per batch, reuses the decoded image for every profile, and drops it at the end of the batch.
- Add: the source path is frozen at batch start; a `replace` derivative is never used as the next job's input.
- Add: per-job failures are isolated; forced deletion cancels the remaining batch between encodes.

- [ ] **Step 2: Update the feature list**

In `docs/features.md` §3.3:

- Add a bullet: multiple profiles for the same photo share one full decode per batch; the decoded image is not retained after the batch.
- Add a bullet: a forced delete cancels the remaining queued profiles for that photo; the profile currently encoding finishes first.
- Clarify that `replace` still reads the original file for every job in the batch.

- [ ] **Step 3: Add the measurement procedure and results**

In `docs/development.md`, add a “圧縮のメモリ・時間計測” section:

- How to generate 4K/8K RGB/RGBA PNGs.
- The `/usr/bin/time -v` command and the `Maximum resident set size` field.
- The two-profile `compression_workers = 1` method.
- A table of the measured before/after values from Task 4, with the machine and commit noted.
- The `compression_workers >= 2` trade-off.

- [ ] **Step 4: Update the test inventory**

In `docs/testing.md`, add to the core table:

- `compressor.rs`: AVIF transparency, borrowed `compress_decoded`, fast-path vs conversion-path output.
- `db.rs`: per-photo batch claim and forced-deletion lookup.

Add to the daemon list:

- Multi-profile batch completion, frozen replace input, per-job failure isolation, forced-deletion cancellation.

- [ ] **Step 5: Verify docs-only scope**

Run: `git diff --name-only`

Expected: only the four files above. No `crates/` paths.

- [ ] **Step 6: Commit and open the docs PR**

```bash
git add docs/architecture.md docs/features.md docs/development.md docs/testing.md
git commit -m "docs: document compression batching and memory behavior"
```

Open a `docs/` PR with the `documentation` and `enhancement` labels, and wait for CI per the `ci-watch` skill before merging.

---

## Self-Review

- **Spec coverage:** zero-copy AVIF with alpha (Task 1), WebP/JPEG/JXL borrowed buffers (Task 1), borrowed `compress_decoded` (Task 1), batch claim (Task 2), forced-deletion lookup (Task 2), one decode per batch (Task 3), frozen source path (Task 3), failure isolation (Task 3), `compress_pending` single claimer (Task 3), measurements (Task 4), product docs (Task 5).
- **Type consistency:** `compress_decoded(&DynamicImage, ...)` is used by the daemon in Task 3; `next_pending_photo_jobs() -> Vec<CompressionJob>` is used by both `worker_loop` and `compress_pending`; `process_photo_batch` returns `JobOutcome` with `status: QueueStatus`; `photo_has_forced_deletion(i64) -> bool` is used between jobs.
- **Review Focus tests:** transparent AVIF (Task 1 `avif_keeps_transparency`), replace-then-keep (Task 3 `batch_keeps_the_original_source_after_a_replace_job`), one-profile failure (Task 3 `batch_continues_after_one_profile_fails`), forced delete mid-batch (Task 3 `forced_deletion_cancels_the_rest_of_the_batch`), RGB channel order (Task 1 fast-path tests).
- **Proportion:** the plan pins signatures, SQL shape, frozen-path rules, cancellation points, and the measurement method; it does not restate the encoder bodies or the existing `process_job` post-processing.
