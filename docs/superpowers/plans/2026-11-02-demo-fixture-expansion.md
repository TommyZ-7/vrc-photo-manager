# Demo Fixture Expansion Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [x]`) syntax for tracking.

**Goal:** Replace the 18-photo demo fixture with a deterministic 300-photo fixture spanning 36 months, using pooled images and a version marker for automatic reseeding.

**Architecture:** A new pure `demo_fixture` module produces metadata (`DemoPhoto` values) from a reference local date. `seed_demo` consumes that plan, copies eight pooled image/thumbnail pairs, and writes the DB rows. `prepare_demo` records a data version in the demo root and rebuilds stale demo data automatically.

**Tech Stack:** Rust (edition 2024), chrono, image, rusqlite-backed `Db`, tempfile for tests.

**Spec:** `docs/superpowers/specs/2026-11-02-demo-fixture-expansion-design.md`

## Global Constraints

- No RNG: the fixture must be identical for the same reference date.
- Exactly 300 photos across 36 months, each month 5-13 photos.
- Favorites = 75, compressed = 100, unlinked = 43 for the full fixture.
- Unlinked photos have no `photo_context` and no `photo_players` rows.
- Generate images/thumbnails once per palette entry, then copy files.
- Do not change the DB schema or production import/linking code.
- Keep `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, and `cargo test --workspace` green.

## Review Focus

- **DST gaps and future dates:** a local wall-clock time may not exist
  (spring-forward). The fixture must move it forward instead of dropping the
  photo, every `captured_at.local` must stay inside its intended month, and
  no photo may be dated after the reference day. Covered by
  `fixture_spans_36_months_and_300_photos`.
- **Stale demo roots:** an existing 18-photo demo must be rebuilt without requiring a CLI flag. Covered by `demo_version_marker_detects_stale_data`; manual check in Task 3.
- **Unlinked filter:** the UI's unlinked view depends on a missing `photo_context` row, not just a missing world name. Covered by the `unlinked` assertions in `seed_demo_populates_three_years_of_library`.
- **Thumbnail coverage:** every photo must have a copied thumbnail, otherwise the gallery shows permanent placeholders. Covered by the thumbnail loop in `seed_demo_populates_three_years_of_library`.
- **Seed time:** 300 copies plus DB writes must not regress noticeably. Manual timing in Task 3.

---

### Task 1: Add the pure fixture generator

**Files:**
- Create: `crates/ui/src/app/demo_fixture.rs`
- Modify: `crates/ui/src/app.rs:5`
- Test: `crates/ui/src/app/demo_fixture.rs` (`#[cfg(test)]`)

**Interfaces:**
- Consumes: `vrc_photon_core::time::{self, LocalStamp}`, `vrc_photon_core::model::PhotoPlayer`
- Produces:
  - `pub(crate) const DEMO_MONTHS: usize = 36`
  - `pub(crate) const DEMO_WORLDS: [&str; 8]`
  - `pub(crate) const DEMO_NAMES: [&str; 8]`
  - `pub(crate) struct DemoPhoto { pub(crate) captured_at: LocalStamp, pub(crate) palette: usize, pub(crate) world: Option<usize>, pub(crate) players: Vec<PhotoPlayer>, pub(crate) tags: Vec<String>, pub(crate) favorite: bool, pub(crate) compressed: bool }`
  - `pub(crate) fn demo_fixture(reference: NaiveDate) -> Vec<DemoPhoto>`

- [x] **Step 1: Write the failing fixture tests**

Add `crates/ui/src/app/demo_fixture.rs` with only the tests below. The module has no implementation yet, so it fails to compile.

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;
    use std::collections::{BTreeMap, BTreeSet};

    fn reference() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 11, 2).unwrap()
    }

    fn month_key(photo: &DemoPhoto) -> String {
        photo.captured_at.local[..7].to_string()
    }

    fn signature(
        photo: &DemoPhoto,
    ) -> (String, usize, Option<usize>, bool, bool, Vec<String>, Vec<String>) {
        (
            photo.captured_at.local.clone(),
            photo.palette,
            photo.world,
            photo.favorite,
            photo.compressed,
            photo.tags.clone(),
            photo
                .players
                .iter()
                .map(|player| player.user_id.clone())
                .collect(),
        )
    }

    #[test]
    fn fixture_spans_36_months_and_300_photos() {
        let photos = demo_fixture(reference());
        assert_eq!(photos.len(), 300);

        let mut counts: BTreeMap<String, usize> = BTreeMap::new();
        for photo in &photos {
            *counts.entry(month_key(photo)).or_default() += 1;
        }

        assert_eq!(counts.len(), DEMO_MONTHS);
        assert_eq!(
            counts.first_key_value().map(|(month, _)| month.as_str()),
            Some("2023-12")
        );
        assert_eq!(
            counts.last_key_value().map(|(month, _)| month.as_str()),
            Some("2026-11")
        );
        for (month, count) in &counts {
            assert!((5..=13).contains(count), "{month} has {count} photos");
        }

        let reference_date = reference().format("%Y-%m-%d").to_string();
        for photo in &photos {
            let date = &photo.captured_at.local[..10];
            assert!(
                date <= reference_date.as_str(),
                "{date} is after {reference_date}"
            );
        }
    }

    #[test]
    fn fixture_spreads_days_and_times_within_each_month() {
        let mut days_by_month: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        let mut times_by_month: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();

        for photo in demo_fixture(reference()) {
            let month = month_key(&photo);
            days_by_month
                .entry(month.clone())
                .or_default()
                .insert(photo.captured_at.local[8..10].to_string());
            times_by_month
                .entry(month)
                .or_default()
                .insert(photo.captured_at.local[11..16].to_string());
        }

        for (month, days) in &days_by_month {
            assert!(days.len() >= 2, "{month} uses {days:?}");
            assert!(
                times_by_month[month].len() >= 3,
                "{month} times: {:?}",
                times_by_month[month]
            );
        }
    }

    #[test]
    fn fixture_keeps_metadata_ratios_and_context() {
        let photos = demo_fixture(reference());
        assert_eq!(photos.iter().filter(|photo| photo.favorite).count(), 75);
        assert_eq!(photos.iter().filter(|photo| photo.compressed).count(), 100);

        let unlinked: Vec<&DemoPhoto> = photos
            .iter()
            .filter(|photo| photo.world.is_none())
            .collect();
        assert_eq!(unlinked.len(), 43);
        for photo in &unlinked {
            assert!(photo.players.is_empty());
            assert_eq!(photo.tags.len(), 1);
        }

        for photo in photos.iter().filter(|photo| photo.world.is_some()) {
            assert!(!photo.players.is_empty());
            assert_eq!(photo.tags.len(), 2);
        }
    }

    #[test]
    fn fixture_is_deterministic() {
        let first: Vec<_> = demo_fixture(reference()).iter().map(signature).collect();
        let second: Vec<_> = demo_fixture(reference()).iter().map(signature).collect();
        assert_eq!(first, second);
    }
}
```

- [x] **Step 2: Register the module and run the tests to verify failure**

Add `pub(crate) mod demo_fixture;` next to `pub(crate) mod demo;` in `crates/ui/src/app.rs`.

Run: `cargo test -p vrc-photon-ui fixture_ --no-run`

Expected: FAIL because `DemoPhoto`, `demo_fixture`, and the constants do not exist yet.

- [x] **Step 3: Implement the fixture generator**

In `crates/ui/src/app/demo_fixture.rs`, add the imports and constants:

```rust
use chrono::{Datelike, NaiveDate};
use vrc_photon_core::model::PhotoPlayer;
use vrc_photon_core::time::{self, LocalStamp};

pub(crate) const DEMO_MONTHS: usize = 36;
pub(crate) const DEMO_WORLDS: [&str; 8] = [
    "Midnight Rooftop",
    "The Black Cat",
    "Furality Aqua",
    "Sunset Bar",
    "Neon District",
    "Cozy Cabin",
    "Snowfield",
    "Deep Space",
];
pub(crate) const DEMO_NAMES: [&str; 8] = ["Mio", "Kenta", "Lua", "Nox", "Sora", "Yui", "Aki", "Ren"];

const MONTH_COUNTS: [usize; 12] = [6, 8, 11, 5, 9, 12, 7, 10, 13, 6, 8, 5];
```

Add `DemoPhoto` with the fields from the Interfaces block, deriving `Debug` and `Clone`.

Implement `demo_fixture` with this algorithm:

1. Keep a `global` index starting at 0 and a `Vec::with_capacity(300)`.
2. Loop `months_ago` over `0..DEMO_MONTHS`.
3. `count = MONTH_COUNTS[(months_ago + (months_ago / 12) * 4) % 12]`.
4. Compute the first day of that month and the first day of the next month
   with a `month_start(reference, months_ago: u32) -> NaiveDate` helper using
   `div_euclid` / `rem_euclid`; `days_in_month` is their difference in days.
   For `months_ago == 0`, use `reference.day()` instead so the current month
   never contains future dates.
5. For `index_in_month in 0..count`:
   - `linked = global % 7 != 0`
   - `day = 1 + (index_in_month * days_in_month / count + months_ago) % days_in_month`
   - `hour = (8 + index_in_month * 3 + months_ago * 5) % 24`
   - `minute = (index_in_month * 13 + months_ago * 7) % 60`
   - `captured_at = local_stamp(date_with_day, hour as u32, minute as u32)`
   - `world = linked.then_some((months_ago + index_in_month) % DEMO_WORLDS.len())`
   - players: when linked, `3 + (months_ago + index_in_month) % 5` names
     starting at `(months_ago * 2 + index_in_month) % DEMO_NAMES.len()`;
     `user_id` is `format!("usr_{name}")`. When unlinked, `Vec::new()`.
   - `palette = world.unwrap_or((months_ago + index_in_month) % DEMO_WORLDS.len())`
   - tags: first tag is `"夜"` when `hour < 6 || hour >= 18`, otherwise
     `"昼"`; linked photos push the first word of `DEMO_WORLDS[world]`.
   - `favorite = global % 4 == 0`, `compressed = global % 3 == 0`.
   - Push the photo and increment `global`.

Add `fn local_stamp(date: NaiveDate, hour: u32, minute: u32) -> LocalStamp`:
build the naive time, call `time::from_naive`, and on `None` add one hour and
retry (at most 24 times) so DST gaps do not drop or panic the fixture. Split
out `local_stamp_with(..., convert: impl FnMut(NaiveDateTime) -> Option<LocalStamp>)`
so the retry branch can be tested with an injected converter; keep the hour
formula modulo 23 so a one-hour shift stays inside the day.

Also add `fixture_keeps_every_photo_inside_its_intended_month` (pair each
photo with the expected `YYYY-MM` from `month_start`) and
`local_stamp_moves_forward_over_a_dst_gap` (inject `None` once, then convert
the next hour).

- [x] **Step 4: Run the fixture tests**

Run: `cargo test -p vrc-photon-ui fixture_ -v`

Expected: all four `fixture_*` tests PASS.

- [x] **Step 5: Commit**

```bash
git add crates/ui/src/app.rs crates/ui/src/app/demo_fixture.rs
git commit -m "feat(ui): add deterministic 300-photo demo fixture"
```

---

### Task 2: Seed from the fixture with pooled assets and auto-reseed

**Files:**
- Modify: `crates/ui/src/app/demo.rs`
- Test: `crates/ui/src/app/demo.rs` (`#[cfg(test)]`)

**Interfaces:**
- Consumes: `demo_fixture::{demo_fixture, DEMO_WORLDS}`
- Produces:
  - `const DEMO_DATA_VERSION: u32 = 2`
  - `const DEMO_VERSION_FILE: &str = "demo.version"`
  - `fn demo_data_is_current(root: &Path) -> bool`
  - `fn seed_demo(db: &Db, photo_dir: &Path, reference: NaiveDate) -> Result<(), String>`

- [x] **Step 1: Write the failing seed and version tests**

Add these tests to the existing `#[cfg(test)] mod tests` in
`crates/ui/src/app/demo.rs`:

```rust
use chrono::NaiveDate;
use std::collections::BTreeSet;
use vrc_photon_core::model::{PhotoFilter, PhotoSort};

use crate::state::year_rail::build_year_buckets;

#[test]
fn demo_version_marker_detects_stale_data() {
    let temp = tempfile::tempdir().unwrap();
    assert!(!demo_data_is_current(temp.path()));

    std::fs::write(
        temp.path().join(DEMO_VERSION_FILE),
        DEMO_DATA_VERSION.to_string(),
    )
    .unwrap();
    assert!(demo_data_is_current(temp.path()));

    std::fs::write(
        temp.path().join(DEMO_VERSION_FILE),
        (DEMO_DATA_VERSION + 1).to_string(),
    )
    .unwrap();
    assert!(!demo_data_is_current(temp.path()));
}

#[test]
fn seed_demo_populates_three_years_of_library() {
    let temp = tempfile::tempdir().unwrap();
    let photo_dir = temp.path().join("photos");
    let db = Db::open_in_memory().unwrap();
    let reference = NaiveDate::from_ymd_opt(2026, 11, 2).unwrap();

    seed_demo(&db, &photo_dir, reference).unwrap();

    assert_eq!(db.count_photos().unwrap(), 300);
    let entries = db.photo_index(PhotoSort::Newest).unwrap();
    let months: BTreeSet<i32> = entries.iter().map(|entry| entry.captured_month).collect();
    assert_eq!(months.len(), 36);
    assert_eq!(months.first().copied(), Some(202312));
    assert_eq!(months.last().copied(), Some(202611));
    let years: Vec<i32> = build_year_buckets(&entries)
        .iter()
        .map(|bucket| bucket.year)
        .collect();
    assert_eq!(years, vec![2026, 2025, 2024, 2023]);

    let fixture = demo_fixture::demo_fixture(reference);
    let june = fixture
        .iter()
        .filter(|photo| photo.captured_at.local.starts_with("2025-06"))
        .count();
    assert!(june > 0);
    let from = time::from_naive(
        NaiveDate::from_ymd_opt(2025, 6, 1)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap(),
    )
    .unwrap()
    .utc;
    let to = time::from_naive(
        NaiveDate::from_ymd_opt(2025, 7, 1)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap(),
    )
    .unwrap()
    .utc
        - 1;
    let filter = PhotoFilter {
        from_utc: Some(from),
        to_utc: Some(to),
        limit: 500,
        ..Default::default()
    };
    assert_eq!(db.count_search(&filter).unwrap(), june as i64);

    let favorites = db
        .count_search(&PhotoFilter {
            favorite: Some(true),
            limit: 500,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(favorites, 75);

    let unlinked = db.unlinked_photos(500).unwrap();
    assert_eq!(unlinked.len(), 43);
    for photo in &unlinked {
        assert!(db.photo_context(photo.id).unwrap().is_none());
        assert!(db.photo_players(photo.id).unwrap().is_empty());
    }

    assert_eq!(db.stats().unwrap().compressed_count, 100);
    assert_eq!(std::fs::read_dir(&photo_dir).unwrap().count(), 300);
    let thumb_dir = photo_dir.parent().unwrap().join("thumbnails");
    for entry in &entries {
        assert!(
            thumb_dir.join(format!("{}.png", entry.id)).exists(),
            "missing thumbnail for {}",
            entry.id
        );
    }
    let pool = temp.path().join("image_pool");
    assert_eq!(std::fs::read_dir(pool).unwrap().count(), 16);
}
```

- [x] **Step 2: Run the tests to verify failure**

Run: `cargo test -p vrc-photon-ui demo_version_marker --no-run`

Expected: FAIL because `DEMO_VERSION_FILE`, `DEMO_DATA_VERSION`,
`demo_data_is_current`, and the new `seed_demo` signature are missing.

- [x] **Step 3: Implement the version marker and auto-reseed**

In `crates/ui/src/app/demo.rs`:

- Replace the `Duration`/`SystemTime` imports with
  `use chrono::{Local, NaiveDate, Utc};`.
- Add `const DEMO_DATA_VERSION: u32 = 2;` and
  `const DEMO_VERSION_FILE: &str = "demo.version";` near the demo root code.
- Add `fn demo_data_is_current(root: &Path) -> bool`: read
  `root.join(DEMO_VERSION_FILE)`, trim and parse it, and compare with
  `Some(DEMO_DATA_VERSION)`.
- Add `fn remove_demo_root(root: &Path) -> Result<(), String>`: remove the
  directory, treat `NotFound` as success, and otherwise return the existing
  localized `demo.remove_error`.
- In `prepare_demo`, change the reset block to
  `if reset || !demo_data_is_current(&root) { remove_demo_root(&root)?; }`.
- After the existing empty-library check, call
  `seed_demo(&db, &photo_dir, Local::now().date_naive())?` when the count is
  zero.
- After seeding (and when the library already has data), write
  `DEMO_DATA_VERSION.to_string()` to `root.join(DEMO_VERSION_FILE)`. A failed
  seed therefore leaves no marker and retries next launch.

- [x] **Step 4: Rewrite `seed_demo` around the fixture and pool**

Change the signature to
`fn seed_demo(db: &Db, photo_dir: &Path, reference: NaiveDate) -> Result<(), String>`.

Implementation requirements:

1. Create `photo_dir`, `thumb_dir` (the existing parent-based path), and
   `pool_dir = root.join("image_pool")`.
2. Add the library root exactly as today.
3. Upsert all eight worlds once before the photo loop, storing their ids in a
   `Vec<i64>` in `DEMO_WORLDS` order.
4. Generate the pool: for each `variant in 0..DEMO_PALETTE.len()`, save
   `demo_image(variant)` to `image_pool/image_{variant}.png`, record its
   metadata length, and save a `thumbnail(256, 256)` to
   `image_pool/thumb_{variant}.png`.
5. Iterate `demo_fixture::demo_fixture(reference).into_iter().enumerate()`:
   - `variant = photo.palette % DEMO_PALETTE.len()`
   - `file_name = format!("VRChat_demo_{index:03}.png")`
   - copy `image_pool/image_{variant}.png` to `photos/{file_name}`
   - call `db.upsert_photo` with the photo's `captured_at` fields,
     `width: 1280`, `height: 800`, the pool image's size, and
     `mtime: photo.captured_at.utc`
   - copy `image_pool/thumb_{variant}.png` to
     `thumbnails/{id}.png`
   - when `photo.world` is `Some(world_index)`, call `db.set_photo_context`
     with `world_id: Some(world_ids[world_index])`,
     `world_name: Some(DEMO_WORLDS[world_index].to_string())`,
     `instance_id: Some(format!("{}~region(jp)", 12000 + index))`,
     `link_method: "demo"`, `confidence: 1.0`, and author `"TommyZ"`
   - when `photo.players` is not empty, call `db.set_photo_players`
   - always call `db.set_photo_tags(id, &photo.tags)`
   - when `photo.favorite`, call `db.set_favorite(id, true)`
   - when `photo.compressed`, call `db.add_derivative` with
     `/demo/compressed/{index}.webp`, `"webp"`, 1280, 800, `1_800_000`,
     `"demo"`, `"keep"`
6. Use the existing localized error style (`format!("{}: {error}", path)` for
   file operations, `.map_err(|error| error.to_string())` for DB calls).

Keep the existing `demo_thumb_path` helper and its test unchanged.

- [x] **Step 5: Run the new tests**

Run:

```bash
cargo test -p vrc-photon-ui demo_version_marker -v
cargo test -p vrc-photon-ui seed_demo_populates -v
```

Expected: both PASS. If the seed test is slow, record the elapsed time and
compare it with the old fixture in Task 3.

- [x] **Step 6: Commit**

```bash
git add crates/ui/src/app/demo.rs
git commit -m "feat(ui): seed demo library from 36-month fixture"
```

---

### Task 3: Verify the whole workspace and the demo UI

**Files:**
- Modify: `docs/superpowers/plans/2026-11-02-demo-fixture-expansion.md` (checkboxes)

- [x] **Step 1: Run formatting, lint, and the full test suite**

Run:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Expected: all commands exit 0.

- [x] **Step 2: Measure and inspect a clean demo seed**

Run:

```bash
rm -rf /tmp/vrc-photon-demo
time cargo run -p vrc-photon-ui -- --demo --demo-reset
```

Confirm the library shows about 300 photos, the year rail spans 2023-2026,
monthly sections are populated, and date-range filtering works. Record the
seed time; it should be in the same range as the old 18-photo fixture (the
pool renders eight images instead of eighteen).

- [x] **Step 3: Verify automatic reseeding**

With the app closed, write an old version marker and start without
`--demo-reset`:

```bash
printf '1' > /tmp/vrc-photon-demo/demo.version
cargo run -p vrc-photon-ui -- --demo
```

Expected: the demo root is rebuilt, the marker becomes `2`, and the library
shows 300 photos without passing `--demo-reset`.

- [x] **Step 4: Update the plan and commit**

Mark every completed checkbox in this file as `- [x]`, then:

```bash
git add docs/superpowers/plans/2026-11-02-demo-fixture-expansion.md
git commit -m "docs: record demo fixture verification"
```
