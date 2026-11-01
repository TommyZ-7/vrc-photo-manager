# VRChat終了時のセッション自動クローズ Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [x]`) syntax for tracking.

**Goal:** VRChat のプロセス消滅を検出して、`Unloading scenes` を経ない終了でもデーモンの在席表示と写真リンクを止める。

**Architecture:** デーモンのログポーリングと同じ周期で VRChat プロセスの有無を確認し、3回連続で未検出なら現在のログファイルの mtime でセッションを閉じる。デーモン起動時には `ended_at IS NULL` のセッションをログファイル mtime で修復する。プロセス存在はセッションを開く・維持する根拠には使わない。

**Tech Stack:** Rust 2024, rusqlite, `/proc` (Linux), ToolHelp32 (Windows)

**Spec:** GitHub issue #250（`fix: VRChat終了後もデーモンが最後のインスタンスに在席していると認識し続ける`）

## Global Constraints

- VRChat プロセスは Linux では `/proc/<pid>/comm`、Windows ではプロセス列挙で `VRChat.exe` を照合する。
- セッションを閉じる `ended_at` は検出時刻ではなく、対象ログファイルの最終更新時刻（mtime）とする。
- 誤判定防止のため、複数回の連続未検出でのみ閉じる（3回、ログポーリング 750ms で約2.25秒）。
- プロセス存在をセッション開始・維持の根拠にしない。
- デーモン起動時に残っているオープンセッションを mtime で修復する。ただし他インスタンスが所有権を持つ保守実行では修復しない。
- 既存の `Unloading scenes` / ログファイル切り替え / 終了処理の挙動は維持する。

## Review Focus

- VRChat 起動中にログが静かなとき、プロセス検出が成功している限りセッションを閉じないこと。
- プロセス検出 API が失敗した場合（`None`）は未検出として数えず、誤って閉じないこと。
- デーモン再起動時に VRChat が起動中でも、残存セッションを閉じる決定（プロセス存在を維持根拠にしない）を守ること。
- ログファイルが削除済みでも、DB の `log_imports.mtime`、最終的に `started_at` へフォールバックして閉じること。
- 終了済みセッションの時刻より後に取り込まれた写真が `session_at` に一致しないこと。

---

### Task 1: DB にオープンセッション一覧とログ mtime 取得を追加

**Files:**
- Modify: `crates/core/src/db.rs`（session 系メソッド付近、log_imports 系メソッド付近）
- Test: `crates/core/src/db.rs` の `mod tests`

**Interfaces:**
- Produces:
  - `pub fn open_sessions(&self) -> Result<Vec<Session>>`
  - `pub fn log_mtime(&self, log_file: &str) -> Result<Option<i64>>`

- [x] **Step 1: 失敗するテストを書く**

`links_context_and_players` の近くに追加する。

```rust
#[test]
fn open_sessions_returns_only_unfinished_sessions() {
    let db = Db::open_in_memory().unwrap();
    let world = db.upsert_world("wrld_test", None, None).unwrap();
    let open = db.create_session("open.txt", 10, world, None).unwrap();
    let closed = db.create_session("closed.txt", 20, world, None).unwrap();
    db.close_session(closed, 30).unwrap();

    assert_eq!(db.open_sessions().unwrap().len(), 1);
    assert_eq!(db.open_sessions().unwrap()[0].id, open);
}

#[test]
fn log_mtime_reads_the_last_ingested_mtime() {
    let db = Db::open_in_memory().unwrap();
    assert_eq!(db.log_mtime("output_log.txt").unwrap(), None);
    db.set_log_offset("output_log.txt", 42, 1_700_000_123, 42, "1")
        .unwrap();
    assert_eq!(
        db.log_mtime("output_log.txt").unwrap(),
        Some(1_700_000_123)
    );
}
```

- [x] **Step 2: テストが失敗することを確認**

Run: `cargo test -p vrc-photo-core db::tests::open_sessions_returns_only_unfinished_sessions db::tests::log_mtime_reads_the_last_ingested_mtime`
Expected: コンパイルエラー（`open_sessions` / `log_mtime` が未定義）

- [x] **Step 3: 最小実装を追加**

`close_session` の近く:

```rust
pub fn open_sessions(&self) -> Result<Vec<Session>> {
    let mut stmt = self.conn.prepare(
        "SELECT id, log_file, started_at, ended_at, world_id, instance_id \
         FROM sessions WHERE ended_at IS NULL ORDER BY started_at",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(Session {
            id: row.get(0)?,
            log_file: row.get(1)?,
            started_at: row.get(2)?,
            ended_at: row.get(3)?,
            world_id: row.get(4)?,
            instance_id: row.get(5)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}
```

`log_offset` の近く:

```rust
pub fn log_mtime(&self, log_file: &str) -> Result<Option<i64>> {
    let mtime = self
        .conn
        .query_row(
            "SELECT mtime FROM log_imports WHERE log_file = ?1",
            params![log_file],
            |row| row.get(0),
        )
        .optional()?;
    Ok(mtime)
}
```

- [x] **Step 4: テストが通ることを確認**

Run: `cargo test -p vrc-photo-core db::tests::open_sessions_returns_only_unfinished_sessions db::tests::log_mtime_reads_the_last_ingested_mtime`
Expected: PASS

- [x] **Step 5: コミット**

```bash
git add crates/core/src/db.rs
git commit -m "feat(core): expose open sessions and log mtimes"
```

---

### Task 2: LogIngestor に現在セッションを閉じる API を追加

**Files:**
- Modify: `crates/core/src/log_parser.rs`
- Test: `crates/core/src/log_parser.rs` の `mod tests`

**Interfaces:**
- Produces: `pub fn close_current_session(&mut self, db: &Db, ended_at: i64) -> Result<bool>`
- `finish` はこの API を再利用する。

- [x] **Step 1: 失敗するテストを書く**

`ingests_a_session` の後に追加する。

```rust
#[test]
fn close_current_session_closes_at_the_given_time() {
    let db = Db::open_in_memory().unwrap();
    let mut ingestor = LogIngestor::new("log.txt");
    ingestor.ingest_line(&db, JOIN).unwrap();
    let started = parse_line(JOIN).unwrap().timestamp_utc();

    assert!(ingestor.close_current_session(&db, started + 60).unwrap());
    assert!(ingestor.session_id().is_none());
    assert!(db.session_at(started + 30).unwrap().is_some());
    assert!(db.session_at(started + 60).unwrap().is_none());
    assert!(!ingestor.close_current_session(&db, started + 120).unwrap());
}
```

- [x] **Step 2: テストが失敗することを確認**

Run: `cargo test -p vrc-photo-core log_parser::tests::close_current_session_closes_at_the_given_time`
Expected: コンパイルエラー（`close_current_session` が未定義）

- [x] **Step 3: 実装**

```rust
/// Closes the session currently tracked by this ingestor and clears the
/// world state so later lines cannot write into the closed session.
pub fn close_current_session(&mut self, db: &Db, ended_at: i64) -> Result<bool> {
    let Some(session_id) = self.session_id.take() else {
        return Ok(false);
    };
    db.close_session(session_id, ended_at)?;
    self.world_db_id = None;
    self.world_id = None;
    Ok(true)
}
```

`finish` は以下に置き換える。

```rust
pub fn finish(&mut self, db: &Db, timestamp_utc: i64) -> Result<()> {
    self.close_current_session(db, timestamp_utc)?;
    Ok(())
}
```

- [x] **Step 4: テストが通ることを確認**

Run: `cargo test -p vrc-photo-core log_parser::tests`
Expected: PASS

- [x] **Step 5: コミット**

```bash
git add crates/core/src/log_parser.rs
git commit -m "feat(core): let LogIngestor close the active session explicitly"
```

---

### Task 3: VRChat プロセス検出モジュールを追加

**Files:**
- Create: `crates/daemon/src/vrchat_process.rs`
- Modify: `crates/daemon/src/lib.rs`（`mod` 宣言）
- Modify: `crates/daemon/Cargo.toml`（Windows feature）
- Test: `crates/daemon/src/vrchat_process.rs`

**Interfaces:**
- Produces: `pub fn is_running() -> Option<bool>`
  - `Some(true)`: VRChat.exe を検出
  - `Some(false)`: 列挙に成功し、存在しない
  - `None`: 判定不能（誤って閉じない）

- [x] **Step 1: 失敗するテストを書く**

`vrchat_process.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::is_vrchat_comm;

    #[test]
    fn matches_vrchat_comm_with_line_endings() {
        assert!(is_vrchat_comm("VRChat.exe\n"));
        assert!(is_vrchat_comm("VRChat.exe"));
        assert!(!is_vrchat_comm("vrchat.exe"));
        assert!(!is_vrchat_comm("VRChat.exe.old"));
        assert!(!is_vrchat_comm("wine64-preloader"));
    }
}
```

- [x] **Step 2: テストが失敗することを確認**

Run: `cargo test -p vrc-photo-daemon vrchat_process::tests::matches_vrchat_comm_with_line_endings`
Expected: コンパイルエラー（モジュール未定義）

- [x] **Step 3: 実装**

Linux:

```rust
pub fn is_running() -> Option<bool> {
    let entries = std::fs::read_dir("/proc").ok()?;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name.to_str() else { continue };
        if !pid.bytes().all(|byte| byte.is_ascii_digit()) {
            continue;
        }
        let Ok(comm) = std::fs::read_to_string(entry.path().join("comm")) else {
            continue;
        };
        if is_vrchat_comm(&comm) {
            return Some(true);
        }
    }
    Some(false)
}
```

Windows（`windows` crate の `Win32_System_Diagnostics_ToolHelp` feature を追加）:

```rust
pub fn is_running() -> Option<bool> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };

    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }.ok()?;
    let mut entry = PROCESSENTRY32W {
        dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    let mut found = false;
    unsafe {
        if Process32FirstW(snapshot, &mut entry).is_ok() {
            loop {
                let len = entry
                    .szExeFile
                    .iter()
                    .position(|&unit| unit == 0)
                    .unwrap_or(entry.szExeFile.len());
                let name = String::from_utf16_lossy(&entry.szExeFile[..len]);
                if is_vrchat_comm(&name) {
                    found = true;
                    break;
                }
                if Process32NextW(snapshot, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snapshot);
    }
    Some(found)
}
```

共通:

```rust
fn is_vrchat_comm(comm: &str) -> bool {
    comm.trim() == "VRChat.exe"
}
```

その他の OS:

```rust
pub fn is_running() -> Option<bool> {
    None
}
```

- [x] **Step 4: Linux テストと Windows ビルド確認**

Run:
```bash
cargo test -p vrc-photo-daemon vrchat_process::tests::matches_vrchat_comm_with_line_endings
cargo check -p vrc-photo-daemon --target x86_64-pc-windows-gnu
```
Expected: 両方 PASS（Windows はコンパイルのみ）

- [x] **Step 5: コミット**

```bash
git add crates/daemon/src/vrchat_process.rs crates/daemon/src/lib.rs crates/daemon/Cargo.toml
git commit -m "feat(daemon): detect the VRChat process on Linux and Windows"
```

---

### Task 4: デーモン起動時に残存セッションを修復

**Files:**
- Modify: `crates/daemon/src/lib.rs`
- Test: `crates/daemon/src/lib.rs` の `mod tests`

**Interfaces:**
- Produces:
  - `fn modified_utc(path: &Path) -> Option<i64>`
  - `fn repair_open_sessions(&mut self) -> Result<usize>`
- `repair_open_sessions` は `self.owns_deletion_executor` が false なら何もしない。
- 呼び出し: `run()` と `run_once()` の先頭。

- [x] **Step 1: 失敗するテストを書く**

```rust
#[test]
fn startup_repairs_open_sessions_at_the_log_mtime() {
    let dir = tempfile::tempdir().unwrap();
    let photo_dir = dir.path().join("photos");
    let log_dir = dir.path().join("logs");
    std::fs::create_dir_all(&photo_dir).unwrap();
    std::fs::create_dir_all(&log_dir).unwrap();
    let log_path = log_dir.join("output_log_2026-10-21_21-00-00.txt");
    std::fs::write(&log_path, "").unwrap();
    let mtime = 1_700_000_500;
    let file = File::options().write(true).open(&log_path).unwrap();
    file.set_modified(std::time::UNIX_EPOCH + Duration::from_secs(mtime))
        .unwrap();

    let db_path = dir.path().join("library.db");
    {
        let db = Db::open(&db_path).unwrap();
        let world = db.upsert_world("wrld_test", Some("Test World"), Some("log")).unwrap();
        let session = db
            .create_session(log_path.to_str().unwrap(), mtime - 120, world, None)
            .unwrap();
        db.upsert_session_player(session, "usr_a", "Mio", mtime - 60).unwrap();
    }

    let mut daemon = Daemon::open(&db_path, Some(photo_dir), Some(log_dir)).unwrap();
    daemon.run_once().unwrap();

    let db = Db::open(&db_path).unwrap();
    assert!(db.open_sessions().unwrap().is_empty());
    let session = db.session_at(mtime - 60).unwrap().unwrap();
    assert_eq!(session.ended_at, Some(mtime));
}
```

- [x] **Step 2: テストが失敗することを確認**

Run: `cargo test -p vrc-photo-daemon startup_repairs_open_sessions_at_the_log_mtime`
Expected: FAIL（`open_sessions` が1件、`ended_at` が `None`）

- [x] **Step 3: 実装**

```rust
fn modified_utc(path: &Path) -> Option<i64> {
    path.metadata()
        .ok()?
        .modified()
        .ok()
        .map(vrc_photo_core::time::from_system_time)
        .map(|stamp| stamp.utc)
}

fn repair_open_sessions(&mut self) -> Result<usize> {
    if !self.owns_deletion_executor {
        return Ok(0);
    }
    let db = self.db.lock().unwrap();
    let sessions = db.open_sessions()?;
    let mut repaired = 0;
    for session in sessions {
        let ended_at = modified_utc(Path::new(&session.log_file))
            .or_else(|| db.log_mtime(&session.log_file).ok().flatten())
            .unwrap_or(session.started_at)
            .max(session.started_at);
        db.close_session(session.id, ended_at)?;
        tracing::info!(
            session_id = session.id,
            log_file = %session.log_file,
            ended_at,
            "closed stale session at startup"
        );
        repaired += 1;
    }
    Ok(repaired)
}
```

`run()` の先頭と `run_once()` の先頭で呼ぶ。

```rust
self.repair_open_sessions()?;
```

- [x] **Step 4: テストが通ることを確認**

Run: `cargo test -p vrc-photo-daemon startup_repairs_open_sessions_at_the_log_mtime`
Expected: PASS

- [x] **Step 5: コミット**

```bash
git add crates/daemon/src/lib.rs
git commit -m "fix(daemon): repair sessions left open across restarts"
```

---

### Task 5: 実行中のプロセス未検出でセッションを閉じる

**Files:**
- Modify: `crates/daemon/src/lib.rs`
- Test: `crates/daemon/src/lib.rs` の `mod tests`

**Interfaces:**
- 追加するフィールド: `vrchat_missing_polls: u8`
- 定数: `VRCHAT_MISSING_POLLS_BEFORE_CLOSE: u8 = 3`
- Produces:
  - `fn poll_vrchat_presence(&mut self) -> Result<()>`
  - `fn observe_vrchat_presence(&mut self, running: Option<bool>) -> Result<()>`
- `poll_vrchat_presence` は `run()` のログポーリングと `run_once()` の `ingest_logs()` 後に呼ぶ。

- [x] **Step 1: 失敗するテストを書く**

```rust
#[test]
fn vrchat_exit_closes_the_session_at_the_log_mtime_after_consecutive_misses() {
    let dir = tempfile::tempdir().unwrap();
    let mut daemon = test_daemon(&dir);
    let log_path = dir.path().join("logs/output_log_2026-10-21_21-00-00.txt");
    let join = "2026.10.21 21:23:12 Debug      -  [Behaviour] Joining wrld_test:12345~region(jp)";
    std::fs::write(&log_path, format!("{join}\n")).unwrap();
    daemon.ingest_logs().unwrap();
    let started = vrc_photo_core::log_parser::parse_line(join)
        .unwrap()
        .timestamp_utc();
    let mtime = started + 120;
    let file = File::options().write(true).open(&log_path).unwrap();
    file.set_modified(std::time::UNIX_EPOCH + Duration::from_secs(mtime as u64))
        .unwrap();
    assert_eq!(daemon.log_tail.as_ref().unwrap().ingestor.session_id(), Some(1));

    daemon.observe_vrchat_presence(Some(false)).unwrap();
    daemon.observe_vrchat_presence(Some(false)).unwrap();
    daemon.observe_vrchat_presence(Some(true)).unwrap();
    daemon.observe_vrchat_presence(Some(false)).unwrap();
    daemon.observe_vrchat_presence(Some(false)).unwrap();
    {
        let db = daemon.db.lock().unwrap();
        assert!(db.session_at(started + 1).unwrap().is_some());
    }
    daemon.observe_vrchat_presence(Some(false)).unwrap();

    assert!(daemon.log_tail.as_ref().unwrap().ingestor.session_id().is_none());
    let db = daemon.db.lock().unwrap();
    let session = db.session_at(started + 1).unwrap().unwrap();
    assert_eq!(session.ended_at, Some(mtime));
    drop(db);
    let status = daemon.status.lock().unwrap();
    assert_eq!(status.world_name, None);
}
```

- [x] **Step 2: テストが失敗することを確認**

Run: `cargo test -p vrc-photo-daemon vrchat_exit_closes_the_session_at_the_log_mtime_after_consecutive_misses`
Expected: コンパイルエラー（`observe_vrchat_presence` が未定義）

- [x] **Step 3: 実装**

```rust
fn poll_vrchat_presence(&mut self) -> Result<()> {
    let has_session = self
        .log_tail
        .as_ref()
        .and_then(|tail| tail.ingestor.session_id())
        .is_some();
    if !has_session {
        self.vrchat_missing_polls = 0;
        return Ok(());
    }
    self.observe_vrchat_presence(vrchat_process::is_running())
}

fn observe_vrchat_presence(&mut self, running: Option<bool>) -> Result<()> {
    let Some(running) = running else {
        return Ok(());
    };
    if running {
        self.vrchat_missing_polls = 0;
        return Ok(());
    }
    self.vrchat_missing_polls = self.vrchat_missing_polls.saturating_add(1);
    if self.vrchat_missing_polls < VRCHAT_MISSING_POLLS_BEFORE_CLOSE {
        return Ok(());
    }

    let ended_at = self
        .log_tail
        .as_ref()
        .and_then(|tail| modified_utc(&tail.path))
        .unwrap_or_else(|| chrono::Utc::now().timestamp());
    let closed = {
        let db = self.db.lock().unwrap();
        self.log_tail
            .as_mut()
            .map(|tail| tail.ingestor.close_current_session(&db, ended_at))
            .transpose()?
            .unwrap_or(false)
    };
    self.vrchat_missing_polls = 0;
    if closed {
        tracing::info!(ended_at, "closed session after VRChat exited");
        self.refresh_status()?;
    }
    Ok(())
}
```

`run_once()` の `ingest_logs()` 後と、`run()` のログポーリングに追加する。

```rust
self.poll_vrchat_presence()?;
```

- [x] **Step 4: テストが通ることを確認**

Run: `cargo test -p vrc-photo-daemon vrchat_exit_closes_the_session_at_the_log_mtime_after_consecutive_misses`
Expected: PASS

- [x] **Step 5: コミット**

```bash
git add crates/daemon/src/lib.rs
git commit -m "fix(daemon): close the active session when VRChat exits"
```

---

### Task 6: デーモン終了時もログ mtime を使う

**Files:**
- Modify: `crates/daemon/src/lib.rs`
- Test: `crates/daemon/src/lib.rs` の `mod tests`

**Interfaces:**
- `shutdown()` は `LogIngestor::finish` に現在時刻ではなくログファイル mtime を渡す。

- [x] **Step 1: 失敗するテストを書く**

```rust
#[test]
fn shutdown_closes_the_session_at_the_log_mtime() {
    let dir = tempfile::tempdir().unwrap();
    let mut daemon = test_daemon(&dir);
    let log_path = dir.path().join("logs/output_log_2026-10-21_21-00-00.txt");
    let join = "2026.10.21 21:23:12 Debug      -  [Behaviour] Joining wrld_test:12345~region(jp)";
    std::fs::write(&log_path, format!("{join}\n")).unwrap();
    daemon.ingest_logs().unwrap();
    let started = vrc_photo_core::log_parser::parse_line(join)
        .unwrap()
        .timestamp_utc();
    let mtime = started + 120;
    let file = File::options().write(true).open(&log_path).unwrap();
    file.set_modified(std::time::UNIX_EPOCH + Duration::from_secs(mtime as u64))
        .unwrap();

    daemon.shutdown();

    let db = daemon.db.lock().unwrap();
    let session = db.session_at(started + 1).unwrap().unwrap();
    assert_eq!(session.ended_at, Some(mtime));
}
```

- [x] **Step 2: テストが失敗することを確認**

Run: `cargo test -p vrc-photo-daemon shutdown_closes_the_session_at_the_log_mtime`
Expected: FAIL（`ended_at` が現在時刻）

- [x] **Step 3: 実装**

`shutdown()` 内の `finish` 呼び出しを変更する。

```rust
let ended_at = modified_utc(&tail.path).unwrap_or_else(|| chrono::Utc::now().timestamp());
if let Err(error) = tail.ingestor.finish(&db, ended_at) {
```

- [x] **Step 4: テストが通ることを確認**

Run: `cargo test -p vrc-photo-daemon shutdown_closes_the_session_at_the_log_mtime`
Expected: PASS

- [x] **Step 5: コミット**

```bash
git add crates/daemon/src/lib.rs
git commit -m "fix(daemon): finish sessions at the log file mtime"
```

---

### Task 7: 全体検証と PR

**Files:**
- Modify: `docs/superpowers/plans/2026-11-02-vrchat-exit-session.md`（完了チェック）

- [x] **Step 1: 整形・lint・テスト**

Run:
```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```
Expected: すべて PASS

- [x] **Step 2: Windows ターゲットのコンパイル確認**

Run: `cargo check -p vrc-photo-daemon --target x86_64-pc-windows-gnu`
Expected: PASS

- [x] **Step 3: 計画のチェックボックスを完了に更新**

- [x] **Step 4: コミットと PR**

```bash
git add -A
git commit -m "chore: mark the VRChat exit session plan complete"
git push -u origin fix/250-vrchat-exit
gh pr create --title "fix: close stale VRChat sessions when the process exits" \
  --label bug --body "Closes #250"
```
