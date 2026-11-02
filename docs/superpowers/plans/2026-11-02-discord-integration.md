# Discord Webhook 連携 実装計画

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** SQLite の outbox とデーモン送信ワーカーを追加し、連携タブから Discord Webhook へ写真を手動・自動送信できるようにする。

**Architecture:** UI は設定と outbox への enqueue だけを行い、デーモンの単一送信ワーカーが multipart POST を実行する。送信画像は原画優先で、20 MiB 超または原画なしのときは連携プロファイルの既存 derivative、なければメモリ上エンコードへフォールバックする。削除キューはアクティブな outbox をドレイン対象にし、待機・失敗中の送信は削除要求時にキャンセルする。

**Tech Stack:** Rust stable (edition 2024), gpui-kit 0.8, rusqlite (SQLite WAL), ureq 3.5 (`multipart`), serde/serde_json, chrono, rust-i18n.

**Spec:** `docs/superpowers/specs/2026-11-02-discord-integration-design.md`

## 完了メモ

- 2026-11-02: Phase 1（PR #284）と Phase 2（PR #285）を main にマージ済み。
- 実装タスク（Task 1〜20）は完了。設計・ADR・機能ドキュメントは Task 21 で追加した。
- 送信上限は実装時に Discord の現行既定に合わせて 20 MiB
  （`20 * 1024 * 1024` バイト）へ修正した。25 MiB は実験枠。
- docs PR の push / PR 作成 / CI 監視（Task 21 Step 4〜5）と、マージ後の
  後片付けは親セッションで行う。

## Global Constraints

- ブランチ: `feat/142-discord-integration`（worktree `../vrc-photo-manager-142`）。
- コード PR は 2 本（Phase 1 / Phase 2）。設計・ADR・計画・機能ドキュメントは別の docs PR。AGENTS §4 により code と docs を同じ PR に混ぜない。
- 既定は `discord_enabled = false`。機能オフまたは Webhook URL 未設定ではネットワーク送信を 1 件もしない。
- Webhook URL は `app_settings` の `discord_webhook_url` キーだけに保存し、`AppSettings` JSON には入れない。URL・トークンをログ、outbox、IPC イベント、トーストに出さない。
- 送信先は HTTPS の `discord.com` / `discordapp.com` / `ptb.discord.com` / `canary.discord.com` の `/api/webhooks/<id>/<token>` のみ。リダイレクトは追わない。
- 20 MiB = `20 * 1024 * 1024` バイト。原画優先。超過時は選択プロファイルの出力へフォールバックする。
- プレイヤー名は既定で含めない（`discord_include_players = false`）。
- 写真 1 枚につきアクティブな outbox は 1 件（DB の部分 UNIQUE インデックス）。
- 429 は `retry_after` に従い `attempts` を増やさない。ネットワーク/5xx は指数バックオフで無期限に再試行し、4xx は `failed`。
- デーモン再起動時の `processing` は自動再送せず `failed(interrupted)`。
- 自動送信は Phase 2。`discord_auto_since` 以降に撮影された写真だけを対象にし、既存ライブラリを一括投稿しない。
- 既存の `compressor` / `deletion_requests` / IPC の挙動を壊さない。`PROTOCOL_VERSION` は 4 に上げる。
- すべての UI 文字列は `crates/i18n/locales/ja.yml` と `en.yml` の両方に追加する。
- 各タスクの完了条件: `cargo fmt --all -- --check`、`cargo clippy --workspace --all-targets -- -D warnings`、対象テストが成功。
- コミット接頭辞: `feat:`, `fix:`, `test:`, `docs:`, `chore:`。

## Review Focus

1. **Webhook URL の安全性**: `http://`、Discord 以外のホスト、トークン欠落、リダイレクトを拒否し、URL/トークンがログ・outbox・IPC に出ない。Task 5 / 8 / 9 のテストで固定する。
2. **機能オフ時の無送信**: `discord_enabled=false` または URL 未設定で、pending 行があっても 1 リクエストも送らない。Task 9 のテストで固定する。
3. **古いパスの送信禁止**: enqueue 後に写真が移動・置換・削除された場合、送信時に現在の DB と実ファイルから解決し、古いパスを送らない。Task 5 / 9 のテストで固定する。
4. **20 MiB 境界**: ちょうど 20 MiB は原画、20 MiB + 1 はフォールバック、フォールバックも超過ならアップロードせず `too_large`。Task 5 / 9 のテストで固定する。
5. **再起動中の二重投稿**: `processing` は `failed(interrupted)` になり、自動再送しない。Task 3 / 9 のテストで固定する。
6. **削除との競合**: アクティブな outbox がある間は `claim_deletion` が進まず、送信中のファイルを消さない。Task 4 のテストで固定する。
7. **文脈の長大・欠落**: 300 文字のワールド名、200 人のプレイヤー、文脈なしでも Discord の embed 上限内で送れる。Task 5 のテストで固定する。
8. **429 の小数 `retry_after`**: 0.5 秒は 1 秒に切り上げ、900 秒を上限にする。Task 8 のテストで固定する。

---

## Phase 1（PR 1）

### Task 1: 設定・Webhook ストア・プロファイルフラグ

**Files:**
- Modify: `crates/core/src/settings.rs`
- Modify: `crates/core/src/model.rs`
- Modify: `crates/core/src/db.rs`（`write_profile`, `clear_other_clipboard_profiles`, `duplicate_profile`）
- Test: 上記ファイル内の `#[cfg(test)]`

**Interfaces:**
- Consumes: 既存の `Db::get_setting` / `set_setting`、`CompressionProfile`。
- Produces:
  - `pub enum DiscordAutoMode { Off, Immediate, Waiting }`（`Default = Off`, serde `snake_case`）
  - `AppSettings` の `discord_enabled: bool`, `discord_destination: String`, `discord_include_players: bool`, `discord_send_original: bool`, `discord_auto_mode: DiscordAutoMode`, `discord_auto_since: Option<i64>`, `discord_consent_at: Option<i64>`
  - `pub const DISCORD_WEBHOOK_KEY: &str = "discord_webhook_url";`
  - `pub fn load_discord_webhook(db: &Db) -> Result<Option<String>>`
  - `pub fn save_discord_webhook(db: &Db, url: &str) -> Result<()>`
  - `pub fn clear_discord_webhook(db: &Db) -> Result<()>`
  - `CompressionProfile.discord: bool`

- [x] **Step 1: Write the failing settings/webhook tests**

`crates/core/src/settings.rs` の tests に追加:

```rust
#[test]
fn discord_settings_default_to_off() {
    let db = Db::open_in_memory().unwrap();
    let settings = AppSettings::load(&db).unwrap();
    assert!(!settings.discord_enabled);
    assert_eq!(settings.discord_destination, "Discord");
    assert!(!settings.discord_include_players);
    assert!(settings.discord_send_original);
    assert_eq!(settings.discord_auto_mode, DiscordAutoMode::Off);
    assert_eq!(settings.discord_auto_since, None);
    assert_eq!(settings.discord_consent_at, None);
}

#[test]
fn old_app_settings_json_keeps_discord_defaults() {
    let mut value = serde_json::to_value(AppSettings::default()).unwrap();
    for key in [
        "discord_enabled", "discord_destination", "discord_include_players",
        "discord_send_original", "discord_auto_mode", "discord_auto_since",
        "discord_consent_at",
    ] {
        value.as_object_mut().unwrap().remove(key);
    }
    let settings: AppSettings = serde_json::from_value(value).unwrap();
    assert!(!settings.discord_enabled);
    assert_eq!(settings.discord_auto_mode, DiscordAutoMode::Off);
    assert!(settings.discord_send_original);
}

#[test]
fn blank_discord_destination_loads_as_discord() {
    let db = Db::open_in_memory().unwrap();
    let mut value = serde_json::to_value(AppSettings::default()).unwrap();
    value["discord_destination"] = serde_json::Value::String("   ".into());
    db.set_setting("app_settings", &value.to_string()).unwrap();
    assert_eq!(AppSettings::load(&db).unwrap().discord_destination, "Discord");
}

#[test]
fn discord_webhook_round_trips_outside_app_settings() {
    let db = Db::open_in_memory().unwrap();
    AppSettings::default().save(&db).unwrap();
    let url = "https://discord.com/api/webhooks/123/token";
    save_discord_webhook(&db, url).unwrap();
    assert_eq!(load_discord_webhook(&db).unwrap().as_deref(), Some(url));
    let json = db.get_setting("app_settings").unwrap().unwrap();
    assert!(!json.contains(url));
    clear_discord_webhook(&db).unwrap();
    assert_eq!(load_discord_webhook(&db).unwrap(), None);
}
```

`crates/core/src/db.rs` の tests に追加:

```rust
#[test]
fn saving_a_discord_profile_clears_the_previous_one() {
    let db = Db::open_in_memory().unwrap();
    let mut first = profile("a", "A");
    first.discord = true;
    db.save_profile(&first).unwrap();
    let mut second = profile("b", "B");
    second.discord = true;
    db.save_profile(&second).unwrap();
    let stored = db.list_profiles().unwrap();
    assert_eq!(stored.iter().filter(|p| p.discord).count(), 1);
    assert!(stored.iter().find(|p| p.id == "b").unwrap().discord);
}

#[test]
fn duplicating_a_discord_profile_clears_the_copy_flag() {
    let db = Db::open_in_memory().unwrap();
    let mut source = profile("a", "A");
    source.discord = true;
    db.save_profile(&source).unwrap();
    let copy = db.duplicate_profile("a").unwrap();
    assert!(!copy.discord);
    assert_eq!(db.list_profiles().unwrap().iter().filter(|p| p.discord).count(), 1);
}

#[test]
fn the_default_discord_preset_is_selected() {
    let db = Db::open_in_memory().unwrap();
    let profiles = db.list_profiles().unwrap();
    let discord = profiles.iter().find(|p| p.name == "discord用webp").unwrap();
    assert!(discord.discord);
    assert_eq!(profiles.iter().filter(|p| p.discord).count(), 1);
}
```

- [x] **Step 2: Run tests to verify they fail**

Run: `cargo test -p vrc-photon-core discord_ -- --nocapture`
Expected: FAIL（`DiscordAutoMode` / `discord` フィールド / Webhook 関数が未定義）

- [x] **Step 3: Implement settings and profile flags**

`crates/core/src/settings.rs`:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum DiscordAutoMode {
    #[default]
    Off,
    Immediate,
    Waiting,
}
```

`AppSettings` にフィールドを追加し、`Default` を
`discord_destination: "Discord".to_string()`, `discord_send_original: true`,
その他 `false` / `None` にする。`discord_destination` は空文字を受け付けない。
`AppSettings::load` のデシリアライズ後と `AppSettings::save` のシリアライズ前に
`trim()` して空なら `"Discord"` へ正規化する（UI だけに依存しない）。

`crates/core/src/settings.rs` の末尾:

```rust
pub const DISCORD_WEBHOOK_KEY: &str = "discord_webhook_url";

pub fn load_discord_webhook(db: &Db) -> Result<Option<String>> {
    Ok(db.get_setting(DISCORD_WEBHOOK_KEY)?.filter(|url| !url.trim().is_empty()))
}

pub fn save_discord_webhook(db: &Db, url: &str) -> Result<()> {
    db.set_setting(DISCORD_WEBHOOK_KEY, url)
}

pub fn clear_discord_webhook(db: &Db) -> Result<()> {
    db.delete_setting(DISCORD_WEBHOOK_KEY)
}
```

`Db` に `delete_setting` がなければ `pub fn delete_setting(&self, key: &str) -> Result<()>`
を追加する（`DELETE FROM app_settings WHERE key = ?1`）。

`crates/core/src/model.rs` の `CompressionProfile` に
`#[serde(default)] pub discord: bool` を追加し、`Default` は `false`。
`CompressionProfile::defaults()` の `discord用webp` だけ `discord: true`。

`crates/core/src/db.rs`:
- `write_profile` で `profile.clipboard` と同様に `if profile.discord { Self::clear_other_discord_profiles(conn, &profile.id)?; }`
- `clear_other_clipboard_profiles` をコピーして `clear_other_discord_profiles` を作る。
- `duplicate_profile` の `CompressionProfile { ... }` に `discord: false` を追加。

- [x] **Step 4: Run tests to verify they pass**

Run: `cargo test -p vrc-photon-core discord_ -- --nocapture`
Expected: PASS。続けて `cargo test -p vrc-photon-core profile` で既存のプロファイルテストも PASS。

- [x] **Step 5: Commit**

```bash
git add crates/core/src/settings.rs crates/core/src/model.rs crates/core/src/db.rs
git commit -m "feat(core): add Discord settings and profile selection flag"
```

---

### Task 2: outbox スキーマと enqueue / claim

**Files:**
- Modify: `crates/core/src/model.rs`
- Modify: `crates/core/src/db.rs`（`SCHEMA_V11`, `migrate`, `migrate_v11`, outbox CRUD）
- Test: `crates/core/src/db.rs` の tests

**Interfaces:**
- Consumes: `photos`、`DeletionReason`。
- Produces:
  - `pub enum SendKind { Photo, Test }`
  - `pub enum SendOrigin { Manual, Auto }`
  - `pub enum SendStatus { Waiting, Pending, Processing, Sent, Failed, Cancelled, Unknown }`（`Unknown` は新しい版が書いた未知の状態。`parse` のフォールバック）
  - `pub enum SendSource { Original, Profile, Derivative }`
  - `pub enum SendErrorCode { Network, RateLimited, Http400, Http401, Http403, Http404, Http413, Http5xx, TooLarge, PhotoMissing, ProfileMissing, EncodeFailed, Interrupted, WebhookInvalid, Unknown }`
  - `pub struct SendOutboxEntry { id, photo_id: Option<i64>, kind, origin, status, source: Option<SendSource>, profile_id: Option<String>, destination: Option<String>, attempts: i64, next_attempt_at: Option<i64>, discord_message_id, discord_channel_id, discord_guild_id, error_code: Option<String>, error_detail: Option<String>, created_at, started_at, finished_at }`
  - `pub fn Db::enqueue_send(&self, photo_id: i64, kind: SendKind, origin: SendOrigin, status: SendStatus) -> Result<Option<i64>>`
  - `pub fn Db::enqueue_test_send(&self) -> Result<i64>`
  - `pub fn Db::claim_next_send(&self, now: i64) -> Result<Option<SendOutboxEntry>>`
  - `pub fn Db::send_entry_by_id(&self, id: i64) -> Result<Option<SendOutboxEntry>>`
  - `pub fn Db::active_send_for_photo(&self, photo_id: i64) -> Result<Option<SendOutboxEntry>>`
  - `pub fn Db::count_pending_sends(&self) -> Result<i64>`
  - `pub fn Db::count_waiting_sends(&self) -> Result<i64>`

- [x] **Step 1: Write the failing schema/enqueue/claim tests**

```rust
#[test]
fn migration_v11_adds_send_outbox() {
    let db = Db::open_in_memory().unwrap();
    let version: i64 = db.conn.pragma_query_value(None, "user_version", |r| r.get(0)).unwrap();
    assert_eq!(version, 11);
    let exists: bool = db.conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='send_outbox')",
        [], |r| r.get(0)).unwrap();
    assert!(exists);
}

#[test]
fn migration_v11_is_retryable() {
    let db = Db::open_in_memory().unwrap();
    db.conn.pragma_update(None, "user_version", 10).unwrap();
    db.migrate_v11().unwrap();
    db.migrate_v11().unwrap();
    let version: i64 = db.conn.pragma_query_value(None, "user_version", |r| r.get(0)).unwrap();
    assert_eq!(version, 11);
}

#[test]
fn enqueue_send_dedupes_active_rows_and_allows_resend_after_terminal() {
    let db = Db::open_in_memory().unwrap();
    let photo = seed_photo(&db);
    let id = db.enqueue_send(photo, SendKind::Photo, SendOrigin::Manual, SendStatus::Pending).unwrap().unwrap();
    assert!(db.enqueue_send(photo, SendKind::Photo, SendOrigin::Auto, SendStatus::Pending).unwrap().is_none());
    db.cancel_send(id).unwrap();
    let again = db.enqueue_send(photo, SendKind::Photo, SendOrigin::Manual, SendStatus::Pending).unwrap();
    assert!(again.is_some());
}

#[test]
fn enqueue_send_rejects_a_deleting_photo() {
    let db = Db::open_in_memory().unwrap();
    let photo = seed_photo(&db);
    db.request_deletion(&[photo], DeletionReason::Manual, false).unwrap();
    assert!(db.enqueue_send(photo, SendKind::Photo, SendOrigin::Manual, SendStatus::Pending).unwrap().is_none());
}

#[test]
fn claim_next_send_respects_status_and_next_attempt() {
    let db = Db::open_in_memory().unwrap();
    let photo = seed_photo(&db);
    let id = db.enqueue_send(photo, SendKind::Photo, SendOrigin::Manual, SendStatus::Pending).unwrap().unwrap();
    db.set_send_next_attempt(id, 200).unwrap();
    assert!(db.claim_next_send(100).unwrap().is_none());
    let claimed = db.claim_next_send(200).unwrap().unwrap();
    assert_eq!(claimed.id, id);
    assert_eq!(claimed.status, SendStatus::Processing);
    assert!(db.claim_next_send(300).unwrap().is_none());
}

#[test]
fn test_send_has_no_photo() {
    let db = Db::open_in_memory().unwrap();
    let id = db.enqueue_test_send().unwrap();
    let entry = db.send_entry_by_id(id).unwrap().unwrap();
    assert_eq!(entry.kind, SendKind::Test);
    assert_eq!(entry.photo_id, None);
}

#[test]
fn send_status_parse_keeps_unknown_values_distinct() {
    assert_eq!(SendStatus::parse("future_state"), SendStatus::Unknown);
    assert_eq!(SendStatus::Unknown.as_str(), "unknown");
    assert!(!SendStatus::Unknown.is_terminal());
}
```

`set_send_next_attempt` はテスト用にも使うため `#[cfg(test)]` ではなく通常の
`pub fn Db::set_send_next_attempt(&self, id: i64, next_attempt_at: i64) -> Result<()>`
として実装する（Task 3 の `reschedule_send` でも使う）。

- [x] **Step 2: Run tests to verify they fail**

Run: `cargo test -p vrc-photon-core send_ -- --nocapture`
Expected: FAIL（`send_outbox` / 型 / メソッドが未定義）

- [x] **Step 3: Implement schema, enums, and CRUD**

`crates/core/src/db.rs` に `SCHEMA_V11_SEND_OUTBOX` を追加（設計 6.2 の SQL をそのまま）。
`migrate` の `if version < 10` の後に `if version < 11 { self.migrate_v11()?; }`。
`migrate_v11` は `IMMEDIATE` トランザクションで `version >= 11` なら return、
`execute_batch(SCHEMA_V11_SEND_OUTBOX)`、`schema_migrations` と `user_version=11`。

`model.rs` に enums と `SendOutboxEntry` を追加。各 enum に
`as_str()` と `parse()` を実装する。`SendStatus::parse` の未知値は
`SendStatus::Unknown`（`as_str() = "unknown"`）、`SendStatus::is_terminal()` は
`Sent | Failed | Cancelled`。`SendKind` / `SendOrigin` / `SendSource` の未知値は
それぞれ `Photo` / `Manual` / `Original` にフォールバックする（表示専用）。
テスト `send_status_parse_keeps_unknown_values_distinct` を追加する。

`db.rs` の enqueue:

```sql
INSERT OR IGNORE INTO send_outbox(photo_id, kind, origin, status, created_at)
SELECT ?1, ?2, ?3, ?4, ?5
WHERE EXISTS (SELECT 1 FROM photos WHERE id = ?1 AND status <> 'deleting');
```

`self.conn.changes() > 0` のときだけ `Some(last_insert_rowid())` を返す。
テスト送信は `INSERT INTO send_outbox(photo_id, kind, origin, status, created_at)
VALUES (NULL, 'test', 'manual', 'pending', ?1)` で `last_insert_rowid()`。

`claim_next_send`:

```sql
UPDATE send_outbox
SET status = 'processing', started_at = ?1
WHERE id = (
    SELECT id FROM send_outbox
    WHERE status = 'pending'
      AND (next_attempt_at IS NULL OR next_attempt_at <= ?2)
    ORDER BY id LIMIT 1
)
RETURNING id, photo_id, kind, origin, status, source, profile_id, destination,
          attempts, next_attempt_at, discord_message_id, discord_channel_id,
          discord_guild_id, error_code, error_detail, created_at, started_at, finished_at;
```

`row_to_send_outbox` を追加。`active_send_for_photo` は
`status IN ('waiting','pending','processing')` を返す。
`count_pending_sends` は `status IN ('waiting','pending')`。

- [x] **Step 4: Run tests to verify they pass**

Run: `cargo test -p vrc-photon-core send_ -- --nocapture`
Expected: PASS。`cargo test -p vrc-photon-core migration` も PASS。

- [x] **Step 5: Commit**

```bash
git add crates/core/src/model.rs crates/core/src/db.rs
git commit -m "feat(core): add durable Discord send outbox"
```

---

### Task 3: outbox の完了・再試行・履歴

**Files:**
- Modify: `crates/core/src/model.rs`（`SendCompletion`, `SendHistoryEntry`）
- Modify: `crates/core/src/db.rs`
- Test: `crates/core/src/db.rs` の tests

**Interfaces:**
- Consumes: Task 2 の型とテーブル。
- Produces:
  - `pub struct SendCompletion { pub source: Option<SendSource>, pub profile_id: Option<String>, pub destination: String, pub message_id: Option<String>, pub channel_id: Option<String>, pub guild_id: Option<String> }`
  - `pub struct SendHistoryEntry { pub entry: SendOutboxEntry, pub photo_name: Option<String>, pub world_name: Option<String> }`
  - `pub fn Db::complete_send(&self, id: i64, completion: &SendCompletion) -> Result<()>`
  - `pub fn Db::reschedule_send(&self, id: i64, attempts: i64, next_attempt_at: i64, error_code: SendErrorCode, error_detail: Option<&str>) -> Result<()>`
  - `pub fn Db::release_send(&self, id: i64) -> Result<()>`（processing → pending。設定が claim 後に無効化されたとき用）
  - `pub fn Db::fail_send(&self, id: i64, error_code: SendErrorCode, error_detail: Option<&str>, source: Option<SendSource>, profile_id: Option<&str>, destination: Option<&str>) -> Result<()>`
  - `pub fn Db::retry_send(&self, id: i64) -> Result<bool>`
  - `pub fn Db::cancel_send(&self, id: i64) -> Result<bool>`
  - `pub fn Db::cancel_waiting_auto_sends(&self) -> Result<usize>`
  - `pub fn Db::release_waiting_sends(&self) -> Result<usize>`
  - `pub fn Db::reset_processing_sends(&self) -> Result<usize>`
  - `pub fn Db::send_history(&self, limit: i64) -> Result<Vec<SendHistoryEntry>>`
  - `pub fn Db::clear_send_history(&self) -> Result<usize>`
  - `pub fn Db::prune_send_history(&self, keep: i64) -> Result<usize>`

- [x] **Step 1: Write the failing tests**

```rust
#[test]
fn complete_send_stores_message_identity_and_source() {
    let db = Db::open_in_memory().unwrap();
    let photo = seed_photo(&db);
    let id = db.enqueue_send(photo, SendKind::Photo, SendOrigin::Manual, SendStatus::Pending).unwrap().unwrap();
    db.claim_next_send(100).unwrap();
    db.complete_send(id, &SendCompletion {
        source: Some(SendSource::Original),
        profile_id: None,
        destination: "My server".into(),
        message_id: Some("42".into()),
        channel_id: Some("7".into()),
        guild_id: Some("9".into()),
    }).unwrap();
    let entry = db.send_entry_by_id(id).unwrap().unwrap();
    assert_eq!(entry.status, SendStatus::Sent);
    assert_eq!(entry.discord_message_id.as_deref(), Some("42"));
    assert_eq!(entry.discord_channel_id.as_deref(), Some("7"));
    assert_eq!(entry.source, Some(SendSource::Original));
}

#[test]
fn retry_send_resets_attempts_and_errors_only_for_failed() {
    let db = Db::open_in_memory().unwrap();
    let photo = seed_photo(&db);
    let id = db.enqueue_send(photo, SendKind::Photo, SendOrigin::Manual, SendStatus::Pending).unwrap().unwrap();
    db.claim_next_send(100).unwrap();
    db.fail_send(id, SendErrorCode::Http401, Some("unauthorized"), None, None, None).unwrap();
    assert!(db.retry_send(id).unwrap());
    let entry = db.send_entry_by_id(id).unwrap().unwrap();
    assert_eq!(entry.status, SendStatus::Pending);
    assert_eq!(entry.attempts, 0);
    assert_eq!(entry.error_code, None);
    assert!(!db.retry_send(id).unwrap());
}

#[test]
fn cancel_send_refuses_processing_rows() {
    let db = Db::open_in_memory().unwrap();
    let photo = seed_photo(&db);
    let id = db.enqueue_send(photo, SendKind::Photo, SendOrigin::Manual, SendStatus::Pending).unwrap().unwrap();
    db.claim_next_send(100).unwrap();
    assert!(!db.cancel_send(id).unwrap());
    assert_eq!(db.send_entry_by_id(id).unwrap().unwrap().status, SendStatus::Processing);
}

#[test]
fn retry_send_refuses_when_another_active_row_exists() {
    let db = Db::open_in_memory().unwrap();
    let photo = seed_photo(&db);
    let failed = db.enqueue_send(photo, SendKind::Photo, SendOrigin::Manual, SendStatus::Pending).unwrap().unwrap();
    db.claim_next_send(100).unwrap();
    db.fail_send(failed, SendErrorCode::Http401, None, None, None, None).unwrap();
    let active = db.enqueue_send(photo, SendKind::Photo, SendOrigin::Manual, SendStatus::Pending).unwrap().unwrap();
    assert!(!db.retry_send(failed).unwrap());
    assert_eq!(db.send_entry_by_id(failed).unwrap().unwrap().status, SendStatus::Failed);
    assert_eq!(db.send_entry_by_id(active).unwrap().unwrap().status, SendStatus::Pending);
}

#[test]
fn reset_processing_sends_marks_interrupted_without_resending() {
    let db = Db::open_in_memory().unwrap();
    let photo = seed_photo(&db);
    let id = db.enqueue_send(photo, SendKind::Photo, SendOrigin::Manual, SendStatus::Pending).unwrap().unwrap();
    db.claim_next_send(100).unwrap();
    assert_eq!(db.reset_processing_sends().unwrap(), 1);
    let entry = db.send_entry_by_id(id).unwrap().unwrap();
    assert_eq!(entry.status, SendStatus::Failed);
    assert_eq!(entry.error_code.as_deref(), Some("interrupted"));
    assert!(db.claim_next_send(200).unwrap().is_none());
}

#[test]
fn release_waiting_and_cancel_waiting_auto_sends() {
    let db = Db::open_in_memory().unwrap();
    let photo = seed_photo(&db);
    let waiting = db.enqueue_send(photo, SendKind::Photo, SendOrigin::Auto, SendStatus::Waiting).unwrap().unwrap();
    assert_eq!(db.release_waiting_sends().unwrap(), 1);
    assert_eq!(db.send_entry_by_id(waiting).unwrap().unwrap().status, SendStatus::Pending);
    let photo2 = seed_other_photo(&db);
    db.enqueue_send(photo2, SendKind::Photo, SendOrigin::Auto, SendStatus::Waiting).unwrap();
    assert_eq!(db.cancel_waiting_auto_sends().unwrap(), 1);
}

#[test]
fn send_history_joins_photo_and_world_and_prunes_to_cap() {
    let db = Db::open_in_memory().unwrap();
    let photo = seed_photo(&db);
    for _ in 0..3 {
        let id = db.enqueue_send(photo, SendKind::Photo, SendOrigin::Manual, SendStatus::Pending).unwrap().unwrap();
        db.claim_next_send(100).unwrap();
        db.complete_send(id, &SendCompletion { source: Some(SendSource::Original), profile_id: None, destination: "D".into(), message_id: Some(id.to_string()), channel_id: None, guild_id: None }).unwrap();
    }
    let history = db.send_history(10).unwrap();
    assert_eq!(history.len(), 3);
    assert!(history[0].photo_name.is_some());
    assert_eq!(db.prune_send_history(2).unwrap(), 1);
    assert_eq!(db.send_history(10).unwrap().len(), 2);
    assert_eq!(db.clear_send_history().unwrap(), 2);
}
```

- [x] **Step 2: Run tests to verify they fail**

Run: `cargo test -p vrc-photon-core send_ -- --nocapture`
Expected: FAIL（メソッド未定義）

- [x] **Step 3: Implement the result/history methods**

- `complete_send`: `UPDATE ... SET status='sent', source=?, profile_id=?, destination=?, discord_message_id=?, discord_channel_id=?, discord_guild_id=?, error_code=NULL, error_detail=NULL, finished_at=? WHERE id=? AND status='processing'`.
- `reschedule_send`: `status='pending', attempts=?, next_attempt_at=?, error_code=?, error_detail=?, started_at=NULL`.
- `fail_send`: `status='failed', error_code=?, error_detail=?, source=?, profile_id=?, destination=?, finished_at=?`.
- `retry_send`: `status='failed'` のみ `pending`, `attempts=0`, `next_attempt_at=NULL`,
  errors NULL, `started_at=NULL`, `finished_at=NULL`。同じ `photo_id` に
  別の active 行（`waiting` / `pending` / `processing`）がある場合は更新しない
  （部分 UNIQUE インデックス違反を防ぐ）。
- `cancel_send`: `status IN ('waiting','pending','failed')` のみ `cancelled`, `finished_at=?`。
- `cancel_waiting_auto_sends`: `status='waiting' AND origin='auto'` → `cancelled`。
- `release_waiting_sends`: `status='waiting'` → `pending`（origin 不問）。
- `reset_processing_sends`: `status='processing'` → `failed`, `error_code='interrupted'`, `error_detail=NULL`, `finished_at=?`。
- `send_history`: `LEFT JOIN photos p` と `LEFT JOIN photo_context c` で `photo_name` / `world_name`。`ORDER BY o.id DESC LIMIT ?`。
- `clear_send_history`: `status IN ('sent','failed','cancelled')` を削除。
- `prune_send_history`: terminal 行のうち最新 `keep` 件より古いものを削除。

`SendErrorCode::as_str()` は `interrupted` などを返す。`parse()` は未知を `Unknown`。

- [x] **Step 4: Run tests to verify they pass**

Run: `cargo test -p vrc-photon-core send_ -- --nocapture`
Expected: PASS

- [x] **Step 5: Commit**

```bash
git add crates/core/src/model.rs crates/core/src/db.rs
git commit -m "feat(core): complete, retry, and list Discord send outbox entries"
```

---

### Task 4: 削除キューとの連携

**Files:**
- Modify: `crates/core/src/db.rs`（`deletion_blockers`, `deletion_job_summary`, `pending_deletion_requests`, `claim_deletion`, `request_deletion`, `force_deletion`）
- Test: `crates/core/src/db.rs` の tests

**Interfaces:**
- Consumes: Task 2/3 の `send_outbox`。
- Produces: 既存の `DeletionBlockers` / `DeletionJobSummary` が send を数える。`claim_deletion` がアクティブ send を待つ。

- [x] **Step 1: Write the failing tests**

```rust
#[test]
fn deletion_blockers_count_active_sends_but_not_failed_sends() {
    let db = Db::open_in_memory().unwrap();
    let photo = seed_photo(&db);
    let id = db.enqueue_send(photo, SendKind::Photo, SendOrigin::Manual, SendStatus::Pending).unwrap().unwrap();
    assert_eq!(db.deletion_blockers(photo).unwrap().active, 1);
    db.claim_next_send(100).unwrap();
    db.fail_send(id, SendErrorCode::Network, None, None, None, None).unwrap();
    assert_eq!(db.deletion_blockers(photo).unwrap().active, 0);
}

#[test]
fn deletion_summary_includes_send_statuses() {
    let db = Db::open_in_memory().unwrap();
    let waiting_photo = seed_photo(&db);
    let pending_photo = seed_other_photo(&db);
    db.enqueue_send(waiting_photo, SendKind::Photo, SendOrigin::Auto, SendStatus::Waiting).unwrap();
    db.enqueue_send(pending_photo, SendKind::Photo, SendOrigin::Manual, SendStatus::Pending).unwrap();
    let summary = db.deletion_job_summary(&[waiting_photo, pending_photo]).unwrap();
    assert_eq!(summary.send.waiting, 1);
    assert_eq!(summary.send.pending, 1);
}

#[test]
fn claim_deletion_will_not_start_while_a_send_is_processing() {
    let db = Db::open_in_memory().unwrap();
    let photo = seed_photo(&db);
    // auto_replace は写真を deleting にしないため、要求後に送信を積める。
    let request = db.request_deletion(&[photo], DeletionReason::AutoReplace, false).unwrap()[0];
    db.enqueue_send(photo, SendKind::Photo, SendOrigin::Manual, SendStatus::Pending).unwrap();
    db.claim_next_send(100).unwrap();
    assert!(db.claim_deletion(request).unwrap().is_none());
}

#[test]
fn manual_deletion_cancels_waiting_pending_and_failed_sends() {
    let db = Db::open_in_memory().unwrap();
    let waiting_photo = seed_photo(&db);
    let pending_photo = seed_other_photo(&db);
    let failed_root = db.add_root("/photos-c", "reference").unwrap();
    let failed_photo = db.upsert_photo(&photo_at(
        failed_root, "c.png", 1_700_000_002, "2026-10-21 21:14:05",
    )).unwrap();
    // 最初に failed 行を作って claim し、processing から failed にする。
    let failed = db.enqueue_send(failed_photo, SendKind::Photo, SendOrigin::Manual, SendStatus::Pending).unwrap().unwrap();
    db.claim_next_send(100).unwrap();
    db.fail_send(failed, SendErrorCode::Http401, None, None, None, None).unwrap();
    // その後で待機・実行待ちを積む。
    let waiting = db.enqueue_send(waiting_photo, SendKind::Photo, SendOrigin::Auto, SendStatus::Waiting).unwrap().unwrap();
    let pending = db.enqueue_send(pending_photo, SendKind::Photo, SendOrigin::Manual, SendStatus::Pending).unwrap().unwrap();
    db.request_deletion(&[waiting_photo, pending_photo, failed_photo], DeletionReason::Manual, false).unwrap();
    assert_eq!(db.send_entry_by_id(waiting).unwrap().unwrap().status, SendStatus::Cancelled);
    assert_eq!(db.send_entry_by_id(pending).unwrap().unwrap().status, SendStatus::Cancelled);
    assert_eq!(db.send_entry_by_id(failed).unwrap().unwrap().status, SendStatus::Cancelled);
}

#[test]
fn force_deletion_leaves_a_processing_send_running() {
    let db = Db::open_in_memory().unwrap();
    let photo = seed_photo(&db);
    let request = db.request_deletion(&[photo], DeletionReason::AutoReplace, false).unwrap()[0];
    let id = db.enqueue_send(photo, SendKind::Photo, SendOrigin::Manual, SendStatus::Pending).unwrap().unwrap();
    db.claim_next_send(100).unwrap();
    db.force_deletion(request).unwrap();
    assert_eq!(db.send_entry_by_id(id).unwrap().unwrap().status, SendStatus::Processing);
    assert!(db.claim_deletion(request).unwrap().is_none());
}
```

- [x] **Step 2: Run tests to verify they fail**

Run: `cargo test -p vrc-photon-core deletion_ -- --nocapture`
Expected: FAIL（send が集計されない / claim_deletion が進む）

- [x] **Step 3: Implement the SQL changes**

- `deletion_blockers`: 既存の SELECT に
  `(SELECT COUNT(*) FROM send_outbox s WHERE s.photo_id = ?1 AND s.status IN ('waiting','pending','processing'))`
  を `active` に加算。`failed` は加算しない。
- `deletion_job_summary`: 各 chunk で `send_outbox` を `photo_id IN (...)` で集計し、
  `waiting/pending/processing/failed` を `send` に入れる。`affected_photos` は
  圧縮と send の合計 distinct photo にする（同じ写真を二重に数えない）。
- `pending_deletion_requests`: 圧縮の `NOT IN` に加えて
  `dr.photo_id NOT IN (SELECT s.photo_id FROM send_outbox s WHERE s.status IN ('waiting','pending','processing'))`
  を追加。
- `claim_deletion`: active 圧縮の `NOT EXISTS` に加えて active send の `NOT EXISTS` を追加。
- `request_deletion`: 各 photo の処理で
  `UPDATE send_outbox SET status='cancelled', finished_at=? WHERE photo_id=? AND status IN ('waiting','pending','failed')`
  を実行。`processing` は残す。
- `force_deletion`: 圧縮のキャンセルに加えて
  `UPDATE send_outbox SET status='cancelled', finished_at=? WHERE photo_id=? AND status IN ('waiting','pending','failed')`
  を実行。
- `cancel_deletion` は outbox を復元しない（キャンセル済み送信は戻らない）。
  この挙動をテストで固定する:

```rust
#[test]
fn cancelling_a_deletion_does_not_restore_cancelled_sends() {
    let db = Db::open_in_memory().unwrap();
    let photo = seed_photo(&db);
    let send = db.enqueue_send(photo, SendKind::Photo, SendOrigin::Manual, SendStatus::Pending).unwrap().unwrap();
    let request = db.request_deletion(&[photo], DeletionReason::Manual, false).unwrap()[0];
    db.cancel_deletion(request).unwrap();
    assert_eq!(db.send_entry_by_id(send).unwrap().unwrap().status, SendStatus::Cancelled);
}
```

- [x] **Step 4: Run tests to verify they pass**

Run: `cargo test -p vrc-photon-core deletion_ -- --nocapture`
Expected: PASS。`cargo test -p vrc-photon-core` 全体も PASS。

- [x] **Step 5: Commit**

```bash
git add crates/core/src/db.rs
git commit -m "feat(core): drain and cancel Discord sends with deletions"
```

---

### Task 5: Webhook URL・embed・送信元の純粋ロジック

**Files:**
- Create: `crates/core/src/discord.rs`
- Modify: `crates/core/src/lib.rs`
- Modify: `crates/core/Cargo.toml`（`url.workspace = true`）
- Test: `crates/core/src/discord.rs` の tests

**Interfaces:**
- Consumes: `Photo`, `CompressionProfile`, `Derivative`, `CompressionFormat`。
- Produces:
  - `pub const DISCORD_MAX_UPLOAD_BYTES: u64 = 20 * 1024 * 1024;`
  - `pub enum WebhookUrlError { Empty, Invalid, NotHttps, HostNotAllowed, PortNotAllowed, InvalidPath }`
  - `pub fn validate_webhook_url(raw: &str) -> Result<url::Url, WebhookUrlError>`
  - `pub fn mask_webhook_url(raw: &str) -> String`
  - `pub enum SendImageFormat { Png, Jpeg, Webp, Avif, Jxl }`（`extension()`, `mime()`）
  - `pub fn format_from_path(path: &Path) -> Option<SendImageFormat>`
  - `impl From<&CompressionFormat> for SendImageFormat`
  - `pub enum SendImageSource { File { path: PathBuf, source: SendSource, format: SendImageFormat }, Encode { source_path: PathBuf, profile_id: String, format: SendImageFormat }, MissingProfile, MissingPhoto, TooLarge { size: u64 } }`
  - `pub fn choose_send_source(photo: &Photo, send_original: bool, profile: Option<&CompressionProfile>, derivatives: &[Derivative]) -> SendImageSource`
  - `pub struct EmbedPhoto<'a> { pub world_name: Option<&'a str>, pub captured_at_utc: i64, pub captured_at_local: &'a str, pub file_name: &'a str, pub players: &'a [String] }`
  - `pub struct EmbedLabels<'a> { pub captured_at: &'a str, pub players: &'a str, pub unlinked: &'a str, pub footer_app: &'a str }`
  - `pub fn photo_payload_json(photo: &EmbedPhoto, labels: &EmbedLabels, include_players: bool, upload_filename: &str) -> String`
  - `pub fn test_payload_json(content: &str) -> String`
  - `pub fn truncate_chars(value: &str, max: usize) -> String`
  - `pub fn upload_filename(photo_id: i64, captured_at_utc: i64, format: SendImageFormat) -> String`
  - `pub fn mime_for_format(format: SendImageFormat) -> &'static str`

- [x] **Step 1: Write the failing tests**

```rust
#[test]
fn webhook_url_requires_https_discord_host_and_token() {
    assert!(validate_webhook_url("https://discord.com/api/webhooks/123/token").is_ok());
    assert!(validate_webhook_url("https://discordapp.com/api/webhooks/123/token").is_ok());
    assert_eq!(validate_webhook_url(""), Err(WebhookUrlError::Empty));
    assert_eq!(validate_webhook_url("http://discord.com/api/webhooks/123/token"), Err(WebhookUrlError::NotHttps));
    assert_eq!(validate_webhook_url("https://example.com/api/webhooks/123/token"), Err(WebhookUrlError::HostNotAllowed));
    assert_eq!(validate_webhook_url("https://discord.com:8443/api/webhooks/123/token"), Err(WebhookUrlError::PortNotAllowed));
    assert_eq!(validate_webhook_url("https://discord.com/api/webhooks/123"), Err(WebhookUrlError::InvalidPath));
}

#[test]
fn masking_never_exposes_the_token() {
    let raw = "https://discord.com/api/webhooks/123456789/abcdefSECRET";
    let masked = mask_webhook_url(raw);
    assert!(!masked.contains("abcdefSECRET"));
    assert!(masked.contains("123456789"));
    assert!(masked.contains("••••"));
}

fn sample_photo(
    id: i64,
    abs_path: &Path,
    file_size: u64,
    status: PhotoStatus,
    display_derivative_id: Option<i64>,
    display_path: Option<&Path>,
) -> Photo {
    Photo {
        id,
        root_id: 1,
        rel_path: "a.png".into(),
        abs_path: abs_path.to_string_lossy().into_owned(),
        file_name: "a.png".into(),
        captured_at_utc: 0,
        captured_at_local: "2026-11-02 21:34:56".into(),
        tz_offset_sec: 0,
        width: 16,
        height: 10,
        file_size,
        mtime: 0,
        blake3: None,
        status,
        is_favorite: false,
        note: None,
        display_derivative_id,
        display_path: display_path.map(|p| p.to_string_lossy().into_owned()),
        display_format: display_path.map(|_| "webp".into()),
        display_file_size: display_path.map(|p| std::fs::metadata(p).unwrap().len()),
        display_width: display_path.map(|_| 16),
        display_height: display_path.map(|_| 10),
        world_name: None,
        instance_id: None,
        link_method: None,
        confidence: None,
        author: None,
    }
}

fn sample_derivative(photo_id: i64, profile_id: &str, path: &Path) -> Derivative {
    Derivative {
        id: 1,
        photo_id,
        path: path.to_string_lossy().into_owned(),
        format: "webp".into(),
        width: 16,
        height: 10,
        file_size: std::fs::metadata(path).unwrap().len(),
        profile_id: profile_id.into(),
        policy: "keep".into(),
        created_at: "2026-11-02T00:00:00Z".into(),
    }
}

#[test]
fn send_source_prefers_original_when_it_fits() {
    let dir = tempfile::tempdir().unwrap();
    let original = dir.path().join("a.png");
    std::fs::write(&original, b"x").unwrap();
    let photo = sample_photo(1, &original, 1, PhotoStatus::Present, None, None);
    let profile = CompressionProfile { id: "discord".into(), ..CompressionProfile::default() };
    assert!(matches!(
        choose_send_source(&photo, true, Some(&profile), &[]),
        SendImageSource::File { source: SendSource::Original, format: SendImageFormat::Png, .. }
    ));
}

#[test]
fn send_source_falls_back_to_profile_then_encode() {
    let dir = tempfile::tempdir().unwrap();
    let original = dir.path().join("a.png");
    let derivative = dir.path().join("a_discord.webp");
    std::fs::write(&original, b"x").unwrap();
    std::fs::write(&derivative, b"y").unwrap();
    let photo = sample_photo(1, &original, 1, PhotoStatus::Present, None, None);
    let profile = CompressionProfile { id: "discord".into(), ..CompressionProfile::default() };
    let derivatives = vec![sample_derivative(1, "discord", &derivative)];
    assert!(matches!(
        choose_send_source(&photo, false, Some(&profile), &derivatives),
        SendImageSource::File { source: SendSource::Profile, format: SendImageFormat::Webp, .. }
    ));
    assert!(matches!(
        choose_send_source(&photo, false, Some(&profile), &[]),
        SendImageSource::Encode { profile_id, format: SendImageFormat::Webp, .. } if profile_id == "discord"
    ));
}

#[test]
fn replaced_photo_uses_its_display_derivative() {
    let dir = tempfile::tempdir().unwrap();
    let original = dir.path().join("a.png");
    let display = dir.path().join("display.webp");
    std::fs::write(&display, b"y").unwrap();
    let photo = sample_photo(1, &original, 1, PhotoStatus::Replaced, Some(7), Some(&display));
    assert!(matches!(
        choose_send_source(&photo, true, None, &[]),
        SendImageSource::File { source: SendSource::Derivative, format: SendImageFormat::Webp, .. }
    ));
}

#[test]
fn missing_original_and_no_derivative_is_missing_photo() {
    let dir = tempfile::tempdir().unwrap();
    let photo = sample_photo(1, &dir.path().join("gone.png"), 1, PhotoStatus::Present, None, None);
    assert!(matches!(
        choose_send_source(&photo, true, None, &[]),
        SendImageSource::MissingPhoto
    ));
}

#[test]
fn source_over_the_limit_is_rejected_even_after_fallback() {
    let dir = tempfile::tempdir().unwrap();
    let original = dir.path().join("a.png");
    let derivative = dir.path().join("a_discord.webp");
    std::fs::write(&original, b"x").unwrap();
    let file = std::fs::File::create(&derivative).unwrap();
    file.set_len(DISCORD_MAX_UPLOAD_BYTES + 1).unwrap();
    let photo = sample_photo(1, &original, 1, PhotoStatus::Present, None, None);
    let profile = CompressionProfile { id: "discord".into(), ..CompressionProfile::default() };
    let derivatives = vec![sample_derivative(1, "discord", &derivative)];
    assert!(matches!(
        choose_send_source(&photo, false, Some(&profile), &derivatives),
        SendImageSource::TooLarge { .. }
    ));
}

#[test]
fn payload_includes_world_date_and_optional_players() {
    let labels = EmbedLabels { captured_at: "撮影日時", players: "プレイヤー", unlinked: "未紐付け", footer_app: "VRC PhotoN 0.10.1" };
    let photo = EmbedPhoto { world_name: Some("Test World"), captured_at_utc: 1_762_000_000, captured_at_local: "2026-11-02 21:34:56", file_name: "VRChat.png", players: &["Alice".into(), "Bob".into()] };
    let with_players = photo_payload_json(&photo, &labels, true, "vrc-photon-1-1762000000.png");
    let value: serde_json::Value = serde_json::from_str(&with_players).unwrap();
    let embed = &value["embeds"][0];
    assert_eq!(embed["title"], "Test World");
    assert_eq!(embed["image"]["url"], "attachment://vrc-photon-1-1762000000.png");
    assert!(with_players.contains("Alice"));
    let without = photo_payload_json(&photo, &labels, false, "x.png");
    assert!(!without.contains("Alice"));
}

#[test]
fn payload_truncates_long_world_and_player_lists() {
    let long_world = "W".repeat(300);
    let players: Vec<String> = (0..200).map(|i| format!("Player{i}")).collect();
    let payload = photo_payload_json(&EmbedPhoto { world_name: Some(&long_world), captured_at_utc: 0, captured_at_local: "x", file_name: "f.png", players: &players }, &EmbedLabels { captured_at: "c", players: "p", unlinked: "u", footer_app: "a" }, true, "x.png");
    assert!(payload.chars().count() < 6000);
    assert!(!payload.contains(&"W".repeat(257)));
    assert!(payload.contains("(+"));
}

#[test]
fn upload_filename_and_mime_follow_the_format() {
    assert_eq!(upload_filename(1, 2, SendImageFormat::Png), "vrc-photon-1-2.png");
    assert_eq!(upload_filename(1, 2, SendImageFormat::Webp), "vrc-photon-1-2.webp");
    assert_eq!(mime_for_format(SendImageFormat::Png), "image/png");
    assert_eq!(mime_for_format(SendImageFormat::Jpeg), "image/jpeg");
    assert_eq!(mime_for_format(SendImageFormat::Webp), "image/webp");
    assert_eq!(mime_for_format(SendImageFormat::Avif), "image/avif");
    assert_eq!(mime_for_format(SendImageFormat::Jxl), "image/jxl");
}

#[test]
fn format_from_path_maps_supported_extensions() {
    assert_eq!(format_from_path(Path::new("a.PNG")), Some(SendImageFormat::Png));
    assert_eq!(format_from_path(Path::new("a.jpg")), Some(SendImageFormat::Jpeg));
    assert_eq!(format_from_path(Path::new("a.webp")), Some(SendImageFormat::Webp));
    assert_eq!(format_from_path(Path::new("a.txt")), None);
}
```

- [x] **Step 2: Run tests to verify they fail**

Run: `cargo test -p vrc-photon-core discord::tests -- --nocapture`
Expected: FAIL（モジュール未定義）

- [x] **Step 3: Implement the module**

- `validate_webhook_url`: `url::Url::parse`、`scheme()=="https"`、`port().is_none() || port()==Some(443)`、host が許可リスト、`path_segments()` が `["api","webhooks",<id>,<token>]` 以上。token は空でない。
- `mask_webhook_url`: 正常 URL なら `https://<host>/api/webhooks/<id>…/••••`、解析不能なら `••••`。
- `choose_send_source`:
  1. `photo.status == Replaced` または `photo.abs_path` がファイルでない:
     `photo.effective_path()` がファイルなら `File { source: Derivative }`。
     なければ選択プロファイルの derivative → `File { source: Profile }`。
     なければ `MissingPhoto`。
  2. `send_original` かつ原画サイズ ≤ 20 MiB → `File { source: Original }`。
     サイズは `fs::metadata(photo.abs_path).len()` を優先し、取得できなければ
     `photo.file_size` を使う。
  3. プロファイルなし → `MissingProfile`。
  4. プロファイルの derivative が実在 → サイズ確認し `File { source: Profile }`。
  5. それ以外 → `Encode { source_path: photo.effective_path(), profile_id }`。
  6. サイズが 20 MiB 超なら `TooLarge { size }`。
- `photo_payload_json`: `serde_json::json!` で設計 6.8 の形。`truncate_chars` を
  `pub fn` で実装（タイトル 256、field value 1024、footer 2048）。
  プレイヤーは `", "` で連結し、1024 文字を超えたら先頭から詰めて `… (+N)`。
- `test_payload_json`: `{"content": content}`。
- `upload_filename`: `format!("vrc-photon-{photo_id}-{captured_at_utc}.{}", format.extension())`。
- `mime_for_format`: 上記の固定文字列。
- `format_from_path`: 拡張子を小文字化して `png` / `jpg` / `jpeg` / `webp` /
  `avif` / `jxl` を `SendImageFormat` へ写像する。
- `impl From<&CompressionFormat> for SendImageFormat`: Jpeg→Jpeg, Webp→Webp,
  Avif→Avif, JpegXl→Jxl。
- `choose_send_source` の `File` は `format_from_path(path)` を使い、
  `Encode` は `SendImageFormat::from(&profile.format)` を返す。拡張子が
  不明な表示用派生は `photo.display_format` を同じ写像で解決し、それも
  不明なら `Png` にする。

- [x] **Step 4: Run tests to verify they pass**

Run: `cargo test -p vrc-photon-core discord::tests -- --nocapture`
Expected: PASS

- [x] **Step 5: Commit**

```bash
git add crates/core/src/discord.rs crates/core/src/lib.rs crates/core/Cargo.toml Cargo.lock
git commit -m "feat(core): validate Discord webhooks and build send payloads"
```

---

### Task 6: メモリ上エンコード

**Files:**
- Modify: `crates/core/src/compressor.rs`
- Test: `crates/core/src/compressor.rs` の tests

**Interfaces:**
- Produces:
  - `pub struct EncodedImage { pub bytes: Vec<u8>, pub format: CompressionFormat, pub width: u32, pub height: u32 }`
  - `pub fn encode_for_send(source: &Path, profile: &CompressionProfile) -> Result<EncodedImage>`

- [x] **Step 1: Write the failing test**

```rust
#[test]
fn encode_for_send_returns_bytes_without_writing_a_file() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("source.png");
    image::RgbaImage::from_pixel(32, 20, image::Rgba([10, 20, 30, 255])).save(&source).unwrap();
    let profile = CompressionProfile { format: CompressionFormat::Webp, lossless: true, resize_enabled: false, ..CompressionProfile::default() };
    let encoded = encode_for_send(&source, &profile).unwrap();
    assert_eq!(encoded.format, CompressionFormat::Webp);
    assert_eq!((encoded.width, encoded.height), (32, 20));
    assert!(!encoded.bytes.is_empty());
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1); // source だけ
}
```

- [x] **Step 2: Run test to verify it fails**

Run: `cargo test -p vrc-photon-core encode_for_send -- --nocapture`
Expected: FAIL（未定義）

- [x] **Step 3: Implement**

`compress_decoded` の中の「resize → encode → width/height」を
`fn encode_image_bytes(image: &DynamicImage, profile: &CompressionProfile) -> Result<(Vec<u8>, u32, u32)>`
へ抽出し、`compress_decoded` と `encode_for_send` の両方から使う。
`encode_for_send` は `image_decode::decode_image` → `encode_image_bytes` → `EncodedImage`。
既存の `compress_decoded` の出力・検証ロジックは変えない。

- [x] **Step 4: Run tests**

Run: `cargo test -p vrc-photon-core compressor -- --nocapture`
Expected: PASS（既存の圧縮テストも含む）

- [x] **Step 5: Commit**

```bash
git add crates/core/src/compressor.rs
git commit -m "feat(core): encode Discord images in memory"
```

---

### Task 7: IPC プロトコル v4

**Files:**
- Modify: `crates/ipc/src/lib.rs`（`PROTOCOL_VERSION = 4`）
- Modify: `crates/ipc/src/protocol.rs`
- Modify: `crates/ipc/src/client.rs`（`StatusSnapshot` テストリテラルに `pending_sends: 0`）
- Modify: `crates/daemon/src/ipc_server.rs`（`status_snapshot` のリテラルに `pending_sends: 0`、`WakeSends` の仮ハンドラ）
- Modify: `crates/ui/src/ipc_bridge.rs`（`StatusSnapshot` テストリテラルに `pending_sends: 0`）
- Modify: `crates/ui/src/app/lifecycle.rs`（`IpcEvent::SendProgress { .. } => {}` の仮アーム）
- Test: `crates/ipc/src/protocol.rs` の tests

**Interfaces:**
- Produces:
  - `Request::WakeSends`
  - `pub enum SendStatus { Waiting, Pending, Processing, Sent, Failed, Cancelled, Unknown }`
  - `Event::SendProgress { outbox_id: i64, photo_id: Option<i64>, status: SendStatus, error_code: Option<String> }`
  - `StatusSnapshot.pending_sends: i64`（`#[serde(default)]`）

- [x] **Step 1: Write the failing tests**

```rust
#[test]
fn wake_sends_round_trips() {
    let value = RequestEnvelope { id: 10, request: Request::WakeSends };
    let json = serde_json::to_string(&value).unwrap();
    assert!(json.contains("\"wake_sends\""));
    assert_eq!(serde_json::from_str::<RequestEnvelope>(&json).unwrap(), value);
}

#[test]
fn send_progress_round_trips_and_defaults_pending_sends() {
    let event = Event::SendProgress { outbox_id: 4, photo_id: Some(9), status: SendStatus::Processing, error_code: None };
    let json = serde_json::to_string(&event).unwrap();
    assert_eq!(serde_json::from_str::<Event>(&json).unwrap(), event);
    let snapshot: StatusSnapshot = serde_json::from_str(r#"{"paused":false,"watching":true,"scanning":false,"scan_phase":null,"scan_processed":0,"scan_total":null,"scan_indexed":0,"scan_root":0,"scan_roots":0,"photo_count":1,"pending_jobs":0}"#).unwrap();
    assert_eq!(snapshot.pending_sends, 0);
}
```

- [x] **Step 2: Run tests to verify they fail**

Run: `cargo test -p vrc-photon-ipc -- --nocapture`
Expected: FAIL（`WakeSends` / `SendStatus` / `pending_sends` 未定義）

- [x] **Step 3: Implement**

- `PROTOCOL_VERSION = 4`。
- `Request::WakeSends` を `WakeJobs` の隣に追加。
- `SendStatus` を `JobStatus` の隣に追加（`snake_case`）。
- `Event::SendProgress { ... }` を追加。
- `StatusSnapshot` に `#[serde(default)] pub pending_sends: i64` を追加。
- 追加フィールドで壊れる構造体リテラルを `pending_sends: 0` で更新する
  （`crates/ipc/src/client.rs`, `crates/daemon/src/ipc_server.rs`,
  `crates/ui/src/ipc_bridge.rs`）。
- `crates/daemon/src/ipc_server.rs` の `Request` match に
  `Request::WakeSends => Response::Error { code: InvalidRequest, message: "wake_sends not wired" }`
  の仮アームを置く。Task 10 で `IpcControl.sends.notify()` + `Response::Ok` に置き換える。
- `crates/ui/src/app/lifecycle.rs` の `IpcEvent` match に
  `IpcEvent::SendProgress { .. } => {}` の仮アームを置く。Task 13 で
  `reload_send_history` に置き換える。
- `status_snapshot` の `pending_sends` は Task 10 で `DaemonStatus` から埋める
  （このタスクでは 0 のままでよい）。

- [x] **Step 4: Run tests**

Run: `cargo test -p vrc-photon-ipc -- --nocapture`
Expected: PASS

- [x] **Step 5: Commit**

```bash
git add crates/ipc/src/lib.rs crates/ipc/src/protocol.rs
git commit -m "feat(ipc): add Discord send wake and progress events"
```

---

### Task 8: HTTP transport（multipart / 429 / 4xx / 5xx）

**Files:**
- Create: `crates/daemon/src/discord_http.rs`
- Modify: `crates/daemon/src/lib.rs`（`mod discord_http;`）
- Modify: `crates/daemon/Cargo.toml`（`ureq.workspace = true`）
- Modify: `Cargo.toml`（`ureq` に `features = ["multipart"]`）
- Test: `crates/daemon/src/discord_http.rs` の tests

**Interfaces:**
- Consumes: `vrc_photon_core::discord::validate_webhook_url`, `SendErrorCode`。
- Produces:
  - `#[derive(Debug, Clone)] pub struct SendImage { pub bytes: Vec<u8>, pub filename: String, pub mime: String }`
  - `#[derive(Debug, Clone)] pub struct SendRequest { pub url: String, pub payload_json: String, pub image: Option<SendImage> }`
  - `#[derive(Debug, Clone)] pub enum SendOutcome { Sent { message_id: Option<String>, channel_id: Option<String>, guild_id: Option<String> }, RateLimited { retry_after: std::time::Duration }, Retryable { detail: String }, Failed { code: vrc_photon_core::model::SendErrorCode, detail: Option<String> } }`
  - `pub trait SendTransport: Send + Sync { fn send(&self, request: &SendRequest) -> SendOutcome; }`
  - `pub struct UreqTransport;` + `pub fn UreqTransport::new() -> Self`
  - `pub fn parse_retry_after(body: &[u8], header: Option<&str>) -> Option<std::time::Duration>`
  - `pub fn classify_response(status: u16, body: &[u8], retry_after_header: Option<&str>) -> SendOutcome`

- [x] **Step 1: Write the failing pure tests**

```rust
#[test]
fn retry_after_prefers_json_and_clamps_to_one_to_900_seconds() {
    assert_eq!(parse_retry_after(br#"{"retry_after":0.5}"#, None), Some(Duration::from_secs(1)));
    assert_eq!(parse_retry_after(br#"{"retry_after":120}"#, None), Some(Duration::from_secs(120)));
    assert_eq!(parse_retry_after(b"", Some("0.2")), Some(Duration::from_secs(1)));
    assert_eq!(parse_retry_after(b"", Some("5000")), Some(Duration::from_secs(900)));
    assert_eq!(parse_retry_after(b"", None), None);
}

#[test]
fn response_classification_maps_discord_statuses() {
    assert!(matches!(classify_response(200, br#"{"id":"42","channel_id":"7","guild_id":"9"}"#, None), SendOutcome::Sent { message_id: Some(id), .. } if id == "42"));
    assert!(matches!(classify_response(204, b"", None), SendOutcome::Sent { message_id: None, .. }));
    assert!(matches!(classify_response(429, br#"{"retry_after":2}"#, None), SendOutcome::RateLimited { .. }));
    assert!(matches!(classify_response(401, br#"{"message":"unauthorized"}"#, None), SendOutcome::Failed { code: SendErrorCode::Http401, .. }));
    assert!(matches!(classify_response(500, b"", None), SendOutcome::Retryable { .. }));
}
```

- [x] **Step 2: Run tests to verify they fail**

Run: `cargo test -p vrc-photon-daemon discord_http -- --nocapture`
Expected: FAIL（モジュール未定義）

- [x] **Step 3: Implement the transport**

- `UreqTransport::send`:
  - `UreqTransport { allow_loopback: bool }` とし、`new()` は `false`。
    `#[cfg(test)] fn for_loopback()` だけが `true` を作る（本番経路では
    private フィールドと cfg(test) コンストラクタのため到達できない）。
  - `allow_loopback == false` なら `validate_webhook_url(&request.url)` を実行し、
    Err → `Failed { WebhookInvalid }`。
  - `let mut form = Form::new().text("payload_json", &request.payload_json);`
  - `if let Some(image) = &request.image { form = form.part("files[0]", Part::bytes(&image.bytes).file_name(&image.filename).mime_str(&image.mime).unwrap()); }`
  - URL に `wait=true` を付ける（既存クエリがあれば `&`）。
  - `ureq::Agent` を `https_only(!allow_loopback)`, `max_redirects(0)`,
    `timeout_connect(10s)`, `timeout_global(120s)`,
    `http_status_as_error(false)`, `user_agent(...)` で構築。
  - レスポンス本文は `response.body_mut().with_config().limit(64 * 1024)` 相当で最大 64 KiB。
  - `classify_response(status, &body, header)` を返す。
- `classify_response`:
  - 200/204: `serde_json::from_slice` で `id`/`channel_id`/`guild_id`（文字列 or 数値）を読む。失敗しても `Sent { None }`。
  - 429: `parse_retry_after`。なければ 5 秒。
  - 400/401/403/404/413: それぞれ `Http400` / `Http401` / `Http403` / `Http404` /
    `Http413` + body の `message` を 200 文字に切り詰め。
  - その他 4xx: `Http400`。
  - 5xx: `Retryable { detail: format!("HTTP {status}") }`。
  - ネットワーク: `Retryable { detail }`。`detail` は `ureq::Error::Io` の
    `kind()` から作り、`ureq::Error` の `Display` や URL を含めない。
- `parse_retry_after`: JSON → `f64`、`ceil`、clamp 1..=900。header は秒の `f64`。
- テスト専用の `#[cfg(test)] fn loopback_agent() -> ureq::Agent` と
  `#[cfg(test)] impl UreqTransport { fn for_loopback() -> Self }` を追加し、
  ローカル `TcpListener` に対して multipart を送る結合テストを書く:

```rust
#[test]
fn multipart_contains_payload_and_image() {
    let (url, received) = spawn_http_server(200, br#"{"id":"42"}"#);
    let transport = UreqTransport::for_loopback();
    let outcome = transport.send(&SendRequest {
        url,
        payload_json: r#"{"content":"hi"}"#.into(),
        image: Some(SendImage { bytes: b"PNGDATA".to_vec(), filename: "vrc-photon-1-2.png".into(), mime: "image/png".into() }),
    });
    assert!(matches!(outcome, SendOutcome::Sent { message_id: Some(id), .. } if id == "42"));
    let body = received.join();
    assert!(body.contains("name=\"payload_json\""));
    assert!(body.contains("name=\"files[0]\""));
    assert!(body.contains("filename=\"vrc-photon-1-2.png\""));
    assert!(body.contains("PNGDATA"));
}

#[test]
fn network_errors_do_not_echo_the_webhook_url() {
    let transport = UreqTransport::for_loopback();
    let outcome = transport.send(&SendRequest {
        url: "http://127.0.0.1:1/api/webhooks/1/SECRET".into(),
        payload_json: "{}".into(),
        image: None,
    });
    match outcome {
        SendOutcome::Retryable { detail } => {
            assert!(!detail.contains("SECRET"));
            assert!(!detail.contains("http"));
        }
        other => panic!("expected Retryable, got {other:?}"),
    }
}
```

- [x] **Step 4: Run tests**

Run: `cargo test -p vrc-photon-daemon discord_http -- --nocapture`
Expected: PASS

- [x] **Step 5: Commit**

```bash
git add crates/daemon/src/discord_http.rs crates/daemon/src/lib.rs crates/daemon/Cargo.toml Cargo.toml Cargo.lock
git commit -m "feat(daemon): add Discord multipart HTTP transport"
```

---

### Task 9: 送信ワーカーの処理単位

**Files:**
- Create: `crates/daemon/src/discord.rs`
- Modify: `crates/daemon/src/lib.rs`（`mod discord;`）
- Test: `crates/daemon/src/discord.rs` の tests

**Interfaces:**
- Consumes: Task 3/5/6/8 の API。
- Produces:
  - `pub(crate) enum ProcessOutcome { Idle, Sent, Rescheduled, Failed }`
  - `pub(crate) fn process_next_send(db: &Db, transport: &dyn SendTransport, events: &EventBus, now: i64) -> anyhow::Result<ProcessOutcome>`
  - `pub(crate) fn recover_interrupted_sends(db: &Db) -> Result<usize>`
  - `pub(crate) fn ipc_send_status(status: SendStatus) -> vrc_photon_ipc::SendStatus`
  - `pub(crate) fn worker_loop(db: Arc<Mutex<Db>>, running: Arc<AtomicBool>, sends: Arc<JobSignal>, events: EventBus, transport: Arc<dyn SendTransport>)`

- [x] **Step 1: Write the failing worker tests with a fake transport**

```rust
#[derive(Default)]
struct FakeTransport {
    requests: Mutex<Vec<SendRequest>>,
    outcomes: Mutex<VecDeque<SendOutcome>>,
}
impl SendTransport for FakeTransport {
    fn send(&self, request: &SendRequest) -> SendOutcome {
        self.requests.lock().unwrap().push(request.clone());
        self.outcomes.lock().unwrap().pop_front().unwrap_or(SendOutcome::Failed { code: SendErrorCode::Unknown, detail: None })
    }
}

fn seed_photo_file(db: &Db, photo_dir: &Path, name: &str) -> i64 {
    let path = photo_dir.join(name);
    image::RgbaImage::from_pixel(16, 10, image::Rgba([10, 20, 30, 255]))
        .save(&path)
        .unwrap();
    let root = db.add_root(photo_dir.to_str().unwrap(), "reference").unwrap();
    let meta = std::fs::metadata(&path).unwrap();
    db.upsert_photo(&NewPhoto {
        root_id: root,
        rel_path: name.to_string(),
        abs_path: path.to_string_lossy().into_owned(),
        file_name: name.to_string(),
        captured_at_utc: 1_700_000_000,
        captured_at_local: "2026-11-02 21:34:56".to_string(),
        tz_offset_sec: 0,
        width: 16,
        height: 10,
        file_size: meta.len(),
        mtime: 1_700_000_000,
        blake3: None,
    }).unwrap()
}

fn discord_db(dir: &tempfile::TempDir) -> (Db, i64) {
    let db = Db::open(dir.path().join("library.db")).unwrap();
    let photo_dir = dir.path().join("photos");
    std::fs::create_dir_all(&photo_dir).unwrap();
    let photo = seed_photo_file(&db, &photo_dir, "VRChat_2026-11-02_21-34-56.000_16x10.png");
    (db, photo)
}

#[test]
fn disabled_integration_sends_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let (db, photo) = discord_db(&dir);
    let mut settings = AppSettings::load(&db).unwrap();
    settings.discord_enabled = false;
    settings.save(&db).unwrap();
    save_discord_webhook(&db, "https://discord.com/api/webhooks/1/t").unwrap();
    db.enqueue_send(photo, SendKind::Photo, SendOrigin::Manual, SendStatus::Pending).unwrap();
    let transport = FakeTransport::default();
    let events = EventBus::default();
    assert!(matches!(process_next_send(&db, &transport, &events, 100).unwrap(), ProcessOutcome::Idle));
    assert!(transport.requests.lock().unwrap().is_empty());
}

fn enable_discord(db: &Db, url: &str) {
    let mut settings = AppSettings::load(db).unwrap();
    settings.discord_enabled = true;
    settings.save(db).unwrap();
    save_discord_webhook(db, url).unwrap();
}

fn sent_outcome() -> SendOutcome {
    SendOutcome::Sent {
        message_id: Some("42".into()),
        channel_id: Some("7".into()),
        guild_id: Some("9".into()),
    }
}

#[test]
fn success_stores_message_id_and_publishes_sent() {
    let dir = tempfile::tempdir().unwrap();
    let (db, photo) = discord_db(&dir);
    enable_discord(&db, "https://discord.com/api/webhooks/1/t");
    let id = db.enqueue_send(photo, SendKind::Photo, SendOrigin::Manual, SendStatus::Pending).unwrap().unwrap();
    let transport = FakeTransport::default();
    transport.outcomes.lock().unwrap().push_back(sent_outcome());
    let events = EventBus::default();
    let rx = events.subscribe();
    assert!(matches!(process_next_send(&db, &transport, &events, 100).unwrap(), ProcessOutcome::Sent));
    let entry = db.send_entry_by_id(id).unwrap().unwrap();
    assert_eq!(entry.status, SendStatus::Sent);
    assert_eq!(entry.discord_message_id.as_deref(), Some("42"));
    assert_eq!(entry.discord_channel_id.as_deref(), Some("7"));
    assert_eq!(entry.source, Some(SendSource::Original));
    let requests = transport.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].image.is_some());
    drop(requests);
    let mut seen = Vec::new();
    while let Ok(event) = rx.try_recv() {
        seen.push(event);
    }
    assert!(seen.iter().any(|event| matches!(
        event,
        Event::SendProgress { status: vrc_photon_ipc::SendStatus::Sent, .. }
    )));
}

#[test]
fn test_send_has_no_image() {
    let dir = tempfile::tempdir().unwrap();
    let (db, _photo) = discord_db(&dir);
    enable_discord(&db, "https://discord.com/api/webhooks/1/t");
    let id = db.enqueue_test_send().unwrap();
    let transport = FakeTransport::default();
    transport.outcomes.lock().unwrap().push_back(sent_outcome());
    let events = EventBus::default();
    assert!(matches!(process_next_send(&db, &transport, &events, 100).unwrap(), ProcessOutcome::Sent));
    assert_eq!(db.send_entry_by_id(id).unwrap().unwrap().kind, SendKind::Test);
    let requests = transport.requests.lock().unwrap();
    assert!(requests[0].image.is_none());
    assert!(requests[0].payload_json.contains("content"));
}

#[test]
fn rate_limit_reschedules_without_incrementing_attempts() {
    let dir = tempfile::tempdir().unwrap();
    let (db, photo) = discord_db(&dir);
    enable_discord(&db, "https://discord.com/api/webhooks/1/t");
    let id = db.enqueue_send(photo, SendKind::Photo, SendOrigin::Manual, SendStatus::Pending).unwrap().unwrap();
    let transport = FakeTransport::default();
    transport.outcomes.lock().unwrap().push_back(SendOutcome::RateLimited { retry_after: Duration::from_secs(2) });
    let events = EventBus::default();
    assert!(matches!(process_next_send(&db, &transport, &events, 100).unwrap(), ProcessOutcome::Rescheduled));
    let entry = db.send_entry_by_id(id).unwrap().unwrap();
    assert_eq!(entry.status, SendStatus::Pending);
    assert_eq!(entry.attempts, 0);
    assert_eq!(entry.next_attempt_at, Some(102));
    assert!(matches!(process_next_send(&db, &transport, &events, 101).unwrap(), ProcessOutcome::Idle));
    assert_eq!(transport.requests.lock().unwrap().len(), 1);
}

#[test]
fn network_error_reschedules_with_attempts_and_backoff() {
    let dir = tempfile::tempdir().unwrap();
    let (db, photo) = discord_db(&dir);
    enable_discord(&db, "https://discord.com/api/webhooks/1/t");
    let id = db.enqueue_send(photo, SendKind::Photo, SendOrigin::Manual, SendStatus::Pending).unwrap().unwrap();
    let transport = FakeTransport::default();
    transport.outcomes.lock().unwrap().push_back(SendOutcome::Retryable { detail: "network error: ConnectionRefused".into() });
    let events = EventBus::default();
    assert!(matches!(process_next_send(&db, &transport, &events, 100).unwrap(), ProcessOutcome::Rescheduled));
    let entry = db.send_entry_by_id(id).unwrap().unwrap();
    assert_eq!(entry.status, SendStatus::Pending);
    assert_eq!(entry.attempts, 1);
    assert_eq!(entry.next_attempt_at, Some(102));
    assert_eq!(entry.error_code.as_deref(), Some("network"));
    assert!(matches!(process_next_send(&db, &transport, &events, 101).unwrap(), ProcessOutcome::Idle));
}

#[test]
fn unauthorized_marks_failed() {
    let dir = tempfile::tempdir().unwrap();
    let (db, photo) = discord_db(&dir);
    enable_discord(&db, "https://discord.com/api/webhooks/1/t");
    let id = db.enqueue_send(photo, SendKind::Photo, SendOrigin::Manual, SendStatus::Pending).unwrap().unwrap();
    let transport = FakeTransport::default();
    transport.outcomes.lock().unwrap().push_back(SendOutcome::Failed {
        code: SendErrorCode::Http401,
        detail: Some("unauthorized".into()),
    });
    let events = EventBus::default();
    assert!(matches!(process_next_send(&db, &transport, &events, 100).unwrap(), ProcessOutcome::Failed));
    let entry = db.send_entry_by_id(id).unwrap().unwrap();
    assert_eq!(entry.status, SendStatus::Failed);
    assert_eq!(entry.error_code.as_deref(), Some("http_401"));
    assert_eq!(entry.error_detail.as_deref(), Some("unauthorized"));
}

#[test]
fn missing_photo_without_derivative_marks_failed() {
    let dir = tempfile::tempdir().unwrap();
    let (db, photo) = discord_db(&dir);
    enable_discord(&db, "https://discord.com/api/webhooks/1/t");
    let path = db.photo_by_id(photo).unwrap().unwrap().abs_path;
    std::fs::remove_file(path).unwrap();
    let id = db.enqueue_send(photo, SendKind::Photo, SendOrigin::Manual, SendStatus::Pending).unwrap().unwrap();
    let transport = FakeTransport::default();
    let events = EventBus::default();
    assert!(matches!(process_next_send(&db, &transport, &events, 100).unwrap(), ProcessOutcome::Failed));
    let entry = db.send_entry_by_id(id).unwrap().unwrap();
    assert_eq!(entry.status, SendStatus::Failed);
    assert_eq!(entry.error_code.as_deref(), Some("photo_missing"));
    assert!(transport.requests.lock().unwrap().is_empty());
}

#[test]
fn replaced_photo_sends_display_derivative() {
    let dir = tempfile::tempdir().unwrap();
    let (db, photo) = discord_db(&dir);
    enable_discord(&db, "https://discord.com/api/webhooks/1/t");
    let display = dir.path().join("display.webp");
    std::fs::write(&display, b"WEBPDATA").unwrap();
    let derivative = db.add_derivative(
        photo,
        display.to_str().unwrap(),
        "webp",
        16,
        10,
        std::fs::metadata(&display).unwrap().len(),
        "other",
        "replace",
    ).unwrap();
    db.set_replaced(photo, derivative).unwrap();
    let id = db.enqueue_send(photo, SendKind::Photo, SendOrigin::Manual, SendStatus::Pending).unwrap().unwrap();
    let transport = FakeTransport::default();
    transport.outcomes.lock().unwrap().push_back(sent_outcome());
    let events = EventBus::default();
    assert!(matches!(process_next_send(&db, &transport, &events, 100).unwrap(), ProcessOutcome::Sent));
    let entry = db.send_entry_by_id(id).unwrap().unwrap();
    assert_eq!(entry.source, Some(SendSource::Derivative));
    let requests = transport.requests.lock().unwrap();
    assert_eq!(requests[0].image.as_ref().unwrap().bytes, b"WEBPDATA");
}

#[test]
fn recover_interrupted_sends_marks_processing_failed() {
    let dir = tempfile::tempdir().unwrap();
    let (db, photo) = discord_db(&dir);
    let id = db.enqueue_send(photo, SendKind::Photo, SendOrigin::Manual, SendStatus::Pending).unwrap().unwrap();
    db.claim_next_send(100).unwrap();
    assert_eq!(recover_interrupted_sends(&db).unwrap(), 1);
    let entry = db.send_entry_by_id(id).unwrap().unwrap();
    assert_eq!(entry.status, SendStatus::Failed);
    assert_eq!(entry.error_code.as_deref(), Some("interrupted"));
}
```

- [x] **Step 2: Run tests to verify they fail**

Run: `cargo test -p vrc-photon-daemon discord::tests -- --nocapture`
Expected: FAIL（未定義）

- [x] **Step 3: Implement `process_next_send`**

1. `let settings = AppSettings::load(db)?;`
2. `let webhook = load_discord_webhook(db)?;`
3. `if !settings.discord_enabled { return Idle; }`
4. URL が `None` または `validate_webhook_url` が Err → `Idle`。
5. `let Some(entry) = db.claim_next_send(now)? else { return Idle; };`
6. claim 後に `AppSettings` と Webhook を再読込する。無効化／URL 消失なら
   `db.release_send(entry.id)` して `Idle` を返す（送信はしない）。
7. 写真を読む:
   - `Test`: 画像なし、`test_payload_json(tr!("discord.test.message"))`。
   - `Photo`: `photo_by_id`。`None` → `fail_send(PhotoMissing)`。
8. プロファイル: `db.list_profiles()` から `profile.discord && profile.enabled` を探す。
9. derivative: `db.derivatives_for_photo(photo_id)?`。
10. `choose_send_source`:
    - `File { path, source, format }` → `fs::metadata` で 20 MiB を再確認し、`fs::read`。
      読み込み後にも `bytes.len()` を再確認する（TOCTOU 対策）。
      `filename = upload_filename(photo_id, captured_at, format)`、
      `mime = mime_for_format(format)`。
    - `Encode { source_path, profile_id, format }` → `encode_for_send`。
      エンコード後にも `encoded.bytes.len()` を確認する。
      `profile_id` から profile を取り、`format` は `SendImageFormat::from(&encoded.format)`
      を使う（enum の `format` と同じ）。
    - `MissingProfile` / `MissingPhoto` / `TooLarge` → `fail_send`。
    - Replaced の表示用派生が 20 MiB 超なら `TooLarge`（原画がないため再エンコードしない）。
11. プレイヤー: `settings.discord_include_players` なら `db.photo_players(photo.id)?` の表示名。
12. `photo_payload_json`。`labels` は `tr!("discord.embed.captured_at")` など。
13. `SendRequest` を組み立て、`transport.send`。
14. 結果:
    - `Sent` → `complete_send`。`db.prune_send_history(500)`。`ProcessOutcome::Sent`。
    - `RateLimited` → `reschedule_send`（attempts 据え置き、`now + retry_after`、
      `error_code = RateLimited`）。
    - `Retryable` → `attempts + 1`, `now + min(2^(attempts-1) * 2, 300)`
      （初回 2 秒、以後 4, 8, …）。
      `error_code` は detail が `HTTP ` で始まるなら `Http5xx`、それ以外は `Network`。
    - `Failed` → `fail_send`。
15. claim 直後に `Processing`、各結果で `Sent` / `Failed` / `Pending`（再試行待ち）を
    `events.publish(Event::SendProgress { ... })` で出す。
16. `error_detail` は `truncate_chars(200)`、URL を含めない。

`worker_loop` は 500 ms ポーリング、`sends.wait(&mut seen, 500ms)`、成功間隔 400 ms。
`recover_interrupted_sends` はワーカー起動時に 1 回呼ぶ。
`ipc_send_status` は core の `SendStatus` を `vrc_photon_ipc::SendStatus` へ
1:1 で写像する。

- `crates/daemon/src/lib.rs` の一時的な `#[allow(dead_code)] mod discord_http;` を
  外す（Task 8 の暫定措置）。
- `SendRequest` は `Debug` に URL を含むため、`?request` でログに出さない。
  ログには `outbox_id` / `photo_id` / `status` だけを出す。
- 成功時は `db.prune_send_history(500)` を呼ぶ。

i18n（ja/en）: `discord.test.message`, `discord.embed.captured_at`,
`discord.embed.players`, `discord.embed.unlinked` を追加する。

- [x] **Step 4: Run tests**

Run: `cargo test -p vrc-photon-daemon discord::tests -- --nocapture`
Expected: PASS

- [x] **Step 5: Commit**

```bash
git add crates/daemon/src/discord.rs crates/daemon/src/lib.rs
git commit -m "feat(daemon): process Discord send outbox entries"
```

---

### Task 10: デーモン配線（worker / IPC / status）

**Files:**
- Modify: `crates/daemon/src/lib.rs`（`Daemon.sends`, `start_workers`, `stop_workers`, `shutdown`, `start_ipc`, `refresh_status`, `status_snapshot`）
- Modify: `crates/daemon/src/ipc_server.rs`（`IpcControl.sends`, `WakeSends`, `pending_sends`）
- Test: `crates/daemon/src/ipc_server.rs` と `crates/daemon/src/lib.rs` の tests

**Interfaces:**
- Consumes: Task 7/9。
- Produces: `WakeSends` で送信ワーカーが起き、`StatusSnapshot.pending_sends` が DB の件数を返す。

- [x] **Step 1: Write the failing tests**

`ipc_server.rs`:

```rust
#[test]
fn wake_sends_wakes_the_send_worker_without_touching_sqlite() {
    let (_dir, endpoint, control) = test_control();
    let mut seen = 0;
    let request = std::thread::spawn(move || vrc_photon_ipc::client::request(&endpoint, Request::WakeSends, Duration::from_secs(1)).unwrap());
    assert!(control.sends.wait(&mut seen, Duration::from_secs(1)));
    assert!(matches!(request.join().unwrap(), Response::Ok));
}
```

`lib.rs`:

```rust
#[test]
fn daemon_status_reports_pending_sends() {
    let dir = tempfile::tempdir().unwrap();
    let mut daemon = test_daemon(&dir);
    let photo = ...; // 既存の seed 方法に合わせる
    {
        let db = daemon.db.lock().unwrap();
        db.enqueue_send(photo, SendKind::Photo, SendOrigin::Manual, SendStatus::Pending).unwrap();
    }
    daemon.refresh_status().unwrap();
    assert_eq!(daemon.status.lock().unwrap().pending_sends, 1);
}
```

- [x] **Step 2: Run tests to verify they fail**

Run: `cargo test -p vrc-photon-daemon wake_sends daemon_status_reports_pending_sends -- --nocapture`
Expected: FAIL（`sends` フィールド / `pending_sends` 未定義）

- [x] **Step 3: Implement wiring**

- `Daemon` に `sends: Arc<JobSignal>` を追加し、`open_inner` で `Arc::new(JobSignal::default())`。
- `DaemonStatus` に `pub pending_sends: i64` を追加。
- `refresh_status` で `db.count_pending_sends()?` を代入。
- `status_snapshot` で `pending_sends: status.pending_sends`。
- `start_ipc` の `IpcControl` に `sends: self.sends.clone()`。
- `start_workers`:
  - `send_handle: Option<JoinHandle<()>>` を `Daemon` に追加。
  - `if self.send_handle.is_none()` の内側で
    `thread::Builder::new().name("discord-send").spawn(move || worker_loop(db, running, sends, events, Arc::new(UreqTransport::new())))`。
    既存の compression worker と同様、二重起動しない。
  - `recover_interrupted_sends` は Task 9 の `worker_loop` が起動時に呼ぶ。
    ここでは重ねて呼ばない。
- `stop_workers` / `shutdown` で `self.sends.notify()`、`send_handle` を join。
- `IpcControl` に `sends: Arc<JobSignal>` を追加し、`test_control` も更新。
- `Request::WakeSends => { control.sends.notify(); Response::Ok }`。
- `Event::SendProgress` は Task 9 の `worker_loop` が publish する。

- [x] **Step 4: Run tests**

Run: `cargo test -p vrc-photon-daemon -- --nocapture`
Expected: PASS

- [x] **Step 5: Commit**

```bash
git add crates/daemon/src/lib.rs crates/daemon/src/ipc_server.rs
git commit -m "feat(daemon): run and wake the Discord send worker"
```

---

### Task 11: UI の状態・設定アクション・プロファイル選択

**Files:**
- Modify: `crates/ui/src/app.rs`（`pub(crate) mod discord;`、App フィールド、`new`、`integrations_discord_motion`）
- Create: `crates/ui/src/app/discord.rs`
- Modify: `crates/ui/src/app/compression.rs`（`set_discord_profile`）
- Modify: `crates/ui/src/app/daemon.rs`（`send_wake_sends`）
- Test: `crates/ui/src/app/discord.rs` の tests

**Interfaces:**
- Consumes: Task 1/3 の core、Task 7 の IPC。
- Produces:
  - `App.discord_url: Option<String>`
  - `App.discord_url_editing: bool`
  - `App.discord_url_input: Entity<InputState>`
  - `App.discord_destination_input: Entity<InputState>`
  - `App._discord_destination_subscription: Subscription`
  - `App.discord_url_error: Option<SharedString>`
  - `App.send_history: Vec<SendHistoryEntry>`
  - `App.send_waiting_count: i64`
  - `App.pending_test_send: Option<i64>`
  - `App.integrations_discord_motion: ToggleMotion`
  - `pub(crate) enum DiscordSendGuardError { Disabled, MissingWebhook }`
  - `pub(crate) fn discord_send_guard(settings: &AppSettings, webhook_present: bool) -> Result<(), DiscordSendGuardError>`
  - `pub(crate) fn normalized_destination(raw: &str) -> String`
  - `pub(crate) fn webhook_error_key(error: WebhookUrlError) -> &'static str`
  - `pub(crate) fn App::send_wake_sends(&self, cx: &mut Context<Self>)`
  - `pub(crate) fn App::set_discord_enabled(&mut self, enabled: bool, cx: &mut Context<Self>)`
  - `pub(crate) fn App::begin_discord_url_edit(&mut self, window: &mut Window, cx: &mut Context<Self>)`
  - `pub(crate) fn App::save_discord_url(&mut self, cx: &mut Context<Self>)`
  - `pub(crate) fn App::clear_discord_url(&mut self, cx: &mut Context<Self>)`
  - `pub(crate) fn App::save_discord_destination(&mut self, cx: &mut Context<Self>)`
  - `pub(crate) fn App::set_discord_send_original(&mut self, value: bool, cx: &mut Context<Self>)`
  - `pub(crate) fn App::set_discord_include_players(&mut self, value: bool, cx: &mut Context<Self>)`
  - `pub(crate) fn App::reload_send_history(&mut self, cx: &mut Context<Self>)`
  - `pub(crate) fn App::send_test_message(&mut self, cx: &mut Context<Self>)`
  - `App::set_clipboard_profile` と同じ形の `set_discord_profile`（`crates/ui/src/app/compression.rs`）
  - `retry_send` / `cancel_send` / `clear_send_history` は Task 13 で実装する
    （Task 11 では追加しない）。

- [x] **Step 1: Write the failing helper tests**

`crates/ui/src/app/discord.rs`:

```rust
#[test]
fn webhook_mask_and_validation_feed_the_editor() {
    assert!(validate_webhook_url("https://discord.com/api/webhooks/1/t").is_ok());
    assert!(!mask_webhook_url("https://discord.com/api/webhooks/1/secret").contains("secret"));
}

#[test]
fn save_destination_falls_back_to_discord_for_blank_input() {
    assert_eq!(normalized_destination("   "), "Discord");
    assert_eq!(normalized_destination(" My server "), "My server");
}

#[test]
fn discord_send_guard_requires_enabled_and_webhook() {
    let mut settings = AppSettings::default();
    assert_eq!(discord_send_guard(&settings, false), Err(DiscordSendGuardError::Disabled));
    settings.discord_enabled = true;
    assert_eq!(discord_send_guard(&settings, false), Err(DiscordSendGuardError::MissingWebhook));
    assert_eq!(discord_send_guard(&settings, true), Ok(()));
}

#[test]
fn webhook_errors_map_to_catalog_keys() {
    assert_eq!(webhook_error_key(WebhookUrlError::NotHttps), "integrations.discord.webhook.invalid_url");
    assert_eq!(webhook_error_key(WebhookUrlError::HostNotAllowed), "integrations.discord.webhook.invalid_host");
    assert_eq!(webhook_error_key(WebhookUrlError::InvalidPath), "integrations.discord.webhook.invalid_path");
}
```

- [x] **Step 2: Run tests to verify they fail**

Run: `cargo test -p vrc-photon-ui discord_ -- --nocapture`
Expected: FAIL（モジュール未定義）

- [x] **Step 3: Implement state and actions**

- `app.rs` にフィールドを追加。`new` で:
  - `discord_url = load_discord_webhook(&db).ok().flatten()`
  - `discord_url_input = cx.new(|cx| InputState::new(window, cx).placeholder(tr!("integrations.discord.webhook.placeholder")))`
  - `discord_destination_input = cx.new(|cx| InputState::new(window, cx)
    .placeholder(tr!("integrations.discord.destination.placeholder"))
    .default_value(initial_settings.discord_destination.clone()))`
  - `_discord_destination_subscription = cx.subscribe(&discord_destination_input, ...)` で
    `InputEvent::PressEnter | InputEvent::Blur` に `save_discord_destination` を呼ぶ
  - `send_history = db.send_history(100).unwrap_or_default()`
  - `send_waiting_count = db.count_waiting_sends().unwrap_or(0)`
  - `integrations_discord_motion = ToggleMotion::settled(if initial_settings.discord_enabled { 0.0 } else { 1.0 })`
  - `update_input_placeholders` の配列に `(&self.discord_url_input, "integrations.discord.webhook.placeholder")`
    と `(&self.discord_destination_input, "integrations.discord.destination.placeholder")` を追加する。
- `reload_aux` で `reload_send_history` を呼び、`settings.discord_enabled` の変化に応じて motion を `settled` にする。
- `app/discord.rs` の実装:
  - `set_discord_enabled`: `settings.discord_enabled` を保存。オフなら motion を 1.0 へ（アニメーションは `ToggleMotion` の既存パターン）。オンにしただけでは auto mode は変えない。`cx.notify()`。
  - `begin_discord_url_edit`: `discord_url_input.set_value("", window, cx)`、
    `discord_url_error=None`、`discord_url_editing=true`。
  - `save_discord_url`: `discord_url_input.read(cx).value()` を trim → `validate_webhook_url`。
    成功なら core の `save_discord_webhook(&db, url)`、`discord_url` 更新、`editing=false`、トースト。
    失敗なら `discord_url_error` に翻訳済みメッセージ。
  - `clear_discord_url`: core の `clear_discord_webhook(&db)`、`discord_url=None`、トースト。
  - `save_discord_destination`: trim して空なら `"Discord"`、保存、入力欄を正規化。
  - `set_discord_send_original` / `set_discord_include_players`: 保存して notify。
  - `set_discord_profile`: `profiles` を走査し、`enabled && id == profile_id` の1件だけ `discord=true`。`db.save_profile`。`set_clipboard_profile` と同じ。
  - `reload_send_history`: `db.send_history(100)` と `db.count_waiting_sends()`。
    `pending_test_send` があれば該当行の状態を見て、terminal ならトーストして
    `None` にする。
  - `send_test_message`: `db.enqueue_test_send()`、`pending_test_send=Some(id)`、`reload_send_history`、`send_wake_sends`、トースト。
- `send_wake_sends` は `self.wake_daemon(IpcRequest::WakeSends, cx)`。
- i18n: Task 11 のコードが `App::new` と URL 保存・テスト送信で使う
  `integrations.discord.webhook.placeholder` / `destination.placeholder` /
  `webhook.invalid_url` / `webhook.invalid_host` / `webhook.invalid_path` /
  `test.queued` / `test.success` / `test.failed` を ja/en に追加する。
  Task 12 以降はカード固有の残り（source / players / profile 等）を追加する。
  未使用キーがあっても `english_catalog_is_complete` は通る。

- [x] **Step 4: Run tests**

Run: `cargo test -p vrc-photon-ui discord_ -- --nocapture`
Expected: PASS

- [x] **Step 5: Commit**

```bash
git add crates/ui/src/app.rs crates/ui/src/app/discord.rs crates/ui/src/app/compression.rs crates/ui/src/app/daemon.rs
git commit -m "feat(ui): add Discord integration state and actions"
```

---

### Task 12: Discord 連携カード

**Files:**
- Create: `crates/ui/src/ui/discord.rs`
- Modify: `crates/ui/src/ui/mod.rs`
- Modify: `crates/ui/src/ui/integrations.rs`（カードを先頭に追加）
- Modify: `crates/i18n/locales/ja.yml` / `en.yml`
- Test: `crates/ui/src/ui/discord.rs` の tests

**Interfaces:**
- Consumes: Task 11 の App フィールドとアクション、`glass_panel`, `Switch`, `Input`, `manual_option_row`。
- Produces: `App::discord_card(&mut self, window, cx) -> AnyElement`

- [x] **Step 1: Write the failing label/format tests**

```rust
#[test]
fn webhook_state_label_distinguishes_missing_and_configured() {
    assert_eq!(webhook_state_label(None), tr!("integrations.discord.webhook.missing"));
    assert!(webhook_state_label(Some("https://discord.com/api/webhooks/1/t")).contains("••••"));
}

#[test]
fn profile_selection_label_requires_a_choice_when_missing() {
    assert_eq!(discord_profile_label(None), tr!("integrations.discord.profile.none"));
    assert_eq!(discord_profile_label(Some("discord用webp")), "discord用webp");
}
```

- [x] **Step 2: Run tests to verify they fail**

Run: `cargo test -p vrc-photon-ui webhook_state_label -- --nocapture`
Expected: FAIL（未定義）

- [x] **Step 3: Implement the card**

`ui/discord.rs` に `impl App { pub(crate) fn discord_card(...) -> AnyElement }` を追加。
`integrations_screen` の先頭に `.child(self.discord_card(window, cx))`。
`integrations.rs` の `collapsible_body` は `pub(crate)` にして `ui/discord.rs` から
使えるようにする（本体は移動しない）。

カードの構成:
- `glass_panel` ヘッダー: タイトル、説明、`Switch`（`settings.discord_enabled` → `set_discord_enabled`）。
- `collapsible_body("integrations-discord-body", self.integrations_discord_motion, DISCORD_BODY_MAX, ...)`。
- 本文:
  - 警告文（enabled 時）。
  - Webhook URL:
    - `discord_url.is_none() && !editing`: 「URL を追加」ボタン。
    - `!editing`: マスク表示 + 「変更」「削除」。
    - `editing`: `Input::new(&self.discord_url_input)`, 「保存」「キャンセル」、`discord_url_error`。
  - 送信先名: `Input::new(&self.discord_destination_input)`、Enter/Blur で
    `save_discord_destination`。保存後の表示が空欄のままにならないよう、
    正規化値（空なら `"Discord"`）を入力欄へ反映する。`InputState::set_value` は
    `&mut Window` を要求するため、保存ボタンの listener など window のある経路で
    反映するか、カード側で保存済み値を別ラベル表示する。
  - 「テスト送信」ボタン（enabled + URL ありで有効）。
  - 「原画を優先」スイッチ + 20 MiB の説明。
  - 「プレイヤー名を含める」スイッチ + 既定オフの説明。
  - 連携用プロファイル: `profiles` を `manual_option_row` で単一選択。`set_discord_profile`。
  - 自動送信セクションは Phase 2（Task 18）までプレースホルダーを置かない。

`DISCORD_BODY_MAX` は本文の高さに合わせて 900 程度（Phase 2 の自動送信セクション
を含めてもクリップしない値）。プロファイル一覧は `max_h(px(160.))` でスクロール。

i18n キー（ja/en）:
`integrations.discord.title`, `hint`, `warning`,
`webhook.add`, `change`, `remove`, `save`, `cancel`, `placeholder`,
`missing`, `invalid_url`, `invalid_host`, `invalid_path`,
`destination.label`, `destination.placeholder`,
`test.button`, `test.queued`, `test.success`, `test.failed`,
`source.original`, `source.original_hint`,
`players.label`, `players.hint`,
`profile.label`, `profile.hint`, `profile.none`.

- [x] **Step 4: Run tests and i18n checks**

Run: `cargo test -p vrc-photon-ui webhook_state_label discord_profile_label -- --nocapture && cargo test -p vrc-photon-i18n`
Expected: PASS

- [x] **Step 5: Commit**

```bash
git add crates/ui/src/ui/discord.rs crates/ui/src/ui/mod.rs crates/ui/src/ui/integrations.rs crates/i18n/locales/ja.yml crates/i18n/locales/en.yml
git commit -m "feat(ui): add the Discord integration card"
```

---

### Task 13: 送信履歴カードと IPC 反映

**Files:**
- Modify: `crates/ui/src/ui/discord.rs`（`send_history_card`）
- Modify: `crates/ui/src/ui/integrations.rs`（履歴カードを末尾に追加）
- Modify: `crates/ui/src/app/lifecycle.rs`（`SendProgress`, `reload_aux`）
- Modify: `crates/i18n/locales/ja.yml` / `en.yml`
- Test: `crates/ui/src/ui/discord.rs` と `crates/ui/src/app/discord.rs` の tests

**Interfaces:**
- Consumes: `SendHistoryEntry`, `SendStatus`, `SendSource`, `SendErrorCode`。
- Produces:
  - `pub(crate) fn send_history_card(&mut self, window, cx) -> AnyElement`
  - `send_status_label`, `send_source_label`, `send_error_label`, `discord_message_url`
  - `App::retry_send`, `App::cancel_send`, `App::clear_send_history`

- [x] **Step 1: Write the failing tests**

```rust
#[test]
fn history_helpers_describe_status_and_source() {
    assert_eq!(send_status_label(SendStatus::Sent), tr!("send.status.sent"));
    assert_eq!(send_source_label(SendSource::Derivative), tr!("integrations.discord.source.derivative"));
    assert_eq!(send_error_label(Some("http_401")), tr!("send.error.http_401"));
    assert_eq!(send_error_label(None), "");
}

#[test]
fn message_url_is_built_only_with_guild_and_channel() {
    assert_eq!(discord_message_url(Some("9"), Some("7"), "42").as_deref(), Some("https://discord.com/channels/9/7/42"));
    assert_eq!(discord_message_url(None, Some("7"), "42"), None);
}
```

- [x] **Step 2: Run tests to verify they fail**

Run: `cargo test -p vrc-photon-ui history_helpers -- --nocapture`
Expected: FAIL（未定義）

- [x] **Step 3: Implement history**

- `send_history_card`: `glass_panel` に最新 100 件。空なら空状態。
  行は `h_flex` で状態バッジ + 写真名/テスト + ワールド + 送信先 + 日時 +
  送信元 + メッセージ ID/リンク + エラー。
  - メッセージリンクは `open_path(Path::new(url))`。
  - failed 行に「再送」→ `App::retry_send`。
  - waiting/pending/failed 行に「取消」→ `App::cancel_send`。
- `integrations_screen` の末尾に履歴カード。
- `app/discord.rs` に `retry_send`, `cancel_send`, `clear_send_history` を追加。
  - `retry_send`: `db.retry_send(id)` が true なら `send_wake_sends` + トースト。
  - `cancel_send`: `db.cancel_send(id)` が true ならトースト。processing は警告。
  - `clear_send_history`: 確認ダイアログ → `db.clear_send_history()` → reload。
- `lifecycle.rs`:
  - `IpcEvent::SendProgress { .. } => { self.reload_send_history(cx); }`
  - `reload_aux` で `reload_send_history`。
- `reload_send_history` の test-send 判定で `Cancelled` を `Failed` と同じ
  「Webhook を確認」トーストにしない。キャンセルは静かに `pending_test_send` を
  クリアする（必要なら `integrations.discord.test.cancelled` を追加）。
- i18n: `integrations.discord.history.title`, `empty`, `clear`, `retry`, `cancel`,
  `message_link`, `test_label`, `send.status.*`, `send.source.*`, `send.error.*`。
- 削除警告の既存 `send` 行は `deletion_job_summary` の変更で自動的に実データになる。
  `crates/ui/src/app/photos.rs` のテストに send 件数を入れたケースを追加する:

```rust
#[test]
fn deletion_warning_lines_list_send_counts() {
    let summary = DeletionJobSummary {
        compression: JobStatusCounts::default(),
        send: JobStatusCounts { waiting: 1, pending: 2, processing: 0, failed: 1 },
        affected_photos: 3,
    };
    let lines = deletion_warning_lines(&summary);
    assert!(lines.iter().any(|line| line.contains("送信") && line.contains("失敗 1")));
}
```

- [x] **Step 4: Run tests**

Run: `cargo test -p vrc-photon-ui history_helpers deletion_warning_lines -- --nocapture && cargo test -p vrc-photon-i18n`
Expected: PASS

- [x] **Step 5: Commit**

```bash
git add crates/ui/src/ui/discord.rs crates/ui/src/ui/integrations.rs crates/ui/src/app/lifecycle.rs crates/ui/src/app/discord.rs crates/ui/src/app/photos.rs crates/i18n/locales/ja.yml crates/i18n/locales/en.yml
git commit -m "feat(ui): show Discord send history and live progress"
```

---

### Task 14: 手動送信ボタン

**Files:**
- Modify: `crates/ui/src/ui/library.rs`（inspector actions / `detail_body`）
- Modify: `crates/ui/src/app/discord.rs`（`enqueue_photo_send`）
- Modify: `crates/i18n/locales/ja.yml` / `en.yml`
- Test: `crates/ui/src/app/discord.rs` の tests

**Interfaces:**
- Consumes: Task 11 の `discord_send_guard`、`send_wake_sends`。
- Produces: `App::enqueue_photo_send(&mut self, photo_id: i64, cx: &mut Context<Self>) -> bool`
  （`true` は guard 失敗で連携タブへ遷移したことを表す。inspector は無視し、
  ビューアは `true` のとき `close_quick_look` してオーバーレイを閉じる）

- [x] **Step 1: Write the failing guard test**

```rust
#[test]
fn manual_send_reports_queued_and_duplicate_states() {
    assert_eq!(manual_send_toast(Ok(true)), tr!("photos.action.discord_queued"));
    assert_eq!(manual_send_toast(Ok(false)), tr!("photos.action.discord_waiting"));
    assert_eq!(manual_send_toast(Err(DiscordSendGuardError::Disabled)), tr!("photos.action.discord_disabled"));
}
```

- [x] **Step 2: Run test to verify it fails**

Run: `cargo test -p vrc-photon-ui manual_send_reports -- --nocapture`
Expected: FAIL

- [x] **Step 3: Implement**

`enqueue_photo_send`:
1. `discord_send_guard(&settings, discord_url.is_some())` → エラーなら警告トースト後、
   `self.switch_screen(Screen::Integrations, cx)` で連携タブへ誘導し、`true` を返す。
2. `self.db.photo_by_id(photo_id)` が `None` / `Deleting` / `Missing` ならエラー。
3. `self.db.enqueue_send(photo_id, SendKind::Photo, SendOrigin::Manual, SendStatus::Pending)`。
4. `Some(_)` なら `reload_send_history` + `send_wake_sends` + 成功トースト。`None` なら「送信待ちです」。
5. `cx.notify()`。`false` を返す。

`library.rs`:
- inspector の `actions` に「Discord へ送る」ボタンを追加（`open-viewer` と圧縮の間）。
- `detail_body` の `compress_button` の後に追加。
- 既存の `Button::new("discord-send")` / `Button::new("viewer-discord-send")` を
  `.small()` で使う。新しいフォーカスハンドルは追加しない（`app.rs` を触らない）。
- ビューア側は `if this.enqueue_photo_send(id, cx) { this.close_quick_look(window, cx); }`
  とし、guard 失敗で連携タブへ遷移したときにオーバーレイを閉じる。
- i18n: `photos.action.discord_send`, `discord_queued`, `discord_waiting`,
  `discord_disabled`, `discord_missing_webhook`, `discord_deleting`, `discord_error`。

- [x] **Step 4: Run tests**

Run: `cargo test -p vrc-photon-ui manual_send_reports -- --nocapture && cargo test -p vrc-photon-i18n`
Expected: PASS

- [x] **Step 5: Commit**

```bash
git add crates/ui/src/ui/library.rs crates/ui/src/app/discord.rs crates/i18n/locales/ja.yml crates/i18n/locales/en.yml
git commit -m "feat(ui): add manual Discord send actions"
```

---

### Task 15: Phase 1 の検証と PR

**Files:** なし（検証のみ）

- [x] **Step 1: Run the full local verification**

Run:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Expected: すべて成功。

- [x] **Step 2: Check the diff for secrets and accidental docs**

Run: `git diff --stat origin/main...HEAD && git diff origin/main...HEAD | rg -n "discord.com/api/webhooks/[0-9]+/[A-Za-z0-9_-]{20,}" || true`
Expected: 実トークンが diff に含まれない。docs 変更がない（設計・計画は未コミットのまま）。

- [x] **Step 3: Push and open PR 1**

```bash
git push -u origin feat/142-discord-integration
gh pr create --title "feat(integrations): Discord Webhook への手動送信と outbox" \
  --body "Phase 1 of #142. Adds the Discord integration card, durable send outbox, daemon worker, manual send, history, and deletion integration. Auto detection lands in a follow-up PR." \
  --label enhancement
```

- [x] **Step 4: Monitor CI**

Follow the `ci-watch` skill. Do not merge until green.

---

## Phase 2（PR 2）

> PR 1 が merge されたら `git fetch origin && git merge origin/main`（競合があれば
> 解消）してから Phase 2 を開始する。Phase 2 の PR は同じブランチから作り、
> Phase 1 のコミットが差分に再表示されないことを確認する。

### Task 16: 自動送信の enqueue と待機キャンセル

**Files:**
- Modify: `crates/daemon/src/discord.rs`（`enqueue_auto_sends`）
- Modify: `crates/core/src/db.rs`（`cancel_waiting_auto_sends` は Task 3 で実装済み）
- Test: `crates/daemon/src/discord.rs` の tests

**Interfaces:**
- Consumes: Task 1 の `DiscordAutoMode`, `discord_auto_since`、Task 2 の `enqueue_send`。
- Produces: `pub(crate) fn enqueue_auto_sends(db: &Db, photo_ids: &[i64]) -> Result<usize>`

- [x] **Step 1: Write the failing tests**

```rust
fn seed_photo_at(db: &Db, captured_at_utc: i64, name: &str) -> i64 {
    let root = db.add_root("/discord-photos", "reference").unwrap();
    db.upsert_photo(&NewPhoto {
        root_id: root,
        rel_path: name.to_string(),
        abs_path: format!("/discord-photos/{name}"),
        file_name: name.to_string(),
        captured_at_utc,
        captured_at_local: "2026-11-02 21:34:56".to_string(),
        tz_offset_sec: 0,
        width: 16,
        height: 10,
        file_size: 10,
        mtime: captured_at_utc,
        blake3: None,
    }).unwrap()
}

#[test]
fn auto_enqueue_respects_mode_webhook_and_auto_since() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("library.db")).unwrap();
    let old = seed_photo_at(&db, 100, "old.png");
    let new = seed_photo_at(&db, 300, "new.png");
    let mut settings = AppSettings::load(&db).unwrap();
    settings.discord_enabled = true;
    settings.discord_auto_mode = DiscordAutoMode::Immediate;
    settings.discord_auto_since = Some(200);
    settings.save(&db).unwrap();
    save_discord_webhook(&db, "https://discord.com/api/webhooks/1/t").unwrap();
    assert_eq!(enqueue_auto_sends(&db, &[old, new]).unwrap(), 1);
    let history = db.send_history(10).unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].entry.photo_id, Some(new));
    assert_eq!(history[0].entry.origin, SendOrigin::Auto);
    assert_eq!(history[0].entry.status, SendStatus::Pending);
}

#[test]
fn auto_enqueue_does_nothing_when_disabled_or_url_missing_or_mode_off() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("library.db")).unwrap();
    let photo = seed_photo_at(&db, 300, "a.png");
    let mut settings = AppSettings::load(&db).unwrap();
    settings.discord_auto_mode = DiscordAutoMode::Immediate;
    settings.discord_auto_since = Some(200);
    settings.save(&db).unwrap();
    save_discord_webhook(&db, "https://discord.com/api/webhooks/1/t").unwrap();
    // enabled=false
    assert_eq!(enqueue_auto_sends(&db, &[photo]).unwrap(), 0);
    // URL なし
    settings.discord_enabled = true;
    settings.save(&db).unwrap();
    clear_discord_webhook(&db).unwrap();
    assert_eq!(enqueue_auto_sends(&db, &[photo]).unwrap(), 0);
    // mode=Off
    save_discord_webhook(&db, "https://discord.com/api/webhooks/1/t").unwrap();
    settings.discord_auto_mode = DiscordAutoMode::Off;
    settings.save(&db).unwrap();
    assert_eq!(enqueue_auto_sends(&db, &[photo]).unwrap(), 0);
}

#[test]
fn waiting_mode_enqueues_waiting_and_release_flips_to_pending() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("library.db")).unwrap();
    let photo = seed_photo_at(&db, 300, "a.png");
    let mut settings = AppSettings::load(&db).unwrap();
    settings.discord_enabled = true;
    settings.discord_auto_mode = DiscordAutoMode::Waiting;
    settings.discord_auto_since = Some(200);
    settings.save(&db).unwrap();
    save_discord_webhook(&db, "https://discord.com/api/webhooks/1/t").unwrap();
    assert_eq!(enqueue_auto_sends(&db, &[photo]).unwrap(), 1);
    let entry = db.send_history(10).unwrap().remove(0).entry;
    assert_eq!(entry.status, SendStatus::Waiting);
    assert_eq!(db.release_waiting_sends().unwrap(), 1);
    assert_eq!(db.send_entry_by_id(entry.id).unwrap().unwrap().status, SendStatus::Pending);
}

#[test]
fn auto_off_cancels_only_waiting_auto_rows() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("library.db")).unwrap();
    let a = seed_photo_at(&db, 300, "a.png");
    let b = seed_photo_at(&db, 300, "b.png");
    let c = seed_photo_at(&db, 300, "c.png");
    let waiting_a = db.enqueue_send(a, SendKind::Photo, SendOrigin::Auto, SendStatus::Waiting).unwrap().unwrap();
    let waiting_b = db.enqueue_send(b, SendKind::Photo, SendOrigin::Auto, SendStatus::Waiting).unwrap().unwrap();
    let pending_c = db.enqueue_send(c, SendKind::Photo, SendOrigin::Auto, SendStatus::Pending).unwrap().unwrap();
    assert_eq!(db.cancel_waiting_auto_sends().unwrap(), 2);
    assert_eq!(db.send_entry_by_id(waiting_a).unwrap().unwrap().status, SendStatus::Cancelled);
    assert_eq!(db.send_entry_by_id(waiting_b).unwrap().unwrap().status, SendStatus::Cancelled);
    assert_eq!(db.send_entry_by_id(pending_c).unwrap().unwrap().status, SendStatus::Pending);
}
```

- [x] **Step 2: Run tests to verify they fail**

Run: `cargo test -p vrc-photon-daemon auto_enqueue -- --nocapture`
Expected: FAIL（`enqueue_auto_sends` がスタブ）

- [x] **Step 3: Implement**

`enqueue_auto_sends`:
1. `AppSettings::load(db)` と `load_discord_webhook(db)`。
2. `!discord_enabled` / URL なし / URL 不正 / `discord_auto_mode == Off` → `Ok(0)`。
   URL は `validate_webhook_url` で検証する。
3. `Some(since) = discord_auto_since` がなければ `Ok(0)`。
4. 各 `photo_id`:
   - `db.photo_by_id(id)` が `Some`、`captured_at_utc >= since`、`status != Deleting`。
   - `status = Immediate ? Pending : Waiting`。
   - `db.enqueue_send(id, SendKind::Photo, SendOrigin::Auto, status)`。
5. 件数を返す。`Immediate` かつ 1 件以上なら呼び出し側で `sends.notify()`。

- [x] **Step 4: Run tests**

Run: `cargo test -p vrc-photon-daemon auto_enqueue -- --nocapture`
Expected: PASS

- [x] **Step 5: Commit**

```bash
git add crates/daemon/src/discord.rs
git commit -m "feat(daemon): enqueue automatic Discord sends"
```

---

### Task 17: 検出後の自動 enqueue 配線

**Files:**
- Modify: `crates/daemon/src/lib.rs`（`index_dirty_paths`, `scan_roots_inner`）
- Test: `crates/daemon/src/lib.rs` の tests

**Interfaces:**
- Consumes: Task 16。
- Produces: 新規写真の紐付け後に `enqueue_auto_sends` が呼ばれ、`self.sends.notify()` される。

- [x] **Step 1: Write the failing test**

```rust
#[test]
fn auto_send_runs_after_linking_and_skips_old_photos() {
    let dir = tempfile::tempdir().unwrap();
    let mut daemon = test_daemon(&dir);
    let log_dir = dir.path().join("logs");
    std::fs::write(
        log_dir.join("output_log_2026-10-21_21-00-00.txt"),
        concat!(
            "2026.10.21 21:23:12 Debug      -  [Behaviour] Joining wrld_test:12345~region(jp)\n",
            "2026.10.21 21:23:13 Debug      -  [Behaviour] Joining or Creating Room: Test World\n",
        ),
    )
    .unwrap();
    write_photo(&original_path(&dir));
    daemon.scan_photos().unwrap();

    let (captured, _) = {
        let db = Db::open(dir.path().join("library.db")).unwrap();
        let photo = db.photo_by_path(&original_path(&dir).to_string_lossy()).unwrap().unwrap();
        (photo.captured_at_utc, photo.id)
    };
    save_daemon_settings(&daemon, |settings| {
        settings.discord_enabled = true;
        settings.discord_auto_mode = DiscordAutoMode::Immediate;
        settings.discord_auto_since = Some(captured + 1);
    });
    {
        let db = daemon.db.lock().unwrap();
        save_discord_webhook(&db, "https://discord.com/api/webhooks/1/t").unwrap();
    }

    let second = dir
        .path()
        .join("photos/VRChat_2026-10-21_21-25-00.000_4x3.png");
    write_photo(&second);
    daemon.dirty_paths.push(second.clone());
    daemon.index_dirty_paths().unwrap();

    let db = Db::open(dir.path().join("library.db")).unwrap();
    let history = db.send_history(10).unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].entry.origin, SendOrigin::Auto);
    assert_eq!(history[0].entry.status, SendStatus::Pending);
    assert_eq!(history[0].photo_name.as_deref(), Some("VRChat_2026-10-21_21-25-00.000_4x3.png"));
    let second_photo = db.photo_by_path(&second.to_string_lossy()).unwrap().unwrap();
    assert_eq!(
        db.photo_context(second_photo.id).unwrap().unwrap().world_name.as_deref(),
        Some("Test World")
    );
}
```

- [x] **Step 2: Run test to verify it fails**

Run: `cargo test -p vrc-photon-daemon auto_send_runs_after_linking -- --nocapture`
Expected: FAIL（呼び出されない）

- [x] **Step 3: Implement**

- `index_dirty_paths`: `self.link_unlinked()?;` の後に
  `let enqueued = enqueue_auto_sends(&db, &new_ids)?;` を呼ぶ。
  DB ロックは `link_unlinked` の後に取り直す（`enqueue_auto_sends` が内部で
  `db.photo_by_id` / `enqueue_send` を呼ぶため）。1 件以上なら `self.sends.notify()`。
- `scan_roots_inner`: `if report.modified > 0 { self.link_unlinked()?; }` の後に
  `report.new_ids` を渡して同じ処理を行う。`initial_scan` の分岐は不要
  （`discord_auto_since` が既存写真を除外する）。
- どちらのパスでも、`enqueue_auto_sends` は DB から最新設定を読むため、
  `self.settings` の 2 秒キャッシュに依存しない。

- [x] **Step 4: Run tests**

Run: `cargo test -p vrc-photon-daemon auto_send_runs_after_linking -- --nocapture`
Expected: PASS。`cargo test -p vrc-photon-daemon` 全体も PASS。

- [x] **Step 5: Commit**

```bash
git add crates/daemon/src/lib.rs
git commit -m "feat(daemon): auto-enqueue Discord sends after linking"
```

---

### Task 18: 自動モード UI と初回同意

**Files:**
- Modify: `crates/ui/src/ui/discord.rs`（自動送信セクション）
- Modify: `crates/ui/src/app/discord.rs`（`set_discord_auto_mode`, consent）
- Modify: `crates/i18n/locales/ja.yml` / `en.yml`
- Test: `crates/ui/src/app/discord.rs`

**Interfaces:**
- Consumes: `DiscordAutoMode`, `discord_consent_at`, `discord_auto_since`, `WindowExt::open_alert_dialog`。
- Produces:
  - `pub(crate) fn needs_discord_consent(settings: &AppSettings) -> bool`
  - `pub(crate) fn apply_discord_auto_mode(settings: &mut AppSettings, mode: DiscordAutoMode, now: i64, was_off: bool)`
  - `App::set_discord_auto_mode(&mut self, mode: DiscordAutoMode, window: &mut Window, cx: &mut Context<Self>)`
  - `App::confirm_discord_auto_mode(&mut self, mode: DiscordAutoMode, cx: &mut Context<Self>)`

- [x] **Step 1: Write the failing tests**

```rust
#[test]
fn enabling_auto_requires_consent_only_the_first_time() {
    let mut settings = AppSettings::default();
    assert!(needs_discord_consent(&settings));
    settings.discord_consent_at = Some(1);
    assert!(!needs_discord_consent(&settings));
}

#[test]
fn enabling_auto_sets_auto_since_only_when_leaving_off() {
    let mut settings = AppSettings::default();
    let now = 1234;
    apply_discord_auto_mode(&mut settings, DiscordAutoMode::Immediate, now, true);
    assert_eq!(settings.discord_auto_since, Some(now));
    assert_eq!(settings.discord_auto_mode, DiscordAutoMode::Immediate);
    apply_discord_auto_mode(&mut settings, DiscordAutoMode::Waiting, 5678, false);
    assert_eq!(settings.discord_auto_since, Some(now)); // 即時→待機ではリセットしない
}
```

- [x] **Step 2: Run tests to verify they fail**

Run: `cargo test -p vrc-photon-ui enabling_auto -- --nocapture`
Expected: FAIL

- [x] **Step 3: Implement**

- `ui/discord.rs` の本文に自動送信セクション:
  - 3 つの選択行（オフ／即時／待機）。`manual_option_row` を使う。
  - 注意書き: 非公開インスタンスも対象、初回同意。
- `app/discord.rs`:
  - `set_discord_auto_mode`:
    - `mode == Off`: `settings.discord_auto_mode = Off` を保存、`db.cancel_waiting_auto_sends()`、
      `reload_send_history`、`cx.notify()`。
    - `mode != Off && needs_discord_consent`: `window.open_alert_dialog` で同意を求め、
      OK で `confirm_discord_auto_mode(mode)`。キャンセルは何もしない。
    - 同意済み: `apply_discord_auto_mode(&mut settings, mode, now, was_off)` を保存。
  - `confirm_discord_auto_mode`: `discord_consent_at = Some(now)` と
    `apply_discord_auto_mode` を保存。
- i18n: `integrations.discord.auto.title`, `hint`, `off`, `immediate`, `waiting`,
  `consent.title`, `consent.body`, `consent.ok`, `warning.private_instances`。

- [x] **Step 4: Run tests**

Run: `cargo test -p vrc-photon-ui enabling_auto -- --nocapture && cargo test -p vrc-photon-i18n`
Expected: PASS

- [x] **Step 5: Commit**

```bash
git add crates/ui/src/ui/discord.rs crates/ui/src/app/discord.rs crates/i18n/locales/ja.yml crates/i18n/locales/en.yml
git commit -m "feat(ui): add Discord auto-send mode and consent"
```

---

### Task 19: 待機キュー一括送信

**Files:**
- Modify: `crates/ui/src/ui/discord.rs`（履歴カードのボタン）
- Modify: `crates/ui/src/app/discord.rs`（`release_waiting_sends`）
- Modify: `crates/ui/src/ui/dashboard.rs`（任意: ダッシュボード導線）
- Modify: `crates/i18n/locales/ja.yml` / `en.yml`
- Test: `crates/ui/src/app/discord.rs`

**Interfaces:**
- Consumes: `Db::release_waiting_sends`。
- Produces: `App::release_waiting_sends(&mut self, cx)`

- [x] **Step 1: Write the failing test**

```rust
#[test]
fn waiting_release_result_messages() {
    assert_eq!(waiting_release_toast(0), tr!("integrations.discord.history.release_none"));
    assert_eq!(waiting_release_toast(3), tr_args!("integrations.discord.history.release_done", count = 3));
}
```

- [x] **Step 2: Run test to verify it fails**

Run: `cargo test -p vrc-photon-ui waiting_release_result -- --nocapture`
Expected: FAIL

- [x] **Step 3: Implement**

- `release_waiting_sends`: `db.release_waiting_sends()` → 0 なら警告、>0 なら
  `send_wake_sends` + トースト、`reload_send_history`。
- 履歴カードのヘッダーに「待機をすべて送信」を置き、`send_waiting_count == 0`
  なら無効化。
- ダッシュボードの圧縮待機カードの隣に送信待機の件数とボタンを追加する
  （既存の `pending_sends` を使う。レイアウトが複雑なら履歴カードだけに置く）。
- i18n: `release`, `release_none`, `release_done`, `release_hint`。

- [x] **Step 4: Run tests**

Run: `cargo test -p vrc-photon-ui waiting_release_result -- --nocapture && cargo test -p vrc-photon-i18n`
Expected: PASS

- [x] **Step 5: Commit**

```bash
git add crates/ui/src/ui/discord.rs crates/ui/src/app/discord.rs crates/ui/src/ui/dashboard.rs crates/i18n/locales/ja.yml crates/i18n/locales/en.yml
git commit -m "feat(ui): release waiting Discord sends"
```

---

### Task 20: Phase 2 の検証と PR

**Files:** なし（検証のみ）

- [x] **Step 1: Run the full local verification**

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Expected: すべて成功。

- [x] **Step 2: Manual smoke check（可能なら）**

- `cargo run -p vrc-photon-ui -- --demo` で連携タブを開き、カードの折りたたみ、
  URL マスク、プロファイル選択、履歴の空状態を確認する。
- デーモンを別プロセスで起動し、URL 未設定で送信履歴が増えないことを確認する。
- 実際の Webhook は使わない（テスト送信はユーザーの明示操作時のみ）。

- [x] **Step 3: Push and open PR 2**

```bash
git push
gh pr create --title "feat(integrations): Discord 自動送信と待機キュー" \
  --body "Phase 2 of #142. Adds auto-send modes, first-use consent, auto-enqueue after linking, and the waiting queue release." \
  --label enhancement
```

- [x] **Step 4: Monitor CI**

Follow the `ci-watch` skill. Do not merge until green.

---

## docs PR（PR 3）

### Task 21: 設計・ADR・機能ドキュメント

**Files:**
- Modify: `docs/features.md`
- Modify: `docs/data-model.md`
- Modify: `docs/architecture.md`
- Modify: `docs/security-privacy.md`
- Modify: `docs/ui.md`
- Modify: `README.md`
- Modify: `docs/HANDOFF.md`
- Create: `docs/adr/0017-discord-webhook.md`
- Add: `docs/superpowers/specs/2026-11-02-discord-integration-design.md`
- Add: `docs/superpowers/plans/2026-11-02-discord-integration.md`
- Test: `cargo test -p vrc-photon-i18n`（docs のみの PR でも既存テストが通る）

- [x] **Step 1: Update the feature and data docs**

- `docs/features.md` に「## 12. Discord 連携」を追加:
  - 既定オフ、機能オフ時の無送信、Webhook URL のマスク、テスト送信。
  - 原画優先 / 20 MiB / プロファイルフォールバック / derivative フォールバック。
  - 手動送信、履歴、メッセージ ID、再送、取消、履歴上限 500。
  - 自動送信のオフ／即時／待機、`discord_auto_since`、初回同意、注意書き。
  - 429 / バックオフ / 4xx / interrupted。
  - 削除要求時の送信キャンセルとドレイン。
- `docs/data-model.md`:
  - スキーマ v11、`send_outbox` の全列・インデックス。
  - `app_settings` の `discord_webhook_url` キー。
  - `AppSettings` の Discord フィールド、`CompressionProfile.discord`。
  - マイグレーション表に 11 を追加。
- `docs/architecture.md`:
  - プロセス役割に送信ワーカー。
  - データフロー 4.9「Discord 送信」。
  - 並行性の表に `discord-send`。
  - IPC の `WakeSends` / `SendProgress` / `pending_sends`。
- `docs/security-privacy.md`:
  - §1 を「既定では外部送信しない。Discord 連携を明示的に有効化した場合だけ」に変更。
  - §2 の保存データに outbox と Webhook URL を追加。
  - §7 に Discord の送信内容、ホスト制限、URL をログに出さないこと、
    DB バックアップに URL が含まれることを追加。
- `docs/ui.md`: 連携タブの Discord カード、履歴、手動送信。
- `README.md`: 機能一覧とプライバシー方針。
- `docs/HANDOFF.md`: 現在の状態に Discord 連携を追加。
- `docs/adr/0017-discord-webhook.md`: Context / Decision / Consequences / Alternatives
  （Webhook 方式、daemon ワーカー、outbox、削除連携、URL を独立キーに保存）。

- [x] **Step 2: Check the plan checkboxes**

`docs/superpowers/plans/2026-11-02-discord-integration.md` の完了したタスクを
`- [x]` に更新する（AGENTS §4）。

- [x] **Step 3: Verify**

```bash
cargo fmt --all -- --check
cargo test -p vrc-photon-i18n
git diff --check
```

Expected: PASS。

- [ ] **Step 4: Commit and open the docs PR**

```bash
git add docs README.md
git commit -m "docs: document the Discord Webhook integration"
git push
gh pr create --title "docs: Discord Webhook 連携の設計と機能仕様" \
  --body "Documents #142: design, ADR, data model, privacy, and UI. No code changes." \
  --label enhancement
```

- [ ] **Step 5: Monitor CI**

Follow the `ci-watch` skill. Do not merge until green.

---

## 実装完了後の後片付け

- [ ] PR 1 / PR 2 / PR 3 がすべて green で merge されたことを確認する。
- [ ] `main` に切り替えて pull する。
- [ ] ローカル・リモートの `feat/142-discord-integration` を削除する。
- [ ] worktree `../vrc-photo-manager-142` を `git worktree remove` し、`git worktree prune`。
- [ ] Issue #142 のチェックボックスを更新し、完了コメントを残す。
