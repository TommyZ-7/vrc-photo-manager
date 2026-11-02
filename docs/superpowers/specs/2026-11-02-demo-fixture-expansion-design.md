# Demo Fixture Expansion Design

**Issue:** #274

## Intent

Demo mode should exercise the library UI against a realistic three-year
history: year rail, monthly sections, date-range search, favorites,
compressed derivatives, and unlinked photos. The current fixture has 18
photos concentrated in the last 18 hours, so those views are barely
represented.

The DB schema and the production import path do not change.

## Goals

- Generate exactly 300 photos across the current month plus the previous
  35 months (36 months total).
- Keep each month between 5 and 13 photos, deterministically (no RNG).
- Spread capture days and times inside every month.
- Shift worlds, players, and tags month by month.
- Keep favorites at about 1/4, compressed derivatives at about 1/3, and
  unlinked photos at about 1/7.
- Generate each 1280x800 image and thumbnail once per palette entry, then
  reuse them by file copy.
- Keep first-seed time in the same range as the current 18-photo fixture.
- Add tests for total count, monthly counts, date range, and the
  favorite/compressed/unlinked ratios.

## Non-goals

- Changing the DB schema or any production import/linking behavior.
- Making the demo photos visually unique per photo; eight palette
  variants are enough.
- Adding a user-facing setting for demo data size.

## Approaches considered

1. **Pure fixture generator plus pooled assets and a data-version marker
   (chosen).** A pure function produces the metadata plan, `seed_demo`
   writes it, and a version file triggers a one-time rebuild of old demo
   roots. Tests can validate the distribution without touching the DB.
2. **Keep the generator inside `seed_demo` and test through SQLite.**
   Less code, but distribution tests become slow and cannot pin the
   reference date without adding test-only parameters anyway.
3. **Generate 300 distinct images.** Most realistic, but slower to seed
   and wastes disk; the issue explicitly asks for a reusable pool.

## Data design

- Reference date: local "today" when seeding. Month offsets `0..36` where
  `0` is the current month.
- Monthly counts: the 12-entry cycle `[6, 8, 11, 5, 9, 12, 7, 10, 13, 6,
  8, 5]` (sum 100), rotated by four positions for each later year. Three
  cycles therefore sum to exactly 300 and every count stays in `5..=13`.
- Within a month, the day is spread evenly with
  `1 + (index * days_in_month / count + month_offset) % days_in_month`.
  Hours and minutes use deterministic modular arithmetic over 24h/60m.
  Invalid local times (DST gaps) are moved forward by one hour. The current
  month uses `reference.day()` as its day count so demo photos never land in
  the future.
- Eight worlds and eight player names keep their current values. A linked
  photo's world starts at `(month_offset + index_in_month) % 8`; players
  start at `(month_offset * 2 + index_in_month) % 8` and number 3-7.
- Tags: a time-of-day tag (`夜` for 18:00-05:59, otherwise `昼`) plus the
  first word of the world name for linked photos. Unlinked photos keep
  only the time tag.
- Global photo index drives the ratios: favorite when `index % 4 == 0`
  (75), compressed when `index % 3 == 0` (100), unlinked when
  `index % 7 == 0` (43). Unlinked photos get no `photo_context` and no
  `photo_players` rows.
- Unlinked means `photo_context` is absent, matching the existing
  `unlinked_photos` / `PhotoFilter::unlinked` behavior.

## Asset design

- Generate eight `image_{variant}.png` files and eight
  `thumb_{variant}.png` files under `<demo root>/image_pool/`.
- Each photo copies its variant's image to
  `photos/VRChat_demo_{index:03}.png`; after inserting the row, it copies
  the variant thumbnail to `thumbnails/{id}.png`.
- The pool lives outside the `photos/` library root so it is never
  mistaken for library content.
- File size comes from the pool image metadata; all copies share it.

## Reseed behavior

- `DEMO_DATA_VERSION = 2` and `<demo root>/demo.version`.
- On startup, if `--demo-reset` is set or the marker is missing/mismatched,
  delete the demo root before opening the DB.
- Seed when the library is empty, then write the marker. A failed seed
  leaves no marker, so the next launch retries.
- This makes a plain `--demo` upgrade an old 18-photo fixture
  automatically. A version bump is required whenever the fixture layout
  changes.

## Tests

- Pure fixture tests (new `demo_fixture` module): 300 photos, 36
  contiguous months, 5-13 per month, day/time spread, deterministic
  output, and exact favorite/compressed/unlinked counts.
- DB integration test in `demo.rs`: seed into a temporary directory with
  a pinned reference date, then verify `count_photos`, 36 distinct
  `captured_month` values, year buckets, a single-month date-range count,
  favorites, unlinked photos (no context/players), compressed count, and
  that all 300 thumbnails and 16 pool files exist.
- Version-marker test: missing, matching, and mismatched markers.

## Files

- Create: `crates/ui/src/app/demo_fixture.rs`
- Modify: `crates/ui/src/app.rs` (module registration)
- Modify: `crates/ui/src/app/demo.rs` (seeding, pool, version marker,
  tests)

## Verification

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
time cargo run -p vrc-photon-ui -- --demo --demo-reset
```

The demo run must show about 300 photos and the year rail must span three
years' worth of months, with monthly sections and date filtering working.
