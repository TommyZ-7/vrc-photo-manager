# 大量取り込み時フリーズ解消 実装計画

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 取り込み完了後の索引処理を取り込み先ルートに限定し、既存写真の無変更スキップと UI の revision デバウンスで、ライブラリ総数に比例する停止をなくす。

**Architecture:** core に `daemon_commands.payload`（schema v8）とパス集合の一括参照 API を追加し、scanner はサイズ・mtime が一致する `present` 行を書き込まない。daemon は `ScanRoot(path)` を 1 ルートだけスキャンし、UI は取り込み完了時にそれを要求する。UI の revision 経路は 2 秒 quiet デバウンスを通し、全件 `reload_library` の連続実行を防ぐ。

**Tech Stack:** Rust (edition 2024), rusqlite, gpui-kit, cargo test/clippy.

**Spec:** `docs/superpowers/specs/2026-10-31-import-scan-performance-design.md`

## Global Constraints

- 新規 crate 依存を追加しない。
- DB スキーマは v8。`migrate_v8` は v4〜v7 と同じく `pragma_table_info` で列存在を確認し、再実行可能にする。
- `cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace` を通す。
- ブランチは worktree で `fix/issue-208-import-scan-freeze`。`main` へ直接コミットしない。
- コミットは `feat:` / `fix:` / `perf:` / `docs:` の Conventional Commits。
- docs 更新は code PR に混ぜず、別 PR にする（AGENTS.md §4）。
- スキャンの `missing` 判定・rebind・`new_ids`・`ScanStarted/Progress/Finished` の意味を変えない。
- 手動の全体 `Rescan` は従来どおり全ルートをスキャンする。

## Review Focus

- **同一サイズ・同一 mtime の内容差し替え**: スキップ対象（設計上のトレードオフ）。サイズ・mtime 変更のテストで検出境界を固定する。
- **ネストしたルート**: 同じ `abs_path` が別ルートの行として存在しても、新規 ID を作らずスキップする。Task 2 の `nested_root_scan_...` で固定する。
- **`scan_root` の空 payload**: DB に壊れた行があっても無視し、起動を失敗させない。Task 1 の model テストで固定する。
- **revision デバウンス中の直接リロード**: `switch_screen`・削除・デモ取り込みの直接リロード後に debounce が二重発火しないこと。Task 5 で全経路を `apply_library_snapshot` に寄せる。
- **`missing` 写真の再出現**: サイズ・mtime が同じでも `present` に戻す。Task 2 のテストで固定する。

---

### Task 1: `ScanRoot` コマンドと schema v8

**Files:**
- Modify: `crates/core/src/model.rs:552-576`
- Modify: `crates/core/src/db.rs:193-207, 335-378, 2380-2407`
- Modify: `crates/daemon/src/lib.rs:388-421, 469-484`
- Test: `crates/core/src/model.rs:636`（既存 tests モジュール）
- Test: `crates/core/src/db.rs:2740` 以降（既存 tests モジュール）

**Interfaces:**
- Produces:
  - `pub enum DaemonCommand { Rescan, Relog, Relink, ScanRoot { path: String } }`（`Clone` のみ、`Copy` なし）
  - `DaemonCommand::as_str(&self) -> &'static str` / `payload(&self) -> Option<&str>` / `parse(command: &str, payload: Option<&str>) -> Option<Self>`
  - `Db::request_daemon_scan_root(&self, path: &str) -> Result<bool>`
  - `daemon_commands.payload TEXT`（schema v8）

- [ ] **Step 1: 失敗する model / DB テストを書く**

`crates/core/src/model.rs` の tests に追加:

```rust
#[test]
fn scan_root_command_round_trips_its_payload() {
    let command = DaemonCommand::ScanRoot {
        path: "/photos/imported".to_string(),
    };
    assert_eq!(command.as_str(), "scan_root");
    assert_eq!(command.payload(), Some("/photos/imported"));
    assert_eq!(
        DaemonCommand::parse("scan_root", Some("/photos/imported")),
        Some(command)
    );
    assert_eq!(DaemonCommand::parse("scan_root", None), None);
    assert_eq!(DaemonCommand::parse("scan_root", Some("")), None);
    assert_eq!(DaemonCommand::parse("rescan", None), Some(DaemonCommand::Rescan));
}
```

`crates/core/src/db.rs` の tests に追加:

```rust
#[test]
fn daemon_scan_root_commands_deduplicate_per_path() {
    let db = Db::open_in_memory().unwrap();
    assert!(db.request_daemon_scan_root("/photos/imported").unwrap());
    assert!(!db.request_daemon_scan_root("/photos/imported").unwrap());
    assert!(db.request_daemon_scan_root("/photos/other").unwrap());

    assert_eq!(
        db.take_daemon_commands().unwrap(),
        vec![
            DaemonCommand::ScanRoot { path: "/photos/imported".into() },
            DaemonCommand::ScanRoot { path: "/photos/other".into() },
        ]
    );
    assert!(db.take_daemon_commands().unwrap().is_empty());
}

#[test]
fn migration_v8_adds_the_command_payload_column() {
    let db = Db::open_in_memory().unwrap();
    let has_payload: bool = db
        .conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('daemon_commands') \
             WHERE name = 'payload')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(has_payload);

    // A crash after ALTER but before user_version must be safe to retry.
    db.conn.pragma_update(None, "user_version", 7).unwrap();
    db.migrate_v8().unwrap();
    let version: i64 = db
        .conn
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(version, 8);
}
```

既存テストの期待値を更新する:
- `migration_upgrades_version_3_database_with_existing_jobs`: `assert_eq!(version, 7)` → `8`
- `migration_v4_recovers_from_interrupted_upgrade`: 同じく `8`

- [ ] **Step 2: テストが失敗することを確認**

Run: `cargo test -p vrc-photo-core scan_root_command_round_trips_its_payload`

Run: `cargo test -p vrc-photo-core migration_v8_adds_the_command_payload_column`

Expected: コンパイルエラー（`ScanRoot` / `request_daemon_scan_root` / `migrate_v8` が無い）。

- [ ] **Step 3: model と DB を実装する**

`crates/core/src/model.rs`:

```rust
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum DaemonCommand {
    Rescan,
    Relog,
    Relink,
    ScanRoot { path: String },
}

impl DaemonCommand {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Rescan => "rescan",
            Self::Relog => "relog",
            Self::Relink => "relink",
            Self::ScanRoot { .. } => "scan_root",
        }
    }

    pub fn payload(&self) -> Option<&str> {
        match self {
            Self::ScanRoot { path } => Some(path),
            _ => None,
        }
    }

    pub fn parse(command: &str, payload: Option<&str>) -> Option<Self> {
        match command {
            "rescan" => Some(Self::Rescan),
            "relog" => Some(Self::Relog),
            "relink" => Some(Self::Relink),
            "scan_root" => payload
                .filter(|path| !path.is_empty())
                .map(|path| Self::ScanRoot { path: path.to_string() }),
            _ => None,
        }
    }
}
```

`crates/core/src/db.rs`:
- v7 の定義の下に `const SCHEMA_V8_COMMAND_PAYLOAD: &str = "ALTER TABLE daemon_commands ADD COLUMN payload TEXT;";`
- `migrate()` の v7 の後に `if version < 8 { self.migrate_v8()?; }`。
- `migrate_v8` は `migrate_v6` と同じ形（Immediate transaction、version 再読、`pragma_table_info('daemon_commands')` で `payload` を確認、無ければ ALTER、`schema_migrations` に 8、`user_version=8`、commit）。
- `request_daemon_command` は `let kind = command.as_str(); let payload = command.payload();` を取り、INSERT に `payload` 列を追加:

```sql
INSERT INTO daemon_commands(command, payload, created_at)
SELECT ?1, ?2, ?3 WHERE NOT EXISTS (
    SELECT 1 FROM daemon_commands WHERE command = ?1 AND payload IS ?2
)
```

- `request_daemon_scan_root` は上と同じ SQL に `'scan_root'` と path を渡す。
- `take_daemon_commands` は `SELECT command, payload FROM daemon_commands ORDER BY id` とし、`DaemonCommand::parse(&command, payload.as_deref())` を使う。

`crates/daemon/src/lib.rs` はコンパイルを通すだけの機械的変更:
- `poll_commands` のループ先頭で `let kind = command.as_str();` を取り、`run_command(&command)`、ログは `command = kind`。
- `run_command(&mut self, command: &DaemonCommand)` に変更。`ScanRoot { .. }` アームは Task 3 まで `self.scan_photos()?` を呼ぶ（一時的に Rescan と同じ挙動）。
- エラー分類の `match command` に `DaemonCommand::ScanRoot { .. } => (DaemonOperation::Scan, "写真フォルダの再スキャンに失敗しました")` を追加。

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo test -p vrc-photo-core`

Expected: PASS。

- [ ] **Step 5: コミット**

```bash
git add crates/core/src/model.rs crates/core/src/db.rs crates/daemon/src/lib.rs
git commit -m "feat: add daemon scan-root command payload"
```

---

### Task 2: パス集合の一括参照と無変更スキップ

**Files:**
- Modify: `crates/core/src/db.rs:710-716`（`photos_for_root` の近くに追加）
- Modify: `crates/core/src/scanner.rs:60-70, 279-371`
- Test: `crates/core/src/db.rs` tests
- Test: `crates/core/src/scanner.rs:373` 以降

**Interfaces:**
- Consumes: `DaemonCommand` は Task 1 のまま。ここでは使わない。
- Produces:
  - `pub struct PhotoScanState { id: i64, abs_path: String, file_size: u64, mtime: i64, blake3: Option<String>, status: PhotoStatus, display_derivative_id: Option<i64> }`
  - `Db::photo_scan_states(&self, paths: &[String]) -> Result<HashMap<String, PhotoScanState>>`
  - `ScanReport::modified: usize`

- [ ] **Step 1: 失敗する DB テストを書く**

`crates/core/src/db.rs` の tests に追加:

```rust
#[test]
fn photo_scan_states_reads_all_chunks_with_metadata() {
    let db = Db::open_in_memory().unwrap();
    let root = db.add_root("/photos", "reference").unwrap();
    let mut paths = Vec::new();
    let mut missing_id = 0;
    for index in 0..501i64 {
        let path = format!("/photos/{index:04}.png");
        let id = db
            .upsert_photo(&NewPhoto {
                root_id: root,
                rel_path: format!("{index:04}.png"),
                abs_path: path.clone(),
                file_name: format!("{index:04}.png"),
                captured_at_utc: 1_700_000_000 + index,
                captured_at_local: "2026-10-21 21:14:03".to_string(),
                tz_offset_sec: 0,
                width: 4,
                height: 3,
                file_size: 100 + index as u64,
                mtime: 200 + index,
                blake3: (index == 0).then(|| "hash".to_string()),
            })
            .unwrap();
        if index == 500 {
            missing_id = id;
        }
        paths.push(path);
    }
    db.mark_missing(missing_id).unwrap();

    let states = db.photo_scan_states(&paths).unwrap();
    assert_eq!(states.len(), 501);
    let first = &states["/photos/0000.png"];
    assert_eq!(first.file_size, 100);
    assert_eq!(first.mtime, 200);
    assert_eq!(first.blake3.as_deref(), Some("hash"));
    assert_eq!(first.status, PhotoStatus::Present);
    assert_eq!(
        states["/photos/0500.png"].status,
        PhotoStatus::Missing
    );
}
```

- [ ] **Step 2: テストが失敗することを確認**

Run: `cargo test -p vrc-photo-core photo_scan_states_reads_all_chunks_with_metadata`
Expected: コンパイルエラー（`photo_scan_states` が無い）。

- [ ] **Step 3: `PhotoScanState` と `photo_scan_states` を実装する**

`crates/core/src/db.rs` の `photos_for_root` の近く:

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhotoScanState {
    pub id: i64,
    pub abs_path: String,
    pub file_size: u64,
    pub mtime: i64,
    pub blake3: Option<String>,
    pub status: PhotoStatus,
    pub display_derivative_id: Option<i64>,
}
```

`photo_scan_states` は 500 件ずつ `abs_path IN (...)` で取得し、`HashMap<String, PhotoScanState>` に詰める。`file_size` は `row.get::<_, i64>(...)` から `u64` へ、`status` は `PhotoStatus::parse` を使う。

- [ ] **Step 4: 失敗する scanner テストを書く**

`crates/core/src/scanner.rs` tests に追加:

```rust
#[test]
fn unchanged_photo_is_skipped_without_rehashing() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("VRChat_2026-10-21_21-14-03.221_4x3.png");
    write_png(&path);
    let metadata = std::fs::metadata(&path).unwrap();
    let mtime = metadata
        .modified()
        .unwrap()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let db = Db::open_in_memory().unwrap();
    let root_id = db.add_root(dir.path().to_str().unwrap(), "reference").unwrap();
    let id = db
        .upsert_photo(&NewPhoto {
            root_id,
            rel_path: "VRChat_2026-10-21_21-14-03.221_4x3.png".to_string(),
            abs_path: path.to_string_lossy().to_string(),
            file_name: "VRChat_2026-10-21_21-14-03.221_4x3.png".to_string(),
            captured_at_utc: 1_700_000_000,
            captured_at_local: "2026-10-21 21:14:03".to_string(),
            tz_offset_sec: 0,
            width: 4,
            height: 3,
            file_size: metadata.len(),
            mtime,
            blake3: None,
        })
        .unwrap();
    let before: String = db
        .connection()
        .query_row("SELECT updated_at FROM photos WHERE id = ?1", [id], |row| {
            row.get(0)
        })
        .unwrap();

    let report = scan_and_index(&db, root_id, dir.path()).unwrap();
    assert_eq!(report.indexed, 1);
    assert_eq!(report.modified, 0);
    let photo = db.photo_by_id(id).unwrap().unwrap();
    assert!(photo.blake3.is_none());
    let after: String = db
        .connection()
        .query_row("SELECT updated_at FROM photos WHERE id = ?1", [id], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(before, after);
}

#[test]
fn changed_file_is_reindexed() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("VRChat_2026-10-21_21-14-03.221_4x3.png");
    write_png(&path);
    let db = Db::open_in_memory().unwrap();
    let root_id = db.add_root(dir.path().to_str().unwrap(), "reference").unwrap();
    scan_and_index(&db, root_id, dir.path()).unwrap();

    image::RgbaImage::from_pixel(8, 3, image::Rgba([1, 2, 3, 255]))
        .save(&path)
        .unwrap();
    let report = scan_and_index(&db, root_id, dir.path()).unwrap();
    assert_eq!(report.modified, 1);
    let photo = db
        .photo_by_path(path.to_str().unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(photo.width, 8);
    assert_eq!(photo.file_size, std::fs::metadata(&path).unwrap().len());
}

#[test]
fn a_reappearing_missing_photo_becomes_present() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("VRChat_2026-10-21_21-14-03.221_4x3.png");
    write_png(&path);
    let db = Db::open_in_memory().unwrap();
    let root_id = db.add_root(dir.path().to_str().unwrap(), "reference").unwrap();
    let first = scan_and_index(&db, root_id, dir.path()).unwrap();
    db.mark_missing(first.new_ids[0]).unwrap();

    let report = scan_and_index(&db, root_id, dir.path()).unwrap();
    assert_eq!(report.modified, 1);
    let photo = db.photo_by_id(first.new_ids[0]).unwrap().unwrap();
    assert_eq!(photo.status, PhotoStatus::Present);
}

#[test]
fn nested_root_scan_skips_a_photo_owned_by_another_root() {
    let dir = tempdir().unwrap();
    let parent = dir.path();
    let child = parent.join("2026");
    std::fs::create_dir(&child).unwrap();
    write_png(&child.join("VRChat_2026-10-21_21-14-03.221_4x3.png"));
    let db = Db::open_in_memory().unwrap();
    let child_id = db.add_root(child.to_str().unwrap(), "reference").unwrap();
    let first = scan_and_index(&db, child_id, &child).unwrap();
    let parent_id = db.add_root(parent.to_str().unwrap(), "reference").unwrap();

    let report = scan_and_index(&db, parent_id, parent).unwrap();
    assert_eq!(report.modified, 0);
    assert_eq!(db.count_photos().unwrap(), 1);
    assert_eq!(
        db.photo_by_path(
            child.join("VRChat_2026-10-21_21-14-03.221_4x3.png")
                .to_str()
                .unwrap()
        )
        .unwrap()
        .unwrap()
        .id,
        first.new_ids[0]
    );
}
```

既存の移動テスト `a_moved_photo_keeps_its_id_and_tags` に `assert_eq!(report.modified, 1);` を追加する。

- [ ] **Step 5: scanner を実装する**

`ScanReport` に `pub modified: usize` を追加。`scan_and_index_observed`:

1. 走査結果から `Vec<String>` の絶対パス一覧を作る。
2. `let states = db.photo_scan_states(&paths)?;`
3. ループ先頭で `seen_paths.push` した後、次を満たす行は `indexed += 1; processed += 1; observe(...); continue;`:
   - `state.status == PhotoStatus::Present`
   - `state.display_derivative_id.is_none()`
   - `state.file_size == photo.file_size`
   - `state.mtime == photo.mtime`
4. スキップしない場合:
   - `existed = states.contains_key(&path)`
   - `blake3` は `state` があり `state.blake3.is_some()` かつサイズ・mtime 一致なら clone、それ以外は `hashing::file_blake3(...)`。
   - rebind 成功時に `modified += 1`。
   - `upsert_photo` 成功時に `indexed += 1; modified += 1`。
5. `db.photo_by_path` の呼び出しを削除する。

- [ ] **Step 6: テストが通ることを確認**

Run: `cargo test -p vrc-photo-core`
Expected: PASS（既存の scanner / db テストを含む）。

- [ ] **Step 7: コミット**

```bash
git add crates/core/src/db.rs crates/core/src/scanner.rs
git commit -m "perf: batch scan states and skip unchanged photos"
```

---

### Task 3: daemon のルート限定スキャン

**Files:**
- Modify: `crates/daemon/src/lib.rs:471-484, 805-980`
- Test: `crates/daemon/src/lib.rs` tests（`2960` 付近のヘルパーを使用）

**Interfaces:**
- Consumes: `DaemonCommand::ScanRoot { path }`（Task 1）、`ScanReport::modified`（Task 2）、`Db::request_daemon_scan_root`（Task 1）。
- Produces:
  - `Daemon::scan_root_photos(&mut self, root: PathBuf) -> Result<ScanReport>`
  - `Daemon::scan_roots(&mut self, roots: Vec<PathBuf>, full_scan: bool) -> Result<ScanReport>`
  - `Daemon::begin_scan(&self, roots: usize)`

- [ ] **Step 1: 失敗する daemon テストを書く**

`crates/daemon/src/lib.rs` tests に追加:

```rust
#[test]
fn scan_root_command_scopes_missing_detection() {
    let dir = tempfile::tempdir().unwrap();
    let root_a = dir.path().join("photos");
    let root_b = dir.path().join("imported");
    let log_dir = dir.path().join("logs");
    std::fs::create_dir_all(&root_a).unwrap();
    std::fs::create_dir_all(&root_b).unwrap();
    std::fs::create_dir_all(&log_dir).unwrap();
    let b_path = root_b.join("VRChat_2026-10-21_21-24-00.000_4x3.png");
    write_photo(&b_path);
    let db_path = dir.path().join("library.db");
    let mut daemon = Daemon::open(&db_path, Some(root_a.clone()), Some(log_dir)).unwrap();
    daemon.set_thumbnail_dir(dir.path().join("thumbnails"));
    save_daemon_settings(&daemon, |settings| {
        settings
            .photo_roots
            .push(root_b.to_string_lossy().to_string());
    });
    {
        let db = Db::open(&db_path).unwrap();
        db.request_daemon_command(DaemonCommand::Rescan).unwrap();
    }
    daemon.poll_commands().unwrap();

    let db = Db::open(&db_path).unwrap();
    assert_eq!(db.count_photos().unwrap(), 1);
    std::fs::remove_file(&b_path).unwrap();
    let a_path = root_a.join("VRChat_2026-10-21_21-25-00.000_4x3.png");
    write_photo(&a_path);
    assert!(
        db.request_daemon_scan_root(root_a.to_str().unwrap())
            .unwrap()
    );
    daemon.poll_commands().unwrap();

    let db = Db::open(&db_path).unwrap();
    assert_eq!(
        db.photo_by_path(b_path.to_str().unwrap())
            .unwrap()
            .unwrap()
            .status,
        PhotoStatus::Present
    );
    assert!(
        db.photo_by_path(a_path.to_str().unwrap())
            .unwrap()
            .is_some()
    );

    db.request_daemon_command(DaemonCommand::Rescan).unwrap();
    daemon.poll_commands().unwrap();
    assert_eq!(
        db.photo_by_path(b_path.to_str().unwrap())
            .unwrap()
            .unwrap()
            .status,
        PhotoStatus::Missing
    );
}

#[test]
fn scan_root_command_indexes_an_unconfigured_directory() {
    let dir = tempfile::tempdir().unwrap();
    let mut daemon = test_daemon(&dir);
    let imported = dir.path().join("imported");
    std::fs::create_dir_all(&imported).unwrap();
    let path = imported.join("VRChat_2026-10-21_21-24-00.000_4x3.png");
    write_photo(&path);
    {
        let db = daemon.db.lock().unwrap();
        db.request_daemon_scan_root(imported.to_str().unwrap())
            .unwrap();
    }
    daemon.poll_commands().unwrap();

    let db = daemon.db.lock().unwrap();
    assert!(db.photo_by_path(path.to_str().unwrap()).unwrap().is_some());
    assert_eq!(db.count_photos().unwrap(), 1);
}

#[test]
fn scan_root_command_enqueues_auto_jobs_without_backlog() {
    let dir = tempfile::tempdir().unwrap();
    let root_a = dir.path().join("photos");
    let root_b = dir.path().join("imported");
    let log_dir = dir.path().join("logs");
    std::fs::create_dir_all(&root_a).unwrap();
    std::fs::create_dir_all(&root_b).unwrap();
    std::fs::create_dir_all(&log_dir).unwrap();
    write_photo(&root_b.join("VRChat_2026-10-21_21-24-00.000_4x3.png"));
    let db_path = dir.path().join("library.db");
    add_auto_profile(&db_path);
    let mut daemon = Daemon::open(&db_path, Some(root_a), Some(log_dir)).unwrap();
    daemon.set_thumbnail_dir(dir.path().join("thumbnails"));
    save_daemon_settings(&daemon, |settings| {
        settings.auto_compress_backlog = false;
        settings
            .photo_roots
            .push(root_b.to_string_lossy().to_string());
    });
    {
        let db = Db::open(&db_path).unwrap();
        db.request_daemon_scan_root(root_b.to_str().unwrap())
            .unwrap();
    }
    daemon.poll_commands().unwrap();

    let db = Db::open(&db_path).unwrap();
    assert_eq!(db.count_jobs(QueueStatus::Pending).unwrap(), 1);
}

#[test]
fn unchanged_rescan_does_not_bump_the_library_revision() {
    let dir = tempfile::tempdir().unwrap();
    let mut daemon = test_daemon(&dir);
    write_photo(
        &dir.path()
            .join("photos/VRChat_2026-10-21_21-14-03.221_4x3.png"),
    );
    daemon.run_once().unwrap();
    let revision = {
        let db = daemon.db.lock().unwrap();
        db.library_revision().unwrap()
    };
    daemon.run_once().unwrap();
    let db = daemon.db.lock().unwrap();
    assert_eq!(db.library_revision().unwrap(), revision);
}

#[test]
fn scan_root_photos_reports_a_single_root() {
    let dir = tempfile::tempdir().unwrap();
    let mut daemon = test_daemon(&dir);
    write_photo(
        &dir.path()
            .join("photos/VRChat_2026-10-21_21-14-03.221_4x3.png"),
    );
    let events = daemon.events.subscribe();
    let root = dir.path().join("photos");
    let report = daemon.scan_root_photos(root).unwrap();
    assert_eq!(report.indexed, 1);
    match events.try_recv().unwrap() {
        Event::ScanStarted { roots } => assert_eq!(roots, 1),
        other => panic!("unexpected event: {other:?}"),
    }
}
```

- [ ] **Step 2: テストが失敗することを確認**

Run: `cargo test -p vrc-photo-daemon scan_root_command_scopes_missing_detection`
Expected: コンパイルエラー（`request_daemon_scan_root` は Task 1 で追加済み。`scan_root_photos` が無い）。

- [ ] **Step 3: daemon を実装する**

- `scan_photos` を:
```rust
fn scan_photos(&mut self) -> Result<ScanReport> {
    let roots = self.settings.photo_roots();
    self.scan_roots(roots, true)
}

fn scan_root_photos(&mut self, root: PathBuf) -> Result<ScanReport> {
    self.scan_roots(vec![root], false)
}

fn scan_roots(&mut self, roots: Vec<PathBuf>, full_scan: bool) -> Result<ScanReport> {
    self.begin_scan(roots.len());
    let result = self.scan_roots_inner(roots, full_scan);
    self.finish_scan(&result);
    result
}
```
- `begin_scan(&self, roots: usize)` は `status.scan_roots = roots` とイベント `ScanStarted { roots }` に渡された数を使う。
- `scan_photos_inner(&mut self, roots: Vec<PathBuf>, full_scan: bool)` にリネームし、内部の `let roots = self.settings.photo_roots();` を削除。`let initial_scan = full_scan && db.count_photos()? == 0;`。`changed |= root_report.modified > 0;`。`if report.modified > 0 { self.link_unlinked()?; }`。
- `run_command` の `ScanRoot { path }` を `self.scan_root_photos(PathBuf::from(path))?; self.link_unlinked()?;` に置き換える。
- `Task 1` で入れた一時的な `self.scan_photos()` 呼び出しを削除する。

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo test -p vrc-photo-daemon`
Expected: PASS（既存の Rescan / 進捗 / 停止テストを含む）。

- [ ] **Step 5: コミット**

```bash
git add crates/daemon/src/lib.rs
git commit -m "feat: scan only the requested import root"
```

---

### Task 4: UI 取り込み完了時の `ScanRoot` 要求

**Files:**
- Modify: `crates/ui/src/app/import.rs:451-504, 619-640`
- Test: `crates/ui/src/app/import.rs:728` 以降

**Interfaces:**
- Consumes: `Db::request_daemon_scan_root`（Task 1）。
- Produces: `pub(crate) fn request_import_scan(db: &Db, dest: &Path) -> Result<bool>`

- [ ] **Step 1: 失敗するテストを書く**

`crates/ui/src/app/import.rs` tests に追加（`use super::request_import_scan;` を追加）:

```rust
#[test]
fn import_scan_request_targets_the_destination_root() {
    let db = vrc_photo_core::Db::open_in_memory().unwrap();
    let dest = Path::new("/photos/imported");
    assert!(request_import_scan(&db, dest).unwrap());
    assert_eq!(
        db.take_daemon_commands().unwrap(),
        vec![vrc_photo_core::model::DaemonCommand::ScanRoot {
            path: "/photos/imported".to_string(),
        }]
    );
}
```

- [ ] **Step 2: テストが失敗することを確認**

Run: `cargo test -p vrc-photo-ui import_scan_request_targets_the_destination_root`
Expected: コンパイルエラー（`request_import_scan` が無い）。

- [ ] **Step 3: 実装する**

`crates/ui/src/app/import.rs`:

```rust
pub(crate) fn request_import_scan(
    db: &vrc_photo_core::Db,
    dest: &Path,
) -> vrc_photo_core::Result<bool> {
    db.request_daemon_scan_root(&dest.to_string_lossy())
}
```

`index_import_destination` の `match self.db.request_daemon_command(DaemonCommand::Rescan)` を `match request_import_scan(&self.db, &report.dest)` に置き換える。コメントを「取り込み先ルートだけを再スキャンし、ウォッチャー再構築はデーモンのコマンド経路が行う」に更新する。`DaemonCommand` の import が未使用になれば削除する。

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo test -p vrc-photo-ui import_scan_request_targets_the_destination_root`
Expected: PASS。

- [ ] **Step 5: コミット**

```bash
git add crates/ui/src/app/import.rs
git commit -m "fix: request a targeted scan after import"
```

---

### Task 5: UI revision デバウンスと直接リロードの集約

**Files:**
- Modify: `crates/ui/src/app.rs:143-148, 583`
- Modify: `crates/ui/src/app/lifecycle.rs:20-30, 131-134, 220-262, 290-385, 547-635`
- Modify: `crates/ui/src/app/deletion.rs:81, 144`
- Modify: `crates/ui/src/app/import.rs:524`
- Test: `crates/ui/src/state/photo_browser.rs:470-495`

**Interfaces:**
- Consumes: `CountDebounce`（既存）。
- Produces:
  - `App::revision_debounce: CountDebounce`
  - `App::apply_library_snapshot(&mut self, observed_revision: Option<i64>, cx: &mut Context<Self>) -> bool`
  - `App::refresh_library_after_scan(&mut self, cx: &mut Context<Self>)`
  - `const LIBRARY_QUIET_PERIOD: Duration = Duration::from_secs(2)`

- [ ] **Step 1: 失敗する debounce テストを書く**

`crates/ui/src/state/photo_browser.rs` tests に追加:

```rust
#[test]
fn revision_debounce_coalesces_a_burst() {
    let mut debounce = CountDebounce::default();
    let quiet = Duration::from_secs(2);
    let start = Instant::now();
    assert!(!debounce.observe(10, start, quiet));
    assert!(!debounce.observe(11, start + Duration::from_secs(3), quiet));
    assert!(!debounce.observe(12, start + Duration::from_secs(6), quiet));
    assert!(debounce.observe(12, start + Duration::from_secs(9), quiet));
    assert!(!debounce.observe(13, start + Duration::from_secs(12), quiet));
}
```

- [ ] **Step 2: テストが失敗することを確認**

Run: `cargo test -p vrc-photo-ui revision_debounce_coalesces_a_burst`
Expected: PASS してしまう場合は `CountDebounce` の既存挙動で問題ない（このテストは Task 5 の前提固定）。以降の UI 配線はコンパイルと手動確認で検証する。

- [ ] **Step 3: フィールドとヘルパーを実装する**

- `crates/ui/src/app.rs`: `pub(crate) revision_debounce: CountDebounce,` を `count_debounce` の隣に追加し、初期化に `revision_debounce: CountDebounce::default(),` を追加。
- `crates/ui/src/app/lifecycle.rs`:
  - `use std::time::{Duration, Instant};` は既存。`const LIBRARY_QUIET_PERIOD: Duration = Duration::from_secs(2);` を追加。
  - `apply_library_snapshot`:
```rust
fn apply_library_snapshot(
    &mut self,
    observed_revision: Option<i64>,
    cx: &mut Context<Self>,
) -> bool {
    if !self.reload_library(true, cx) {
        self.mark_library_dirty();
        return false;
    }
    if let Some(revision) = observed_revision {
        self.last_library_revision = self.last_library_revision.max(revision);
    }
    self.count_debounce.reset();
    self.revision_debounce.reset();
    true
}
```
  - `apply_library_change`: revision 増加時、
    - `Screen::Photos` → `revision_debounce.observe(revision, now, LIBRARY_QUIET_PERIOD)` が true なら `apply_library_snapshot(Some(revision), cx)`、false なら `mark_library_dirty`。
    - `Screen::Search` かつ `applied_search.is_some()` → true なら `last_library_revision = revision; revision_debounce.reset(); start_search(filter, cx);`、false なら `mark_library_dirty`。
    - それ以外 → `last_library_revision = revision; mark_library_dirty();` と選択中メタの更新。
  - 件数経路は `count_debounce.observe(count, now, LIBRARY_QUIET_PERIOD)` とし、成功時 `apply_library_snapshot(revision, cx)`。
  - `refresh_after_compression` に `self.revision_debounce.reset(); self.count_debounce.reset();` を追加。
  - `refresh_library_after_scan(&mut self, cx)` を追加し、`IpcEvent::ScanFinished` で `self.refresh_live(cx); self.refresh_library_after_scan(cx);` を呼ぶ。中身は設計書の「Screen::Photos / Search / それ以外」の 3 分岐。
- `switch_screen` の 2 か所の `self.reload_library(true, cx)` を `let revision = self.db.library_revision().ok(); self.apply_library_snapshot(revision, cx);` に置き換える。
- `crates/ui/src/app/deletion.rs:81, 144` と `crates/ui/src/app/import.rs:524` も同じ形に置き換える（`let revision = self.db.library_revision().ok();` を先に読む）。

- [ ] **Step 4: テストとビルドを確認**

Run: `cargo test -p vrc-photo-ui`
Expected: PASS。

Run: `cargo clippy -p vrc-photo-ui --all-targets -- -D warnings`
Expected: PASS。

- [ ] **Step 5: コミット**

```bash
git add crates/ui/src/app.rs crates/ui/src/app/lifecycle.rs crates/ui/src/app/deletion.rs crates/ui/src/app/import.rs crates/ui/src/state/photo_browser.rs
git commit -m "fix: debounce revision-driven library rebuilds"
```

---

### Task 6: ワークスペース検証と PR

**Files:**
- 変更なし（検証とコミットのみ）

- [ ] **Step 1: ローカル検証**

Run:
```bash
cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace
```
Expected: すべて PASS。

- [ ] **Step 2: 差分を確認**

Run: `git diff --stat origin/main...HEAD`
Expected: `crates/core`、`crates/daemon`、`crates/ui` のみ。docs の変更が混ざっていないこと。

- [ ] **Step 3: コミット漏れを確認**

Run: `git status --short`
Expected: 空。

- [ ] **Step 4: push と PR**

```bash
git push -u origin fix/issue-208-import-scan-freeze
gh pr create --title "fix: 大量取り込み時のフリーズを解消 (#208)" \
  --label bug \
  --body "取り込み後の索引を取り込み先ルートに限定し、無変更写真の upsert/再ハッシュと UI の連続全件リロードをなくします。設計: docs/superpowers/specs/2026-10-31-import-scan-performance-design.md。Refs #208"
```

PR 作成後は `ci-watch` スキルで CI を監視し、緑ならマージする。

- [ ] **Step 5: 手動確認（マージ前）**

`gnome-wayland-visual-qa` スキルでアプリを起動し、次を確認する:

1. 数百枚の写真を新規フォルダへ取り込み中、Photos 画面がスクロール・選択に応答する。
2. 取り込み完了後、3〜6 秒待たずに写真が表示される（`ScanFinished` の即時反映）。
3. スクロール位置が取り込み完了後も維持される。
4. 設定画面の「再スキャン」は従来どおり全ルートを対象に動作する。

---

## 別 PR（コード PR マージ後）

- `docs/architecture.md` §4.6 と daemon コマンドの説明に `scan_root` と
  ルート限定スキャンを追記。
- `docs/features.md` §4 の取り込み説明を「取り込み先ルートのみ再スキャン」に更新し、
  #97 の記述を「daemon 稼働中の取り込みは自動圧縮に載る。停止中取り込みは既知の
  ギャップが残る」に修正。
- `docs/testing.md` に scanner / db / daemon / UI の追加テストを反映。
