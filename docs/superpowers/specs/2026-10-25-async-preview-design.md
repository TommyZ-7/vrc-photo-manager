# Async Preview Loading and Disk Cache: Design

- Date: 2026-10-25
- Status: Approved
- Target: `crates/core/src/preview.rs` (new), `crates/core/src/lib.rs`,
  `crates/ui/src/main.rs`, `docs/ui.md`, `docs/security-privacy.md`,
  `docs/data-model.md`

## 1. Intent

Selecting a library card currently blocks the UI thread until the 1200 px
preview has been decoded from the source image. Measured on the development
machine: 405–408 ms per click (debug, 1280×800 PNG) and 20–99 ms per selection
(release, 1280×800–4K). The selection highlight cannot paint until the handler
returns, so the decode time is the perceived lag.

The goal is to make selection immediate, load the preview off the UI thread,
and cache the 1200 px result on disk so repeat selections do not touch the
source image.

## 2. Goals

- `select_photo` returns in well under 1 ms; no image decode on the UI thread.
- Decode previews in `background_executor` and apply the result only when the
  photo is still selected.
- Cache 1200 px previews as WebP under the data directory, keyed by photo ID
  and display derivative ID, with source-mtime freshness.
- Re-clicking the already selected photo does not reload metadata or preview.
- Rapid viewer navigation keeps one decode in flight and drops stale results.
- Warm previews for newly imported photos in the background.
- Bound the cache size and keep cache files safe to delete.

## 3. Non-goals

- No preview generation in the daemon and no bulk backfill of existing
  libraries. Existing photos fill the cache lazily on first selection.
- No changes to the 256 px thumbnail pipeline or its in-memory LRU.
- No new database columns, migrations, or user-visible settings.
- No change to the 260 ms inspector cross-fade. On a slow first decode the
  cross-fade may finish before the preview arrives; the thumbnail stays
  visible instead.

## 4. Approaches

### A. Async UI decode + lazy disk cache + import warming (chosen)

Move the existing decode to the background executor, add a bounded WebP cache,
and warm new imports after indexing. Selection paints immediately; repeat
selections decode a small WebP instead of the full source.

Trade-off: two places can write the same cache entry (import warmer and
selection worker), so writes must stay atomic and duplicate work is possible
in a small race window.

### B. Async decode only

Simpler, but every selection still decodes the full source and re-encodes the
resize. Viewer navigation stays CPU-heavy. Rejected because the repeat cost is
the common case.

### C. Daemon pre-generation / bulk backfill

Best first-click latency for an existing library, but it writes 1200 px files
for every photo (potentially many GB at 100k) and adds daemon work. Rejected
for now; can be added later on top of the same cache format.

## 5. Selection and preview flow

- `select_photo` updates the selection list and anchor, loads metadata
  synchronously (measured at 0.2–0.6 ms), snapshots the previous preview and
  colour for the cross-fade, requests the preview, and notifies.
- The request key is `(photo_id, display_derivative_id.unwrap_or(0))`.
  `display_derivative_id` changes when a compressed replacement becomes the
  displayed file, so each displayed variant gets its own cache entry.
- Only one preview decode runs at a time. The latest requested key is kept as
  the next job; when the current decode finishes, a stale result is dropped
  and the latest key starts.
- The UI installs the decoded image only if the photo is still selected and
  the request key still matches.
- While the preview is pending, the inspector keeps the existing tint and
  256 px thumbnail layers and shows `プレビューを読み込み中…` instead of
  `プレビューを読み込めません`. The viewer already keeps the thumbnail behind
  the preview; no loading text is needed there.
- Clicking the selected photo again keeps the loaded preview and skips the
  metadata reload. Metadata is still reloaded by the existing 3-second live
  refresh.
- The 3-second refresh requests a preview only when a preview was already
  loaded for a different key (for example after a replacement), so a missing
  or failed source is not retried every tick.
- On import, after the new files are indexed, a background task generates the
  cache entries for the imported IDs. Failures are logged and never affect the
  import report.

## 6. Cache format and policy

- Location: `<data_dir>/previews/<photo_id>-<variant>.webp` for the real
  library. Demo mode uses `<demo root>/previews/` and never writes the real
  cache.
- Size: at most 1200 px on the long edge, matching today's preview.
- Encoding: WebP quality 85, mirroring the existing 256 px thumbnail path.
- Freshness: an entry is valid when it is a non-empty file, decodes
  successfully, and the source file's mtime is not newer than the cache file.
  If the source is missing, a valid cache entry is still usable.
- A corrupt entry is deleted and regenerated from the source. If the cache
  cannot be written, the decoded image is still returned for display.
- Writes are atomic: write a `.webp.tmp` file next to the target, then rename.
- Bounding: at most 2048 cache files. Every 32nd new entry, list the cache and
  delete the oldest files by mtime down to 1536. Pruning runs on the
  background worker and never fails the preview.
- The cache is regenerable and is not part of backups.

## 7. Testing

Core unit tests:

- Creates an entry, returns RGBA, and reuses the entry when the source is gone.
- Keeps variants separate for the same photo ID.
- Regenerates when the source is newer than the cache.
- Repairs a corrupt cache file from the source.
- Returns `None` when both source and cache are missing.
- Prunes the oldest files down to the target count and leaves unrelated files
  alone.

UI helper tests:

- Preview key/variant selection (original vs. replaced derivative).
- Request queue: one in flight, latest key wins, stale completion is dropped,
  and re-requesting the in-flight key clears the queued key.

Verification:

- `VRC_PHOTO_TIMING=1` on a debug build: the click handler stays under 1 ms;
  a cache-miss decode is logged in the background; a repeat click uses the
  cache. The existing timing probes remain env-gated.
- Manual demo run: click cards, navigate the viewer with arrows, open a
  replaced photo if available, and confirm selection highlights immediately.
- `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`,
  `cargo test --workspace`.

## 8. Risks

- WebP encoding on a cache miss adds background CPU. A single in-flight job
  keeps it bounded and the UI thread stays free.
- The 2048-file cap is about 150–250 MB at typical preview sizes (an
  84 KB sample for a noisy 1200×675 WebP at quality 85). Eviction is FIFO by
  creation time, not true LRU.
- Cache freshness relies on the derivative ID plus source mtime. A manual file
  replacement that preserves mtime is not detected, matching the existing
  thumbnail cache limitation.
