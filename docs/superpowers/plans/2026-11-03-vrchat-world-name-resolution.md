# VRChat API ワールド名解決 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 名前がない VRChat ワールドIDを、既定OFFの任意機能として未認証 VRChat API から解決し、ローカルにキャッシュして写真表示へ反映する。

**Architecture:** デーモンに専用の `world-name` ワーカーを追加する。SQLite の `world_name_lookups` がキューと再試行状態を持ち、`worlds.name` が名前のキャッシュになる。API 呼び出しは60秒間隔で、429/5xx/404 を区別してバックオフする。UI は設定カードと、名前がないときのワールドID表示だけを担当する。

**Tech Stack:** Rust 2024, rusqlite, ureq 3.5, gpui-kit, serde_json, chrono

**Spec:** `docs/superpowers/specs/2026-11-03-vrchat-world-name-resolution-design.md`

## Global Constraints

- 機能は既定OFF。`AppSettings.world_name_resolution_enabled` の既定は `false`。
- 認証情報（パスワード、Cookie、トークン、2FA）は要求も保存も送信もしない。
- 送信先は `https://api.vrchat.cloud/api/1/worlds/{worldId}` に固定。リダイレクトは追わない。
- User-Agent は `vrc-photon/<version> (https://github.com/TommyZ7/vrc-photon)`。
- 送信するのは検証済みのワールドID、User-Agent、標準HTTPヘッダーだけ。
- グローバルなAPI呼び出し間隔は60秒。`world_name_next_request_at` を `app_settings` に保存し、再起動後も維持する。
- 429 は `Retry-After`（1〜3600秒）に従い、`attempts` を増やさない。
- 5xx / ネットワーク / タイムアウトは `60 * 2^(attempts-1)` 秒（最大6時間）の指数バックオフ。
- 404 は `not_found` として24時間後に再試行。その他の4xx・3xx・不正レスポンスは terminal。
- API 名は `worlds.name` が NULL のときだけ保存し、`name_source='vrchat-api'` を付ける。ログ/XMP/手動エイリアスを API 名で上書きしない。
- API 名が先に入った場合、後から届いたログ/XMP名は上書きできる。ログ/XMP同士は先着を維持する。
- 名前を保存したら、同じワールドの `photo_context.world_name` が NULL の行を埋め、`library_revision` を進める。
- 推定リンク（`log-estimated`）では XMP 名を `worlds.name` に書き込まない。
- UI のフォールバックは 名前 → ワールドID（`wrld_...`）→「未紐付け」。
- `--once` はワーカーを起動せず、ネットワーク送信もしない。
- 失敗は `daemon_errors` に出さない。ログは `world_db_id` と固定コードだけ。
- 既存のログ/XMP/圧縮/Discord の挙動を変えない。

## Review Focus

1. **ログ/XMP名とAPI名の競合**: API 応答が後から届いても、ログ/XMP名と既存の `photo_context` を API 名で壊さないこと。
2. **再試行状態機械**: 404 / 429 / 5xx / network / invalid が、それぞれ24時間 / Retry-After / 指数バックオフ / terminal になり、短間隔のホットループにならないこと。
3. **無効時・デーモン停止時**: 機能OFF、または UI だけが動いている状態で、API リクエストが1件も発生しないこと。
4. **既存 v11 データ**: `worlds.name` はあるが `photo_context.world_name` が NULL の写真が、API を呼ばずに表示名を得ること。
5. **推定リンク**: `log-estimated` の XMP 名が、推定先のワールドを誤って改名しないこと。
6. **再起動直後**: 前回のリクエストから60秒以内（429時は `Retry-After` 以内）に送信しないこと。

---

### Task 1: 設定フラグ

**Files:**
- Modify: `crates/core/src/settings.rs`（`AppSettings` と `Default`、`mod tests`）
- Test: `crates/core/src/settings.rs` の `mod tests`

**Interfaces:**
- Produces: `AppSettings.world_name_resolution_enabled: bool`（既定 `false`）

- [ ] **Step 1: 失敗するテストを書く**

`mod tests` に追加する。

```rust
#[test]
fn world_name_resolution_defaults_off_and_roundtrips() {
    let db = Db::open_in_memory().unwrap();
    assert!(!AppSettings::default().world_name_resolution_enabled);

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
    let old: AppSettings = serde_json::from_str(json).unwrap();
    assert!(!old.world_name_resolution_enabled);

    let settings = AppSettings {
        world_name_resolution_enabled: true,
        ..Default::default()
    };
    settings.save(&db).unwrap();
    assert!(AppSettings::load(&db).unwrap().world_name_resolution_enabled);
}
```

- [ ] **Step 2: テストが失敗することを確認**

Run: `cargo test -p vrc-photon-core settings::tests::world_name_resolution_defaults_off_and_roundtrips`
Expected: コンパイルエラー（`world_name_resolution_enabled` が未定義）

- [ ] **Step 3: 最小実装を追加**

`AppSettings` に `#[serde(default)] pub world_name_resolution_enabled: bool` を追加し、`Default` で `false` にする。

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo test -p vrc-photon-core settings::tests::world_name_resolution_defaults_off_and_roundtrips`
Expected: PASS

- [ ] **Step 5: コミット**

```bash
git add crates/core/src/settings.rs
git commit -m "feat(core): add the world name resolution setting"
```

---

### Task 2: v12 マイグレーションと core のワールド名型

**Files:**
- Create: `crates/core/src/world_name.rs`
- Modify: `crates/core/src/lib.rs`（`pub mod world_name;`）
- Modify: `crates/core/src/db.rs`（`SCHEMA_V12`、`migrate_v12`、`migrate()`）
- Test: `crates/core/src/world_name.rs` の `mod tests`、`crates/core/src/db.rs` の `mod tests`

**Interfaces:**
- Produces:
  - `WorldNameLookupStatus::{Pending, Retry, Resolved, NotFound, Failed}` と `as_str()` / `parse()`
  - `WorldNameLookup { world_db_id: i64, world_id: String, status: WorldNameLookupStatus, attempts: i64, next_attempt_at: Option<i64> }`
  - `WorldNameFailure::{NotFound, RateLimited { retry_after: i64 }, Retryable { code: &'static str }, Invalid { code: &'static str }}`
  - `WorldNameLookupCounts { pending: i64, resolved: i64, failed: i64 }`
  - `REQUEST_INTERVAL_SECONDS: i64 = 60`
  - `NOT_FOUND_RETRY_SECONDS: i64 = 86_400`
  - `MAX_BACKOFF_SECONDS: i64 = 21_600`
  - `MAX_WORLD_NAME_CHARS: usize = 256`
  - `backoff_seconds(attempts: i64) -> i64`
  - `retry_after_seconds(header: Option<&str>) -> i64`
  - `parse_world_name_response(body: &[u8], expected_world_id: &str) -> Result<String, WorldNameParseError>`
  - `WorldNameParseError::{InvalidJson, MissingName, EmptyName, TooLong, ControlChars, WorldIdMismatch}`
  - `Db::migrate_v12()`

- [ ] **Step 1: 失敗するテストを書く**

`world_name.rs` の `mod tests`:

```rust
#[test]
fn status_roundtrips() {
    for status in [
        WorldNameLookupStatus::Pending,
        WorldNameLookupStatus::Retry,
        WorldNameLookupStatus::Resolved,
        WorldNameLookupStatus::NotFound,
        WorldNameLookupStatus::Failed,
    ] {
        assert_eq!(WorldNameLookupStatus::parse(status.as_str()), Some(status));
    }
}

#[test]
fn backoff_doubles_and_caps_at_six_hours() {
    assert_eq!(backoff_seconds(1), 60);
    assert_eq!(backoff_seconds(2), 120);
    assert_eq!(backoff_seconds(3), 240);
    assert_eq!(backoff_seconds(7), 3_840);
    assert_eq!(backoff_seconds(8), 7_680);
    assert_eq!(backoff_seconds(9), 21_600);
    assert_eq!(backoff_seconds(20), 21_600);
}

#[test]
fn retry_after_is_clamped_and_defaults_to_sixty() {
    assert_eq!(retry_after_seconds(Some("120")), 120);
    assert_eq!(retry_after_seconds(Some("0")), 1);
    assert_eq!(retry_after_seconds(Some("9999")), 3_600);
    assert_eq!(retry_after_seconds(None), 60);
    assert_eq!(retry_after_seconds(Some("abc")), 60);
}

#[test]
fn parses_a_valid_world_name() {
    let body = br#"{"id":"wrld_test","name":"  Test World  "}"#;
    assert_eq!(
        parse_world_name_response(body, "wrld_test"),
        Ok("Test World".to_string())
    );
}

#[test]
fn rejects_invalid_world_names() {
    for body in [
        br#"{"id":"wrld_test"}"#.as_slice(),
        br#"{"id":"wrld_test","name":""}"#,
        br#"{"id":"wrld_test","name":"line\nbreak"}"#,
        br#"{"id":"other","name":"Test"}"#,
        br#"not json"#,
    ] {
        assert!(parse_world_name_response(body, "wrld_test").is_err());
    }
    let long = "x".repeat(MAX_WORLD_NAME_CHARS + 1);
    let body = serde_json::json!({"id": "wrld_test", "name": long});
    assert!(parse_world_name_response(body.to_string().as_bytes(), "wrld_test").is_err());
}
```

`db.rs` の `mod tests`:

```rust
#[test]
fn migration_v12_adds_world_name_lookups() {
    let db = Db::open_in_memory().unwrap();
    let version: i64 = db
        .conn
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(version, 12);
    let table: i64 = db
        .conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='world_name_lookups'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(table, 1);
    let index: i64 = db
        .conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name='idx_world_name_lookups_due'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(index, 1);
}

#[test]
fn migration_v12_is_retryable() {
    let db = Db::open_in_memory().unwrap();
    db.conn.pragma_update(None, "user_version", 11).unwrap();
    db.migrate_v12().unwrap();
    db.migrate_v12().unwrap();
    let version: i64 = db
        .conn
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(version, 12);
}
```

既存の `assert_eq!(version, 11)` をすべて `12` に更新する（`rg -n "assert_eq!\\(version, 11\\)" crates/core/src/db.rs`）。

- [ ] **Step 2: テストが失敗することを確認**

Run: `cargo test -p vrc-photon-core world_name::tests migration_v12`
Expected: コンパイルエラー（`world_name` モジュール、`migrate_v12` が未定義）

- [ ] **Step 3: 最小実装を追加**

`SCHEMA_V12`:

```sql
CREATE TABLE IF NOT EXISTS world_name_lookups (
    world_db_id INTEGER PRIMARY KEY REFERENCES worlds(id) ON DELETE CASCADE,
    status TEXT NOT NULL,
    attempts INTEGER NOT NULL DEFAULT 0,
    next_attempt_at INTEGER,
    last_attempt_at INTEGER,
    last_error TEXT,
    updated_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_world_name_lookups_due
    ON world_name_lookups(status, next_attempt_at);
```

`migrate_v12` は既存の `migrate_v11` と同じ即時トランザクション + `IF NOT EXISTS` + `user_version` 再確認パターンで実装する。`migrate()` の最後に `if version < 12 { self.migrate_v12()?; }` を追加する。

`parse_world_name_response` は `serde_json::Value` で `id` と `name` を読み、`name.trim()`、空、256文字超、制御文字、`id` 不一致を検証する。

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo test -p vrc-photon-core world_name::tests migration_v12`
Expected: PASS

- [ ] **Step 5: コミット**

```bash
git add crates/core/src/world_name.rs crates/core/src/lib.rs crates/core/src/db.rs
git commit -m "feat(core): add world name lookup schema and types"
```

---

### Task 3: ルックアップのキューと再試行状態

**Files:**
- Modify: `crates/core/src/db.rs`（`worlds / sessions` 付近と `mod tests`）
- Test: `crates/core/src/db.rs` の `mod tests`

**Interfaces:**
- Consumes: Task 2 の型とテーブル
- Produces:
  - `Db::enqueue_world_name_lookup(world_db_id: i64) -> Result<()>`
  - `Db::enqueue_missing_world_name_lookups() -> Result<usize>`
  - `Db::next_due_world_name_lookup(now: i64) -> Result<Option<WorldNameLookup>>`
  - `Db::world_name_lookup(world_db_id: i64) -> Result<Option<WorldNameLookup>>`
  - `Db::mark_world_name_lookup_satisfied(world_db_id: i64) -> Result<()>`
  - `Db::mark_world_name_lookup_resolved(world_db_id: i64, now: i64) -> Result<()>`
  - `Db::record_world_name_failure(world_db_id: i64, failure: WorldNameFailure, now: i64) -> Result<()>`
  - `Db::world_name_next_request_at() -> Result<Option<i64>>`
  - `Db::set_world_name_next_request_at(timestamp: i64) -> Result<()>`
  - `Db::world_name_lookup_counts() -> Result<WorldNameLookupCounts>`

- [ ] **Step 1: 失敗するテストを書く**

`db.rs` の `mod tests` に追加する。既存の `sample_photo` / `photo_at` / `set_photo_context` を使う。

```rust
#[test]
fn enqueue_missing_world_name_lookups_queues_only_unresolved_referenced_worlds() {
    let db = Db::open_in_memory().unwrap();
    let root = db.add_root("/photos", "reference").unwrap();
    let photo_a = db.upsert_photo(&sample_photo(root, "a.png", 100)).unwrap();
    let photo_b = db.upsert_photo(&sample_photo(root, "b.png", 200)).unwrap();
    let photo_c = db.upsert_photo(&sample_photo(root, "c.png", 300)).unwrap();

    let unresolved = db.upsert_world("wrld_unresolved", None, None).unwrap();
    let known = db.upsert_world("wrld_known", Some("Known"), Some("log")).unwrap();
    let xmp_only = db.upsert_world("wrld_xmp", None, None).unwrap();
    let unreferenced = db.upsert_world("wrld_unreferenced", None, None).unwrap();
    let aliased = db.upsert_world("wrld_alias", None, None).unwrap();
    db.conn
        .execute(
            "UPDATE worlds SET alias = ?1 WHERE id = ?2",
            params!["Alias", aliased],
        )
        .unwrap();

    for (photo, world, name) in [
        (photo_a, unresolved, None),
        (photo_b, known, None),
        (photo_c, xmp_only, Some("XMP World")),
    ] {
        db.set_photo_context(&PhotoContext {
            photo_id: photo,
            session_id: None,
            world_id: Some(world),
            world_name: name.map(str::to_string),
            instance_id: None,
            link_method: "log".to_string(),
            confidence: 0.9,
            author: None,
            linked_at: "2026-11-03T00:00:00+00:00".to_string(),
        })
        .unwrap();
    }

    assert_eq!(db.enqueue_missing_world_name_lookups().unwrap(), 1);
    assert_eq!(db.enqueue_missing_world_name_lookups().unwrap(), 0);
    let queued = db.next_due_world_name_lookup(0).unwrap().unwrap();
    assert_eq!(queued.world_db_id, unresolved);
    assert_eq!(queued.world_id, "wrld_unresolved");
    assert_eq!(queued.status, WorldNameLookupStatus::Pending);
    assert!(db.next_due_world_name_lookup(0).unwrap().is_some());
    assert!(db.world_by_id(unreferenced).unwrap().is_some());
}
```

```rust
#[test]
fn next_due_world_name_lookup_respects_schedule_and_not_found() {
    let db = Db::open_in_memory().unwrap();
    let pending = db.upsert_world("wrld_pending", None, None).unwrap();
    let retry = db.upsert_world("wrld_retry", None, None).unwrap();
    let not_found = db.upsert_world("wrld_404", None, None).unwrap();
    db.enqueue_world_name_lookup(pending).unwrap();
    db.enqueue_world_name_lookup(retry).unwrap();
    db.enqueue_world_name_lookup(not_found).unwrap();
    db.record_world_name_failure(
        retry,
        WorldNameFailure::Retryable { code: "http_5xx" },
        100,
    )
    .unwrap();
    db.record_world_name_failure(not_found, WorldNameFailure::NotFound, 100)
        .unwrap();

    assert_eq!(
        db.next_due_world_name_lookup(100).unwrap().unwrap().world_db_id,
        pending
    );
    assert_eq!(
        db.next_due_world_name_lookup(159).unwrap().unwrap().world_db_id,
        pending
    );
    db.mark_world_name_lookup_resolved(pending, 159).unwrap();
    assert_eq!(
        db.next_due_world_name_lookup(160).unwrap().unwrap().world_db_id,
        retry
    );
    assert_eq!(
        db.next_due_world_name_lookup(86_499).unwrap().unwrap().world_db_id,
        retry
    );
    db.mark_world_name_lookup_resolved(retry, 86_499).unwrap();
    assert_eq!(
        db.next_due_world_name_lookup(86_500).unwrap().unwrap().world_db_id,
        not_found
    );
}
```

```rust
#[test]
fn failures_schedule_the_expected_backoff() {
    let db = Db::open_in_memory().unwrap();
    let world = db.upsert_world("wrld_test", None, None).unwrap();
    db.enqueue_world_name_lookup(world).unwrap();

    db.record_world_name_failure(world, WorldNameFailure::RateLimited { retry_after: 120 }, 1_000)
        .unwrap();
    let row = db.world_name_lookup(world).unwrap().unwrap();
    assert_eq!(row.status, WorldNameLookupStatus::Retry);
    assert_eq!(row.attempts, 0);
    assert_eq!(row.next_attempt_at, Some(1_120));

    db.record_world_name_failure(world, WorldNameFailure::Retryable { code: "http_5xx" }, 2_000)
        .unwrap();
    let row = db.world_name_lookup(world).unwrap().unwrap();
    assert_eq!(row.status, WorldNameLookupStatus::Retry);
    assert_eq!(row.attempts, 1);
    assert_eq!(row.next_attempt_at, Some(2_060));

    db.record_world_name_failure(world, WorldNameFailure::NotFound, 3_000)
        .unwrap();
    let row = db.world_name_lookup(world).unwrap().unwrap();
    assert_eq!(row.status, WorldNameLookupStatus::NotFound);
    assert_eq!(row.next_attempt_at, Some(89_400));

    db.record_world_name_failure(world, WorldNameFailure::Invalid { code: "invalid_response" }, 4_000)
        .unwrap();
    let row = db.world_name_lookup(world).unwrap().unwrap();
    assert_eq!(row.status, WorldNameLookupStatus::Failed);
    assert_eq!(row.next_attempt_at, None);
}
```

`world_name_lookup(world_db_id)` はテスト用の読み取りメソッドとして追加する。

```rust
#[test]
fn next_request_at_and_counts_roundtrip() {
    let db = Db::open_in_memory().unwrap();
    assert_eq!(db.world_name_next_request_at().unwrap(), None);
    db.set_world_name_next_request_at(1_700_000_000).unwrap();
    assert_eq!(
        db.world_name_next_request_at().unwrap(),
        Some(1_700_000_000)
    );

    let pending = db.upsert_world("wrld_pending", None, None).unwrap();
    let retry = db.upsert_world("wrld_retry", None, None).unwrap();
    let resolved = db.upsert_world("wrld_resolved", None, None).unwrap();
    let failed = db.upsert_world("wrld_failed", None, None).unwrap();
    for world in [pending, retry, resolved, failed] {
        db.enqueue_world_name_lookup(world).unwrap();
    }
    db.record_world_name_failure(retry, WorldNameFailure::NotFound, 0)
        .unwrap();
    db.mark_world_name_lookup_resolved(resolved, 0).unwrap();
    db.record_world_name_failure(failed, WorldNameFailure::Invalid { code: "invalid_response" }, 0)
        .unwrap();

    let counts = db.world_name_lookup_counts().unwrap();
    assert_eq!(counts.pending, 2);
    assert_eq!(counts.resolved, 1);
    assert_eq!(counts.failed, 1);
}
```

- [ ] **Step 2: テストが失敗することを確認**

Run: `cargo test -p vrc-photon-core db::tests::enqueue_missing_world_name_lookups_queues_only_unresolved_referenced_worlds`
Expected: コンパイルエラー（メソッド未定義）

- [ ] **Step 3: 最小実装を追加**

- `enqueue_missing_world_name_lookups` は `INSERT INTO world_name_lookups ... SELECT ... WHERE alias IS NULL AND name IS NULL AND EXISTS (SELECT 1 FROM photo_context c WHERE c.world_id = w.id AND c.world_name IS NULL) AND NOT EXISTS (...)`。
- `next_due_world_name_lookup` は `status IN ('pending','retry','not_found')` かつ `next_attempt_at IS NULL OR next_attempt_at <= ?` を `COALESCE(next_attempt_at, 0), world_db_id` 順で1件。
- `record_world_name_failure` は `WorldNameFailure` ごとに `status` / `attempts` / `next_attempt_at` / `last_attempt_at` / `last_error` を更新する。429 は `attempts` を増やさない。
- `mark_world_name_lookup_satisfied` / `mark_world_name_lookup_resolved` は `status IN ('pending','retry','not_found')` の行だけを `resolved` にする。`mark_world_name_lookup_resolved` は `last_attempt_at = now` も記録する。
- `world_name_next_request_at` / `set_world_name_next_request_at` は `app_settings` の `world_name_next_request_at` キーを使う。
- `world_name_lookup_counts` は `pending` に `pending` / `retry` / `not_found` をまとめる。

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo test -p vrc-photon-core db::tests::enqueue_missing_world_name_lookups_queues_only_unresolved_referenced_worlds db::tests::next_due_world_name_lookup_respects_schedule_and_not_found db::tests::failures_schedule_the_expected_backoff db::tests::next_request_at_and_counts_roundtrip`
Expected: PASS

- [ ] **Step 5: コミット**

```bash
git add crates/core/src/db.rs
git commit -m "feat(core): queue and retry world name lookups"
```

---

### Task 4: 名前の優先順位と photo_context バックフィル

**Files:**
- Modify: `crates/core/src/db.rs`（`upsert_world`、`world_by_id` 付近、`mod tests`）
- Modify: `crates/core/src/linker.rs`（確定リンクのXMP名、`mod tests`）
- Test: `crates/core/src/db.rs` と `crates/core/src/linker.rs` の `mod tests`

**Interfaces:**
- Consumes: Task 3 の `mark_world_name_lookup_satisfied` / `mark_world_name_lookup_resolved`
- Produces:
  - `Db::backfill_photo_context_world_names() -> Result<usize>`
  - `Db::record_world_name_resolved(world_db_id: i64, name: &str, now: i64) -> Result<()>`
  - `Db::world_by_world_id(world_id: &str) -> Result<Option<World>>`
  - `upsert_world` の優先順位とコンテキストバックフィル
  - `link_photo_with` の確定リンクXMP名反映

- [ ] **Step 1: 失敗するテストを書く**

`db.rs` の `mod tests`:

```rust
#[test]
fn backfill_photo_context_world_names_fills_nulls_with_alias_then_name() {
    let db = Db::open_in_memory().unwrap();
    let root = db.add_root("/photos", "reference").unwrap();
    let photo = db.upsert_photo(&sample_photo(root, "a.png", 100)).unwrap();
    let world = db.upsert_world("wrld_test", Some("World Name"), Some("log")).unwrap();
    db.conn
        .execute(
            "UPDATE worlds SET alias = ?1 WHERE id = ?2",
            params!["Alias Name", world],
        )
        .unwrap();
    db.set_photo_context(&PhotoContext {
        photo_id: photo,
        session_id: None,
        world_id: Some(world),
        world_name: None,
        instance_id: None,
        link_method: "log".to_string(),
        confidence: 0.9,
        author: None,
        linked_at: "2026-11-03T00:00:00+00:00".to_string(),
    })
    .unwrap();
    let before = db.library_revision().unwrap();

    assert_eq!(db.backfill_photo_context_world_names().unwrap(), 1);
    assert_eq!(
        db.photo_context(photo).unwrap().unwrap().world_name.as_deref(),
        Some("Alias Name")
    );
    assert!(db.library_revision().unwrap() > before);
    assert_eq!(db.backfill_photo_context_world_names().unwrap(), 0);
}
```

```rust
#[test]
fn record_world_name_resolved_fills_only_nulls_and_keeps_log_names() {
    let db = Db::open_in_memory().unwrap();
    let root = db.add_root("/photos", "reference").unwrap();
    let null_photo = db.upsert_photo(&sample_photo(root, "a.png", 100)).unwrap();
    let xmp_photo = db.upsert_photo(&sample_photo(root, "b.png", 200)).unwrap();
    let world = db.upsert_world("wrld_test", None, None).unwrap();
    for (photo, name) in [(null_photo, None), (xmp_photo, Some("XMP World"))] {
        db.set_photo_context(&PhotoContext {
            photo_id: photo,
            session_id: None,
            world_id: Some(world),
            world_name: name.map(str::to_string),
            instance_id: None,
            link_method: "log".to_string(),
            confidence: 0.9,
            author: None,
            linked_at: "2026-11-03T00:00:00+00:00".to_string(),
        })
        .unwrap();
    }
    db.enqueue_world_name_lookup(world).unwrap();

    db.record_world_name_resolved(world, "API World", 1_000).unwrap();
    let stored = db.world_by_id(world).unwrap().unwrap();
    assert_eq!(stored.name.as_deref(), Some("API World"));
    assert_eq!(stored.name_source.as_deref(), Some("vrchat-api"));
    assert_eq!(
        db.photo_context(null_photo).unwrap().unwrap().world_name.as_deref(),
        Some("API World")
    );
    assert_eq!(
        db.photo_context(xmp_photo).unwrap().unwrap().world_name.as_deref(),
        Some("XMP World")
    );
    assert_eq!(
        db.world_name_lookup(world).unwrap().unwrap().status,
        WorldNameLookupStatus::Resolved
    );

    db.record_world_name_resolved(world, "Ignored API World", 2_000).unwrap();
    assert_eq!(
        db.world_by_id(world).unwrap().unwrap().name.as_deref(),
        Some("API World")
    );
}
```

```rust
#[test]
fn api_result_after_log_name_fills_null_contexts_with_the_log_name() {
    let db = Db::open_in_memory().unwrap();
    let root = db.add_root("/photos", "reference").unwrap();
    let photo = db.upsert_photo(&sample_photo(root, "a.png", 100)).unwrap();
    let world = db
        .upsert_world("wrld_test", Some("Log World"), Some("log"))
        .unwrap();
    db.set_photo_context(&PhotoContext {
        photo_id: photo,
        session_id: None,
        world_id: Some(world),
        world_name: None,
        instance_id: None,
        link_method: "log".to_string(),
        confidence: 0.9,
        author: None,
        linked_at: "2026-11-03T00:00:00+00:00".to_string(),
    })
    .unwrap();
    db.enqueue_world_name_lookup(world).unwrap();

    db.record_world_name_resolved(world, "API World", 1_000)
        .unwrap();
    assert_eq!(
        db.world_by_id(world).unwrap().unwrap().name.as_deref(),
        Some("Log World")
    );
    assert_eq!(
        db.photo_context(photo).unwrap().unwrap().world_name.as_deref(),
        Some("Log World")
    );
}
```

```rust
#[test]
fn log_name_overwrites_api_name_and_updates_contexts() {
    let db = Db::open_in_memory().unwrap();
    let root = db.add_root("/photos", "reference").unwrap();
    let photo = db.upsert_photo(&sample_photo(root, "a.png", 100)).unwrap();
    let world = db.upsert_world("wrld_test", Some("API World"), Some("vrchat-api")).unwrap();
    db.set_photo_context(&PhotoContext {
        photo_id: photo,
        session_id: None,
        world_id: Some(world),
        world_name: Some("API World".to_string()),
        instance_id: None,
        link_method: "log".to_string(),
        confidence: 0.9,
        author: None,
        linked_at: "2026-11-03T00:00:00+00:00".to_string(),
    })
    .unwrap();
    db.enqueue_world_name_lookup(world).unwrap();

    db.upsert_world("wrld_test", Some("Log World"), Some("log")).unwrap();
    let stored = db.world_by_id(world).unwrap().unwrap();
    assert_eq!(stored.name.as_deref(), Some("Log World"));
    assert_eq!(stored.name_source.as_deref(), Some("log"));
    assert_eq!(
        db.photo_context(photo).unwrap().unwrap().world_name.as_deref(),
        Some("Log World")
    );
    assert_eq!(
        db.world_name_lookup(world).unwrap().unwrap().status,
        WorldNameLookupStatus::Resolved
    );

    db.upsert_world("wrld_test", Some("XMP World"), Some("xmp")).unwrap();
    assert_eq!(
        db.world_by_id(world).unwrap().unwrap().name.as_deref(),
        Some("Log World")
    );
}
```

`linker.rs` の `mod tests`:

```rust
#[test]
fn confirmed_link_promotes_xmp_world_name_to_worlds() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("VRChat_2026-10-21_21-24-00.000_4x3.png");
    write_png_with_xmp(&path, r#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description xmlns:vrc="https://vrchat.com/xmp/1.0/" vrc:WorldDisplayName="XMP World" vrc:WorldId="wrld_xmp"/></rdf:RDF></x:xmpmeta>"#);
    let db = Db::open_in_memory().unwrap();
    let world = db.upsert_world("wrld_xmp", None, None).unwrap();
    db.create_session("log.txt", 1_700_000_000, world, None).unwrap();
    let photo_id = insert_photo_at(&db, 1_700_000_001, &path);
    let photo = db.photo_by_id(photo_id).unwrap().unwrap();

    assert!(link_photo(&db, &photo).unwrap());
    let stored = db.world_by_id(world).unwrap().unwrap();
    assert_eq!(stored.name.as_deref(), Some("XMP World"));
    assert_eq!(stored.name_source.as_deref(), Some("xmp"));
}

#[test]
fn estimated_link_does_not_promote_xmp_world_name() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("VRChat_2026-10-21_21-24-00.000_4x3.png");
    write_png_with_xmp(&path, r#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description xmlns:vrc="https://vrchat.com/xmp/1.0/" vrc:WorldDisplayName="XMP World" vrc:WorldId="wrld_xmp"/></rdf:RDF></x:xmpmeta>"#);
    let db = Db::open_in_memory().unwrap();
    let world = db.upsert_world("wrld_guessed", None, None).unwrap();
    db.create_session("log.txt", 1_700_000_000, world, None).unwrap();
    // photo はセッション開始の3秒以内だが、セッションには含まれない時刻にする
    let photo_id = insert_photo_at(&db, 1_699_999_998, &path);
    let photo = db.photo_by_id(photo_id).unwrap().unwrap();

    assert!(link_photo(&db, &photo).unwrap());
    assert_eq!(db.world_by_id(world).unwrap().unwrap().name, None);
    assert_eq!(
        db.photo_context(photo_id).unwrap().unwrap().world_name.as_deref(),
        Some("XMP World")
    );
}
```

テストモジュールに次のヘルパーを追加する。`write_png_with_xmp` は既存の `links_xmp_only_photos` の PNG 書き込みコードを関数化し、`insert_photo_at` は `abs_path` を差し替えられるようにした `insert_photo` の拡張とする。

```rust
fn write_png_with_xmp(path: &Path, xmp: &str) {
    let file = std::fs::File::create(path).unwrap();
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), 1, 1);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder
        .add_itxt_chunk("XML:com.adobe.xmp".to_string(), xmp.to_string())
        .unwrap();
    let mut writer = encoder.write_header().unwrap();
    writer.write_image_data(&[255, 0, 0, 255]).unwrap();
}

fn insert_photo_at(db: &Db, ts: i64, path: &Path) -> i64 {
    let root = db.add_root("/photos", "reference").unwrap();
    db.upsert_photo(&NewPhoto {
        root_id: root,
        rel_path: "a.png".to_string(),
        abs_path: path.to_string_lossy().to_string(),
        file_name: "a.png".to_string(),
        captured_at_utc: ts,
        captured_at_local: "2026-10-21 21:24:00.000".to_string(),
        tz_offset_sec: 9 * 3600,
        width: 4,
        height: 3,
        file_size: 100,
        mtime: ts,
        blake3: None,
    })
    .unwrap()
}
```

既存の `insert_photo` は `insert_photo_at(db, ts, Path::new("/photos/a.png"))` を呼ぶ形にしてもよい。

- [ ] **Step 2: テストが失敗することを確認**

Run: `cargo test -p vrc-photon-core db::tests::backfill_photo_context_world_names_fills_nulls_with_alias_then_name db::tests::record_world_name_resolved_fills_only_nulls_and_keeps_log_names db::tests::api_result_after_log_name_fills_null_contexts_with_the_log_name db::tests::log_name_overwrites_api_name_and_updates_contexts linker::tests::confirmed_link_promotes_xmp_world_name_to_worlds linker::tests::estimated_link_does_not_promote_xmp_world_name`
Expected: コンパイルエラー（メソッド未定義）

- [ ] **Step 3: 最小実装を追加**

- `backfill_photo_context_world_names` は `photo_context.world_name IS NULL` の行を `COALESCE(worlds.alias, worlds.name)` で埋め、更新件数が1以上なら `library_revision` を進める。
- `record_world_name_resolved` は `world_by_id` で文字列IDを取得し、`upsert_world(&world.world_id, Some(name), Some("vrchat-api"))` を呼び、`mark_world_name_lookup_resolved(world_db_id, now)` を呼ぶ。
- `upsert_world` は次の優先順位で名前を更新する:
  - 既存 name が NULL → 新しい name を保存
  - 既存 source が `vrchat-api` かつ新しい source が `log` / `xmp` → 新しい name を保存
  - それ以外 → 既存 name を維持
  - 名前が変わったら、`photo_context.world_name` が NULL または旧API名と一致する行を新しい実効名（`COALESCE(alias, name)`）で更新する
  - 名前が変わらない場合も、`photo_context.world_name` が NULL の行は実効名で埋める
  - ログ/XMP名を保存したら `mark_world_name_lookup_satisfied` を呼ぶ
  - コンテキスト更新件数が1以上なら `library_revision` を進める
- `link_photo_with` の確定リンク分岐で、XMP の `world_name` があり、`xmp.world_id` が `None` またはセッションの `world.world_id` と一致する場合、`worlds.name` が NULL または `name_source='vrchat-api'` なら `upsert_world(..., Some(xmp_name), Some("xmp"))` を呼ぶ。推定リンク分岐では呼ばない。

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo test -p vrc-photon-core db::tests::backfill_photo_context_world_names_fills_nulls_with_alias_then_name db::tests::record_world_name_resolved_fills_only_nulls_and_keeps_log_names db::tests::api_result_after_log_name_fills_null_contexts_with_the_log_name db::tests::log_name_overwrites_api_name_and_updates_contexts linker::tests::confirmed_link_promotes_xmp_world_name_to_worlds linker::tests::estimated_link_does_not_promote_xmp_world_name`
Expected: PASS

- [ ] **Step 5: コミット**

```bash
git add crates/core/src/db.rs crates/core/src/linker.rs
git commit -m "feat(core): prefer log and XMP names and backfill photo contexts"
```

---

### Task 5: レスポンス解析とバックオフの純粋関数

**Files:**
- Modify: `crates/core/src/world_name.rs`
- Test: `crates/core/src/world_name.rs` の `mod tests`

**Interfaces:**
- Consumes: Task 2 の定数と型
- Produces: Task 2 で定義した `parse_world_name_response` / `backoff_seconds` / `retry_after_seconds` の完成

- [ ] **Step 1: 追加の失敗テストを書く**

```rust
#[test]
fn rejects_a_name_with_a_tab_or_carriage_return() {
    for name in ["line\tbreak", "line\rbreak"] {
        let body = serde_json::json!({"id": "wrld_test", "name": name});
        assert!(
            parse_world_name_response(body.to_string().as_bytes(), "wrld_test").is_err()
        );
    }
}

#[test]
fn accepts_a_name_at_the_limit() {
    let name = "x".repeat(MAX_WORLD_NAME_CHARS);
    let body = serde_json::json!({"id": "wrld_test", "name": name.clone()});
    assert_eq!(
        parse_world_name_response(body.to_string().as_bytes(), "wrld_test"),
        Ok(name)
    );
}
```

- [ ] **Step 2: テストが失敗することを確認**

Run: `cargo test -p vrc-photon-core world_name::tests::rejects_a_name_with_a_tab_or_carriage_return world_name::tests::accepts_a_name_at_the_limit`
Expected: 制御文字テストが FAIL（実装がタブ/CRを許可している場合）

- [ ] **Step 3: 最小実装を追加**

`parse_world_name_response` の検証に `char::is_control()` を追加する。`backoff_seconds` は `60 * 2^(attempts-1)` を `MAX_BACKOFF_SECONDS` で cap する。`retry_after_seconds` は整数秒を1〜3600に clamp し、失敗時は60を返す。

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo test -p vrc-photon-core world_name::tests`
Expected: PASS

- [ ] **Step 5: コミット**

```bash
git add crates/core/src/world_name.rs
git commit -m "test(core): cover world name parsing and backoff limits"
```

---

### Task 6: VRChat API HTTP トランスポート

**Files:**
- Create: `crates/daemon/src/world_name_http.rs`
- Modify: `crates/daemon/src/lib.rs`（`mod world_name_http;`）
- Test: `crates/daemon/src/world_name_http.rs` の `mod tests`

**Interfaces:**
- Consumes: `vrc_photon_core::world_name::{parse_world_name_response, retry_after_seconds}`
- Produces:
  - `#[derive(Debug, Clone, PartialEq, Eq)] WorldNameOutcome::{Found(String), NotFound, RateLimited { retry_after: Duration }, Retryable { code: &'static str }, Invalid { code: &'static str }}`
  - `trait WorldNameTransport: Send + Sync { fn fetch(&self, world_id: &str) -> WorldNameOutcome; }`
  - `UreqWorldNameTransport::new()`
  - `#[cfg(test)] UreqWorldNameTransport::for_loopback()`
  - `classify_response(status: u16, retry_after_header: Option<&str>, body: &[u8], expected_world_id: &str) -> WorldNameOutcome`

- [ ] **Step 1: 失敗するテストを書く**

```rust
#[test]
fn classifies_success_and_failures() {
    assert_eq!(
        classify_response(200, None, br#"{"id":"wrld_test","name":"Test"}"#, "wrld_test"),
        WorldNameOutcome::Found("Test".to_string())
    );
    assert_eq!(
        classify_response(404, None, b"", "wrld_test"),
        WorldNameOutcome::NotFound
    );
    assert_eq!(
        classify_response(429, Some("120"), b"", "wrld_test"),
        WorldNameOutcome::RateLimited {
            retry_after: Duration::from_secs(120)
        }
    );
    assert_eq!(
        classify_response(500, None, b"", "wrld_test"),
        WorldNameOutcome::Retryable { code: "http_5xx" }
    );
    assert_eq!(
        classify_response(400, None, b"", "wrld_test"),
        WorldNameOutcome::Invalid { code: "http_4xx" }
    );
    assert_eq!(
        classify_response(302, None, b"", "wrld_test"),
        WorldNameOutcome::Invalid { code: "redirect" }
    );
}

#[test]
fn rejects_invalid_world_ids_without_a_request() {
    let transport = UreqWorldNameTransport::new();
    assert_eq!(
        transport.fetch("../../etc/passwd"),
        WorldNameOutcome::Invalid { code: "invalid_world_id" }
    );
}

#[test]
fn sends_the_required_user_agent() {
    let (url, request) = loopback_server_once(|request| {
        assert!(request.contains("GET /api/1/worlds/wrld_test HTTP/1.1"));
        assert!(request.to_ascii_lowercase().contains("user-agent: vrc-photon/"));
        assert!(request.contains("(https://github.com/TommyZ7/vrc-photon)"));
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 32\r\n\r\n{\"id\":\"wrld_test\",\"name\":\"Test\"}".to_string()
    });
    let transport = UreqWorldNameTransport::for_loopback_with_base(url);
    assert_eq!(
        transport.fetch("wrld_test"),
        WorldNameOutcome::Found("Test".to_string())
    );
    request.join().unwrap();
}

#[test]
fn rejects_oversized_response_bodies() {
    let body = "x".repeat(64 * 1024 + 1);
    let (url, request) = loopback_server_once(move |_| {
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        )
    });
    let transport = UreqWorldNameTransport::for_loopback_with_base(url);
    assert_eq!(
        transport.fetch("wrld_test"),
        WorldNameOutcome::Invalid {
            code: "response_too_large"
        }
    );
    request.join().unwrap();
}
```

`loopback_server_once` と `for_loopback_with_base` はテスト専用ヘルパー。既存の `discord_http.rs` の loopback テストを参考にする。本番の `UreqWorldNameTransport::new()` は固定URLを使い、テスト用だけ base URL を差し替える。

- [ ] **Step 2: テストが失敗することを確認**

Run: `cargo test -p vrc-photon-daemon world_name_http::tests`
Expected: コンパイルエラー（モジュール未定義）

- [ ] **Step 3: 最小実装を追加**

- `UreqWorldNameTransport` は `ureq::Agent` を構築する:
  - `https_only(true)`
  - `max_redirects(0)`
  - `timeout_connect(10s)`
  - `timeout_global(30s)`
  - `http_status_as_error(false)`
  - `user_agent("vrc-photon/<version> (https://github.com/TommyZ7/vrc-photon)")`
- `fetch` は `wrld_` + 英数字・`-`・`_`、最大64文字を検証し、URL を `format!("{base}/api/1/worlds/{world_id}")` で作る。
- レスポンス本文は `64 * 1024` バイトで制限する。
- `classify_response` は 2xx / 404 / 429 / 5xx / その他で分岐し、2xx は `parse_world_name_response` に渡す。パース失敗は `Invalid { code: "invalid_response" }`。
- ネットワークエラーは `Retryable { code: "network" }`。

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo test -p vrc-photon-daemon world_name_http::tests`
Expected: PASS

- [ ] **Step 5: コミット**

```bash
git add crates/daemon/src/world_name_http.rs crates/daemon/src/lib.rs
git commit -m "feat(daemon): add the VRChat world name HTTP transport"
```

---

### Task 7: ワールド名ワーカー

**Files:**
- Create: `crates/daemon/src/world_names.rs`
- Modify: `crates/daemon/src/lib.rs`（`mod world_names;`）
- Test: `crates/daemon/src/world_names.rs` の `mod tests`

**Interfaces:**
- Consumes: Task 3 / 4 の DB メソッド、Task 6 の `WorldNameTransport`
- Produces:
  - `#[derive(Debug, Clone, PartialEq, Eq)] WorldNameStepOutcome::{Resolved, NotFound, RateLimited { retry_after: i64 }, RetryScheduled { next_attempt_at: i64 }, Invalid}`
  - `#[derive(Debug, Clone, PartialEq, Eq)] WorldNameStep { outcome: WorldNameStepOutcome, next_request_at: i64 }`
  - `process_next_world_name(db: &Arc<Mutex<Db>>, transport: &dyn WorldNameTransport, now: i64) -> anyhow::Result<Option<WorldNameStep>>`
  - `worker_loop(db: Arc<Mutex<Db>>, running: Arc<AtomicBool>, enabled: Arc<AtomicBool>, wake: Arc<JobSignal>, transport: Arc<dyn WorldNameTransport>)`
  - `initial_next_request_at(db: &Db) -> anyhow::Result<i64>`

- [ ] **Step 1: 失敗するテストを書く**

```rust
#[derive(Default)]
struct FakeTransport {
    calls: Mutex<Vec<String>>,
    outcomes: Mutex<VecDeque<WorldNameOutcome>>,
}

impl WorldNameTransport for FakeTransport {
    fn fetch(&self, world_id: &str) -> WorldNameOutcome {
        self.calls.lock().unwrap().push(world_id.to_string());
        self.outcomes
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(WorldNameOutcome::NotFound)
    }
}

#[test]
fn success_updates_the_database_and_schedules_sixty_seconds() {
    let db = Arc::new(Mutex::new(Db::open_in_memory().unwrap()));
    let world = {
        let db = db.lock().unwrap();
        db.upsert_world("wrld_test", None, None).unwrap()
    };
    db.lock().unwrap().enqueue_world_name_lookup(world).unwrap();
    let transport = FakeTransport {
        outcomes: Mutex::new(VecDeque::from([WorldNameOutcome::Found("API World".to_string())])),
        ..Default::default()
    };

    let step = process_next_world_name(&db, &transport, 1_000).unwrap().unwrap();
    assert_eq!(step.outcome, WorldNameStepOutcome::Resolved);
    assert_eq!(step.next_request_at, 1_060);
    assert_eq!(transport.calls.lock().unwrap().as_slice(), ["wrld_test"]);
    assert_eq!(
        db.lock().unwrap().world_by_id(world).unwrap().unwrap().name.as_deref(),
        Some("API World")
    );
}

#[test]
fn failure_outcomes_schedule_the_expected_next_request() {
    for (outcome, expected_outcome, expected_next) in [
        (
            WorldNameOutcome::NotFound,
            WorldNameStepOutcome::NotFound,
            1_060,
        ),
        (
            WorldNameOutcome::RateLimited {
                retry_after: Duration::from_secs(120),
            },
            WorldNameStepOutcome::RateLimited { retry_after: 120 },
            1_120,
        ),
        (
            WorldNameOutcome::Retryable { code: "http_5xx" },
            WorldNameStepOutcome::RetryScheduled {
                next_attempt_at: 1_060,
            },
            1_060,
        ),
        (
            WorldNameOutcome::Invalid { code: "invalid_response" },
            WorldNameStepOutcome::Invalid,
            1_060,
        ),
    ] {
        let db = Arc::new(Mutex::new(Db::open_in_memory().unwrap()));
        let world = {
            let db = db.lock().unwrap();
            db.upsert_world("wrld_test", None, None).unwrap()
        };
        db.lock().unwrap().enqueue_world_name_lookup(world).unwrap();
        let transport = FakeTransport {
            outcomes: Mutex::new(VecDeque::from([outcome])),
            ..Default::default()
        };
        let step = process_next_world_name(&db, &transport, 1_000).unwrap().unwrap();
        assert_eq!(step.outcome, expected_outcome);
        assert_eq!(step.next_request_at, expected_next);
    }
}

#[test]
fn no_due_lookup_returns_none_without_a_request() {
    let db = Arc::new(Mutex::new(Db::open_in_memory().unwrap()));
    let transport = FakeTransport::default();
    assert!(process_next_world_name(&db, &transport, 1_000).unwrap().is_none());
    assert!(transport.calls.lock().unwrap().is_empty());
}

#[test]
fn initial_next_request_at_reads_the_persisted_value() {
    let db = Db::open_in_memory().unwrap();
    assert_eq!(initial_next_request_at(&db).unwrap(), 0);
    db.set_world_name_next_request_at(1_700_000_000).unwrap();
    assert_eq!(initial_next_request_at(&db).unwrap(), 1_700_000_000);
}

#[test]
fn disabled_worker_never_fetches() {
    let db = Arc::new(Mutex::new(Db::open_in_memory().unwrap()));
    let running = Arc::new(AtomicBool::new(true));
    let enabled = Arc::new(AtomicBool::new(false));
    let wake = Arc::new(JobSignal::default());
    let transport = Arc::new(FakeTransport::default());
    let handle = {
        let db = db.clone();
        let running = running.clone();
        let enabled = enabled.clone();
        let wake = wake.clone();
        let transport = transport.clone();
        std::thread::spawn(move || worker_loop(db, running, enabled, wake, transport))
    };
    std::thread::sleep(Duration::from_millis(50));
    assert!(transport.calls.lock().unwrap().is_empty());
    running.store(false, Ordering::Relaxed);
    wake.notify();
    handle.join().unwrap();
}

#[test]
fn worker_loop_reserves_the_next_request_before_fetching() {
    let db = Arc::new(Mutex::new(Db::open_in_memory().unwrap()));
    let world = {
        let db = db.lock().unwrap();
        db.upsert_world("wrld_test", None, None).unwrap()
    };
    db.lock().unwrap().enqueue_world_name_lookup(world).unwrap();
    let running = Arc::new(AtomicBool::new(true));
    let enabled = Arc::new(AtomicBool::new(true));
    let wake = Arc::new(JobSignal::default());
    let transport = Arc::new(FakeTransport {
        outcomes: Mutex::new(VecDeque::from([WorldNameOutcome::Found(
            "API World".to_string(),
        )])),
        ..Default::default()
    });
    let handle = {
        let db = db.clone();
        let running = running.clone();
        let enabled = enabled.clone();
        let wake = wake.clone();
        let transport = transport.clone();
        std::thread::spawn(move || worker_loop(db, running, enabled, wake, transport))
    };
    let deadline = Instant::now() + Duration::from_secs(2);
    while transport.calls.lock().unwrap().is_empty() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    running.store(false, Ordering::Relaxed);
    wake.notify();
    handle.join().unwrap();

    assert_eq!(transport.calls.lock().unwrap().len(), 1);
    let next = db
        .lock()
        .unwrap()
        .world_name_next_request_at()
        .unwrap()
        .unwrap();
    assert!(next >= chrono::Utc::now().timestamp() + 55);
}
```

テストモジュールに `use std::collections::VecDeque; use std::sync::Mutex; use std::time::{Duration, Instant};` を追加する。

- [ ] **Step 2: テストが失敗することを確認**

Run: `cargo test -p vrc-photon-daemon world_names::tests`
Expected: コンパイルエラー（モジュール未定義）

- [ ] **Step 3: 最小実装を追加**

- `process_next_world_name` は DB から due を1件取得し、ロックを解放して `fetch` し、結果を `record_world_name_resolved(world_db_id, name, now)` / `record_world_name_failure(world_db_id, failure, now)` に渡す。
- 成功: `next_request_at = now + 60`。
- 429: `next_request_at = now + retry_after`。
- 5xx / network: `next_request_at = now + max(60, backoff_seconds(attempts))`。
- 404: `next_request_at = now + 60`、行は24時間後。
- invalid: `next_request_at = now + 60`、行は terminal。
- `worker_loop` は `initial_next_request_at` を読み、送信前に `now + 60` を予約して `set_world_name_next_request_at` に保存する。429 の後は `Retry-After` で延長する。無効時は `wait(5s)`。
- DB の記録に失敗した場合は行を terminal にせず、60秒待って次のループで再試行する。
- ログは `world_db_id` と固定コードだけ。

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo test -p vrc-photon-daemon world_names::tests`
Expected: PASS

- [ ] **Step 5: コミット**

```bash
git add crates/daemon/src/world_names.rs crates/daemon/src/lib.rs
git commit -m "feat(daemon): process world name lookups in a worker"
```

---

### Task 8: デーモンへの組み込み

**Files:**
- Modify: `crates/daemon/src/lib.rs`（`Daemon` 構造体、`open_inner`、`start_workers`、`stop_workers`、`shutdown`、`refresh_compression_settings`、`link_unlinked`、`mod tests`）
- Test: `crates/daemon/src/lib.rs` の `mod tests`

**Interfaces:**
- Consumes: Task 7 の `worker_loop`
- Produces:
  - `Daemon.world_names: Arc<JobSignal>`
  - `Daemon.world_name_resolution_enabled: Arc<AtomicBool>`
  - `Daemon.world_name_handle: Option<JoinHandle<()>>`
  - `Daemon::enqueue_world_name_backfill(&self)`

- [ ] **Step 1: 失敗するテストを書く**

```rust
fn insert_unresolved_world_photo(db: &Db) -> (i64, i64) {
    let root = db.add_root("/photos", "reference").unwrap();
    let photo = db
        .upsert_photo(&NewPhoto {
            root_id: root,
            rel_path: "a.png".to_string(),
            abs_path: "/photos/a.png".to_string(),
            file_name: "a.png".to_string(),
            captured_at_utc: 100,
            captured_at_local: "2026-10-21 21:14:03".to_string(),
            tz_offset_sec: 0,
            width: 1,
            height: 1,
            file_size: 1,
            mtime: 100,
            blake3: None,
        })
        .unwrap();
    let world = db.upsert_world("wrld_test", None, None).unwrap();
    db.set_photo_context(&PhotoContext {
        photo_id: photo,
        session_id: None,
        world_id: Some(world),
        world_name: None,
        instance_id: None,
        link_method: "log".to_string(),
        confidence: 0.9,
        author: None,
        linked_at: "2026-11-03T00:00:00+00:00".to_string(),
    })
    .unwrap();
    (world, photo)
}

#[test]
fn start_workers_spawns_and_stops_the_world_name_worker() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("library.db");
    let mut daemon = Daemon::open(&db_path, Some(dir.path().to_path_buf()), None).unwrap();
    daemon.settings.world_name_resolution_enabled = false;
    daemon.start_workers();
    assert!(daemon.world_name_handle.is_some());
    daemon.stop_workers();
    assert!(daemon.world_name_handle.is_none());
}

#[test]
fn enabling_world_name_resolution_enqueues_backfill() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("library.db");
    let mut daemon = Daemon::open(&db_path, Some(dir.path().to_path_buf()), None).unwrap();
    let (world, photo) = {
        let db = daemon.db.lock().unwrap();
        insert_unresolved_world_photo(&db)
    };
    daemon.settings.world_name_resolution_enabled = false;
    daemon
        .db
        .lock()
        .unwrap()
        .save(&AppSettings {
            world_name_resolution_enabled: false,
            ..daemon.settings.clone()
        })
        .unwrap();

    daemon
        .db
        .lock()
        .unwrap()
        .save(&AppSettings {
            world_name_resolution_enabled: true,
            ..daemon.settings.clone()
        })
        .unwrap();
    daemon.refresh_compression_settings();

    assert!(daemon.world_name_resolution_enabled.load(Ordering::Relaxed));
    assert!(daemon.db.lock().unwrap().world_name_lookup(world).unwrap().is_some());
    assert_eq!(
        daemon.db.lock().unwrap().photo_context(photo).unwrap().unwrap().world_name,
        None
    );
}

#[test]
fn run_once_does_not_start_the_world_name_worker() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("library.db");
    let mut daemon = Daemon::open(&db_path, Some(dir.path().to_path_buf()), None).unwrap();
    daemon.run_once().unwrap();
    assert!(daemon.world_name_handle.is_none());
}
```

daemon のテストモジュールに `use vrc_photon_core::model::{NewPhoto, PhotoContext};` を追加する。

- [ ] **Step 2: テストが失敗することを確認**

Run: `cargo test -p vrc-photon-daemon start_workers_spawns_and_stops_the_world_name_worker enabling_world_name_resolution_enqueues_backfill run_once_does_not_start_the_world_name_worker`
Expected: コンパイルエラー（フィールド/メソッド未定義）

- [ ] **Step 3: 最小実装を追加**

- `Daemon` に `world_names: Arc<JobSignal>`、`world_name_resolution_enabled: Arc<AtomicBool>`、`world_name_handle: Option<JoinHandle<()>>` を追加し、`open_inner` で初期化する。
- `start_workers` で、設定がONなら `enqueue_world_name_backfill()` を実行してから `world-name` スレッドを spawn する。無効でもスレッドは spawn し、5秒待機させる。`world_name_handle.is_none()` のときだけ spawn する。
- `stop_workers` / `shutdown` で `world_names.notify()` を呼び、`world_name_handle` を join する。
- `refresh_compression_settings` で `world_name_resolution_enabled` を更新する。false→true なら `enqueue_world_name_backfill()` と `world_names.notify()`、true→false なら `world_names.notify()`。
- `link_unlinked` で `linked > 0` かつ機能ONなら `enqueue_world_name_backfill()` と `world_names.notify()`。
- `enqueue_world_name_backfill` は DB ロックを取り、`backfill_photo_context_world_names()` → `enqueue_missing_world_name_lookups()` を実行する。エラーは `tracing::warn!` に留める。

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo test -p vrc-photon-daemon start_workers_spawns_and_stops_the_world_name_worker enabling_world_name_resolution_enqueues_backfill run_once_does_not_start_the_world_name_worker`
Expected: PASS

- [ ] **Step 5: コミット**

```bash
git add crates/daemon/src/lib.rs
git commit -m "feat(daemon): wire world name resolution into the daemon"
```

---

### Task 9: ワールドIDフォールバック

**Files:**
- Modify: `crates/core/src/model.rs`（`Photo`）
- Modify: `crates/core/src/db.rs`（`PHOTO_SELECT` / `PHOTO_FROM` / `row_to_photo`、`mod tests`）
- Modify: `crates/core/src/discord.rs`（テストの `Photo` リテラル）
- Modify: `crates/ui/src/state/test_support.rs`（`Photo` リテラル）
- Modify: `crates/ui/src/ui/components.rs`（`photo_world_label`、`mod tests`）
- Modify: `crates/ui/src/ui/gallery.rs`
- Modify: `crates/ui/src/ui/library.rs`
- Modify: `crates/ui/src/app/photos.rs`
- Modify: `crates/ui/src/app/live.rs`
- Modify: `crates/ui/src/app/daemon.rs`
- Test: `crates/core/src/db.rs`、`crates/ui/src/ui/components.rs`、`cargo test -p vrc-photon-ui`

**Interfaces:**
- Consumes: 既存の `Photo`
- Produces:
  - `Photo.world_id: Option<String>`（VRChat の `wrld_...`。DB の FK ではない）
  - `pub(crate) fn photo_world_label(photo: &Photo) -> String`

- [ ] **Step 1: 失敗するテストを書く**

`crates/core/src/db.rs` の `mod tests`:

```rust
#[test]
fn photo_query_includes_the_vrchat_world_id() {
    let db = Db::open_in_memory().unwrap();
    let root = db.add_root("/photos", "reference").unwrap();
    let photo = db.upsert_photo(&sample_photo(root, "a.png", 100)).unwrap();
    let world = db.upsert_world("wrld_test", Some("Test World"), Some("log")).unwrap();
    db.set_photo_context(&PhotoContext {
        photo_id: photo,
        session_id: None,
        world_id: Some(world),
        world_name: Some("Test World".to_string()),
        instance_id: None,
        link_method: "log".to_string(),
        confidence: 0.9,
        author: None,
        linked_at: "2026-11-03T00:00:00+00:00".to_string(),
    })
    .unwrap();

    let stored = db.photo_by_id(photo).unwrap().unwrap();
    assert_eq!(stored.world_id.as_deref(), Some("wrld_test"));
}
```

`crates/ui/src/ui/components.rs` の `mod tests`:

```rust
#[test]
fn photo_world_label_prefers_name_then_id_then_unlinked() {
    let mut photo = crate::state::test_support::test_photo(1);
    photo.world_name = Some("Test World".to_string());
    photo.world_id = Some("wrld_test".to_string());
    assert_eq!(photo_world_label(&photo), "Test World");

    photo.world_name = None;
    assert_eq!(photo_world_label(&photo), "wrld_test");

    photo.world_id = None;
    assert_eq!(photo_world_label(&photo), tr!("photos.unlinked"));
}
```

- [ ] **Step 2: テストが失敗することを確認**

Run: `cargo test -p vrc-photon-core db::tests::photo_query_includes_the_vrchat_world_id`
Expected: コンパイルエラー（`Photo.world_id` が未定義）

- [ ] **Step 3: 最小実装を追加**

- `Photo` に `pub world_id: Option<String>` を追加する。
- `PHOTO_SELECT` の末尾に `pw.world_id` を追加し、`PHOTO_FROM` に `LEFT JOIN worlds pw ON pw.id = c.world_id` を追加する。`row_to_photo` のインデックス27に `world_id` を追加する。
- `Photo { ... }` リテラルすべてに `world_id: None` を追加する（コンパイラの指摘に従う。既知: `crates/core/src/discord.rs`、`crates/ui/src/state/test_support.rs`、`crates/ui/src/state/preview.rs` のテスト）。
- `photo_world_label` を `ui/components.rs` に追加する:
  - `world_name` が trim 後に空でなければその値
  - なければ `world_id` が trim 後に空でなければその値
  - なければ `tr!("photos.unlinked")`
- `ui/gallery.rs` の `photo.world_name.clone().unwrap_or_else(|| tr!("gallery.unlinked"))` を `photo_world_label(photo)` に置き換える。
- `ui/library.rs` の2箇所の同じフォールバックを `photo_world_label(photo)` に置き換える。
- `app/photos.rs` のシーン色フォールバックを `photo.world_name.as_deref().or(photo.world_id.as_deref()).unwrap_or("")` にする。
- `app/live.rs` と `app/daemon.rs` の現在ワールド表示を `world.alias.or(world.name).or(Some(world.world_id))` にする。

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo test -p vrc-photon-core db::tests::photo_query_includes_the_vrchat_world_id && cargo test -p vrc-photon-ui photo_world_label_prefers_name_then_id_then_unlinked`
Expected: PASS

追加で `cargo test -p vrc-photon-ui` と `cargo test -p vrc-photon-core` を実行し、`Photo` リテラルの追加漏れがないことを確認する。

- [ ] **Step 5: コミット**

```bash
git add crates/core/src/model.rs crates/core/src/db.rs crates/core/src/discord.rs crates/ui/src
git commit -m "feat(ui): show the world ID when no name is available"
```

---

### Task 10: 設定カードと i18n

**Files:**
- Modify: `crates/ui/src/ui/settings.rs`
- Modify: `crates/i18n/locales/ja.yml`
- Modify: `crates/i18n/locales/en.yml`
- Test: `cargo test -p vrc-photon-i18n`

**Interfaces:**
- Consumes: `AppSettings.world_name_resolution_enabled`、`App::setting_switch`
- Produces: 設定画面の「ワールド名解決」カード

- [ ] **Step 1: 失敗するテストを書く**

`crates/i18n/tests/catalog.rs` の `english_catalog_is_complete` が自動で新キーを検出する。先に `ja.yml` だけにキーを追加し、`en.yml` を空のままにして失敗させる。

`ja.yml` の `settings.library` ブロックの後に追加する。

```yaml
settings.world_name.title: "ワールド名解決"
settings.world_name.enabled: "VRChat API でワールド名を解決"
settings.world_name.hint: "名前がないワールドIDを検索し、結果をローカルにキャッシュします。写真・ログ・ユーザー情報は送りません。解決には時間がかかります。"
settings.world_name.privacy: "有効にすると、ワールドIDのみを api.vrchat.cloud へ送信します。認証情報は使いません。既定はOFFです。"
```

- [ ] **Step 2: テストが失敗することを確認**

Run: `cargo test -p vrc-photon-i18n english_catalog_is_complete`
Expected: FAIL（`en` に `settings.world_name.*` がない）

- [ ] **Step 3: 最小実装を追加**

`en.yml` に追加する。

```yaml
settings.world_name.title: "World name resolution"
settings.world_name.enabled: "Resolve world names with the VRChat API"
settings.world_name.hint: "Look up world IDs that have no name and cache the result locally. Photos, logs, and user information are not sent. Resolution can take time."
settings.world_name.privacy: "When enabled, only the world ID is sent to api.vrchat.cloud. No credentials are used. This is off by default."
```

`settings.rs` のライブラリカードの直後、表示カードの前に追加する。

```rust
.child(
    glass_panel(&self.palette)
        .flex()
        .flex_col()
        .p(px(14.))
        .gap(px(12.))
        .rounded(px(17.))
        .child(
            div()
                .text_size(px(13.))
                .font_semibold()
                .text_color(rgb(p.text))
                .child(tr!("settings.world_name.title")),
        )
        .child(self.setting_switch(
            tr!("settings.world_name.enabled"),
            tr!("settings.world_name.hint"),
            self.settings.world_name_resolution_enabled,
            false,
            cx,
            |settings, value| settings.world_name_resolution_enabled = value,
        ))
        .child(
            div()
                .text_size(px(10.))
                .text_color(rgb(p.muted_2))
                .child(tr!("settings.world_name.privacy")),
        ),
)
```

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo test -p vrc-photon-i18n && cargo test -p vrc-photon-ui`
Expected: PASS

- [ ] **Step 5: コミット**

```bash
git add crates/ui/src/ui/settings.rs crates/i18n/locales/ja.yml crates/i18n/locales/en.yml
git commit -m "feat(ui): add the world name resolution setting card"
```

---

### Task 11: ドキュメントと ADR

**Files:**
- Modify: `docs/security-privacy.md`
- Modify: `docs/features.md`
- Modify: `docs/data-model.md`
- Modify: `docs/architecture.md`
- Modify: `docs/log-and-xmp.md`
- Modify: `docs/roadmap.md`
- Modify: `docs/testing.md`
- Create: `docs/adr/0018-vrchat-world-name-resolution.md`
- Modify: `docs/adr/README.md`

**Interfaces:**
- Consumes: 実装済みの設定名、テーブル名、ワーカー名、API制約
- Produces: 利用者・保守者向けのドキュメント

**Note:** `AGENTS.md` の「コードと docs を同じ PR に混ぜない」に従う場合、
このタスクはコードPRと別の `docs/25-world-name-resolution` ブランチで実施する。
その場合も Issue #25 の受け入れ条件は docs のマージまで完了としない。

- [ ] **Step 1: ドキュメントの不足を確認**

Run: `rg -n "world_name_resolution|world_name_lookups|vrchat-api|ワールド名解決" docs`
Expected: 設計メモ以外にヒットしない

- [ ] **Step 2: ドキュメントを更新**

- `docs/security-privacy.md`:
  - ネットワーク節の現行通信一覧に「VRChat API ワールド名解決」を追加。
  - `### 7.2 VRChat API ワールド名解決（オプトイン）` を追加。既定OFF、送信はワールドIDのみ、認証情報なし、User-Agent、60秒間隔、429/5xx/404、キャッシュ、フォールバック、ログに出さないことを明記。
  - 「将来、VRChat APIやクラウド同期を追加する場合」の段落を、認証付きAPI・クラウド同期など未実装項目に限定して更新。
- `docs/features.md`: 文脈取得の節に「任意のVRChat APIワールド名解決（既定OFF）」と、名前→ID→未紐付けのフォールバックを追加。
- `docs/data-model.md`: `world_name_lookups` テーブルと `name_source='vrchat-api'` を追加。`worlds.name` の説明を更新。
- `docs/architecture.md`: デーモンの `world_names` ワーカー、`world_name_lookups`、`world_name_next_request_at` を追加。データフローに「リンク完了→未解決ワールドをキュー→60秒間隔で解決→photo_context バックフィル」を追加。
- `docs/log-and-xmp.md`: 既知の制約の「ID表示になる」を、任意機能で名前を補完できる旨に更新。推定リンクではXMP名をworldsへ昇格しないことを追記。
- `docs/roadmap.md`: 「ワールド名解決（認証が必要）」を、認証なしの解決は実装済み、認証付きAPIは将来、に更新。
- `docs/testing.md`: 手動確認項目を追加（有効化/無効化、404、再起動、DB/ログに認証情報がない）。
- `docs/adr/0018-vrchat-world-name-resolution.md`: 設計メモの「検討した方式」「設計」を要約し、Context / Decision / Consequences / Alternatives を書く。
- `docs/adr/README.md`: ADR-0018 の行を追加。

- [ ] **Step 3: ドキュメントの整合性を確認**

Run: `rg -n "world_name_lookups|world_name_resolution_enabled|vrchat-api|ワールド名解決" docs`
Expected: 設計メモ、ADR、security-privacy、data-model、architecture、features、testing、roadmap にヒットする

- [ ] **Step 4: 空白とリンクを確認**

Run: `git diff --check && ! rg -n "TODO|TBD" docs/adr/0018-vrchat-world-name-resolution.md`
Expected: `git diff --check` が無出力、TODO/TBD がない

- [ ] **Step 5: コミット**

```bash
git add docs
git commit -m "docs: document VRChat world name resolution"
```

---

### Task 12: 最終検証

**Files:**
- 変更なし（検証のみ）

**Interfaces:**
- Consumes: Task 1〜11 のすべて

- [ ] **Step 1: フォーマットを確認**

Run: `cargo fmt --all -- --check`
Expected: 無出力

- [ ] **Step 2: Clippy を確認**

Run: `cargo clippy --workspace --all-targets -- -D warnings`
Expected: 警告なし

- [ ] **Step 3: ワークスペースのテストを実行**

Run: `cargo test --workspace`
Expected: すべて PASS

- [ ] **Step 4: 手動確認**

以下を確認する。

- 名前のないワールドの写真がある状態で機能をONにすると、60秒間隔で名前が付く。
- OFFのままでは API リクエストが発生しない。
- 存在しないワールドIDは `wrld_...` 表示のまま。
- デーモンを再起動しても、解決済みワールドを再取得しない。
- 429 の後は `Retry-After` 以上待つ。
- DB とログに認証情報がない。
- `worlds.name_source` が `vrchat-api` になり、ログ/XMP名を上書きしていない。
- 推定リンクの XMP 名が `worlds.name` を変更していない。

- [ ] **Step 5: 差分を確認**

Run: `git diff --check && git status --short`
Expected: 意図したファイルだけが変更されている。`opencode.json` や `.opencode/` など、この作業と無関係な既存の変更をコミットしない。
