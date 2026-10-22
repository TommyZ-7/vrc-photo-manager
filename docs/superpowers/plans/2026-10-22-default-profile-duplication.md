# Default Compression Profile Duplication Fix Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Seed the four built-in compression profiles only when the database has none, so restarts stop duplicating them.

**Architecture:** Move the built-in preset list from the UI into `vrc-photo-core` as `CompressionProfile::defaults()`, add an idempotent `Db::ensure_default_profiles()`, and reduce the UI's `ensure_profiles()` to a call into it. Core gets unit tests against an in-memory SQLite database; the UI change is verified by the existing suite plus a two-launch demo check.

**Tech Stack:** Rust 2024, SQLite via rusqlite, GPUI (gpui-kit 0.8.0).

**Spec:** `docs/superpowers/specs/2026-10-22-default-profile-duplication-design.md`

## Global Constraints

- Use a worktree for the code branch; never commit or push directly to `main` (AGENTS.md §1).
- Implement this plan in a code PR separate from the docs PR that carries this plan (AGENTS.md §4).
- Do not change the DB schema, `user_version`, or add a migration.
- Do not add dependencies.
- Do not delete or merge profiles that already exist in a user database.
- Do not change the compression UI, profile editing, or daemon behavior.
- Preset names and settings must stay equivalent to `crates/ui/src/main.rs:394-420` at the start of the branch.
- Pre-push verification: `cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`
- Do not create tags, run the Release workflow manually, or publish a release.

## Review Focus

1. Fresh database: exactly four profiles exist after the first `ensure_default_profiles()` call, with the preset names and auto-compress flags the UI used to seed.
2. Restart on the same database: the row count stays four and every ID is unchanged; no new UUIDs appear.
3. Database with one user-created profile: no built-in profile is added, and the user's row is untouched.
4. Corrupt profile JSON: `ensure_default_profiles()` returns an error and inserts zero rows, rather than half-seeding or masking the corruption.
5. Demo flow: `--demo --demo-reset`, quit, then `--demo` shows four profiles on the compression screen, not eight.

---

## Task 1: Add idempotent default-profile seeding to core

**Files:**
- Modify: `crates/core/src/model.rs` (after `impl Default for CompressionProfile`, ~line 108)
- Modify: `crates/core/src/db.rs` (after `list_profiles`, ~line 813; tests in `mod tests`, ~line 1127)

**Interfaces:**
- Consumes: existing `CompressionProfile` fields, `Db::list_profiles() -> Result<Vec<CompressionProfile>>`, `Db::save_profile(&CompressionProfile) -> Result<()>`.
- Produces: `CompressionProfile::defaults() -> Vec<CompressionProfile>`.
- Produces: `Db::ensure_default_profiles(&self) -> Result<Vec<CompressionProfile>>`.

- [ ] **Step 1: Write the failing tests**

In `crates/core/src/db.rs`'s `mod tests`, add:

```rust
#[test]
fn ensure_default_profiles_is_idempotent() {
    let db = Db::open_in_memory().unwrap();
    let first = db.ensure_default_profiles().unwrap();
    assert_eq!(first.len(), 4);

    let mut names: Vec<&str> = first.iter().map(|profile| profile.name.as_str()).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        ["AVIF 容量優先", "JPEG XL ロスレス", "JPEG 互換用", "WebP 高品質"]
    );
    let by_name = |name: &str| first.iter().find(|profile| profile.name == name).unwrap();
    assert!(by_name("AVIF 容量優先").auto_enabled);
    assert!(!by_name("JPEG XL ロスレス").auto_enabled);
    assert!(!by_name("JPEG 互換用").auto_enabled);

    let first_ids: Vec<String> = first.iter().map(|profile| profile.id.clone()).collect();
    let second = db.ensure_default_profiles().unwrap();
    assert_eq!(second.len(), 4);
    let second_ids: Vec<String> = second.iter().map(|profile| profile.id.clone()).collect();
    assert_eq!(first_ids, second_ids);
    assert_eq!(db.list_profiles().unwrap().len(), 4);
}

#[test]
fn ensure_default_profiles_keeps_existing_rows() {
    let db = Db::open_in_memory().unwrap();
    let custom = CompressionProfile {
        name: "custom".to_string(),
        ..CompressionProfile::default()
    };
    db.save_profile(&custom).unwrap();

    let profiles = db.ensure_default_profiles().unwrap();
    assert_eq!(profiles.len(), 1);
    assert_eq!(profiles[0].id, custom.id);
    assert_eq!(profiles[0].name, "custom");
}

#[test]
fn ensure_default_profiles_does_not_seed_on_read_error() {
    let db = Db::open_in_memory().unwrap();
    db.connection()
        .execute(
            "INSERT INTO compression_profiles(id, name, enabled, auto_enabled, json, sort_order) \
             VALUES ('broken', 'broken', 1, 0, '{', 0)",
            [],
        )
        .unwrap();

    assert!(db.ensure_default_profiles().is_err());
    let count: i64 = db
        .connection()
        .query_row("SELECT COUNT(*) FROM compression_profiles", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 1);
}
```

- [ ] **Step 2: Run the tests and confirm they fail**

Run: `cargo test -p vrc-photo-core ensure_default_profiles`

Expected: compile error, `no method named ensure_default_profiles found for struct Db`.

- [ ] **Step 3: Add `CompressionProfile::defaults()`**

In `crates/core/src/model.rs`, directly after `impl Default for CompressionProfile`, add an inherent `impl CompressionProfile` block with a doc comment ("Built-in presets seeded into an empty database.") and:

```rust
pub fn defaults() -> Vec<Self> {
    vec![
        Self::default(),
        Self {
            name: "AVIF 容量優先".to_string(),
            format: CompressionFormat::Avif,
            quality: 68,
            encode_speed: 6,
            suffix: "_avif".to_string(),
            ..Self::default()
        },
        Self {
            name: "JPEG XL ロスレス".to_string(),
            format: CompressionFormat::JpegXl,
            lossless: true,
            auto_enabled: false,
            suffix: "_jxl".to_string(),
            ..Self::default()
        },
        Self {
            name: "JPEG 互換用".to_string(),
            format: CompressionFormat::Jpeg,
            quality: 88,
            auto_enabled: false,
            suffix: "_jpg".to_string(),
            ..Self::default()
        },
    ]
}
```

These values are a direct move of `crates/ui/src/main.rs:394-420`; do not rename the presets or change the flags.

- [ ] **Step 4: Add `Db::ensure_default_profiles()`**

In `crates/core/src/db.rs`, after `list_profiles()`, add:

```rust
/// Seeds the built-in presets when no profile exists.
///
/// Returns the profiles currently stored. A read error is returned before
/// any preset is written, so a corrupt table is never masked by defaults.
pub fn ensure_default_profiles(&self) -> Result<Vec<CompressionProfile>> {
    let existing = self.list_profiles()?;
    if !existing.is_empty() {
        return Ok(existing);
    }
    for profile in CompressionProfile::defaults() {
        self.save_profile(&profile)?;
    }
    self.list_profiles()
}
```

- [ ] **Step 5: Run the core tests**

Run: `cargo test -p vrc-photo-core`

Expected: PASS, including the three new tests.

- [ ] **Step 6: Commit**

```bash
git add crates/core/src/model.rs crates/core/src/db.rs
git commit -m "feat: add idempotent default profile seeding to core"
```

---

## Task 2: Use the core API from the UI

**Files:**
- Modify: `crates/ui/src/main.rs:390-425` (`ensure_profiles`)
- Test: no new UI test; the logic is covered by Task 1. Existing UI tests must keep passing.

**Interfaces:**
- Consumes: `Db::ensure_default_profiles() -> Result<Vec<CompressionProfile>>` from Task 1.

- [ ] **Step 1: Replace the UI implementation**

Replace the body of `App::ensure_profiles()` (keep the method and its call site at `crates/ui/src/main.rs:336`) with:

```rust
fn ensure_profiles(&mut self) {
    self.profiles = self.db.ensure_default_profiles().unwrap_or_default();
}
```

Delete the `defaults` array and the save loop. Keep the `CompressionProfile` import because the “＋ プロファイル” action at ~line 2648 still uses `CompressionProfile::default()`.

- [ ] **Step 2: Run the UI tests and clippy**

Run: `cargo test -p vrc-photo-ui`

Expected: PASS.

Run: `cargo clippy -p vrc-photo-ui --all-targets -- -D warnings`

Expected: no warnings.

- [ ] **Step 3: Verify the demo restart flow manually**

If a display is available:

```bash
cargo run -p vrc-photo-ui -- --demo --demo-reset
# close the window
cargo run -p vrc-photo-ui -- --demo
# close the window
sqlite3 /tmp/vrc-photo-manager-demo/library.db "SELECT COUNT(*) FROM compression_profiles;"
```

Expected: `4`. Also open the Compression screen and confirm four rows. If `sqlite3` is unavailable, query the same file with any SQLite client. If no display is available, record that this step was skipped and rely on the core tests.

- [ ] **Step 4: Run the full pre-push verification**

Run: `cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`

Expected: all PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/ui/src/main.rs
git commit -m "fix: stop re-seeding compression profiles on startup"
```

---

## Verification

- `cargo test -p vrc-photo-core ensure_default_profiles` covers Review Focus items 1–4.
- The demo restart check covers item 5.
- Full pre-push command must be green before opening the PR.
