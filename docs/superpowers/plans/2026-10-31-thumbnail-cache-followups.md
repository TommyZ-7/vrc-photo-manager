# サムネイルキャッシュ追補 実装計画

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** #187/#189/#190/#191 を解消する。メモリヒットでも最近使用を記録し、UI 生成分を IPC で通知して定期全走査を廃止し、削除失敗と Windows ロックをテストし、削除競合のログを静かにする。

**Architecture:** core が「実際に生成したか」とサイズを返し、削除関数へ失敗を注入できるようにする。IPC に `ThumbnailsGenerated` を追加してプロトコルを v2 に上げ、UI は生成分をバッチ通知する。デーモンは起動時の実測値を起点に追跡バイト数を増減し、上限超過時だけ全走査する。UI のメモリ LRU は保留 ID を描画後にまとめてバックグラウンドで mtime 更新する。

**Tech Stack:** Rust 2024, `std::fs`, `AtomicU64`, `mpsc`, GPUI background executor, `tempfile` in tests.

**Spec:** `docs/superpowers/specs/2026-10-31-thumbnail-cache-followups-design.md`

## Global Constraints

- `thumbnail_cache_mb = 0` は無制限。削除も全走査もしない。
- 削除対象は thumbnail ディレクトリ直下の `<u64>.webp` のみ。元写真、`.tmp`、サブディレクトリ、非数値 `.webp` には触れない。
- UI スレッドでファイル I/O をしない。mtime 更新と通知送信は background executor で行う。
- 新規依存クレートを追加しない。DB スキーマと設定 UI は変更しない。
- IPC は `PROTOCOL_VERSION = 2`。UI とデーモンは同時に更新する。
- 削除はデーモンのサムネイルワーカーで行う。UI は削除しない。
- ログ: 想定内の削除競合は `debug`、回復不能時のみ `warn` 以上。
- コード PR に product docs を混ぜない。docs はマージ後の別 PR。
- コミット接頭辞は `feat:`, `fix:`, `ci:`, `chore:`。

## Review Focus

- **起動時に上限超過している**: 初回スキャンの前に削除が走り、スキャン中も超過が残らない。既存の `startup_prunes_thumbnail_cache_to_limit` と、`run()` の呼び出し順で確認する。
- **上限 0 / 実行中の上限変更**: 0 では全走査せず、有限値へ変えた瞬間に1回だけ走査して削除する。Task 3 の `ThumbnailCacheBudget` テストで固定する。
- **削除失敗（権限・Windows ロック）**: 失敗したファイルを報告に含めず処理を継続し、次回パスで再試行する。Task 1 の注入テストとプラットフォーム別テストで固定する。
- **表示中の削除競合**: 消えたファイルを1回だけ再生成し、回復した場合は `debug`。既存ファイルの破損は再生成せず `warn`。Task 5 のテストで固定する。
- **通知の重複・取りこぼし**: 重複は余分な走査にしかならず、削除は実測値に基づく。取りこぼしは次回起動時の全走査で補正する。Task 3 の budget テストと既存の起動時削除テストで固定する。

---

## File Structure

- `crates/core/src/thumbnail.rs` — 生成状態、サイズ、LRU 削除、失敗注入、mtime 更新。
- `crates/ipc/src/lib.rs`, `crates/ipc/src/protocol.rs` — プロトコル v2 と通知リクエスト。
- `crates/daemon/src/ipc_server.rs` — 通知の受け取りと外部バイト数への加算。
- `crates/daemon/src/lib.rs` — 起動前削除、追跡 budget、定期走査の削除、ワーカー配線。
- `crates/ui/src/state/thumbnail.rs` — メモリ LRU の最近使用キュー、デコード結果、通知バッチ。
- `crates/ui/src/app/photos.rs` — サムネイル要求、mtime 更新の実行、通知送信。
- `crates/ui/src/ui/gallery.rs` — 描画後の保留 ID 回収。
- `crates/ui/src/app.rs`, `crates/ui/src/app/lifecycle.rs` — 通知バッチ状態と再接続時 flush。
- `.github/workflows/ci.yml` — サムネイル関連変更時だけ Windows core テスト。

---

### Task 1: core の生成状態・削除失敗テスト

**Files:**
- Modify: `crates/core/src/thumbnail.rs`
- Test: inline `#[cfg(test)] mod tests` in the same file

**Interfaces:**
- Produces:
  - `pub struct EnsuredThumbnail { pub path: PathBuf, pub created: bool, pub bytes: u64 }`
  - `pub fn ensure_thumbnail_in_with_status(photo_id: i64, source: &Path, cache_dir: &Path) -> Result<Option<EnsuredThumbnail>>`
  - `pub fn ensure_thumbnail_in(photo_id: i64, source: &Path, cache_dir: &Path) -> Result<Option<PathBuf>>`（既存シグネチャのラッパー）
  - `pub const ACCESS_REFRESH_AFTER: Duration`（既存 private 定数を公開）
  - private `fn prune_thumbnails_with<Remove>(cache_dir: &Path, max_bytes: u64, remove: Remove) -> Result<ThumbnailPruneReport> where Remove: FnMut(&Path) -> std::io::Result<()>`
- Consumes: 既存の `collect_thumbnail_entries`、`ThumbnailPruneReport`。

- [ ] **Step 1: 生成状態の失敗テストを書く**

`creates_and_reuses_thumbnail` を拡張し、次の2テストを追加する。

```rust
#[test]
fn ensure_reports_created_bytes_for_a_new_thumbnail() {
    // source.png を作り、ensure_thumbnail_in_with_status を1回呼ぶ
    // created == true、bytes > 0、path.is_file()
}

#[test]
fn ensure_reports_a_cache_hit_without_creation() {
    // 1回生成した後に再度呼ぶ
    // created == false、bytes == 1回目の bytes、path は同じ
}
```

- [ ] **Step 2: 失敗を確認する**

Run: `cargo test -p vrc-photo-core thumbnail::tests`
Expected: `ensure_thumbnail_in_with_status` と `EnsuredThumbnail` が未定義でコンパイル失敗。

- [ ] **Step 3: `EnsuredThumbnail` と `ensure_thumbnail_in_with_status` を実装する**

`ensure_thumbnail_in` の本体を `ensure_thumbnail_in_with_status` に移し、既存の `ensure_thumbnail_in` は `.map(|ensured| ensured.path)` を返すラッパーにする。生成成功時は `created: true`、既存ファイル利用時は `created: false`。`bytes` は既存なら `metadata.len()`、生成なら `encoded.len() as u64`。`ACCESS_REFRESH_AFTER` を `pub` にする。

- [ ] **Step 4: テストが通ることを確認する**

Run: `cargo test -p vrc-photo-core thumbnail::tests`
Expected: PASS。

- [ ] **Step 5: 削除失敗の失敗テストを書く**

`prune_thumbnails_with` を使うテストを追加する。

```rust
#[test]
fn prune_continues_after_a_removal_error_and_retries_on_the_next_pass() {
    // 1000 bytes の 1.webp/2.webp/3.webp を古い順に作る
    // limit 2500、1.webp だけ Err を返す remover で prune
    // removed_files == 1, removed_bytes == 1000, remaining_bytes == 2000
    // 1.webp は残り、2.webp は消え、3.webp は残る
    // 次に全削除成功の remover で再実行 -> 1.webp が消える
}

#[test]
fn prune_reports_removal_failures_without_stopping_other_deletions() {
    // 先頭と末尾の削除を失敗させ、中間だけ消える
    // removed_files と remaining_bytes が実態と一致する
}
```

- [ ] **Step 6: 失敗を確認する**

Run: `cargo test -p vrc-photo-core thumbnail::tests`
Expected: `prune_thumbnails_with` が未定義でコンパイル失敗。

- [ ] **Step 7: `prune_thumbnails_with` を実装する**

`prune_thumbnails_in` は `prune_thumbnails_with(cache_dir, max_bytes, |path| fs::remove_file(path))` を呼ぶ。`remove` の `Err` は `tracing::debug!` を出して継続し、`removed_*` に数えない。成功時だけ `remaining` から引く。

- [ ] **Step 8: プラットフォーム別テストを書く**

```rust
#[cfg(unix)]
#[test]
fn prune_retries_after_directory_permissions_are_restored() {
    // 1000 bytes × 2、limit 500
    // ディレクトリを 0o555 にして prune -> removed_files == 0
    // root などで削除できてしまった場合は早期 return
    // 0o755 に戻して再実行 -> removed_files == 2、remaining_bytes == 0
}

#[cfg(windows)]
#[test]
fn prune_retries_a_file_locked_by_another_handle() {
    // 1000 bytes × 2、limit 500
    // 古い方のファイルを OpenOptionsExt::share_mode(0) で開く
    // 1回目: removed_files == 1、ロック中のファイルは残る
    // ハンドルを drop して2回目: removed_files == 1、残りも消える
}
```

- [ ] **Step 9: テストが通ることを確認する**

Run: `cargo test -p vrc-photo-core thumbnail::tests`
Expected: PASS（Windows 固有テストは Linux では未コンパイル）。

- [ ] **Step 10: コミット**

```bash
git add crates/core/src/thumbnail.rs
git commit -m "feat(core): report thumbnail creation and cover removal failures"
```

---

### Task 2: IPC 通知リクエストと受け口

**Files:**
- Modify: `crates/ipc/src/lib.rs`
- Modify: `crates/ipc/src/protocol.rs`
- Modify: `crates/daemon/src/ipc_server.rs`
- Modify: `crates/daemon/src/lib.rs`（フィールド追加と `start_ipc` の配線のみ）
- Test: inline tests in `protocol.rs` and `ipc_server.rs`

**Interfaces:**
- Produces:
  - `pub const PROTOCOL_VERSION: u32 = 2;`
  - `Request::ThumbnailsGenerated { files: u64, bytes: u64 }`
  - `IpcControl.thumbnail_external_bytes: Arc<AtomicU64>`
  - `Daemon.thumbnail_external_bytes: Arc<AtomicU64>`
- Consumes: なし。

- [ ] **Step 1: プロトコルの失敗テストを書く**

```rust
#[test]
fn thumbnails_generated_round_trips() {
    let value = RequestEnvelope {
        id: 8,
        request: Request::ThumbnailsGenerated { files: 3, bytes: 4096 },
    };
    let json = serde_json::to_string(&value).unwrap();
    assert!(json.contains("\"thumbnails_generated\""));
    assert_eq!(serde_json::from_str::<RequestEnvelope>(&json).unwrap(), value);
}
```

- [ ] **Step 2: 失敗を確認する**

Run: `cargo test -p vrc-photo-ipc protocol::tests::thumbnails_generated_round_trips`
Expected: `ThumbnailsGenerated` が未定義でコンパイル失敗。

- [ ] **Step 3: プロトコルを実装する**

`Request` に `ThumbnailsGenerated { files: u64, bytes: u64 }` を追加し、`PROTOCOL_VERSION` を 2 にする。

- [ ] **Step 4: 受け口の失敗テストを書く**

`ipc_server.rs` の `test_control()` に `thumbnail_external_bytes: Arc::new(AtomicU64::new(0))` を足し、次を追加する。

```rust
#[test]
fn thumbnails_generated_adds_external_bytes() {
    let (_dir, endpoint, control) = test_control();
    let _server = IpcServer::start(endpoint.clone(), control.clone()).unwrap();
    let response = request(
        &endpoint,
        Request::ThumbnailsGenerated { files: 3, bytes: 4096 },
        Duration::from_secs(1),
    )
    .unwrap();
    assert!(matches!(response, Response::Ok));
    assert_eq!(
        control.thumbnail_external_bytes.load(Ordering::Relaxed),
        4096
    );
}
```

- [ ] **Step 5: 失敗を確認する**

Run: `cargo test -p vrc-photo-daemon thumbnails_generated_adds_external_bytes`
Expected: `IpcControl` にフィールドがなくコンパイル失敗。

- [ ] **Step 6: デーモン側の受け口を実装する**

`Daemon` に `thumbnail_external_bytes: Arc<AtomicU64>` を追加し、`Daemon::new` で `Arc::new(AtomicU64::new(0))` を入れる。`start_ipc` の `IpcControl` に clone を渡す。`IpcControl` にフィールドを追加し、ハンドラで `fetch_add(bytes, Ordering::Relaxed)`、`tracing::debug!(files, bytes, "UI reported generated thumbnails")`、`Response::Ok` を返す。`bytes == 0` でも応答は `Ok`。

- [ ] **Step 7: テストが通ることを確認する**

Run: `cargo test -p vrc-photo-ipc && cargo test -p vrc-photo-daemon thumbnails_generated_adds_external_bytes`
Expected: PASS。

- [ ] **Step 8: コミット**

```bash
git add crates/ipc/src/lib.rs crates/ipc/src/protocol.rs crates/daemon/src/ipc_server.rs crates/daemon/src/lib.rs
git commit -m "feat(ipc): accept batched thumbnail generation notices"
```

---

### Task 3: デーモンの増分 budget・定期走査の廃止・起動前削除

**Files:**
- Modify: `crates/daemon/src/lib.rs`
- Test: inline tests in `crates/daemon/src/lib.rs`

**Interfaces:**
- Consumes:
  - `Daemon.thumbnail_external_bytes: Arc<AtomicU64>`（Task 2）
  - `thumbnail::ensure_thumbnail_in_with_status`、`EnsuredThumbnail`、`ThumbnailPruneReport`（Task 1）
- Produces:
  - private `struct ThumbnailCacheBudget { limit_bytes: u64, known_bytes: Option<u64> }`
  - `ThumbnailCacheBudget::new(limit: u64)`, `set_limit(limit: u64)`, `note_generated(bytes: u64)`, `note_external(bytes: u64)`, `note_pruned(report: &thumbnail::ThumbnailPruneReport)`, `needs_prune() -> bool`
  - `fn prune_thumbnail_cache(cache_dir: &Path, limit_bytes: u64) -> Option<thumbnail::ThumbnailPruneReport>`
  - `fn thumbnail_loop(db, running, rx, cache_dir, cache_limit, external_bytes: Arc<AtomicU64>)`

- [ ] **Step 1: budget の失敗テストを書く**

```rust
#[test]
fn thumbnail_budget_prunes_only_above_the_limit() {
    let mut budget = ThumbnailCacheBudget::new(1000);
    assert!(budget.needs_prune()); // 未計測
    budget.note_pruned(&report_with_remaining(1000));
    assert!(!budget.needs_prune());
    budget.note_generated(1);
    assert!(budget.needs_prune());
}

#[test]
fn thumbnail_budget_tracks_generation_external_growth_and_reset() {
    let mut budget = ThumbnailCacheBudget::new(1000);
    budget.note_pruned(&report_with_remaining(500));
    budget.note_generated(200);
    budget.note_external(100);
    assert!(!budget.needs_prune());
    budget.note_generated(300);
    assert!(budget.needs_prune());
    budget.note_pruned(&report_with_remaining(400));
    assert!(!budget.needs_prune());
}

#[test]
fn thumbnail_budget_never_prunes_when_unlimited() {
    let mut budget = ThumbnailCacheBudget::new(0);
    budget.note_generated(10_000_000);
    budget.note_external(10_000_000);
    assert!(!budget.needs_prune());
    budget.note_pruned(&report_with_remaining(0));
    assert!(!budget.needs_prune());
}
```

`report_with_remaining(bytes)` は `ThumbnailPruneReport { remaining_bytes: bytes, ..Default::default() }` を返すテストヘルパー。

- [ ] **Step 2: 失敗を確認する**

Run: `cargo test -p vrc-photo-daemon thumbnail_budget_`
Expected: `ThumbnailCacheBudget` が未定義でコンパイル失敗。

- [ ] **Step 3: budget を実装する**

`known_bytes` が `None` のときは「未計測」として `limit > 0` なら `needs_prune() == true`。`set_limit` は値が変わったときだけ `known_bytes = None`。`note_pruned` は `limit > 0` なら `Some(report.remaining_bytes)`、0 なら `None`。加算は `saturating_add`。

- [ ] **Step 4: budget テストが通ることを確認する**

Run: `cargo test -p vrc-photo-daemon thumbnail_budget_`
Expected: PASS。

- [ ] **Step 5: 外部通知で削除される失敗テストを書く**

```rust
#[test]
fn external_thumbnail_growth_prunes_over_the_limit() {
    // cache に 600_000 bytes × 3、設定 2MB、start_workers
    // 起動時は 1.8MB で上限以下なので3件とも残る
    // daemon.thumbnail_external_bytes.fetch_add(1_000_000, Ordering::Relaxed)
    // 1.webp が消えるまで wait_for（500ms 間隔の取り込み）
    // 3.webp は残る
}
```

- [ ] **Step 6: 失敗を確認する**

Run: `cargo test -p vrc-photo-daemon external_thumbnail_growth_prunes_over_the_limit`
Expected: 外部バイト数が worker に渡っておらず削除されないため FAIL。

- [ ] **Step 7: worker を budget ベースへ書き換える**

- `THUMBNAIL_PRUNE_INTERVAL` を削除する。
- `ThumbnailPruneSchedule` から `interval`、`last_prune`、`periodic_due` を削除し、`new(batch: usize)` にする。
- `prune_thumbnail_cache` は `Option<ThumbnailPruneReport>` を返す。`limit == 0` は `None`（走査しない）。エラーは `warn` して `None`。
- `thumbnail_loop` の起動時: `external_bytes.swap(0, Ordering::Relaxed)` で捨ててから prune し、`budget.note_pruned`。
- ループ先頭で `cache_limit` の変化を検出し、`budget.set_limit` 後に即 prune。
- `Ok(id)` 分岐: `ensure_thumbnail_in_with_status` を使い、`created` のときだけ `budget.note_generated(ensured.bytes)`。`schedule.note_generated()` が true かつ `budget.needs_prune()` のとき prune。
- `Timeout` 分岐: `external_bytes.swap(0)` を `budget.note_external` に渡す。`(schedule.has_pending() || external > 0) && budget.needs_prune()` のとき prune。
- `Disconnected` は従来どおり break。
- `run()` の先頭（`start_ipc()` より前）で `prune_thumbnail_cache(&self.thumbnail_dir, limit)` を呼ぶ。
- `start_workers` は `self.thumbnail_external_bytes.clone()` を worker に渡す。

- [ ] **Step 8: 既存の schedule テストを更新する**

`thumbnail_prune_schedule_fires_when_the_interval_elapses` を削除する。バッチ・アイドル・リセットの3テストは `new(batch)` に合わせる。

- [ ] **Step 9: 全テストが通ることを確認する**

Run: `cargo test -p vrc-photo-daemon thumbnail`
Expected: PASS。

- [ ] **Step 10: コミット**

```bash
git add crates/daemon/src/lib.rs
git commit -m "feat(daemon): track thumbnail cache growth without periodic scans"
```

---

### Task 4: メモリキャッシュの最近使用記録 (#187)

**Files:**
- Modify: `crates/ui/src/state/thumbnail.rs`
- Modify: `crates/ui/src/app/photos.rs`
- Modify: `crates/ui/src/ui/gallery.rs`
- Test: inline tests in `crates/ui/src/state/thumbnail.rs`

**Interfaces:**
- Consumes: `thumbnail::ACCESS_REFRESH_AFTER`（Task 1）
- Produces:
  - `ThumbnailCache::take_access_refreshes(&mut self) -> Vec<i64>`
  - `App::refresh_thumbnail_access(&self, ids: Vec<i64>, cx: &mut Context<Self>)`

- [ ] **Step 1: 失敗テストを書く**

```rust
#[test]
fn thumbnail_cache_queues_one_disk_refresh_per_hour() {
    let mut cache = ThumbnailCache::default();
    cache.insert(1, test_render_image(), 0);
    cache
        .last_disk_refresh
        .insert(1, Instant::now() - thumbnail::ACCESS_REFRESH_AFTER - Duration::from_secs(1));
    assert!(cache.get(1).is_some());
    assert_eq!(cache.take_access_refreshes(), vec![1]);
    assert!(cache.get(1).is_some());
    assert!(cache.take_access_refreshes().is_empty());
}

#[test]
fn thumbnail_cache_insert_cancels_a_pending_disk_refresh() {
    let mut cache = ThumbnailCache::default();
    cache.insert(1, test_render_image(), 0);
    cache
        .last_disk_refresh
        .insert(1, Instant::now() - thumbnail::ACCESS_REFRESH_AFTER - Duration::from_secs(1));
    assert!(cache.get(1).is_some());
    cache.insert(1, test_render_image(), 0);
    assert!(cache.take_access_refreshes().is_empty());
}
```

- [ ] **Step 2: 失敗を確認する**

Run: `cargo test -p vrc-photo-ui thumbnail_cache_queues_one_disk_refresh_per_hour`
Expected: フィールドとメソッドがなくコンパイル失敗。

- [ ] **Step 3: メモリ LRU を実装する**

`ThumbnailCache` に `last_disk_refresh: HashMap<i64, Instant>` と `disk_refresh_due: HashSet<i64>` を追加する。

- `insert`: `last_disk_refresh.insert(id, Instant::now())`、`disk_refresh_due.remove(&id)`。
- `get`: 既存のヒット処理後、`last_disk_refresh.get(&id)` が `None` または `now.duration_since(at) >= thumbnail::ACCESS_REFRESH_AFTER` なら、現在時刻を記録して `disk_refresh_due.insert(id)`。
- `evict`: 両方から victim を削除。
- `take_access_refreshes`: `disk_refresh_due.drain().collect()`。

- [ ] **Step 4: UI テストが通ることを確認する**

Run: `cargo test -p vrc-photo-ui thumbnail_cache_`
Expected: PASS。

- [ ] **Step 5: 描画後の回収とバックグラウンド更新を実装する**

`gallery.rs` のリスト描画クロージャで、行構築後に
`list_shared.thumbnails.borrow_mut().take_access_refreshes()` を呼ぶ。
空でなければ既存の `cx.defer` で `App::refresh_thumbnail_access(ids, cx)` を呼ぶ。

`photos.rs` に次を追加する。

```rust
pub(crate) fn refresh_thumbnail_access(&self, ids: Vec<i64>, cx: &mut Context<Self>) {
    if ids.is_empty() || self.demo_assets.is_some() {
        return;
    }
    cx.spawn(async move |_this, cx| {
        cx.background_executor()
            .spawn(async move {
                for id in ids {
                    if let Some(path) = thumbnail::thumbnail_path(id) {
                        thumbnail::refresh_thumbnail_access(&path);
                    }
                }
            })
            .await;
    })
    .detach();
}
```

- [ ] **Step 6: ワークスペースの UI テストが通ることを確認する**

Run: `cargo test -p vrc-photo-ui`
Expected: PASS。

- [ ] **Step 7: コミット**

```bash
git add crates/ui/src/state/thumbnail.rs crates/ui/src/app/photos.rs crates/ui/src/ui/gallery.rs
git commit -m "fix(ui): refresh thumbnail recency from the memory cache"
```

---

### Task 5: デコード結果の生成サイズと競合ログ (#189/#191)

**Files:**
- Modify: `crates/ui/src/state/thumbnail.rs`
- Modify: `crates/ui/src/app/photos.rs`
- Test: inline tests in `crates/ui/src/state/thumbnail.rs`

**Interfaces:**
- Consumes: `thumbnail::EnsuredThumbnail`（Task 1）
- Produces:
  - `pub(crate) struct DecodedThumbnail { pub(crate) color: u32, pub(crate) image: image::RgbaImage, pub(crate) generated_bytes: Option<u64> }`
  - `decode_thumbnail(...) -> Option<DecodedThumbnail>`（戻り値の型を変更）
- `decode_thumbnail_with` の `Ensure` は
  `Fn(i64, &Path, &Path) -> Result<Option<thumbnail::EnsuredThumbnail>>` に変わる。

- [ ] **Step 1: 生成サイズの失敗テストを書く**

```rust
#[test]
fn decode_thumbnail_reports_bytes_only_when_it_creates_the_file() {
    // source.png から1回目: generated_bytes == Some(> 0)
    // 2回目: generated_bytes == None
}

#[test]
fn decode_thumbnail_does_not_generate_without_a_cache_dir() {
    // cache_dir None、既存 png を cache_path に置く
    // generated_bytes == None
}
```

- [ ] **Step 2: 失敗を確認する**

Run: `cargo test -p vrc-photo-ui decode_thumbnail_reports_bytes_only_when_it_creates_the_file`
Expected: `DecodedThumbnail` が未定義でコンパイル失敗。

- [ ] **Step 3: `DecodedThumbnail` とサイズ返却を実装する**

`decode_thumbnail_file` はログを出さず `Option<(u32, RgbaImage)>` を返す。
`decode_thumbnail_with` は `EnsuredThumbnail` を受け取り、`created` のとき
`generated_bytes = Some(bytes)` を保持する。再生成した場合も同様。
既存ファイルのデコード成功時は `refresh_thumbnail_access` を呼ぶ。
`photos.rs` の `request_thumbnail` は `decoded.color` / `decoded.image` を使う。

- [ ] **Step 4: サイズのテストが通ることを確認する**

Run: `cargo test -p vrc-photo-ui decode_thumbnail`
Expected: PASS。

- [ ] **Step 5: ログ経路の失敗テストを書く**

```rust
#[test]
fn decode_thumbnail_retries_once_after_the_cache_file_disappears() {
    // 既存テストを DecodedThumbnail に合わせる
    // ensure 1回目: created=false で存在しない path
    // ensure 2回目: created=true で regenerated path
    // decode 1回目: None、2回目: Some
    // calls == 2、generated_bytes == Some(10)
}

#[test]
fn decode_thumbnail_does_not_regenerate_an_undecodable_existing_file() {
    // ensure: created=false で existing path を返す（実ファイルあり）
    // decode: 常に None
    // 結果 None、ensure の呼び出し回数 == 1
}
```

- [ ] **Step 6: 失敗を確認する**

Run: `cargo test -p vrc-photo-ui decode_thumbnail_`
Expected: 既存実装は再生成後のサイズを返さないため FAIL。

- [ ] **Step 7: ログレベルを実装する**

- 消えた場合の再生成開始: `tracing::debug!("thumbnail cache entry disappeared during decode; regenerating")`。
- 再生成が `Ok(None)`: `tracing::debug!("thumbnail source disappeared during decode")`。
- 再生成が `Err`: `tracing::warn!("cannot regenerate thumbnail")`。
- 既存ファイルのデコード失敗: `tracing::warn!("cannot decode thumbnail")`。
- 再生成後のデコード失敗: `tracing::warn!("cannot decode regenerated thumbnail")`。
- `cache_dir` なしの読み取り失敗: `tracing::warn!("cannot decode thumbnail")`。

- [ ] **Step 8: UI テストが通ることを確認する**

Run: `cargo test -p vrc-photo-ui`
Expected: PASS。

- [ ] **Step 9: コミット**

```bash
git add crates/ui/src/state/thumbnail.rs crates/ui/src/app/photos.rs
git commit -m "fix(ui): report generated thumbnail sizes and quiet deletion races"
```

---

### Task 6: UI からの生成通知バッチ (#189)

**Files:**
- Modify: `crates/ui/src/state/thumbnail.rs`
- Modify: `crates/ui/src/app.rs`
- Modify: `crates/ui/src/app/photos.rs`
- Modify: `crates/ui/src/app/lifecycle.rs`
- Test: inline tests in `crates/ui/src/state/thumbnail.rs`

**Interfaces:**
- Consumes:
  - `Request::ThumbnailsGenerated { files, bytes }`（Task 2）
  - `DecodedThumbnail.generated_bytes`（Task 5）
  - `App::wake_daemon`（既存）
- Produces:
  - `pub(crate) struct ThumbnailNoticeBatch` with `add(bytes: u64) -> bool` and `take(&mut self) -> Option<(u64, u64)>`
  - `App::flush_thumbnail_notice(&mut self, cx: &mut Context<Self>)`

- [ ] **Step 1: バッチの失敗テストを書く**

```rust
#[test]
fn thumbnail_notice_batch_flushes_at_file_and_byte_thresholds() {
    let mut batch = ThumbnailNoticeBatch::default();
    for _ in 0..(THUMBNAIL_NOTICE_BATCH_FILES - 1) {
        assert!(!batch.add(1));
    }
    assert!(batch.add(1));
    assert_eq!(batch.take(), Some((THUMBNAIL_NOTICE_BATCH_FILES, THUMBNAIL_NOTICE_BATCH_FILES)));
    assert_eq!(batch.take(), None);

    let mut batch = ThumbnailNoticeBatch::default();
    assert!(batch.add(THUMBNAIL_NOTICE_BATCH_BYTES));
    assert_eq!(batch.take(), Some((1, THUMBNAIL_NOTICE_BATCH_BYTES)));
}
```

- [ ] **Step 2: 失敗を確認する**

Run: `cargo test -p vrc-photo-ui thumbnail_notice_batch`
Expected: `ThumbnailNoticeBatch` が未定義でコンパイル失敗。

- [ ] **Step 3: バッチを実装する**

`files: u64` と `bytes: u64` を持ち、`add` で加算して
`files >= 32 || bytes >= 1024 * 1024` を返す。`take` は空なら `None`、あれば値を返して 0 に戻す。

- [ ] **Step 4: バッチテストが通ることを確認する**

Run: `cargo test -p vrc-photo-ui thumbnail_notice_batch`
Expected: PASS。

- [ ] **Step 5: App の配線を実装する**

`app.rs` の `App` に `thumbnail_notice: ThumbnailNoticeBatch` と
`thumbnail_notice_scheduled: bool` を追加し、初期化する。

`photos.rs`:
- `request_thumbnail` の成功時に `decoded.generated_bytes` があれば
  `self.note_generated_thumbnail(bytes, cx)` を呼ぶ。
- `note_generated_thumbnail` はバッチへ追加し、閾値到達なら
  `flush_thumbnail_notice`、未満なら1秒タイマーを1回だけ予約する。
- `flush_thumbnail_notice`:
  ```rust
  if !self.ipc_connected { return; }
  let Some((files, bytes)) = self.thumbnail_notice.take() else { return; };
  self.wake_daemon(IpcRequest::ThumbnailsGenerated { files, bytes }, cx);
  ```
  `photos.rs` に `use vrc_photo_ipc::Request as IpcRequest;` を追加する。
- デモモード（`self.demo_assets.is_some()`）では何もしない。

`lifecycle.rs` の `Incoming::Connected` 分岐で `self.flush_thumbnail_notice(cx)` を呼ぶ。

- [ ] **Step 6: UI テストとビルドを確認する**

Run: `cargo test -p vrc-photo-ui && cargo check --workspace`
Expected: PASS。

- [ ] **Step 7: コミット**

```bash
git add crates/ui/src/state/thumbnail.rs crates/ui/src/app.rs crates/ui/src/app/photos.rs crates/ui/src/app/lifecycle.rs
git commit -m "feat(ui): notify the daemon about generated thumbnails"
```

---

### Task 7: Windows CI の条件付き実行

**Files:**
- Modify: `.github/workflows/ci.yml`

**Interfaces:**
- Consumes: `cargo test -p vrc-photo-core thumbnail::tests`（Task 1 のテスト）
- Produces: `changes` ジョブの `thumbnails` 出力。

- [ ] **Step 1: `changes` ジョブに出力を追加する**

`outputs` に `thumbnails: ${{ steps.check.outputs.thumbnails }}` を追加する。
`check` ステップで:
- 非 PR（`push` / `workflow_dispatch`）は `thumbnails=true`。
- PR は `FILES` に次のいずれかが含まれるとき `true`、それ以外は `false`:
  `crates/core/src/thumbnail.rs`、`crates/ui/src/state/thumbnail.rs`、
  `crates/daemon/src/lib.rs`、`crates/ipc/src/protocol.rs`、
  `.github/workflows/ci.yml`。

- [ ] **Step 2: Windows ジョブにステップを追加する**

`ipc-windows` の最後に:

```yaml
      - name: Test thumbnail cache on Windows
        if: needs.changes.outputs.thumbnails == 'true'
        run: cargo test -p vrc-photo-core thumbnail::tests
```

新しい必須チェックは増やさない（既存 `ipc-windows` 内の条件付きステップ）。

- [ ] **Step 3: YAML の構文を確認する**

Run: `git diff --check && sed -n '18,50p' .github/workflows/ci.yml`
Expected: インデントと出力名が一致し、`git diff --check` が空。

- [ ] **Step 4: コミット**

```bash
git add .github/workflows/ci.yml
git commit -m "ci: run thumbnail tests on Windows when they change"
```

---

### Task 8: 全体検証

**Files:** なし

- [ ] **Step 1: ローカル検証を実行する**

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Expected: すべて PASS。

- [ ] **Step 2: 差分を確認する**

```bash
git diff --stat origin/main...HEAD
git status --short
```

Expected: 意図したファイル群のみ。未追跡ファイルや `opencode.json` を含めない。

- [ ] **Step 3: PR を作成する**

PR 本文に `Closes #187`, `Closes #189`, `Closes #190`, `Closes #191` を記載し、
ラベル `enhancement` と `bug` を付ける。`ci-watch` で CI を監視する。

---

## Post-merge docs task（別 PR）

コードマージ後に docs-only ブランチを切り、次を更新する。

- `docs/architecture.md`: サムネイル削除の節を「起動時＋上限超過時。UI 生成分は IPC 通知で追跡。60秒定期走査なし」に変更。
- `docs/features.md`: LRU の説明から「約60秒ごとの定期削除」を削除し、通知方式を追記。
- `docs/troubleshooting.md`: サムネイル削除・再生成のログレベルの記述を更新。

ラベルは `bug` または `enhancement`（内容に合わせる）。CI は docs-only の fast pass。
