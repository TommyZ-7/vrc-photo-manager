# Issue #97 取り込み直後の自動圧縮 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [x]`) syntax for tracking.

**Goal:** デーモン稼働中・停止中のどちらで取り込んでも、新規写真を自動圧縮キューへ重複なく登録し、取り込み結果に登録件数を表示する。

**Architecture:** 取り込みは引き続きデーモンの `ScanRoot` が索引する。起動時は保留中のコマンドを初回フルスキャンより先に処理し、初回スキャン判定を固定してバックログ設定を守る。自動圧縮専用の DB enqueue を冪等化し、`ScanFinished` に登録件数を載せて UI が取り込み結果へ反映する。

**Tech Stack:** Rust stable (edition 2024), SQLite (rusqlite), gpui-kit, vrc-photo-core / daemon / ipc / ui

**Spec:** `docs/superpowers/specs/2026-10-31-issue-97-import-auto-compress-design.md`

## Global Constraints

- Rust stable / edition 2024。既存のモジュール構成と命名に合わせる。
- ローカル検証は `cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`。
- `main` へ直接 push しない。作業ブランチは `fix/97-import-auto-compress`、PR ラベルは `bug`。
- コード変更と docs 変更は別 PR。docs 更新はコード PR マージ後に行う。
- `auto_compress_backlog = false` の既存バックログ抑制を壊さない。
- 手動圧縮の重複ジョブ許容（同じ写真・プロファイルを意図的に複数回積める）を変更しない。
- UI が写真を先に索引してジョブを登録する方式へ戻さない。索引と enqueue はデーモンが所有する。

## Review Focus

- 空ライブラリ + デーモン停止中の取り込み: 取り込んだ写真だけが enqueue され、他ルートの既存写真はバックログとして enqueue されないこと。
- デーモン稼働中に watcher が先に索引した場合: `new_ids` が空でも二重ジョブを作らず、ログには watcher 経由の登録が残ること。
- 連続した取り込み / 取り込み結果を閉じた後: 自動圧縮件数が次の結果へ漏れないこと。
- 自動圧縮OFF、または有効な自動プロファイルが無い場合: ジョブも表示も増えないこと（OFF は呼び出し側の既存ガード、プロファイル無しは `enqueue_auto_jobs` のテストで固定）。
- `auto_compress_immediate = false`: `pending` ではなく `waiting` で積まれ、ワーカーに取得されないこと。

---

### Task 1: 自動圧縮 enqueue を DB レベルで冪等化する

**Files:**
- Modify: `crates/core/src/db.rs:2157-2214`（`enqueue_job_for_existing_profile_with_status` の近く）
- Test: `crates/core/src/db.rs` の `#[cfg(test)] mod tests`

**Interfaces:**
- Consumes: 既存の `Db`, `QueueStatus`, `CompressionProfile`
- Produces:
  ```rust
  pub fn enqueue_auto_job_for_existing_profile_with_status(
      &self,
      photo_id: i64,
      profile_id: &str,
      policy: &str,
      status: QueueStatus,
  ) -> Result<bool>
  ```
  同じ `photo_id` + `profile_id` のジョブが1件でも存在すれば `false` を返し、挿入しない。

- [x] **Step 1: 失敗するテストを書く**

`crates/core/src/db.rs` の tests に追加する。

```rust
#[test]
fn auto_job_enqueue_skips_an_existing_photo_profile_job() {
    let db = Db::open_in_memory().unwrap();
    let id = seed_photo(&db);
    db.save_profile(&profile("p", "P")).unwrap();

    assert!(
        db.enqueue_auto_job_for_existing_profile_with_status(
            id,
            "p",
            "keep",
            QueueStatus::Pending,
        )
        .unwrap()
    );
    assert!(
        !db.enqueue_auto_job_for_existing_profile_with_status(
            id,
            "p",
            "keep",
            QueueStatus::Waiting,
        )
        .unwrap()
    );
    assert_eq!(db.list_jobs(10).unwrap().len(), 1);
}
```

- [x] **Step 2: テストが失敗することを確認する**

Run: `cargo test -p vrc-photo-core auto_job_enqueue_skips_an_existing_photo_profile_job`

Expected: FAIL（`enqueue_auto_job_for_existing_profile_with_status` が未定義）

- [x] **Step 3: 最小実装を追加する**

`enqueue_job_for_existing_profile_with_options` と同じ挿入条件に、次の条件を加えた専用メソッドを追加する。

```sql
AND NOT EXISTS (
    SELECT 1 FROM compression_jobs
    WHERE photo_id = ?2 AND profile_id = ?3
)
```

ステータスは問わない（`completed` / `failed` / `cancelled` も重複扱い）。自動圧縮は「その写真・プロファイルにつき一度だけ」とし、再索引で再実行しない。手動圧縮用の既存メソッドは変更しない。

- [x] **Step 4: テストが通ることを確認する**

Run: `cargo test -p vrc-photo-core auto_job_enqueue_skips_an_existing_photo_profile_job`

Expected: PASS

- [x] **Step 5: コミット**

```bash
git add crates/core/src/db.rs
git commit -m "fix(core): deduplicate automatic compression jobs"
```

---

### Task 2: 起動時に取り込みスキャンを先に処理し、enqueue 件数をイベントに載せる

**Files:**
- Modify: `crates/ipc/src/protocol.rs:115-121`（`Event::ScanFinished`）
- Modify: `crates/daemon/src/lib.rs:392-430`（`poll_commands`）
- Modify: `crates/daemon/src/lib.rs:480-537`（`run_command`, `run_once`）
- Modify: `crates/daemon/src/lib.rs:566-593`（`run`）
- Modify: `crates/daemon/src/lib.rs:811-867`（scan 入口と `finish_scan`）
- Modify: `crates/daemon/src/lib.rs:869-994`（`scan_roots_inner`）
- Modify: `crates/daemon/src/lib.rs:1648-1674`（`enqueue_auto_jobs`）
- Test: `crates/daemon/src/lib.rs` の tests

**Interfaces:**
- Consumes: Task 1 の `Db::enqueue_auto_job_for_existing_profile_with_status`
- Produces:
  ```rust
  fn startup_scan(&mut self) -> Result<()>
  fn poll_commands_with_initial(&mut self, initial_scan: Option<bool>) -> Result<()>
  fn run_command_with_initial(
      &mut self,
      command: &DaemonCommand,
      initial_scan: Option<bool>,
  ) -> Result<()>
  fn scan_photos_with_initial(&mut self, initial_scan: Option<bool>) -> Result<ScanReport>
  fn scan_roots_with_initial(
      &mut self,
      roots: Vec<PathBuf>,
      full_scan: bool,
      initial_scan: Option<bool>,
  ) -> Result<ScanReport>
  fn enqueue_auto_jobs(
      db: &Db,
      new_ids: &[i64],
      keep_original: bool,
      immediate: bool,
  ) -> Result<usize>
  ```
  `scan_roots_inner` は `Result<(ScanReport, usize)>` を返し、第2要素は登録した自動圧縮ジョブ数。`Event::ScanFinished` に `auto_enqueued: usize` が加わる。

- [x] **Step 1: 失敗するテストを書く**

`crates/daemon/src/lib.rs` の tests に追加する。

```rust
#[test]
fn startup_scan_enqueues_imported_photos_before_backlog() {
    let dir = tempfile::tempdir().unwrap();
    let backlog_root = dir.path().join("photos");
    let import_root = dir.path().join("imported");
    let log_dir = dir.path().join("logs");
    std::fs::create_dir_all(&backlog_root).unwrap();
    std::fs::create_dir_all(&import_root).unwrap();
    std::fs::create_dir_all(&log_dir).unwrap();
    let backlog_photo = backlog_root.join("VRChat_2026-10-21_21-14-03.221_4x3.png");
    let imported_photo = import_root.join("VRChat_2026-10-21_21-24-00.000_4x3.png");
    write_photo(&backlog_photo);
    write_photo(&imported_photo);

    let db_path = dir.path().join("library.db");
    add_auto_profile(&db_path);
    let mut daemon =
        Daemon::open(&db_path, Some(backlog_root.clone()), Some(log_dir)).unwrap();
    daemon.set_thumbnail_dir(dir.path().join("thumbnails"));
    daemon.set_ipc_endpoint(None);
    save_daemon_settings(&daemon, |settings| {
        settings.auto_compress = true;
        settings.auto_compress_backlog = false;
        settings.auto_compress_immediate = true;
        settings.photo_roots = vec![
            backlog_root.to_string_lossy().to_string(),
            import_root.to_string_lossy().to_string(),
        ];
    });
    {
        let db = Db::open(&db_path).unwrap();
        db.request_daemon_scan_root(import_root.to_str().unwrap())
            .unwrap();
    }

    daemon.startup_scan().unwrap();

    let db = Db::open(&db_path).unwrap();
    assert_eq!(db.count_photos().unwrap(), 2);
    let imported_id = db
        .photo_by_path(imported_photo.to_str().unwrap())
        .unwrap()
        .unwrap()
        .id;
    let backlog_id = db
        .photo_by_path(backlog_photo.to_str().unwrap())
        .unwrap()
        .unwrap()
        .id;
    let jobs = db.list_jobs(10).unwrap();
    assert_eq!(
        jobs.iter().filter(|job| job.photo_id == imported_id).count(),
        1
    );
    assert_eq!(
        jobs.iter().filter(|job| job.photo_id == backlog_id).count(),
        0
    );
    assert_eq!(jobs[0].status, QueueStatus::Pending);
}

#[test]
fn scan_root_finished_event_reports_auto_enqueued_jobs() {
    let dir = tempfile::tempdir().unwrap();
    let photo_dir = dir.path().join("photos");
    let log_dir = dir.path().join("logs");
    std::fs::create_dir_all(&photo_dir).unwrap();
    std::fs::create_dir_all(&log_dir).unwrap();
    write_photo(&photo_dir.join("VRChat_2026-10-21_21-24-00.000_4x3.png"));

    let db_path = dir.path().join("library.db");
    add_auto_profile(&db_path);
    let mut daemon = Daemon::open(&db_path, Some(photo_dir.clone()), Some(log_dir)).unwrap();
    daemon.set_thumbnail_dir(dir.path().join("thumbnails"));
    daemon.set_ipc_endpoint(None);
    let events = daemon.events.subscribe();
    daemon.scan_root_photos(photo_dir).unwrap();

    let mut finished = None;
    while let Ok(event) = events.try_recv() {
        if matches!(event, Event::ScanFinished { .. }) {
            finished = Some(event);
        }
    }
    match finished {
        Some(Event::ScanFinished { auto_enqueued, .. }) => assert_eq!(auto_enqueued, 1),
        _ => panic!("scan did not finish"),
    }
}

#[test]
fn enqueue_auto_jobs_skips_profiles_that_are_not_auto_enabled() {
    let dir = tempfile::tempdir().unwrap();
    let mut daemon = test_daemon(&dir);
    write_photo(
        &dir.path()
            .join("photos/VRChat_2026-10-21_21-24-00.000_4x3.png"),
    );
    daemon.run_once().unwrap();

    let db = daemon.db.lock().unwrap();
    let photo_id = db
        .photo_by_path(
            dir.path()
                .join("photos/VRChat_2026-10-21_21-24-00.000_4x3.png")
                .to_str()
                .unwrap(),
        )
        .unwrap()
        .unwrap()
        .id;
    db.save_profile(&CompressionProfile {
        id: "manual-only".to_string(),
        name: "Manual only".to_string(),
        enabled: true,
        auto_enabled: false,
        ..CompressionProfile::default()
    })
    .unwrap();

    assert_eq!(enqueue_auto_jobs(&db, &[photo_id], true, true).unwrap(), 0);
    assert!(db.list_jobs(10).unwrap().is_empty());
}
```

- [x] **Step 2: テストが失敗することを確認する**

Run:

```bash
cargo test -p vrc-photo-daemon startup_scan_enqueues_imported_photos_before_backlog
cargo test -p vrc-photo-daemon scan_root_finished_event_reports_auto_enqueued_jobs
cargo test -p vrc-photo-daemon enqueue_auto_jobs_skips_profiles_that_are_not_auto_enabled
```

Expected: FAIL（`startup_scan` が未定義、またはジョブ / `auto_enqueued` が0件）

- [x] **Step 3: 起動処理を実装する**

`run` の `ensure_roots` + `scan_photos` を `startup_scan` に置き換える。

```rust
fn startup_scan(&mut self) -> Result<()> {
    self.ensure_roots()?;
    if self.stopping() {
        return Ok(());
    }
    let initial_scan = {
        let db = self.db.lock().unwrap();
        db.count_photos()? == 0
    };
    self.poll_commands_with_initial(Some(initial_scan))?;
    if self.stopping() {
        return Ok(());
    }
    self.scan_photos_with_initial(Some(initial_scan))?;
    Ok(())
}
```

`poll_commands_with_initial` は `initial_scan == Some(true)` のとき、`ScanRoot` を先頭へ安定ソートしてから実行する。`Rescan` は `scan_photos_with_initial(initial_scan)` を使う。`run_once` も最初に `initial_scan` を捕捉し、`poll_commands_with_initial(Some(initial_scan))` と `scan_photos_with_initial(Some(initial_scan))` を使う。

`scan_photos` / `scan_root_photos` / `scan_roots` は既存シグネチャを保ち、内部で `*_with_initial(..., None)` に委譲する。`scan_roots_inner` は `initial_scan.unwrap_or(full_scan && db.count_photos()? == 0)` を使い、末尾で `(report, auto_enqueued)` を返す。

- [x] **Step 4: 自動圧縮 enqueue を冪等化し、件数をイベントへ載せる**

`enqueue_auto_jobs` を `Result<usize>` に変更し、Task 1 の専用メソッドを使う。登録数を数えて返し、次のログを出す。

```rust
tracing::info!(
    photos = new_ids.len(),
    profiles = auto_profiles.len(),
    jobs = enqueued,
    "auto compression jobs enqueued"
);
```

`index_dirty_paths` と `scan_roots_inner` の両呼び出しを新しい戻り値に合わせる。`scan_roots_with_initial` は `(ScanReport, auto_enqueued)` を分解し、`finish_scan(&result, auto_enqueued)` を呼ぶ。`finish_scan` は `Event::ScanFinished { ..., auto_enqueued }` を発行する。

`Event::ScanFinished` には `#[serde(default)] auto_enqueued: usize` を追加する。これで旧デーモン / 新 UI の組み合わせでもデシリアライズできる。

- [x] **Step 5: テストが通ることを確認する**

Run:

```bash
cargo test -p vrc-photo-daemon startup_scan_enqueues_imported_photos_before_backlog
cargo test -p vrc-photo-daemon scan_root_finished_event_reports_auto_enqueued_jobs
cargo test -p vrc-photo-daemon enqueue_auto_jobs_skips_profiles_that_are_not_auto_enabled
```

Expected: PASS

- [x] **Step 6: 既存の自動圧縮テストを確認する**

Run: `cargo test -p vrc-photo-daemon auto_`

Expected: PASS（`auto_jobs_use_replace_policy_and_waiting_status`, `auto_compress_holds_waiting_jobs_until_released`, `end_to_end_auto_compress` を含む）

- [x] **Step 7: コミット**

```bash
git add crates/ipc/src/protocol.rs crates/daemon/src/lib.rs
git commit -m "fix(daemon): enqueue imported photos before the startup backlog scan"
```

---

### Task 3: 取り込み結果に「自動圧縮 N」を表示する

**Files:**
- Modify: `crates/ui/src/app.rs:226` 付近と `:652` 付近（App フィールド）
- Modify: `crates/ui/src/app/import.rs:401-423` と `:486-503`
- Modify: `crates/ui/src/app/lifecycle.rs:165-169`
- Modify: `crates/ui/src/ui/notifications.rs:440-468` と `:516`
- Test: `crates/ui/src/ui/notifications.rs` tests

**Interfaces:**
- Consumes: Task 2 の `Event::ScanFinished { auto_enqueued, .. }`
- Produces: `App` に `import_scan_pending: bool` と `import_auto_compress_enqueued: usize`。

- [x] **Step 1: 失敗するテストを書く**

`crates/ui/src/ui/notifications.rs` の tests に追加する。

```rust
#[test]
fn auto_compress_badge_is_hidden_for_zero_and_shows_the_count() {
    assert_eq!(auto_compress_badge_label(0), None);
    assert_eq!(
        auto_compress_badge_label(3).as_deref(),
        Some("自動圧縮 3")
    );
}
```

- [x] **Step 2: テストが失敗することを確認する**

Run: `cargo test -p vrc-photo-ui auto_compress_badge_is_hidden_for_zero_and_shows_the_count`

Expected: FAIL（`auto_compress_badge_label` が未定義）

- [x] **Step 3: UI の取り込み待ち状態を実装する**

`App` に以下を追加し、初期化する。

```rust
pub(crate) import_scan_pending: bool,
pub(crate) import_auto_compress_enqueued: usize,
```

`index_import_destination` の `request_import_scan` 成功時に `self.import_scan_pending = true;` を設定する。

`finish_import_run` では、レポートを構築した直後・`if import_should_index` の直前で `self.import_auto_compress_enqueued = 0;` を設定する。イベント受信後にリセットすると件数を消す競合になるため、スキャン要求より前にリセットする。`import_should_index` が false のときも、新しいレポートに古い件数が残らないよう同じ位置でリセットする。

`handle_ipc_event` の `ScanFinished` で次を行う。

```rust
IpcEvent::ScanFinished { auto_enqueued, .. } => {
    if self.import_scan_pending {
        self.import_scan_pending = false;
        self.import_auto_compress_enqueued = self
            .import_auto_compress_enqueued
            .saturating_add(auto_enqueued);
    }
    self.scan_progress = None;
    self.refresh_live(cx);
    self.refresh_library_after_scan(cx);
}
```

- [x] **Step 4: バッジを表示する**

`crates/ui/src/ui/notifications.rs` に純粋関数を追加する。

```rust
fn auto_compress_badge_label(count: usize) -> Option<String> {
    (count > 0).then(|| format!("自動圧縮 {count}"))
}
```

`import_notification` のバッジ列で、`self.import_auto_compress_enqueued` が 0 より大きいときに `self.badge(&label, p.green, p.green_soft)` を追加する。取り込み結果を閉じるとき（`notification-import-dismiss`）は `self.import_auto_compress_enqueued = 0;` と `self.import_scan_pending = false;` もリセットする。

- [x] **Step 5: テストが通ることを確認する**

Run: `cargo test -p vrc-photo-ui auto_compress_badge_is_hidden_for_zero_and_shows_the_count`

Expected: PASS

- [x] **Step 6: コミット**

```bash
git add crates/ui/src/app.rs crates/ui/src/app/import.rs \
  crates/ui/src/app/lifecycle.rs crates/ui/src/ui/notifications.rs
git commit -m "feat(ui): show auto compression jobs added by an import"
```

---

### Task 4: 全体検証とコード PR

**Files:**
- 変更なし（検証と PR のみ）

- [x] **Step 1: フォーマットを確認する**

Run: `cargo fmt --all -- --check`

Expected: 差分なし

- [x] **Step 2: Clippy を実行する**

Run: `cargo clippy --workspace --all-targets -- -D warnings`

Expected: warning / error なし

- [x] **Step 3: ワークスペース全体のテストを実行する**

Run: `cargo test --workspace`

Expected: 全テスト PASS

- [x] **Step 4: PR を作成する**

```bash
gh pr create --base main --head fix/97-import-auto-compress \
  --title "fix: 取り込み直後の写真を自動圧縮キューに載せる (#97)" \
  --body "Closes #97

- 起動時に保留中の ScanRoot を初回フルスキャンより先に処理
- 初回スキャン判定を固定し、auto_compress_backlog=false のバックログ抑制を維持
- 自動圧縮 enqueue を DB レベルで冪等化
- 取り込み結果に「自動圧縮 N」バッジを追加
- ScanFinished に auto_enqueued を追加し、デーモンログにも記録"
gh pr edit --add-label bug
```

- [x] **Step 5: CI を監視する**

`ci-watch` スキルに従い、最新の `CI` run を再解決しながら監視する。成功後にマージする。

---

### Task 5: docs 更新（コード PR マージ後の別 PR）

**Files:**
- Modify: `docs/features.md:162`
- Modify: `docs/testing.md:80-81`

- [x] **Step 1: 別 worktree / ブランチを main から作る**

```bash
git worktree add ../vrc-photo-manager-97-docs -b docs/97-import-auto-compress
```

- [x] **Step 2: 機能仕様を更新する**

`docs/features.md` の取り込み章にある既知ギャップの記述を、次の趣旨に置き換える。

- デーモン稼働中・停止中のどちらでも、取り込み直後の新規写真は自動圧縮に載る。
- ただし `auto_compress_backlog = false` のとき、初回スキャンで見つかった既存写真は従来どおり圧縮しない。
- 自動圧縮ジョブは同じ写真・プロファイルにつき重複登録されない。
- 取り込み結果に「自動圧縮 N」を表示する。

- [x] **Step 3: テスト仕様を更新する**

`docs/testing.md` の daemon テスト一覧に以下を追加する。

- 起動時に保留中の `scan_root` が初回フルスキャンより先に処理され、取り込み写真だけが自動圧縮に載ること。
- 同じ写真・プロファイルの自動圧縮 enqueue が重複しないこと。
- `ScanFinished` が `auto_enqueued` を運ぶこと。

UI テスト一覧に「自動圧縮件数バッジが0件で非表示、1件以上で件数表示」を追加する。

- [x] **Step 4: 検証と PR**

```bash
cargo fmt --all -- --check
git add docs/features.md docs/testing.md
git commit -m "docs: describe import auto compression coverage"
gh pr create --base main --head docs/97-import-auto-compress \
  --title "docs: 取り込み直後の自動圧縮の仕様とテストを更新 (#97)" \
  --body "Refs #97"
gh pr edit --add-label bug
```

CI green 後にマージし、issue #97 をクローズする。
