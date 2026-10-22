# Default Compression Profile Duplication Fix Design

- Date: 2026-10-22
- Issue: [#40 fix(ui): 起動のたびに既定の圧縮プロファイルが重複登録される](https://github.com/TommyZ-7/vrc-photo-manager/issues/40)
- Status: Approved in session
- Target: `crates/core/src/model.rs`, `crates/core/src/db.rs`, `crates/ui/src/main.rs`

## 1. Intent

Every launch inserts the four built-in compression profiles again. `CompressionProfile::default()` creates a fresh UUID per call, so the `ON CONFLICT(id)` upsert in `save_profile` cannot collapse the copies. `App::new` calls `ensure_profiles()` while the in-memory `profiles` list is empty, and `ensure_profiles()` only checks that list, never the database. The compression screen therefore grows by four rows per launch, and the current UI has no delete action to clean it up.

## 2. Goals

- Seed the built-in profiles only when `compression_profiles` is empty.
- If any profile already exists, return it and write nothing.
- Repeated calls and restarts leave the row count and IDs unchanged.
- Keep the decision logic in core so it is unit-testable without GPUI.

## 3. Non-goals

- Removing or merging duplicates already present in existing databases. The issue scope was explicitly limited to prevention.
- Changes to the profile management UI (#11).
- Schema changes, `user_version` bumps, or data migrations.
- Cross-process locking for two simultaneous first launches.

## 4. Approaches

| Approach | Trade-off | Decision |
|---|---|---|
| A: check the DB inside the UI's `ensure_profiles` | Smallest diff, but the logic is only testable through GPUI or a UI-local free function. | Rejected |
| B: `Db::ensure_default_profiles` in core | Moves the preset list from the UI to core, but gives one seat of logic and in-memory SQLite tests. | Chosen |

A fixed-ID + `INSERT OR IGNORE` design was also considered and rejected: it would resurrect a built-in profile after a future delete-everything action, which conflicts with the expected behavior of seeding only an empty database.

## 5. Design

- `CompressionProfile::defaults() -> Vec<CompressionProfile>` returns the four built-in presets currently defined at `crates/ui/src/main.rs:394-420`. Names and settings stay unchanged.
- `Db::ensure_default_profiles() -> Result<Vec<CompressionProfile>>` calls `list_profiles()`. If the result is non-empty, it returns it. If empty, it saves each preset and re-reads the table. A read error is returned before any preset is written, so a corrupt table is not masked by defaults.
- `App::ensure_profiles()` becomes a thin wrapper: `self.profiles = self.db.ensure_default_profiles().unwrap_or_default();`. The UI stops carrying preset data.
- The call order in `App::new` (`ensure_profiles`, then `reload`) is unchanged.

## 6. Risks

- Databases that already ran v0.2.0 keep their duplicate rows. This is accepted scope; a cleanup can be filed separately.
- Two processes seeding an empty database at the same instant can still race. The UI is the only seeder and is normally single-instance, so this is not addressed here.
- The preset copy moves to core. This is additive and does not change serialization or the schema.

## 7. Verification

- `cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`
- Core tests cover idempotency, existing rows, and the read-error path.
- Manual demo check: `--demo --demo-reset`, quit, then `--demo`; the compression screen must show four profiles, and `compression_profiles` must still contain four rows.
