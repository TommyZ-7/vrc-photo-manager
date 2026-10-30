use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use rusqlite::types::Value;
use rusqlite::{
    Connection, OpenFlags, OptionalExtension, Row, Transaction, TransactionBehavior, params,
    params_from_iter,
};

use crate::error::{CoreError, Result};
use crate::model::{
    CompressionJob, CompressionProfile, DaemonCommand, DaemonError, DaemonOperation,
    DeletionBlockers, DeletionCounts, DeletionJobSummary, DeletionReason, DeletionRequest,
    DeletionStatus, Derivative, JobStatusCounts, LibraryRoot, LibraryStats, MAX_JOB_RETRIES,
    MAX_PROFILE_NAME_CHARS, NewPhoto, Photo, PhotoContext, PhotoFilter, PhotoIndexEntry,
    PhotoPlayer, PhotoSort, PhotoStatus, PhotoTextFilter, QueueStatus, Session, SessionPlayer,
    World,
};

const SCHEMA_V1: &str = r#"
CREATE TABLE IF NOT EXISTS schema_migrations (
    version INTEGER PRIMARY KEY,
    applied_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS library_roots (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    path TEXT NOT NULL UNIQUE,
    kind TEXT NOT NULL,
    created_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS worlds (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    world_id TEXT NOT NULL UNIQUE,
    name TEXT,
    name_source TEXT,
    alias TEXT,
    first_seen TEXT NOT NULL,
    last_seen TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS sessions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    log_file TEXT NOT NULL,
    started_at INTEGER NOT NULL,
    ended_at INTEGER,
    world_id INTEGER NOT NULL REFERENCES worlds(id),
    instance_id TEXT
);

CREATE TABLE IF NOT EXISTS session_players (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    user_id TEXT NOT NULL,
    display_name TEXT NOT NULL,
    joined_at INTEGER NOT NULL,
    left_at INTEGER,
    UNIQUE(session_id, user_id)
);

CREATE TABLE IF NOT EXISTS photos (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    root_id INTEGER NOT NULL REFERENCES library_roots(id) ON DELETE CASCADE,
    rel_path TEXT NOT NULL,
    abs_path TEXT NOT NULL UNIQUE,
    file_name TEXT NOT NULL,
    captured_at_utc INTEGER NOT NULL,
    captured_at_local TEXT NOT NULL,
    tz_offset_sec INTEGER NOT NULL,
    width INTEGER NOT NULL,
    height INTEGER NOT NULL,
    file_size INTEGER NOT NULL,
    mtime INTEGER NOT NULL,
    blake3 TEXT,
    status TEXT NOT NULL DEFAULT 'present',
    is_favorite INTEGER NOT NULL DEFAULT 0,
    note TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    UNIQUE(root_id, rel_path)
);

CREATE INDEX IF NOT EXISTS idx_photos_captured ON photos(captured_at_utc DESC);
CREATE INDEX IF NOT EXISTS idx_photos_favorite ON photos(is_favorite) WHERE is_favorite = 1;
CREATE INDEX IF NOT EXISTS idx_photos_blake3 ON photos(blake3);

CREATE TABLE IF NOT EXISTS photo_context (
    photo_id INTEGER PRIMARY KEY REFERENCES photos(id) ON DELETE CASCADE,
    session_id INTEGER REFERENCES sessions(id) ON DELETE SET NULL,
    world_id INTEGER REFERENCES worlds(id) ON DELETE SET NULL,
    world_name TEXT,
    instance_id TEXT,
    link_method TEXT NOT NULL,
    confidence REAL NOT NULL DEFAULT 0,
    linked_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_context_world ON photo_context(world_id);
CREATE INDEX IF NOT EXISTS idx_context_instance ON photo_context(instance_id);

CREATE TABLE IF NOT EXISTS photo_players (
    photo_id INTEGER NOT NULL REFERENCES photos(id) ON DELETE CASCADE,
    user_id TEXT NOT NULL,
    display_name TEXT NOT NULL,
    PRIMARY KEY(photo_id, user_id)
);

CREATE INDEX IF NOT EXISTS idx_photo_players_user ON photo_players(user_id);
CREATE INDEX IF NOT EXISTS idx_photo_players_name ON photo_players(display_name);

CREATE TABLE IF NOT EXISTS tags (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL UNIQUE
);

CREATE TABLE IF NOT EXISTS photo_tags (
    photo_id INTEGER NOT NULL REFERENCES photos(id) ON DELETE CASCADE,
    tag_id INTEGER NOT NULL REFERENCES tags(id) ON DELETE CASCADE,
    PRIMARY KEY(photo_id, tag_id)
);

CREATE INDEX IF NOT EXISTS idx_photo_tags_tag ON photo_tags(tag_id);

CREATE TABLE IF NOT EXISTS compression_profiles (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    enabled INTEGER NOT NULL,
    auto_enabled INTEGER NOT NULL,
    json TEXT NOT NULL,
    sort_order INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS derivatives (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    photo_id INTEGER NOT NULL REFERENCES photos(id) ON DELETE CASCADE,
    path TEXT NOT NULL UNIQUE,
    format TEXT NOT NULL,
    width INTEGER NOT NULL,
    height INTEGER NOT NULL,
    file_size INTEGER NOT NULL,
    profile_id TEXT NOT NULL,
    policy TEXT NOT NULL,
    created_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_derivatives_photo ON derivatives(photo_id);

CREATE TABLE IF NOT EXISTS compression_jobs (
    id TEXT PRIMARY KEY,
    photo_id INTEGER NOT NULL REFERENCES photos(id) ON DELETE CASCADE,
    profile_id TEXT NOT NULL,
    status TEXT NOT NULL,
    progress REAL NOT NULL DEFAULT 0,
    error TEXT,
    output_path TEXT,
    policy TEXT NOT NULL DEFAULT 'keep',
    created_at TEXT NOT NULL,
    started_at TEXT,
    finished_at TEXT
);

CREATE INDEX IF NOT EXISTS idx_jobs_status ON compression_jobs(status);

CREATE TABLE IF NOT EXISTS log_imports (
    log_file TEXT PRIMARY KEY,
    size INTEGER NOT NULL,
    mtime INTEGER NOT NULL,
    last_offset INTEGER NOT NULL,
    parser_version TEXT NOT NULL,
    parsed_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS app_settings (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
"#;

const SCHEMA_V2: &str = r#"
ALTER TABLE photo_context ADD COLUMN author TEXT;
"#;

const SCHEMA_V3: &str = r#"
CREATE INDEX IF NOT EXISTS idx_photos_captured_id
    ON photos(captured_at_utc DESC, id DESC)
    WHERE status = 'present';
"#;

const SCHEMA_V4_RETRY_COUNT: &str =
    "ALTER TABLE compression_jobs ADD COLUMN retry_count INTEGER NOT NULL DEFAULT 0;";

const SCHEMA_V4_DAEMON_TABLES: &str = r#"
CREATE TABLE IF NOT EXISTS daemon_errors (
    operation TEXT PRIMARY KEY,
    message TEXT NOT NULL,
    detail TEXT,
    occurred_at TEXT NOT NULL,
    attempts INTEGER NOT NULL DEFAULT 1
);

CREATE TABLE IF NOT EXISTS daemon_commands (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    command TEXT NOT NULL,
    created_at TEXT NOT NULL
);
"#;

const SCHEMA_V5_DISPLAY_DERIVATIVE: &str = r#"
ALTER TABLE photos ADD COLUMN display_derivative_id INTEGER
    REFERENCES derivatives(id) ON DELETE SET NULL;
"#;

const SCHEMA_V5_VISIBLE_INDEX: &str = r#"
DROP INDEX IF EXISTS idx_photos_captured_id;
CREATE INDEX IF NOT EXISTS idx_photos_captured_id
    ON photos(captured_at_utc DESC, id DESC)
    WHERE status IN ('present', 'replaced');
"#;

const SCHEMA_V5_MOVE_INDEX: &str = r#"
CREATE INDEX IF NOT EXISTS idx_photos_move_match
    ON photos(status, file_size, captured_at_utc);
"#;

const SCHEMA_V6_JOB_OUTPUT_DIR: &str = "ALTER TABLE compression_jobs ADD COLUMN output_dir TEXT;";

const SCHEMA_V7_DELETION: &str = r#"
CREATE TABLE IF NOT EXISTS deletion_requests (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    photo_id INTEGER NOT NULL REFERENCES photos(id) ON DELETE CASCADE,
    reason TEXT NOT NULL,
    status TEXT NOT NULL,
    force INTEGER NOT NULL DEFAULT 0,
    blocked_reason TEXT,
    error TEXT,
    requested_at TEXT NOT NULL,
    started_at TEXT,
    finished_at TEXT
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_deletion_requests_active
    ON deletion_requests(photo_id)
    WHERE status IN ('pending', 'blocked', 'running');

CREATE INDEX IF NOT EXISTS idx_deletion_requests_status
    ON deletion_requests(status, requested_at);
"#;

const SCHEMA_V8_COMMAND_PAYLOAD: &str = "ALTER TABLE daemon_commands ADD COLUMN payload TEXT;";

const PHOTO_SELECT: &str = "p.id, p.root_id, p.rel_path, p.abs_path, p.file_name, \
    p.captured_at_utc, p.captured_at_local, p.tz_offset_sec, p.width, p.height, \
    p.file_size, p.mtime, p.blake3, p.status, p.is_favorite, p.note, \
    p.display_derivative_id, d.path, d.format, d.file_size, d.width, d.height, \
    c.world_name, c.instance_id, c.link_method, c.confidence, c.author";

const PHOTO_FROM: &str = " FROM photos p \
    LEFT JOIN derivatives d ON d.id = p.display_derivative_id \
    LEFT JOIN photo_context c ON c.photo_id = p.id ";

/// Statuses that belong in the library. `missing` rows stay in the database
/// so tags and context survive a restore, but they are not displayed.
const VISIBLE_STATUS: &str = "p.status IN ('present', 'replaced')";

/// Local capture month as YYYYMM, falling back to the UTC capture time and
/// finally to 0 when neither can be parsed.
const PHOTO_MONTH: &str = "COALESCE(\
    CAST(strftime('%Y%m', p.captured_at_local) AS INTEGER), \
    CAST(strftime('%Y%m', p.captured_at_utc, 'unixepoch') AS INTEGER), \
    0)";

pub struct Db {
    conn: Connection,
    path: Option<PathBuf>,
}

/// The database state a rescan compares against a scanned file.
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

impl Db {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        Self::configure(&conn)?;
        let db = Self {
            conn,
            path: Some(path.to_path_buf()),
        };
        db.migrate()?;
        Ok(db)
    }

    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        Self::configure(&conn)?;
        let db = Self { conn, path: None };
        db.migrate()?;
        Ok(db)
    }

    /// Opens an existing database file without running migrations, for readers
    /// such as background index workers. Callers should fall back to a
    /// synchronous query when this fails.
    pub fn open_read_only(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        Self::configure(&conn)?;
        Ok(Self {
            conn,
            path: Some(path.to_path_buf()),
        })
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    fn configure(conn: &Connection) -> Result<()> {
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        // Best-effort tuning hints; the results are intentionally ignored. WAL
        // is unavailable for in-memory databases (they stay in `memory` journal
        // mode), and read-only connections may reject these writes.
        let _ = conn.pragma_update(None, "journal_mode", "WAL");
        let _ = conn.pragma_update(None, "synchronous", "NORMAL");
        // Library snapshots and the stats aggregates scan six figures of rows
        // on a regular cadence. Keep a larger page cache and avoid spilling
        // ORDER BY temp b-trees to disk. These are per-connection hints; read-
        // only handles accept them, in-memory databases ignore mmap.
        let _ = conn.pragma_update(None, "cache_size", -65_536i64);
        let _ = conn.pragma_update(None, "mmap_size", 268_435_456i64);
        let _ = conn.pragma_update(None, "temp_store", "MEMORY");
        Ok(())
    }

    fn migrate(&self) -> Result<()> {
        let version: i64 = self
            .conn
            .pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version < 1 {
            self.conn.execute_batch(SCHEMA_V1)?;
            self.conn.execute(
                "INSERT OR REPLACE INTO schema_migrations(version, applied_at) VALUES (1, ?1)",
                params![now_string()],
            )?;
            self.conn.pragma_update(None, "user_version", 1)?;
        }
        if version < 2 {
            self.conn.execute_batch(SCHEMA_V2)?;
            self.conn.execute(
                "INSERT OR REPLACE INTO schema_migrations(version, applied_at) VALUES (2, ?1)",
                params![now_string()],
            )?;
            self.conn.pragma_update(None, "user_version", 2)?;
        }
        if version < 3 {
            self.conn.execute_batch(SCHEMA_V3)?;
            self.conn.execute(
                "INSERT OR REPLACE INTO schema_migrations(version, applied_at) VALUES (3, ?1)",
                params![now_string()],
            )?;
            self.conn.pragma_update(None, "user_version", 3)?;
        }
        if version < 4 {
            self.migrate_v4()?;
        }
        if version < 5 {
            self.migrate_v5()?;
        }
        if version < 6 {
            self.migrate_v6()?;
        }
        if version < 7 {
            self.migrate_v7()?;
        }
        if version < 8 {
            self.migrate_v8()?;
        }
        Ok(())
    }

    /// Applies schema version 4 in one immediate transaction. The write lock
    /// serializes concurrent UI/daemon startups, and the existence checks make
    /// the step safe to retry after an interrupted upgrade.
    fn migrate_v4(&self) -> Result<()> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        // Re-read under the write lock: another process may have completed V4
        // between the caller's version check and this transaction.
        let version: i64 = tx.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version >= 4 {
            tx.commit()?;
            return Ok(());
        }

        let has_retry_count: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('compression_jobs') \
             WHERE name = 'retry_count')",
            [],
            |row| row.get(0),
        )?;
        if !has_retry_count {
            tx.execute_batch(SCHEMA_V4_RETRY_COUNT)?;
        }
        tx.execute_batch(SCHEMA_V4_DAEMON_TABLES)?;
        tx.execute(
            "INSERT OR REPLACE INTO schema_migrations(version, applied_at) VALUES (4, ?1)",
            params![now_string()],
        )?;
        tx.pragma_update(None, "user_version", 4)?;
        tx.commit()?;
        Ok(())
    }

    /// Adds the primary derivative reference used by `replaced` photos and
    /// rebuilds the visible-photo index. The column check makes the step safe
    /// to retry after a crash between the ALTER and `user_version`.
    fn migrate_v5(&self) -> Result<()> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let version: i64 = tx.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version >= 5 {
            tx.commit()?;
            return Ok(());
        }

        let has_display_derivative: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('photos') \
             WHERE name = 'display_derivative_id')",
            [],
            |row| row.get(0),
        )?;
        if !has_display_derivative {
            tx.execute_batch(SCHEMA_V5_DISPLAY_DERIVATIVE)?;
        }
        tx.execute_batch(SCHEMA_V5_VISIBLE_INDEX)?;
        tx.execute_batch(SCHEMA_V5_MOVE_INDEX)?;
        tx.execute(
            "INSERT OR REPLACE INTO schema_migrations(version, applied_at) VALUES (5, ?1)",
            params![now_string()],
        )?;
        tx.pragma_update(None, "user_version", 5)?;
        tx.commit()?;
        Ok(())
    }

    /// Adds the per-job output directory used by manual compression. The
    /// column check makes the step safe to retry after an interrupted upgrade.
    fn migrate_v6(&self) -> Result<()> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let version: i64 = tx.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version >= 6 {
            tx.commit()?;
            return Ok(());
        }

        let has_output_dir: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('compression_jobs') \
             WHERE name = 'output_dir')",
            [],
            |row| row.get(0),
        )?;
        if !has_output_dir {
            tx.execute_batch(SCHEMA_V6_JOB_OUTPUT_DIR)?;
        }
        tx.execute(
            "INSERT OR REPLACE INTO schema_migrations(version, applied_at) VALUES (6, ?1)",
            params![now_string()],
        )?;
        tx.pragma_update(None, "user_version", 6)?;
        tx.commit()?;
        Ok(())
    }

    /// Adds the durable deletion queue. The table and indexes use
    /// `IF NOT EXISTS`, so the transaction is safe to retry after a crash
    /// between the DDL and `user_version`.
    fn migrate_v7(&self) -> Result<()> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let version: i64 = tx.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version >= 7 {
            tx.commit()?;
            return Ok(());
        }

        tx.execute_batch(SCHEMA_V7_DELETION)?;
        tx.execute(
            "INSERT OR REPLACE INTO schema_migrations(version, applied_at) VALUES (7, ?1)",
            params![now_string()],
        )?;
        tx.pragma_update(None, "user_version", 7)?;
        tx.commit()?;
        Ok(())
    }

    /// Adds the optional payload used by path-targeted daemon commands. The
    /// column check makes the step safe to retry after a crash between the
    /// ALTER and `user_version`.
    fn migrate_v8(&self) -> Result<()> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let version: i64 = tx.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version >= 8 {
            tx.commit()?;
            return Ok(());
        }

        let has_payload: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('daemon_commands') \
             WHERE name = 'payload')",
            [],
            |row| row.get(0),
        )?;
        if !has_payload {
            tx.execute_batch(SCHEMA_V8_COMMAND_PAYLOAD)?;
        }
        tx.execute(
            "INSERT OR REPLACE INTO schema_migrations(version, applied_at) VALUES (8, ?1)",
            params![now_string()],
        )?;
        tx.pragma_update(None, "user_version", 8)?;
        tx.commit()?;
        Ok(())
    }

    pub fn connection(&self) -> &Connection {
        &self.conn
    }

    // ---------- library roots ----------

    pub fn add_root(&self, path: &str, kind: &str) -> Result<i64> {
        let id = self.conn.query_row(
            "INSERT INTO library_roots(path, kind, created_at) VALUES (?1, ?2, ?3) \
             ON CONFLICT(path) DO UPDATE SET kind = \
                CASE WHEN library_roots.kind = 'managed' THEN 'managed' ELSE excluded.kind END \
             RETURNING id",
            params![path, kind, now_string()],
            |row| row.get(0),
        )?;
        Ok(id)
    }

    pub fn roots(&self) -> Result<Vec<LibraryRoot>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, path, kind FROM library_roots ORDER BY id")?;
        let rows = stmt.query_map([], |row| {
            Ok(LibraryRoot {
                id: row.get(0)?,
                path: row.get(1)?,
                kind: row.get(2)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    // ---------- photos ----------

    pub fn upsert_photo(&self, photo: &NewPhoto) -> Result<i64> {
        let now = now_string();
        let id = self.conn.query_row(
            "INSERT INTO photos(root_id, rel_path, abs_path, file_name, captured_at_utc, \
                captured_at_local, tz_offset_sec, width, height, file_size, mtime, blake3, \
                status, created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, 'present', ?13, ?13) \
             ON CONFLICT(abs_path) DO UPDATE SET \
                root_id = excluded.root_id, rel_path = excluded.rel_path, \
                file_name = excluded.file_name, captured_at_utc = excluded.captured_at_utc, \
                captured_at_local = excluded.captured_at_local, \
                tz_offset_sec = excluded.tz_offset_sec, width = excluded.width, \
                height = excluded.height, file_size = excluded.file_size, \
                mtime = excluded.mtime, blake3 = COALESCE(excluded.blake3, photos.blake3), \
                status = CASE WHEN EXISTS ( \
                    SELECT 1 FROM deletion_requests dr \
                    WHERE dr.photo_id = photos.id \
                      AND dr.status IN ('pending', 'blocked', 'running') \
                ) THEN photos.status ELSE 'present' END, \
                display_derivative_id = CASE WHEN EXISTS ( \
                    SELECT 1 FROM deletion_requests dr \
                    WHERE dr.photo_id = photos.id \
                      AND dr.status IN ('pending', 'blocked', 'running') \
                ) THEN photos.display_derivative_id ELSE NULL END, \
                updated_at = excluded.updated_at \
             RETURNING id",
            params![
                photo.root_id,
                photo.rel_path,
                photo.abs_path,
                photo.file_name,
                photo.captured_at_utc,
                photo.captured_at_local,
                photo.tz_offset_sec,
                photo.width as i64,
                photo.height as i64,
                photo.file_size as i64,
                photo.mtime,
                photo.blake3,
                now,
            ],
            |row| row.get(0),
        )?;
        Ok(id)
    }

    pub fn photo_by_id(&self, id: i64) -> Result<Option<Photo>> {
        let sql = format!("SELECT {PHOTO_SELECT}{PHOTO_FROM} WHERE p.id = ?1");
        let mut stmt = self.conn.prepare(&sql)?;
        let photo = stmt.query_row(params![id], row_to_photo).optional()?;
        Ok(photo)
    }

    pub fn photo_by_path(&self, path: &str) -> Result<Option<Photo>> {
        let sql = format!("SELECT {PHOTO_SELECT}{PHOTO_FROM} WHERE p.abs_path = ?1");
        let mut stmt = self.conn.prepare(&sql)?;
        let photo = stmt.query_row(params![path], row_to_photo).optional()?;
        Ok(photo)
    }

    /// Returns a lightweight, ordered snapshot of every present photo. The
    /// `photo_context` join is 1:1 and keeps every `PhotoSort` executable,
    /// including world order.
    pub fn photo_index(&self, sort: PhotoSort) -> Result<Vec<PhotoIndexEntry>> {
        let order = photo_order(sort);
        let sql = format!(
            "SELECT p.id, p.captured_at_utc, {PHOTO_MONTH}{PHOTO_FROM} \
             WHERE {VISIBLE_STATUS} ORDER BY {order}"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map([], row_to_photo_index)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Returns an ordered index snapshot matching the text filters.
    pub fn photo_index_text(&self, filter: &PhotoTextFilter) -> Result<Vec<PhotoIndexEntry>> {
        let (where_sql, values) = build_text_filter(
            filter.world.as_deref(),
            filter.player.as_deref(),
            filter.tag.as_deref(),
            filter.favorite,
            filter.from_utc,
            filter.to_utc,
        );
        let sql = format!(
            "SELECT p.id, p.captured_at_utc, {PHOTO_MONTH}{PHOTO_FROM} WHERE {where_sql} \
             ORDER BY p.captured_at_utc DESC, p.id DESC"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(values), row_to_photo_index)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Fetches photos by primary key, preserving the order of `ids` and
    /// skipping ids that no longer exist.
    pub fn photos_by_ids(&self, ids: &[i64]) -> Result<Vec<Photo>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let mut by_id = HashMap::with_capacity(ids.len());
        for chunk in ids.chunks(500) {
            let placeholders = vec!["?"; chunk.len()].join(",");
            let sql = format!("SELECT {PHOTO_SELECT}{PHOTO_FROM} WHERE p.id IN ({placeholders})");
            let mut stmt = self.conn.prepare(&sql)?;
            let rows = stmt.query_map(params_from_iter(chunk.iter().copied()), row_to_photo)?;
            for photo in rows {
                let photo = photo?;
                by_id.insert(photo.id, photo);
            }
        }
        Ok(ids.iter().filter_map(|id| by_id.remove(id)).collect())
    }

    pub fn list_photos(&self, limit: i64, offset: i64, sort: PhotoSort) -> Result<Vec<Photo>> {
        let order = photo_order(sort);
        let sql = format!(
            "SELECT {PHOTO_SELECT}{PHOTO_FROM} WHERE {VISIBLE_STATUS} \
             ORDER BY {order} LIMIT ?1 OFFSET ?2"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params![limit.max(1), offset.max(0)], row_to_photo)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn search_photos(&self, filter: &PhotoFilter) -> Result<Vec<Photo>> {
        let (where_sql, values) = build_photo_filter(filter);
        let sort = filter.sort.unwrap_or(PhotoSort::Newest);
        let order = photo_order(sort);
        let limit = if filter.limit > 0 { filter.limit } else { 500 };
        let offset = filter.offset.max(0);
        let sql = format!(
            "SELECT {PHOTO_SELECT}{PHOTO_FROM} WHERE {where_sql} \
             ORDER BY {order} LIMIT ? OFFSET ?"
        );
        let mut all_values = values;
        all_values.push(Value::Integer(limit));
        all_values.push(Value::Integer(offset));
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(all_values), row_to_photo)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn count_search(&self, filter: &PhotoFilter) -> Result<i64> {
        let (where_sql, values) = build_photo_filter(filter);
        let sql = format!("SELECT COUNT(*){PHOTO_FROM} WHERE {where_sql}");
        let mut stmt = self.conn.prepare(&sql)?;
        let count = stmt.query_row(params_from_iter(values), |row| row.get(0))?;
        Ok(count)
    }

    /// Text search used by the UI. Empty filters are ignored; categories are
    /// combined with AND.
    #[allow(clippy::too_many_arguments)]
    pub fn search_photos_text(
        &self,
        world: Option<&str>,
        player: Option<&str>,
        tag: Option<&str>,
        favorite: bool,
        from_utc: Option<i64>,
        to_utc: Option<i64>,
        limit: i64,
    ) -> Result<Vec<Photo>> {
        let (where_sql, mut values) =
            build_text_filter(world, player, tag, favorite, from_utc, to_utc);

        let sql = format!(
            "SELECT {PHOTO_SELECT}{PHOTO_FROM} WHERE {where_sql} \
             ORDER BY p.captured_at_utc DESC, p.id DESC LIMIT ?"
        );
        values.push(Value::Integer(limit.max(1)));
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(values), row_to_photo)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn unlinked_photos(&self, limit: i64) -> Result<Vec<Photo>> {
        let sql = format!(
            "SELECT {PHOTO_SELECT}{PHOTO_FROM} WHERE c.photo_id IS NULL \
             AND {VISIBLE_STATUS} ORDER BY p.captured_at_utc DESC LIMIT ?1"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params![limit.max(1)], row_to_photo)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn photos_for_root(&self, root_id: i64) -> Result<Vec<(i64, String)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, abs_path FROM photos WHERE root_id = ?1 AND status = 'present'")?;
        let rows = stmt.query_map(params![root_id], |row| Ok((row.get(0)?, row.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Fetches the row state a rescan needs for every path, in chunks that
    /// stay under SQLite's variable limit. Missing paths are simply absent
    /// from the map.
    pub fn photo_scan_states(&self, paths: &[String]) -> Result<HashMap<String, PhotoScanState>> {
        let mut states = HashMap::with_capacity(paths.len());
        for chunk in paths.chunks(500) {
            let placeholders = vec!["?"; chunk.len()].join(",");
            let sql = format!(
                "SELECT id, abs_path, file_size, mtime, blake3, status, display_derivative_id \
                 FROM photos WHERE abs_path IN ({placeholders})"
            );
            let mut stmt = self.conn.prepare(&sql)?;
            let rows = stmt.query_map(params_from_iter(chunk.iter()), |row| {
                Ok(PhotoScanState {
                    id: row.get(0)?,
                    abs_path: row.get(1)?,
                    file_size: row.get::<_, i64>(2)? as u64,
                    mtime: row.get(3)?,
                    blake3: row.get(4)?,
                    status: PhotoStatus::parse(&row.get::<_, String>(5)?),
                    display_derivative_id: row.get(6)?,
                })
            })?;
            for state in rows {
                let state = state?;
                states.insert(state.abs_path.clone(), state);
            }
        }
        Ok(states)
    }

    pub fn count_photos(&self) -> Result<i64> {
        let count = self.conn.query_row(
            &format!("SELECT COUNT(*) FROM photos p WHERE {VISIBLE_STATUS}"),
            [],
            |row| row.get(0),
        )?;
        Ok(count)
    }

    pub fn set_favorite(&self, photo_id: i64, favorite: bool) -> Result<()> {
        self.conn.execute(
            "UPDATE photos SET is_favorite = ?1, updated_at = ?2 WHERE id = ?3",
            params![favorite as i64, now_string(), photo_id],
        )?;
        Ok(())
    }

    pub fn set_note(&self, photo_id: i64, note: Option<&str>) -> Result<()> {
        self.conn.execute(
            "UPDATE photos SET note = ?1, updated_at = ?2 WHERE id = ?3",
            params![note, now_string(), photo_id],
        )?;
        Ok(())
    }

    pub fn mark_missing(&self, photo_id: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE photos SET status = 'missing', updated_at = ?1 WHERE id = ?2",
            params![now_string(), photo_id],
        )?;
        Ok(())
    }

    /// Marks a photo missing only when the row still points at `abs_path`.
    /// A concurrent move may already have rebound the row to another root.
    pub fn mark_missing_if_path(&self, photo_id: i64, abs_path: &str) -> Result<bool> {
        let changed = self.conn.execute(
            "UPDATE photos SET status = 'missing', updated_at = ?1 \
             WHERE id = ?2 AND abs_path = ?3 AND status = 'present'",
            params![now_string(), photo_id, abs_path],
        )?;
        Ok(changed == 1)
    }

    /// Promotes a completed derivative to the library display asset. The
    /// original file is expected to be removed by the caller.
    pub fn set_replaced(&self, photo_id: i64, derivative_id: i64) -> Result<bool> {
        let changed = self.conn.execute(
            "UPDATE photos SET display_derivative_id = ?1, \
                status = CASE WHEN status = 'deleting' THEN 'deleting' ELSE 'replaced' END, \
                updated_at = ?2 \
             WHERE id = ?3 AND EXISTS (SELECT 1 FROM derivatives \
                 WHERE id = ?1 AND photo_id = photos.id)",
            params![derivative_id, now_string(), photo_id],
        )?;
        Ok(changed == 1)
    }

    /// Returns a replaced photo to the original-file state. Used when trashing
    /// the original fails after the database was updated optimistically.
    pub fn restore_present(&self, photo_id: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE photos SET display_derivative_id = NULL, status = 'present', \
                updated_at = ?1 WHERE id = ?2 AND status <> 'deleting'",
            params![now_string(), photo_id],
        )?;
        Ok(())
    }

    /// Removes the display derivative reference. The caller decides whether
    /// the row should also become missing or present.
    pub fn clear_display_derivative(&self, photo_id: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE photos SET display_derivative_id = NULL, updated_at = ?1 WHERE id = ?2",
            params![now_string(), photo_id],
        )?;
        Ok(())
    }

    /// Finds a unique existing row that describes the same original file.
    /// Exact BLAKE3 matches win; size/name/capture-time matching is the
    /// fallback for rows indexed before hashing was introduced.
    pub fn find_rebind_candidate(&self, photo: &NewPhoto) -> Result<Option<Photo>> {
        if let Some(hash) = photo.blake3.as_deref() {
            let sql = format!(
                "SELECT {PHOTO_SELECT}{PHOTO_FROM} \
                 WHERE p.blake3 = ?1 AND p.abs_path <> ?2 \
                   AND p.status IN ('present', 'replaced', 'missing') \
                   AND NOT EXISTS (SELECT 1 FROM deletion_requests dr \
                       WHERE dr.photo_id = p.id \
                         AND dr.status IN ('pending', 'blocked', 'running')) \
                 ORDER BY p.id LIMIT 2"
            );
            let mut stmt = self.conn.prepare(&sql)?;
            let rows = stmt.query_map(params![hash, photo.abs_path], row_to_photo)?;
            let mut matches = rows.collect::<rusqlite::Result<Vec<_>>>()?;
            if matches.len() > 1 {
                return Ok(None);
            }
            if let Some(candidate) = matches.pop() {
                if candidate.blake3.as_deref().is_some_and(|old| old != hash) {
                    return Ok(None);
                }
                return Ok(Some(candidate));
            }
        }

        let sql = format!(
            "SELECT {PHOTO_SELECT}{PHOTO_FROM} \
             WHERE p.abs_path <> ?1 AND p.file_name = ?2 AND p.file_size = ?3 \
               AND p.width = ?4 AND p.height = ?5 AND p.captured_at_utc = ?6 \
               AND p.status IN ('present', 'replaced', 'missing') \
               AND NOT EXISTS (SELECT 1 FROM deletion_requests dr \
                   WHERE dr.photo_id = p.id \
                     AND dr.status IN ('pending', 'blocked', 'running')) \
             ORDER BY p.id LIMIT 2"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(
            params![
                photo.abs_path,
                photo.file_name,
                photo.file_size as i64,
                photo.width as i64,
                photo.height as i64,
                photo.captured_at_utc,
            ],
            row_to_photo,
        )?;
        let mut matches = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        if matches.len() != 1 {
            return Ok(None);
        }
        let candidate = matches.pop().unwrap();
        if let (Some(old), Some(new)) = (candidate.blake3.as_deref(), photo.blake3.as_deref())
            && old != new
        {
            return Ok(None);
        }
        Ok(Some(candidate))
    }

    /// Rebinds a row to a new path without changing its id or any related
    /// tags, notes, context, derivatives, or jobs. Capture time and dimensions
    /// stay as originally indexed even when the file was renamed. Returns
    /// false when the new path or relative path is already occupied.
    pub fn rebind_photo(&self, photo_id: i64, photo: &NewPhoto) -> Result<bool> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let abs_conflict: i64 = tx.query_row(
            "SELECT COUNT(*) FROM photos WHERE abs_path = ?1 AND id <> ?2",
            params![photo.abs_path, photo_id],
            |row| row.get(0),
        )?;
        if abs_conflict > 0 {
            return Ok(false);
        }
        let rel_conflict: i64 = tx.query_row(
            "SELECT COUNT(*) FROM photos WHERE root_id = ?1 AND rel_path = ?2 AND id <> ?3",
            params![photo.root_id, photo.rel_path, photo_id],
            |row| row.get(0),
        )?;
        if rel_conflict > 0 {
            return Ok(false);
        }
        let changed = tx.execute(
            "UPDATE photos SET root_id = ?1, rel_path = ?2, abs_path = ?3, file_name = ?4, \
                file_size = ?5, mtime = ?6, blake3 = COALESCE(?7, blake3), status = 'present', \
                display_derivative_id = NULL, updated_at = ?8 \
             WHERE id = ?9 AND NOT EXISTS (SELECT 1 FROM deletion_requests dr \
                 WHERE dr.photo_id = photos.id \
                   AND dr.status IN ('pending', 'blocked', 'running'))",
            params![
                photo.root_id,
                photo.rel_path,
                photo.abs_path,
                photo.file_name,
                photo.file_size as i64,
                photo.mtime,
                photo.blake3,
                now_string(),
                photo_id,
            ],
        )?;
        tx.commit()?;
        Ok(changed == 1)
    }

    pub fn photos_without_hash(&self, limit: i64) -> Result<Vec<(i64, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, abs_path FROM photos \
             WHERE status = 'present' AND blake3 IS NULL ORDER BY id LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit.max(1)], |row| Ok((row.get(0)?, row.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Returns replaced photos and the derivative path the library displays.
    /// The daemon stats these paths so a manually deleted compressed file does
    /// not leave a broken card behind.
    pub fn display_derivative_paths(
        &self,
        limit: i64,
    ) -> Result<Vec<(i64, Option<String>, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT p.id, d.path, r.path FROM photos p \
             LEFT JOIN derivatives d ON d.id = p.display_derivative_id \
             JOIN library_roots r ON r.id = p.root_id \
             WHERE p.status = 'replaced' ORDER BY p.id LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit.max(1)], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn set_photo_blake3(&self, photo_id: i64, blake3: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE photos SET blake3 = ?1 WHERE id = ?2 AND blake3 IS NULL",
            params![blake3, photo_id],
        )?;
        Ok(())
    }

    /// Returns ids whose content hash is shared with another visible photo.
    /// Hashing itself does not merge rows; this query is the duplicate report
    /// surface used by move matching and future duplicate-management UI.
    pub fn duplicate_photo_ids(&self) -> Result<Vec<i64>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT p.id FROM photos p \
             WHERE {VISIBLE_STATUS} AND p.blake3 IS NOT NULL \
               AND EXISTS (SELECT 1 FROM photos q \
                 WHERE q.blake3 = p.blake3 AND q.id <> p.id \
                   AND q.status IN ('present', 'replaced')) \
             ORDER BY p.id"
        ))?;
        let rows = stmt.query_map([], |row| row.get(0))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Monotonic counter used by the UI to detect daemon-side changes even
    /// when the number of photos stays the same (for example a moved file).
    pub fn library_revision(&self) -> Result<i64> {
        Ok(self
            .get_setting("library_revision")?
            .and_then(|value| value.parse().ok())
            .unwrap_or(0))
    }

    pub fn bump_library_revision(&self) -> Result<i64> {
        let revision = self.conn.query_row(
            "INSERT INTO app_settings(key, value) VALUES ('library_revision', '1') \
             ON CONFLICT(key) DO UPDATE SET \
                value = CAST(CAST(app_settings.value AS INTEGER) + 1 AS TEXT) \
             RETURNING CAST(value AS INTEGER)",
            [],
            |row| row.get(0),
        )?;
        Ok(revision)
    }

    pub fn stats(&self) -> Result<LibraryStats> {
        let (photo_count, total_bytes): (i64, i64) = self.conn.query_row(
            &format!(
                "SELECT COUNT(*), COALESCE(SUM(\
                    CASE WHEN p.display_derivative_id IS NOT NULL \
                         THEN d.file_size ELSE p.file_size END), 0) \
                 FROM photos p LEFT JOIN derivatives d ON d.id = p.display_derivative_id \
                 WHERE {VISIBLE_STATUS}"
            ),
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let (compressed_count, derivative_bytes): (i64, i64) = self.conn.query_row(
            &format!(
                "SELECT COUNT(DISTINCT d.photo_id), COALESCE(SUM(d.file_size), 0) \
                 FROM derivatives d JOIN photos p ON p.id = d.photo_id \
                 WHERE {VISIBLE_STATUS}"
            ),
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        Ok(LibraryStats {
            photo_count,
            total_bytes: total_bytes.max(0) as u64,
            compressed_count,
            derivative_bytes: derivative_bytes.max(0) as u64,
        })
    }

    // ---------- worlds / sessions ----------

    pub fn upsert_world(
        &self,
        world_id: &str,
        name: Option<&str>,
        source: Option<&str>,
    ) -> Result<i64> {
        let now = now_string();
        let id = self.conn.query_row(
            "INSERT INTO worlds(world_id, name, name_source, first_seen, last_seen) \
             VALUES (?1, ?2, ?3, ?4, ?4) \
             ON CONFLICT(world_id) DO UPDATE SET \
                name = COALESCE(excluded.name, worlds.name), \
                name_source = COALESCE(excluded.name_source, worlds.name_source), \
                last_seen = excluded.last_seen \
             RETURNING id",
            params![world_id, name, source, now],
            |row| row.get(0),
        )?;
        Ok(id)
    }

    pub fn world_by_id(&self, id: i64) -> Result<Option<World>> {
        let world = self
            .conn
            .query_row(
                "SELECT id, world_id, name, name_source, alias FROM worlds WHERE id = ?1",
                params![id],
                |row| {
                    Ok(World {
                        id: row.get(0)?,
                        world_id: row.get(1)?,
                        name: row.get(2)?,
                        name_source: row.get(3)?,
                        alias: row.get(4)?,
                    })
                },
            )
            .optional()?;
        Ok(world)
    }

    pub fn create_session(
        &self,
        log_file: &str,
        started_at: i64,
        world_id: i64,
        instance_id: Option<&str>,
    ) -> Result<i64> {
        let id = self.conn.query_row(
            "INSERT INTO sessions(log_file, started_at, world_id, instance_id) \
             VALUES (?1, ?2, ?3, ?4) RETURNING id",
            params![log_file, started_at, world_id, instance_id],
            |row| row.get(0),
        )?;
        Ok(id)
    }

    pub fn close_session(&self, session_id: i64, ended_at: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET ended_at = ?1 WHERE id = ?2 AND ended_at IS NULL",
            params![ended_at, session_id],
        )?;
        Ok(())
    }

    pub fn session_at(&self, timestamp: i64) -> Result<Option<Session>> {
        let session = self
            .conn
            .query_row(
                "SELECT id, log_file, started_at, ended_at, world_id, instance_id \
                 FROM sessions WHERE started_at <= ?1 AND (ended_at IS NULL OR ?1 < ended_at) \
                 ORDER BY started_at DESC LIMIT 1",
                params![timestamp],
                |row| {
                    Ok(Session {
                        id: row.get(0)?,
                        log_file: row.get(1)?,
                        started_at: row.get(2)?,
                        ended_at: row.get(3)?,
                        world_id: row.get(4)?,
                        instance_id: row.get(5)?,
                    })
                },
            )
            .optional()?;
        Ok(session)
    }

    pub fn upsert_session_player(
        &self,
        session_id: i64,
        user_id: &str,
        display_name: &str,
        joined_at: i64,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO session_players(session_id, user_id, display_name, joined_at) \
             VALUES (?1, ?2, ?3, ?4) \
             ON CONFLICT(session_id, user_id) DO UPDATE SET \
                display_name = excluded.display_name, left_at = NULL",
            params![session_id, user_id, display_name, joined_at],
        )?;
        Ok(())
    }

    pub fn close_session_player(&self, session_id: i64, user_id: &str, left_at: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE session_players SET left_at = ?1 \
             WHERE session_id = ?2 AND user_id = ?3 AND left_at IS NULL",
            params![left_at, session_id, user_id],
        )?;
        Ok(())
    }

    pub fn players_at(&self, session_id: i64, timestamp: i64) -> Result<Vec<SessionPlayer>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, session_id, user_id, display_name, joined_at, left_at \
             FROM session_players WHERE session_id = ?1 AND joined_at <= ?2 \
             AND (left_at IS NULL OR ?2 < left_at) ORDER BY joined_at",
        )?;
        let rows = stmt.query_map(params![session_id, timestamp], |row| {
            Ok(SessionPlayer {
                id: row.get(0)?,
                session_id: row.get(1)?,
                user_id: row.get(2)?,
                display_name: row.get(3)?,
                joined_at: row.get(4)?,
                left_at: row.get(5)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    // ---------- photo context ----------

    pub fn set_photo_context(&self, context: &PhotoContext) -> Result<()> {
        self.conn.execute(
            "INSERT INTO photo_context(photo_id, session_id, world_id, world_name, instance_id, \
                link_method, confidence, author, linked_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9) \
             ON CONFLICT(photo_id) DO UPDATE SET \
                session_id = excluded.session_id, world_id = excluded.world_id, \
                world_name = excluded.world_name, instance_id = excluded.instance_id, \
                link_method = excluded.link_method, confidence = excluded.confidence, \
                author = excluded.author, linked_at = excluded.linked_at",
            params![
                context.photo_id,
                context.session_id,
                context.world_id,
                context.world_name,
                context.instance_id,
                context.link_method,
                context.confidence,
                context.author,
                context.linked_at,
            ],
        )?;
        Ok(())
    }

    pub fn photo_context(&self, photo_id: i64) -> Result<Option<PhotoContext>> {
        let context = self
            .conn
            .query_row(
                "SELECT photo_id, session_id, world_id, world_name, instance_id, \
                    link_method, confidence, author, linked_at FROM photo_context WHERE photo_id = ?1",
                params![photo_id],
                |row| {
                    Ok(PhotoContext {
                        photo_id: row.get(0)?,
                        session_id: row.get(1)?,
                        world_id: row.get(2)?,
                        world_name: row.get(3)?,
                        instance_id: row.get(4)?,
                        link_method: row.get(5)?,
                        confidence: row.get(6)?,
                        author: row.get(7)?,
                        linked_at: row.get(8)?,
                    })
                },
            )
            .optional()?;
        Ok(context)
    }

    pub fn set_photo_players(&self, photo_id: i64, players: &[PhotoPlayer]) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "DELETE FROM photo_players WHERE photo_id = ?1",
            params![photo_id],
        )?;
        for player in players {
            if player.user_id.trim().is_empty() {
                continue;
            }
            tx.execute(
                "INSERT OR REPLACE INTO photo_players(photo_id, user_id, display_name) \
                 VALUES (?1, ?2, ?3)",
                params![photo_id, player.user_id, player.display_name],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn photo_players(&self, photo_id: i64) -> Result<Vec<PhotoPlayer>> {
        let mut stmt = self.conn.prepare(
            "SELECT user_id, display_name FROM photo_players WHERE photo_id = ?1 \
             ORDER BY display_name COLLATE NOCASE",
        )?;
        let rows = stmt.query_map(params![photo_id], |row| {
            Ok(PhotoPlayer {
                user_id: row.get(0)?,
                display_name: row.get(1)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    // ---------- tags ----------

    pub fn add_tag(&self, name: &str) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO tags(name) VALUES (?1) ON CONFLICT(name) DO NOTHING",
            params![name],
        )?;
        let id = self.conn.query_row(
            "SELECT id FROM tags WHERE name = ?1",
            params![name],
            |row| row.get(0),
        )?;
        Ok(id)
    }

    pub fn set_photo_tags(&self, photo_id: i64, tags: &[String]) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "DELETE FROM photo_tags WHERE photo_id = ?1",
            params![photo_id],
        )?;
        for name in tags {
            let name = name.trim();
            if name.is_empty() {
                continue;
            }
            tx.execute(
                "INSERT INTO tags(name) VALUES (?1) ON CONFLICT(name) DO NOTHING",
                params![name],
            )?;
            let tag_id: i64 = tx.query_row(
                "SELECT id FROM tags WHERE name = ?1",
                params![name],
                |row| row.get(0),
            )?;
            tx.execute(
                "INSERT OR IGNORE INTO photo_tags(photo_id, tag_id) VALUES (?1, ?2)",
                params![photo_id, tag_id],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn photo_tags(&self, photo_id: i64) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT t.name FROM tags t JOIN photo_tags pt ON pt.tag_id = t.id \
             WHERE pt.photo_id = ?1 ORDER BY t.name COLLATE NOCASE",
        )?;
        let rows = stmt.query_map(params![photo_id], |row| row.get(0))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    // ---------- compression profiles ----------

    pub fn save_profile(&self, profile: &CompressionProfile) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        let sort_order = Self::next_sort_order(&tx)?;
        Self::write_profile(&tx, profile, sort_order)?;
        tx.commit()?;
        Ok(())
    }

    fn next_sort_order(conn: &Connection) -> Result<i64> {
        Ok(conn.query_row(
            "SELECT COALESCE(MAX(sort_order) + 1, 0) FROM compression_profiles",
            [],
            |row| row.get(0),
        )?)
    }

    fn write_profile(
        conn: &Connection,
        profile: &CompressionProfile,
        sort_order: i64,
    ) -> Result<()> {
        if profile.clipboard {
            Self::clear_other_clipboard_profiles(conn, &profile.id)?;
        }
        let json = serde_json::to_string(profile)?;
        conn.execute(
            "INSERT INTO compression_profiles(id, name, enabled, auto_enabled, json, sort_order) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
             ON CONFLICT(id) DO UPDATE SET name = excluded.name, enabled = excluded.enabled, \
                auto_enabled = excluded.auto_enabled, json = excluded.json",
            params![
                profile.id,
                profile.name,
                profile.enabled as i64,
                profile.auto_enabled as i64,
                json,
                sort_order,
            ],
        )?;
        Ok(())
    }

    /// Keeps the clipboard target unique even when a caller other than the
    /// profile switch saves a flagged profile.
    fn clear_other_clipboard_profiles(conn: &Connection, keep_id: &str) -> Result<()> {
        let mut stmt = conn.prepare("SELECT id, json FROM compression_profiles WHERE id != ?1")?;
        let rows = stmt.query_map(params![keep_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut updates = Vec::new();
        for row in rows {
            let (id, json) = row?;
            let mut profile: CompressionProfile = serde_json::from_str(&json)?;
            if profile.clipboard {
                profile.clipboard = false;
                updates.push((id, serde_json::to_string(&profile)?));
            }
        }
        drop(stmt);
        for (id, json) in updates {
            conn.execute(
                "UPDATE compression_profiles SET json = ?1 WHERE id = ?2",
                params![json, id],
            )?;
        }
        Ok(())
    }

    pub fn list_profiles(&self) -> Result<Vec<CompressionProfile>> {
        let mut stmt = self.conn.prepare(
            "SELECT json FROM compression_profiles ORDER BY sort_order, name COLLATE NOCASE",
        )?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        let mut profiles = Vec::new();
        for row in rows {
            let json = row?;
            profiles.push(serde_json::from_str(&json)?);
        }
        Ok(profiles)
    }

    pub fn reorder_profiles(&self, ids: &[String]) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        let stored: i64 = tx.query_row("SELECT COUNT(*) FROM compression_profiles", [], |row| {
            row.get(0)
        })?;
        if stored != ids.len() as i64 {
            return Err(CoreError::Invalid(
                "profile order must list every profile".to_string(),
            ));
        }
        let mut seen = HashSet::new();
        for id in ids {
            if !seen.insert(id.as_str()) {
                return Err(CoreError::Invalid(
                    "profile order contains duplicate ids".to_string(),
                ));
            }
        }
        for (index, id) in ids.iter().enumerate() {
            let changed = tx.execute(
                "UPDATE compression_profiles SET sort_order = ?1 WHERE id = ?2",
                params![index as i64, id],
            )?;
            if changed != 1 {
                return Err(CoreError::NotFound(format!("compression profile {id}")));
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Generated copy names keep the source name readable but never exceed
    /// the UI's display limit, even when the source is already at the cap.
    fn copy_profile_name(source: &str, suffix: usize) -> String {
        let ending = if suffix <= 1 {
            " のコピー".to_string()
        } else {
            format!(" のコピー {suffix}")
        };
        let keep = MAX_PROFILE_NAME_CHARS
            .saturating_sub(ending.chars().count())
            .max(1);
        let mut name: String = source.chars().take(keep).collect();
        name.push_str(&ending);
        name
    }

    pub fn duplicate_profile(&self, id: &str) -> Result<CompressionProfile> {
        let tx = self.conn.unchecked_transaction()?;
        let ordered = {
            let mut stmt = tx.prepare(
                "SELECT id, json FROM compression_profiles \
                 ORDER BY sort_order, name COLLATE NOCASE",
            )?;
            let rows = stmt.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            let mut ordered = Vec::new();
            for row in rows {
                let (stored_id, json) = row?;
                ordered.push((
                    stored_id,
                    serde_json::from_str::<CompressionProfile>(&json)?,
                ));
            }
            ordered
        };

        let source_index = ordered
            .iter()
            .position(|(stored_id, _)| stored_id == id)
            .ok_or_else(|| CoreError::NotFound(format!("compression profile {id}")))?;

        let source_name = ordered[source_index].1.name.clone();
        let mut suffix = 1;
        let mut name = Self::copy_profile_name(&source_name, suffix);
        while ordered.iter().any(|(_, profile)| profile.name == name) {
            suffix += 1;
            name = Self::copy_profile_name(&source_name, suffix);
        }

        for (index, (stored_id, _)) in ordered.iter().enumerate() {
            tx.execute(
                "UPDATE compression_profiles SET sort_order = ?1 WHERE id = ?2",
                params![index as i64, stored_id],
            )?;
        }
        tx.execute(
            "UPDATE compression_profiles SET sort_order = sort_order + 1 WHERE sort_order > ?1",
            params![source_index as i64],
        )?;

        let copy = CompressionProfile {
            id: uuid::Uuid::new_v4().to_string(),
            name,
            auto_enabled: false,
            // The clipboard target is unique system-wide (the UI switch clears
            // every other profile). A duplicate must not bypass that invariant.
            clipboard: false,
            ..ordered[source_index].1.clone()
        };
        Self::write_profile(&tx, &copy, source_index as i64 + 1)?;
        tx.commit()?;
        Ok(copy)
    }

    /// Seeds the built-in presets when no profile exists.
    ///
    /// Returns the profiles currently stored. A read or write error is returned
    /// without leaving a partial seed: existing rows are never touched, and the
    /// preset inserts commit together.
    pub fn ensure_default_profiles(&self) -> Result<Vec<CompressionProfile>> {
        let existing = self.list_profiles()?;
        if !existing.is_empty() {
            return Ok(existing);
        }
        let transaction = self.conn.unchecked_transaction()?;
        for (index, profile) in CompressionProfile::defaults().into_iter().enumerate() {
            Self::write_profile(&transaction, &profile, index as i64)?;
        }
        transaction.commit()?;
        self.list_profiles()
    }

    pub fn active_job_count_for_profile(&self, profile_id: &str) -> Result<i64> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*) FROM compression_jobs \
             WHERE profile_id = ?1 AND status IN ('waiting', 'pending', 'processing')",
            params![profile_id],
            |row| row.get(0),
        )?)
    }

    pub fn delete_profile(&self, id: &str) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        let exists: i64 = tx.query_row(
            "SELECT COUNT(*) FROM compression_profiles WHERE id = ?1",
            params![id],
            |row| row.get(0),
        )?;
        if exists == 0 {
            return Err(CoreError::NotFound(format!("compression profile {id}")));
        }

        let active: i64 = tx.query_row(
            "SELECT COUNT(*) FROM compression_jobs \
             WHERE profile_id = ?1 AND status IN ('waiting', 'pending', 'processing')",
            params![id],
            |row| row.get(0),
        )?;
        if active > 0 {
            return Err(CoreError::Invalid(format!(
                "compression profile {id} has {active} active jobs"
            )));
        }

        let total: i64 = tx.query_row("SELECT COUNT(*) FROM compression_profiles", [], |row| {
            row.get(0)
        })?;
        if total <= 1 {
            return Err(CoreError::Invalid(
                "at least one compression profile is required".to_string(),
            ));
        }

        let deleted = tx.execute(
            "DELETE FROM compression_profiles WHERE id = ?1",
            params![id],
        )?;
        if deleted == 0 {
            return Err(CoreError::NotFound(format!("compression profile {id}")));
        }
        tx.commit()?;
        Ok(())
    }

    // ---------- deletion requests ----------

    /// Records one durable deletion request per photo. Manual requests hide the
    /// photo immediately; auto-replace requests leave `replaced` visible until
    /// the executor removes only the original file.
    pub fn request_deletion(
        &self,
        photo_ids: &[i64],
        reason: DeletionReason,
        force: bool,
    ) -> Result<Vec<i64>> {
        if reason == DeletionReason::Unknown {
            return Err(CoreError::Invalid(
                "cannot record an unknown deletion reason".to_string(),
            ));
        }
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let now = now_string();
        let mut request_ids = Vec::new();
        for photo_id in photo_ids {
            let exists: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM photos WHERE id = ?1)",
                params![photo_id],
                |row| row.get(0),
            )?;
            if !exists {
                continue;
            }

            let active = tx
                .query_row(
                    "SELECT id, status, reason, force FROM deletion_requests \
                     WHERE photo_id = ?1 AND status IN ('pending', 'blocked', 'running')",
                    params![photo_id],
                    |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, i64>(3)? != 0,
                        ))
                    },
                )
                .optional()?;

            let request_id = if let Some((id, status, current_reason, current_force)) = active {
                if status == "running" {
                    return Err(CoreError::Invalid(
                        "deletion is already running".to_string(),
                    ));
                }
                let next_reason = if reason == DeletionReason::Manual {
                    "manual"
                } else {
                    current_reason.as_str()
                };
                let next_force = current_force || force;
                let next_status = if next_force && status == "blocked" {
                    "pending"
                } else {
                    status.as_str()
                };
                tx.execute(
                    "UPDATE deletion_requests SET reason = ?1, force = ?2, status = ?3 \
                     WHERE id = ?4",
                    params![next_reason, next_force as i64, next_status, id],
                )?;
                id
            } else {
                tx.execute(
                    "INSERT INTO deletion_requests \
                        (photo_id, reason, status, force, requested_at) \
                     VALUES (?1, ?2, 'pending', ?3, ?4)",
                    params![photo_id, reason.as_str(), force as i64, now],
                )?;
                tx.last_insert_rowid()
            };

            if reason == DeletionReason::Manual {
                tx.execute(
                    "UPDATE photos SET status = 'deleting', updated_at = ?1 WHERE id = ?2",
                    params![now, photo_id],
                )?;
            }
            request_ids.push(request_id);
        }
        tx.commit()?;
        Ok(request_ids)
    }

    pub fn active_deletion_request(&self, photo_id: i64) -> Result<Option<DeletionRequest>> {
        let sql = "SELECT dr.id, dr.photo_id, p.file_name, dr.reason, dr.status, dr.force, \
                dr.blocked_reason, dr.error, dr.requested_at, dr.started_at, dr.finished_at \
             FROM deletion_requests dr JOIN photos p ON p.id = dr.photo_id \
             WHERE dr.photo_id = ?1 AND dr.status IN ('pending', 'blocked', 'running') \
             ORDER BY dr.id DESC LIMIT 1";
        let mut stmt = self.conn.prepare(sql)?;
        Ok(stmt
            .query_row(params![photo_id], row_to_deletion_request)
            .optional()?)
    }

    pub fn active_deletion_requests(&self, limit: i64) -> Result<Vec<DeletionRequest>> {
        let sql = "SELECT dr.id, dr.photo_id, p.file_name, dr.reason, dr.status, dr.force, \
                dr.blocked_reason, dr.error, dr.requested_at, dr.started_at, dr.finished_at \
             FROM deletion_requests dr JOIN photos p ON p.id = dr.photo_id \
             WHERE dr.status IN ('pending', 'blocked', 'running') \
             ORDER BY dr.requested_at, dr.id LIMIT ?1";
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt.query_map(params![limit.max(1)], row_to_deletion_request)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// True when a forced deletion for the photo is waiting to run. The
    /// compression batch uses this between jobs to cancel the remainder.
    pub fn photo_has_forced_deletion(&self, photo_id: i64) -> Result<bool> {
        let exists: i64 = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM deletion_requests \
             WHERE photo_id = ?1 AND force = 1 AND status IN ('pending', 'blocked'))",
            params![photo_id],
            |row| row.get(0),
        )?;
        Ok(exists != 0)
    }

    pub fn deletion_counts(&self) -> Result<DeletionCounts> {
        let (pending, blocked, running): (i64, i64, i64) = self.conn.query_row(
            "SELECT COALESCE(SUM(status = 'pending'), 0), \
                    COALESCE(SUM(status = 'blocked'), 0), \
                    COALESCE(SUM(status = 'running'), 0) \
             FROM deletion_requests",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        Ok(DeletionCounts {
            pending,
            blocked,
            running,
        })
    }

    /// Counts compression jobs per status for the warning modal. The `send`
    /// bucket stays empty until #142 adds the outbox.
    pub fn deletion_job_summary(&self, photo_ids: &[i64]) -> Result<DeletionJobSummary> {
        let mut compression = JobStatusCounts::default();
        let mut affected_photos = 0;
        for chunk in photo_ids.chunks(500) {
            if chunk.is_empty() {
                continue;
            }
            let placeholders = vec!["?"; chunk.len()].join(",");
            let sql = format!(
                "SELECT status, COUNT(*) FROM compression_jobs \
                 WHERE photo_id IN ({placeholders}) GROUP BY status"
            );
            let mut stmt = self.conn.prepare(&sql)?;
            let rows = stmt.query_map(params_from_iter(chunk.iter().copied()), |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })?;
            for row in rows {
                let (status, count) = row?;
                match QueueStatus::parse(&status) {
                    QueueStatus::Waiting => compression.waiting += count,
                    QueueStatus::Pending => compression.pending += count,
                    QueueStatus::Processing => compression.processing += count,
                    QueueStatus::Failed => compression.failed += count,
                    QueueStatus::Completed | QueueStatus::Cancelled => {}
                }
            }
            affected_photos += self.conn.query_row(
                &format!(
                    "SELECT COUNT(DISTINCT photo_id) FROM compression_jobs \
                     WHERE photo_id IN ({placeholders}) \
                       AND status IN ('waiting', 'pending', 'processing', 'failed')"
                ),
                params_from_iter(chunk.iter().copied()),
                |row| row.get::<_, i64>(0),
            )?;
        }
        Ok(DeletionJobSummary {
            compression,
            send: JobStatusCounts::default(),
            affected_photos,
        })
    }

    /// Counts work that still blocks a deletion. The #142 outbox will add its
    /// own active/failed counts here.
    pub fn deletion_blockers(&self, photo_id: i64) -> Result<DeletionBlockers> {
        let blockers = self.conn.query_row(
            "SELECT \
                (SELECT COUNT(*) FROM compression_jobs \
                 WHERE photo_id = ?1 AND status IN ('waiting', 'pending', 'processing')), \
                (SELECT COUNT(*) FROM compression_jobs \
                 WHERE photo_id = ?1 AND status = 'failed')",
            params![photo_id],
            |row| {
                Ok(DeletionBlockers {
                    active: row.get(0)?,
                    failed: row.get(1)?,
                })
            },
        )?;
        Ok(blockers)
    }

    /// Pending requests the executor can try now, ordered so manual requests
    /// come first: a manual deletion must not wait behind any auto_replace
    /// backlog, and the row limit must not hide it. Each group keeps
    /// `requested_at` order. Known reasons with an active compression job are
    /// skipped so they cannot starve ready requests behind the row limit,
    /// while unknown reasons are always returned so they can be blocked
    /// immediately. Requests with failed jobs stay in the list: the executor
    /// still needs to transition them to `blocked`.
    pub fn pending_deletion_requests(&self, limit: i64) -> Result<Vec<DeletionRequest>> {
        let sql = "SELECT dr.id, dr.photo_id, p.file_name, dr.reason, dr.status, dr.force, \
                dr.blocked_reason, dr.error, dr.requested_at, dr.started_at, dr.finished_at \
             FROM deletion_requests dr JOIN photos p ON p.id = dr.photo_id \
             WHERE dr.status = 'pending' \
               AND ( \
                   dr.reason NOT IN ('auto_replace', 'manual') \
                   OR dr.photo_id NOT IN ( \
                       SELECT j.photo_id FROM compression_jobs j \
                       WHERE j.status IN ('waiting', 'pending', 'processing') \
                   ) \
               ) \
             ORDER BY (dr.reason = 'manual') DESC, dr.requested_at, dr.id LIMIT ?1";
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt.query_map(params![limit.max(1)], row_to_deletion_request)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn deletion_request_by_id(&self, request_id: i64) -> Result<Option<DeletionRequest>> {
        let sql = "SELECT dr.id, dr.photo_id, p.file_name, dr.reason, dr.status, dr.force, \
                dr.blocked_reason, dr.error, dr.requested_at, dr.started_at, dr.finished_at \
             FROM deletion_requests dr JOIN photos p ON p.id = dr.photo_id \
             WHERE dr.id = ?1";
        let mut stmt = self.conn.prepare(sql)?;
        Ok(stmt
            .query_row(params![request_id], row_to_deletion_request)
            .optional()?)
    }

    /// Atomically claims a pending request and returns the current row. The
    /// caller must execute from the returned object, not from an earlier
    /// snapshot: a UI request may have upgraded `auto_replace` to `manual`
    /// between the snapshot and this call.
    pub fn claim_deletion(&self, request_id: i64) -> Result<Option<DeletionRequest>> {
        let changed = self.conn.execute(
            "UPDATE deletion_requests \
             SET status = 'running', started_at = ?1, blocked_reason = NULL, error = NULL \
             WHERE id = ?2 AND status = 'pending' \
               AND NOT EXISTS ( \
                   SELECT 1 FROM compression_jobs j \
                   WHERE j.photo_id = deletion_requests.photo_id \
                     AND j.status IN ('waiting', 'pending', 'processing') \
               ) \
               AND ( \
                   force = 1 OR NOT EXISTS ( \
                       SELECT 1 FROM compression_jobs j \
                       WHERE j.photo_id = deletion_requests.photo_id \
                         AND j.status = 'failed' \
                   ) \
               )",
            params![now_string(), request_id],
        )?;
        if changed == 0 {
            return Ok(None);
        }
        self.deletion_request_by_id(request_id)
    }

    /// Atomically marks a pending request blocked when a failed job exists and
    /// the request is not forced. Keeping this in SQL avoids acting on a stale
    /// `force` value from the executor's snapshot.
    pub fn block_deletion_if_jobs_failed(&self, request_id: i64) -> Result<bool> {
        let changed = self.conn.execute(
            "UPDATE deletion_requests \
             SET status = 'blocked', blocked_reason = 'job_failed', error = NULL \
             WHERE id = ?1 AND status = 'pending' AND force = 0 \
               AND NOT EXISTS ( \
                   SELECT 1 FROM compression_jobs j \
                   WHERE j.photo_id = deletion_requests.photo_id \
                     AND j.status IN ('waiting', 'pending', 'processing') \
               ) \
               AND EXISTS ( \
                   SELECT 1 FROM compression_jobs j \
                   WHERE j.photo_id = deletion_requests.photo_id \
                     AND j.status = 'failed' \
               )",
            params![request_id],
        )?;
        Ok(changed == 1)
    }

    /// Atomically marks a pending request blocked when its stored reason is
    /// still not a known one. Keeping this in SQL avoids acting on a stale
    /// snapshot: a request the UI upgraded to `manual` after the executor
    /// collected it must not be blocked as `unknown_reason`.
    pub fn block_deletion_if_unknown(&self, request_id: i64) -> Result<bool> {
        let changed = self.conn.execute(
            "UPDATE deletion_requests \
             SET status = 'blocked', blocked_reason = 'unknown_reason', error = NULL \
             WHERE id = ?1 AND status = 'pending' \
               AND reason NOT IN ('auto_replace', 'manual')",
            params![request_id],
        )?;
        Ok(changed == 1)
    }

    pub fn block_deletion(
        &self,
        request_id: i64,
        reason: &str,
        error: Option<&str>,
    ) -> Result<bool> {
        let changed = self.conn.execute(
            "UPDATE deletion_requests SET status = 'blocked', blocked_reason = ?1, error = ?2 \
             WHERE id = ?3 AND status IN ('pending', 'blocked', 'running')",
            params![reason, error, request_id],
        )?;
        Ok(changed == 1)
    }

    pub fn complete_deletion(&self, request_id: i64) -> Result<bool> {
        let changed = self.conn.execute(
            "UPDATE deletion_requests \
             SET status = 'completed', finished_at = ?1, blocked_reason = NULL, error = NULL \
             WHERE id = ?2 AND status = 'running'",
            params![now_string(), request_id],
        )?;
        Ok(changed == 1)
    }

    pub fn retry_deletion(&self, request_id: i64) -> Result<bool> {
        let changed = self.conn.execute(
            "UPDATE deletion_requests \
             SET status = 'pending', blocked_reason = NULL, error = NULL \
             WHERE id = ?1 AND status = 'blocked'",
            params![request_id],
        )?;
        Ok(changed == 1)
    }

    /// Ignores failed jobs and cancels queued work. A running compression job
    /// still has to finish before `claim_deletion` can advance.
    pub fn force_deletion(&self, request_id: i64) -> Result<bool> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let request: Option<(i64, String)> = tx
            .query_row(
                "SELECT photo_id, status FROM deletion_requests WHERE id = ?1",
                params![request_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((photo_id, status)) = request else {
            return Ok(false);
        };
        if status != "pending" && status != "blocked" {
            return Ok(false);
        }
        let now = now_string();
        tx.execute(
            "UPDATE deletion_requests \
             SET force = 1, status = 'pending', blocked_reason = NULL, error = NULL \
             WHERE id = ?1",
            params![request_id],
        )?;
        tx.execute(
            "UPDATE compression_jobs SET status = 'cancelled', progress = 0, finished_at = ?1 \
             WHERE photo_id = ?2 AND status IN ('waiting', 'pending')",
            params![now, photo_id],
        )?;
        tx.commit()?;
        Ok(true)
    }

    pub fn cancel_deletion(&self, request_id: i64) -> Result<bool> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let request: Option<(i64, String, String, Option<String>)> = tx
            .query_row(
                "SELECT photo_id, reason, status, started_at FROM deletion_requests WHERE id = ?1",
                params![request_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;
        let Some((photo_id, reason, status, started_at)) = request else {
            return Ok(false);
        };
        if reason != "manual"
            || (status != "pending" && status != "blocked")
            || started_at.is_some()
        {
            return Ok(false);
        }
        let now = now_string();
        tx.execute(
            "UPDATE deletion_requests SET status = 'cancelled', finished_at = ?1 WHERE id = ?2",
            params![now, request_id],
        )?;
        if reason == "manual" {
            tx.execute(
                "UPDATE photos SET \
                    status = CASE WHEN display_derivative_id IS NOT NULL \
                                  THEN 'replaced' ELSE 'present' END, \
                    updated_at = ?1 \
                 WHERE id = ?2 AND status = 'deleting'",
                params![now, photo_id],
            )?;
        }
        tx.commit()?;
        Ok(true)
    }

    /// Finishes the database half of a manual deletion after every file step
    /// succeeded. The photo row stays for metadata recovery; derivative rows
    /// are removed because their files are in the OS trash.
    pub fn complete_manual_deletion(&self, photo_id: i64) -> Result<()> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        tx.execute(
            "DELETE FROM derivatives WHERE photo_id = ?1",
            params![photo_id],
        )?;
        tx.execute(
            "UPDATE photos SET status = 'missing', display_derivative_id = NULL, updated_at = ?1 \
             WHERE id = ?2 AND status = 'deleting'",
            params![now_string(), photo_id],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn reset_running_deletions(&self) -> Result<usize> {
        // Keep started_at: it is the only evidence that a crash may have left
        // some files already in the OS trash, and cancel must refuse then.
        let changed = self.conn.execute(
            "UPDATE deletion_requests SET status = 'pending' WHERE status = 'running'",
            [],
        )?;
        Ok(changed)
    }

    /// Returns a job-failure block to the queue once every job for the photo is
    /// terminal success. File-error blocks stay blocked until the user retries.
    pub fn resume_blocked_deletions_for_photo(&self, photo_id: i64) -> Result<usize> {
        let changed = self.conn.execute(
            "UPDATE deletion_requests \
             SET status = 'pending', blocked_reason = NULL, error = NULL \
             WHERE photo_id = ?1 AND status = 'blocked' AND blocked_reason = 'job_failed' \
               AND NOT EXISTS ( \
                   SELECT 1 FROM compression_jobs j \
                   WHERE j.photo_id = ?1 \
                     AND j.status IN ('waiting', 'pending', 'processing', 'failed') \
               )",
            params![photo_id],
        )?;
        Ok(changed)
    }

    // ---------- compression jobs ----------

    pub fn enqueue_job(&self, photo_id: i64, profile_id: &str, policy: &str) -> Result<String> {
        self.enqueue_job_with_options(photo_id, profile_id, policy, QueueStatus::Pending, None)
    }

    pub fn enqueue_job_with_status(
        &self,
        photo_id: i64,
        profile_id: &str,
        policy: &str,
        status: QueueStatus,
    ) -> Result<String> {
        self.enqueue_job_with_options(photo_id, profile_id, policy, status, None)
    }

    /// Enqueues a manual job whose output directory overrides the profile.
    pub fn enqueue_job_with_output_dir(
        &self,
        photo_id: i64,
        profile_id: &str,
        policy: &str,
        output_dir: Option<&str>,
    ) -> Result<String> {
        self.enqueue_job_with_options(
            photo_id,
            profile_id,
            policy,
            QueueStatus::Pending,
            output_dir,
        )
    }

    fn enqueue_job_with_options(
        &self,
        photo_id: i64,
        profile_id: &str,
        policy: &str,
        status: QueueStatus,
        output_dir: Option<&str>,
    ) -> Result<String> {
        let id = uuid::Uuid::new_v4().to_string();
        let inserted = self.conn.execute(
            "INSERT INTO compression_jobs(id, photo_id, profile_id, status, progress, policy, output_dir, created_at) \
             SELECT ?1, ?2, ?3, ?4, 0, ?5, ?6, ?7 \
             WHERE EXISTS (SELECT 1 FROM photos WHERE id = ?2 AND status <> 'deleting') \
               AND NOT EXISTS (SELECT 1 FROM deletion_requests \
                   WHERE photo_id = ?2 AND status IN ('pending', 'blocked', 'running'))",
            params![
                id,
                photo_id,
                profile_id,
                status.as_str(),
                policy,
                output_dir,
                now_string()
            ],
        )?;
        if inserted == 0 {
            return Err(CoreError::Invalid("photo is pending deletion".to_string()));
        }
        Ok(id)
    }

    pub fn enqueue_job_for_existing_profile(
        &self,
        photo_id: i64,
        profile_id: &str,
        policy: &str,
    ) -> Result<bool> {
        self.enqueue_job_for_existing_profile_with_options(
            photo_id,
            profile_id,
            policy,
            QueueStatus::Pending,
            None,
        )
    }

    pub fn enqueue_job_for_existing_profile_with_status(
        &self,
        photo_id: i64,
        profile_id: &str,
        policy: &str,
        status: QueueStatus,
    ) -> Result<bool> {
        self.enqueue_job_for_existing_profile_with_options(
            photo_id, profile_id, policy, status, None,
        )
    }

    /// Enqueues a manual job for an existing profile with an optional output
    /// directory override.
    pub fn enqueue_job_for_existing_profile_with_output_dir(
        &self,
        photo_id: i64,
        profile_id: &str,
        policy: &str,
        output_dir: Option<&str>,
    ) -> Result<bool> {
        self.enqueue_job_for_existing_profile_with_options(
            photo_id,
            profile_id,
            policy,
            QueueStatus::Pending,
            output_dir,
        )
    }

    fn enqueue_job_for_existing_profile_with_options(
        &self,
        photo_id: i64,
        profile_id: &str,
        policy: &str,
        status: QueueStatus,
        output_dir: Option<&str>,
    ) -> Result<bool> {
        let id = uuid::Uuid::new_v4().to_string();
        let inserted = self.conn.execute(
            "INSERT INTO compression_jobs(id, photo_id, profile_id, status, progress, policy, output_dir, created_at) \
             SELECT ?1, ?2, ?3, ?4, 0, ?5, ?6, ?7 \
             WHERE EXISTS (SELECT 1 FROM compression_profiles WHERE id = ?3) \
               AND EXISTS (SELECT 1 FROM photos WHERE id = ?2 AND status <> 'deleting') \
               AND NOT EXISTS (SELECT 1 FROM deletion_requests \
                   WHERE photo_id = ?2 AND status IN ('pending', 'blocked', 'running'))",
            params![
                id,
                photo_id,
                profile_id,
                status.as_str(),
                policy,
                output_dir,
                now_string()
            ],
        )?;
        Ok(inserted == 1)
    }

    pub fn next_pending_job(&self) -> Result<Option<CompressionJob>> {
        let job = self
            .conn
            .query_row(
                "UPDATE compression_jobs SET status = 'processing', started_at = ?1 \
                 WHERE id = (SELECT id FROM compression_jobs WHERE status = 'pending' \
                    ORDER BY created_at, rowid LIMIT 1) \
                 RETURNING id, photo_id, profile_id, status, progress, error, output_path, policy, output_dir, \
                    retry_count, created_at, started_at, finished_at",
                params![now_string()],
                row_to_job,
            )
            .optional()?;
        Ok(job)
    }

    /// Claims every pending job for the photo owning the oldest pending job.
    ///
    /// The immediate transaction keeps the photo selection and the claim
    /// atomic across connections (the daemon and a maintenance CLI can run
    /// at the same time), so a claimer never sees an empty batch while
    /// another connection still has pending work.
    pub fn next_pending_photo_jobs(&self) -> Result<Vec<CompressionJob>> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let photo_id: Option<i64> = tx
            .query_row(
                "SELECT photo_id FROM compression_jobs WHERE status = 'pending' \
                 ORDER BY created_at, rowid LIMIT 1",
                [],
                |row| row.get(0),
            )
            .optional()?;
        let Some(photo_id) = photo_id else {
            tx.commit()?;
            return Ok(Vec::new());
        };
        let mut jobs = {
            let mut stmt = tx.prepare(
                "UPDATE compression_jobs SET status = 'processing', started_at = ?1 \
                 WHERE status = 'pending' AND photo_id = ?2 \
                 RETURNING id, photo_id, profile_id, status, progress, error, output_path, policy, output_dir, \
                    retry_count, created_at, started_at, finished_at, rowid",
            )?;
            let rows = stmt.query_map(params![now_string(), photo_id], |row| {
                Ok((row.get::<_, i64>(13)?, row_to_job(row)?))
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        tx.commit()?;
        jobs.sort_by_key(|(rowid, _)| *rowid);
        Ok(jobs.into_iter().map(|(_, job)| job).collect())
    }

    /// Marks a pending or processing job failed. Terminal jobs are left
    /// untouched and their current status is returned instead.
    pub fn fail_job_if_active(&self, id: &str, error: &str) -> Result<Option<QueueStatus>> {
        let changed = self.conn.execute(
            "UPDATE compression_jobs SET status = 'failed', progress = 0, error = ?1, \
                finished_at = ?2 WHERE id = ?3 AND status IN ('pending', 'processing')",
            params![error, now_string(), id],
        )?;
        if changed == 1 {
            return Ok(Some(QueueStatus::Failed));
        }
        let status = self
            .conn
            .query_row(
                "SELECT status FROM compression_jobs WHERE id = ?1",
                params![id],
                |row| Ok(QueueStatus::parse(&row.get::<_, String>(0)?)),
            )
            .optional()?;
        Ok(status)
    }

    pub fn update_job(
        &self,
        id: &str,
        status: QueueStatus,
        progress: f64,
        error: Option<&str>,
        output_path: Option<&str>,
    ) -> Result<()> {
        let finished_at = if matches!(
            status,
            QueueStatus::Completed | QueueStatus::Failed | QueueStatus::Cancelled
        ) {
            Some(now_string())
        } else {
            None
        };
        self.conn.execute(
            "UPDATE compression_jobs SET status = ?1, progress = ?2, error = ?3, \
                output_path = ?4, finished_at = COALESCE(?5, finished_at) WHERE id = ?6",
            params![
                status.as_str(),
                progress,
                error,
                output_path,
                finished_at,
                id,
            ],
        )?;
        Ok(())
    }

    pub fn list_jobs(&self, limit: i64) -> Result<Vec<CompressionJob>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, photo_id, profile_id, status, progress, error, output_path, policy, \
                output_dir, retry_count, created_at, started_at, finished_at FROM compression_jobs \
             ORDER BY created_at DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit.max(1)], row_to_job)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn list_jobs_by_status(
        &self,
        status: QueueStatus,
        limit: i64,
    ) -> Result<Vec<CompressionJob>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, photo_id, profile_id, status, progress, error, output_path, policy, \
                output_dir, retry_count, created_at, started_at, finished_at FROM compression_jobs \
             WHERE status = ?1 ORDER BY created_at DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![status.as_str(), limit.max(1)], row_to_job)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Promotes every waiting job to pending so workers can pick it up,
    /// restamping the current global keep/replace policy. Returns how many
    /// rows changed.
    pub fn release_waiting_jobs(&self, policy: &str) -> Result<usize> {
        let changed = self.conn.execute(
            "UPDATE compression_jobs SET status = 'pending', progress = 0, error = NULL, \
                started_at = NULL, finished_at = NULL, policy = ?1 \
             WHERE status = 'waiting'",
            params![policy],
        )?;
        Ok(changed)
    }

    /// Returns a failed job to the queue once, incrementing its retry count.
    /// Returns false when the job is missing, not failed, or already at the
    /// retry cap.
    pub fn retry_job(&self, id: &str) -> Result<bool> {
        let changed = self.conn.execute(
            "UPDATE compression_jobs SET status = 'pending', progress = 0, error = NULL, \
                started_at = NULL, finished_at = NULL, retry_count = retry_count + 1 \
             WHERE id = ?1 AND status = 'failed' AND retry_count < ?2 \
               AND NOT EXISTS (SELECT 1 FROM deletion_requests dr \
                   WHERE dr.photo_id = compression_jobs.photo_id AND dr.status = 'running')",
            params![id, MAX_JOB_RETRIES],
        )?;
        Ok(changed == 1)
    }

    /// Returns every eligible failed job to the queue and reports how many
    /// rows changed. Jobs at the retry cap are left untouched.
    pub fn retry_failed_jobs(&self) -> Result<usize> {
        let changed = self.conn.execute(
            "UPDATE compression_jobs SET status = 'pending', progress = 0, error = NULL, \
                started_at = NULL, finished_at = NULL, retry_count = retry_count + 1 \
             WHERE status = 'failed' AND retry_count < ?1 \
               AND NOT EXISTS (SELECT 1 FROM deletion_requests dr \
                   WHERE dr.photo_id = compression_jobs.photo_id AND dr.status = 'running')",
            params![MAX_JOB_RETRIES],
        )?;
        Ok(changed)
    }

    pub fn count_jobs(&self, status: QueueStatus) -> Result<i64> {
        let count = self.conn.query_row(
            "SELECT COUNT(*) FROM compression_jobs WHERE status = ?1",
            params![status.as_str()],
            |row| row.get(0),
        )?;
        Ok(count)
    }

    /// Counts jobs a user still expects to run or finish: waiting, pending,
    /// and processing. Failed and completed jobs are excluded.
    pub fn count_active_jobs(&self) -> Result<i64> {
        let count = self.conn.query_row(
            "SELECT COUNT(*) FROM compression_jobs \
             WHERE status IN ('waiting', 'pending', 'processing')",
            [],
            |row| row.get(0),
        )?;
        Ok(count)
    }

    // ---------- daemon errors and commands ----------

    /// Records a daemon failure for `operation`, replacing any previous
    /// message/detail/time and incrementing the attempt count.
    pub fn record_daemon_error(
        &self,
        operation: DaemonOperation,
        message: &str,
        detail: Option<&str>,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO daemon_errors(operation, message, detail, occurred_at, attempts) \
             VALUES (?1, ?2, ?3, ?4, 1) \
             ON CONFLICT(operation) DO UPDATE SET message = excluded.message, \
                detail = excluded.detail, occurred_at = excluded.occurred_at, \
                attempts = daemon_errors.attempts + 1",
            params![operation.as_str(), message, detail, now_string()],
        )?;
        Ok(())
    }

    /// Removes the stored error for `operation`, if any.
    pub fn clear_daemon_error(&self, operation: DaemonOperation) -> Result<()> {
        self.conn.execute(
            "DELETE FROM daemon_errors WHERE operation = ?1",
            params![operation.as_str()],
        )?;
        Ok(())
    }

    /// Returns every stored daemon error, newest first. Rows whose operation
    /// string is unknown to this build are skipped.
    pub fn daemon_errors(&self) -> Result<Vec<DaemonError>> {
        let mut stmt = self.conn.prepare(
            "SELECT operation, message, detail, occurred_at, attempts FROM daemon_errors \
             ORDER BY occurred_at DESC, operation",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, i64>(4)?,
            ))
        })?;
        let mut errors = Vec::new();
        for row in rows {
            let (operation, message, detail, occurred_at, attempts) = row?;
            let Some(operation) = DaemonOperation::parse(&operation) else {
                continue;
            };
            errors.push(DaemonError {
                operation,
                message,
                detail,
                occurred_at,
                attempts,
            });
        }
        Ok(errors)
    }

    /// Inserts `command` unless the same command and payload are already
    /// queued. Returns whether a row was inserted.
    pub fn request_daemon_command(&self, command: DaemonCommand) -> Result<bool> {
        let kind = command.as_str();
        let payload = command.payload();
        let changed = self.conn.execute(
            "INSERT INTO daemon_commands(command, payload, created_at) \
             SELECT ?1, ?2, ?3 WHERE NOT EXISTS \
                (SELECT 1 FROM daemon_commands WHERE command = ?1 AND payload IS ?2)",
            params![kind, payload, now_string()],
        )?;
        Ok(changed == 1)
    }

    /// Queues a scan of one photo root. Multiple imports into the same root
    /// collapse into one request; different roots stay separate.
    pub fn request_daemon_scan_root(&self, path: &str) -> Result<bool> {
        let changed = self.conn.execute(
            "INSERT INTO daemon_commands(command, payload, created_at) \
             SELECT 'scan_root', ?1, ?2 WHERE NOT EXISTS \
                (SELECT 1 FROM daemon_commands WHERE command = 'scan_root' AND payload = ?1)",
            params![path, now_string()],
        )?;
        Ok(changed == 1)
    }

    /// Atomically removes and returns every queued command in request order.
    /// Unknown command strings are discarded.
    pub fn take_daemon_commands(&self) -> Result<Vec<DaemonCommand>> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let mut commands = Vec::new();
        {
            let mut stmt =
                tx.prepare("SELECT command, payload FROM daemon_commands ORDER BY id")?;
            let rows = stmt.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
            })?;
            for row in rows {
                let (command, payload) = row?;
                if let Some(command) = DaemonCommand::parse(&command, payload.as_deref()) {
                    commands.push(command);
                }
            }
        }
        tx.execute("DELETE FROM daemon_commands", [])?;
        tx.commit()?;
        Ok(commands)
    }

    // ---------- derivatives ----------

    #[allow(clippy::too_many_arguments)]
    pub fn add_derivative(
        &self,
        photo_id: i64,
        path: &str,
        format: &str,
        width: u32,
        height: u32,
        file_size: u64,
        profile_id: &str,
        policy: &str,
    ) -> Result<i64> {
        let id = self.conn.query_row(
            "INSERT INTO derivatives(photo_id, path, format, width, height, file_size, profile_id, policy, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9) \
             ON CONFLICT(path) DO UPDATE SET file_size = excluded.file_size, \
                width = excluded.width, height = excluded.height RETURNING id",
            params![
                photo_id,
                path,
                format,
                width as i64,
                height as i64,
                file_size as i64,
                profile_id,
                policy,
                now_string(),
            ],
            |row| row.get(0),
        )?;
        Ok(id)
    }

    pub fn compressed_photo_ids(&self) -> Result<Vec<i64>> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT d.photo_id FROM derivatives d \
                 JOIN photos p ON p.id = d.photo_id WHERE p.status IN ('present', 'replaced')",
        )?;
        let rows = stmt.query_map([], |row| row.get(0))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn derivatives_for_photo(&self, photo_id: i64) -> Result<Vec<Derivative>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, photo_id, path, format, width, height, file_size, profile_id, policy, created_at \
             FROM derivatives WHERE photo_id = ?1 ORDER BY created_at DESC",
        )?;
        let rows = stmt.query_map(params![photo_id], |row| {
            Ok(Derivative {
                id: row.get(0)?,
                photo_id: row.get(1)?,
                path: row.get(2)?,
                format: row.get(3)?,
                width: row.get(4)?,
                height: row.get(5)?,
                file_size: row.get::<_, i64>(6)? as u64,
                profile_id: row.get(7)?,
                policy: row.get(8)?,
                created_at: row.get(9)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    // ---------- log imports / settings ----------

    pub fn log_offset(&self, log_file: &str) -> Result<Option<i64>> {
        let offset = self
            .conn
            .query_row(
                "SELECT last_offset FROM log_imports WHERE log_file = ?1",
                params![log_file],
                |row| row.get(0),
            )
            .optional()?;
        Ok(offset)
    }

    pub fn set_log_offset(
        &self,
        log_file: &str,
        size: i64,
        mtime: i64,
        last_offset: i64,
        parser_version: &str,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO log_imports(log_file, size, mtime, last_offset, parser_version, parsed_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
             ON CONFLICT(log_file) DO UPDATE SET size = excluded.size, mtime = excluded.mtime, \
                last_offset = excluded.last_offset, parser_version = excluded.parser_version, \
                parsed_at = excluded.parsed_at",
            params![log_file, size, mtime, last_offset, parser_version, now_string()],
        )?;
        Ok(())
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO app_settings(key, value) VALUES (?1, ?2) \
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn get_setting(&self, key: &str) -> Result<Option<String>> {
        let value = self
            .conn
            .query_row(
                "SELECT value FROM app_settings WHERE key = ?1",
                params![key],
                |row| row.get(0),
            )
            .optional()?;
        Ok(value)
    }
}

fn photo_order(sort: PhotoSort) -> &'static str {
    match sort {
        PhotoSort::Newest => "p.captured_at_utc DESC, p.id DESC",
        PhotoSort::Oldest => "p.captured_at_utc ASC, p.id ASC",
        PhotoSort::Largest => "p.file_size DESC, p.id DESC",
        PhotoSort::World => "c.world_name COLLATE NOCASE ASC, p.captured_at_utc DESC",
    }
}

fn build_photo_filter(filter: &PhotoFilter) -> (String, Vec<Value>) {
    let mut clauses = vec![VISIBLE_STATUS.to_string()];
    let mut values = Vec::new();

    if !filter.world_ids.is_empty() {
        let placeholders = vec!["?"; filter.world_ids.len()].join(",");
        clauses.push(format!("c.world_id IN ({placeholders})"));
        values.extend(filter.world_ids.iter().map(|id| Value::Integer(*id)));
    }

    if !filter.player_user_ids.is_empty() {
        let placeholders = vec!["?"; filter.player_user_ids.len()].join(",");
        clauses.push(format!(
            "EXISTS (SELECT 1 FROM photo_players pp WHERE pp.photo_id = p.id \
             AND pp.user_id IN ({placeholders}))"
        ));
        values.extend(
            filter
                .player_user_ids
                .iter()
                .map(|id| Value::Text(id.clone())),
        );
    }

    if !filter.tags.is_empty() {
        let placeholders = vec!["?"; filter.tags.len()].join(",");
        clauses.push(format!(
            "EXISTS (SELECT 1 FROM photo_tags pt JOIN tags t ON t.id = pt.tag_id \
             WHERE pt.photo_id = p.id AND t.name IN ({placeholders}))"
        ));
        values.extend(filter.tags.iter().map(|tag| Value::Text(tag.clone())));
    }

    if let Some(from) = filter.from_utc {
        clauses.push("p.captured_at_utc >= ?".to_string());
        values.push(Value::Integer(from));
    }
    if let Some(to) = filter.to_utc {
        clauses.push("p.captured_at_utc <= ?".to_string());
        values.push(Value::Integer(to));
    }
    if filter.favorite == Some(true) {
        clauses.push("p.is_favorite = 1".to_string());
    }
    if filter.unlinked == Some(true) {
        clauses.push("c.photo_id IS NULL".to_string());
    }

    (clauses.join(" AND "), values)
}

fn build_text_filter(
    world: Option<&str>,
    player: Option<&str>,
    tag: Option<&str>,
    favorite: bool,
    from_utc: Option<i64>,
    to_utc: Option<i64>,
) -> (String, Vec<Value>) {
    let mut clauses = vec![VISIBLE_STATUS.to_string()];
    let mut values: Vec<Value> = Vec::new();

    if let Some(query) = world.map(str::trim).filter(|query| !query.is_empty()) {
        let like = format!("%{query}%");
        clauses.push(
            "(c.world_name LIKE ? OR c.instance_id LIKE ? OR EXISTS (\
                SELECT 1 FROM worlds w WHERE w.id = c.world_id AND w.world_id LIKE ?))"
                .to_string(),
        );
        values.push(Value::Text(like.clone()));
        values.push(Value::Text(like.clone()));
        values.push(Value::Text(like));
    }
    if let Some(query) = player.map(str::trim).filter(|query| !query.is_empty()) {
        let like = format!("%{query}%");
        clauses.push(
            "EXISTS (SELECT 1 FROM photo_players pp WHERE pp.photo_id = p.id \
             AND (pp.display_name LIKE ? OR pp.user_id LIKE ?))"
                .to_string(),
        );
        values.push(Value::Text(like.clone()));
        values.push(Value::Text(like));
    }
    if let Some(query) = tag.map(str::trim).filter(|query| !query.is_empty()) {
        let like = format!("%{query}%");
        clauses.push(
            "EXISTS (SELECT 1 FROM photo_tags pt JOIN tags t ON t.id = pt.tag_id \
             WHERE pt.photo_id = p.id AND t.name LIKE ?)"
                .to_string(),
        );
        values.push(Value::Text(like));
    }
    if favorite {
        clauses.push("p.is_favorite = 1".to_string());
    }
    if let Some(from) = from_utc {
        clauses.push("p.captured_at_utc >= ?".to_string());
        values.push(Value::Integer(from));
    }
    if let Some(to) = to_utc {
        clauses.push("p.captured_at_utc <= ?".to_string());
        values.push(Value::Integer(to));
    }

    (clauses.join(" AND "), values)
}

fn row_to_photo(row: &Row<'_>) -> rusqlite::Result<Photo> {
    Ok(Photo {
        id: row.get(0)?,
        root_id: row.get(1)?,
        rel_path: row.get(2)?,
        abs_path: row.get(3)?,
        file_name: row.get(4)?,
        captured_at_utc: row.get(5)?,
        captured_at_local: row.get(6)?,
        tz_offset_sec: row.get(7)?,
        width: row.get(8)?,
        height: row.get(9)?,
        file_size: row.get::<_, i64>(10)? as u64,
        mtime: row.get(11)?,
        blake3: row.get(12)?,
        status: PhotoStatus::parse(&row.get::<_, String>(13)?),
        is_favorite: row.get::<_, i64>(14)? != 0,
        note: row.get(15)?,
        display_derivative_id: row.get(16)?,
        display_path: row.get(17)?,
        display_format: row.get(18)?,
        display_file_size: row.get::<_, Option<i64>>(19)?.map(|size| size as u64),
        display_width: row.get(20)?,
        display_height: row.get(21)?,
        world_name: row.get(22)?,
        instance_id: row.get(23)?,
        link_method: row.get(24)?,
        confidence: row.get(25)?,
        author: row.get(26)?,
    })
}

fn row_to_photo_index(row: &Row<'_>) -> rusqlite::Result<PhotoIndexEntry> {
    Ok(PhotoIndexEntry {
        id: row.get(0)?,
        captured_at_utc: row.get(1)?,
        captured_month: row.get(2)?,
    })
}

fn row_to_job(row: &Row<'_>) -> rusqlite::Result<CompressionJob> {
    Ok(CompressionJob {
        id: row.get(0)?,
        photo_id: row.get(1)?,
        profile_id: row.get(2)?,
        status: QueueStatus::parse(&row.get::<_, String>(3)?),
        progress: row.get(4)?,
        error: row.get(5)?,
        output_path: row.get(6)?,
        policy: row.get(7)?,
        output_dir: row.get(8)?,
        retry_count: row.get(9)?,
        created_at: row.get(10)?,
        started_at: row.get(11)?,
        finished_at: row.get(12)?,
    })
}

fn row_to_deletion_request(row: &Row<'_>) -> rusqlite::Result<DeletionRequest> {
    Ok(DeletionRequest {
        id: row.get(0)?,
        photo_id: row.get(1)?,
        photo_name: row.get(2)?,
        reason: DeletionReason::parse(&row.get::<_, String>(3)?).unwrap_or(DeletionReason::Unknown),
        status: DeletionStatus::parse(&row.get::<_, String>(4)?),
        force: row.get::<_, i64>(5)? != 0,
        blocked_reason: row.get(6)?,
        error: row.get(7)?,
        requested_at: row.get(8)?,
        started_at: row.get(9)?,
        finished_at: row.get(10)?,
    })
}

fn now_string() -> String {
    chrono::Utc::now().to_rfc3339()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        DeletionReason, DeletionStatus, NewPhoto, PhotoFilter, PhotoPlayer, PhotoStatus,
    };

    fn sample_photo(root_id: i64, path: &str, ts: i64) -> NewPhoto {
        NewPhoto {
            root_id,
            rel_path: path.trim_start_matches('/').to_string(),
            abs_path: path.to_string(),
            file_name: path.rsplit('/').next().unwrap_or(path).to_string(),
            captured_at_utc: ts,
            captured_at_local: "2026-10-21 21:14:03".to_string(),
            tz_offset_sec: 9 * 3600,
            width: 3840,
            height: 2160,
            file_size: 12_400_000,
            mtime: ts,
            blake3: None,
        }
    }

    fn photo_at(root_id: i64, path: &str, utc: i64, local: &str) -> NewPhoto {
        let mut photo = sample_photo(root_id, path, utc);
        photo.captured_at_local = local.to_string();
        photo
    }

    fn profile(id: &str, name: &str) -> CompressionProfile {
        CompressionProfile {
            id: id.to_string(),
            name: name.to_string(),
            ..CompressionProfile::default()
        }
    }

    fn seed_photo(db: &Db) -> i64 {
        let root = db.add_root("/photos", "reference").unwrap();
        db.upsert_photo(&NewPhoto {
            root_id: root,
            rel_path: "a.png".to_string(),
            abs_path: "/photos/a.png".to_string(),
            file_name: "a.png".to_string(),
            captured_at_utc: 1_700_000_000,
            captured_at_local: "2026-10-21 21:14:03".to_string(),
            tz_offset_sec: 9 * 3600,
            width: 16,
            height: 10,
            file_size: 10,
            mtime: 1_700_000_000,
            blake3: None,
        })
        .unwrap()
    }

    fn seed_other_photo(db: &Db) -> i64 {
        let root = db.add_root("/photos-b", "reference").unwrap();
        db.upsert_photo(&NewPhoto {
            root_id: root,
            rel_path: "b.png".to_string(),
            abs_path: "/photos-b/b.png".to_string(),
            file_name: "b.png".to_string(),
            captured_at_utc: 1_700_000_001,
            captured_at_local: "2026-10-21 21:14:04".to_string(),
            tz_offset_sec: 9 * 3600,
            width: 16,
            height: 10,
            file_size: 10,
            mtime: 1_700_000_001,
            blake3: None,
        })
        .unwrap()
    }

    fn photo_to_new(photo: &Photo) -> NewPhoto {
        NewPhoto {
            root_id: photo.root_id,
            rel_path: photo.rel_path.clone(),
            abs_path: photo.abs_path.clone(),
            file_name: photo.file_name.clone(),
            captured_at_utc: photo.captured_at_utc,
            captured_at_local: photo.captured_at_local.clone(),
            tz_offset_sec: photo.tz_offset_sec,
            width: photo.width,
            height: photo.height,
            file_size: photo.file_size,
            mtime: photo.mtime,
            blake3: photo.blake3.clone(),
        }
    }

    fn insert_deletion_request(db: &Db, photo_id: i64, reason: &str, status: &str) {
        db.connection()
            .execute(
                "INSERT INTO deletion_requests(photo_id, reason, status, requested_at) \
                 VALUES (?1, ?2, ?3, ?4)",
                params![photo_id, reason, status, "2026-10-29T00:00:00+00:00"],
            )
            .unwrap();
    }

    /// Builds a version-3 database file directly from the V1-V3 DDL with one
    /// failed compression job. When `retry_count` is set, the retry column is
    /// added too, simulating an upgrade interrupted after the ALTER.
    fn seed_version_3_database(path: &Path, retry_count: Option<i64>) {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(SCHEMA_V1).unwrap();
        conn.execute_batch(SCHEMA_V2).unwrap();
        conn.execute_batch(SCHEMA_V3).unwrap();
        for version in 1..=3 {
            conn.execute(
                "INSERT INTO schema_migrations(version, applied_at) VALUES (?1, ?2)",
                params![version, now_string()],
            )
            .unwrap();
        }
        conn.execute(
            "INSERT INTO library_roots(path, kind, created_at) VALUES ('/photos', 'reference', ?1)",
            params![now_string()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO photos(root_id, rel_path, abs_path, file_name, captured_at_utc, \
                captured_at_local, tz_offset_sec, width, height, file_size, mtime, blake3, \
                status, created_at, updated_at) \
             VALUES (1, 'a.png', '/photos/a.png', 'a.png', 100, '2026-10-21 21:14:03', 32400, \
                3840, 2160, 12400000, 100, NULL, 'present', ?1, ?1)",
            params![now_string()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO compression_jobs(id, photo_id, profile_id, status, progress, error, \
                output_path, policy, created_at, started_at, finished_at) \
             VALUES ('job-1', 1, 'p', 'failed', 0, 'boom', NULL, 'keep', ?1, ?1, ?1)",
            params![now_string()],
        )
        .unwrap();
        if let Some(count) = retry_count {
            conn.execute_batch(SCHEMA_V4_RETRY_COUNT).unwrap();
            conn.execute(
                "UPDATE compression_jobs SET retry_count = ?1 WHERE id = 'job-1'",
                params![count],
            )
            .unwrap();
        }
        conn.pragma_update(None, "user_version", 3).unwrap();
    }

    #[test]
    fn migration_v6_adds_job_output_dir() {
        let db = Db::open_in_memory().unwrap();
        let version: i64 = db
            .conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, 8);
        let has_output_dir: bool = db
            .conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM pragma_table_info('compression_jobs') \
                 WHERE name = 'output_dir')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(has_output_dir);
    }

    #[test]
    fn migration_v7_adds_deletion_requests() {
        let db = Db::open_in_memory().unwrap();
        let version: i64 = db
            .connection()
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, 8);
        let table: i64 = db
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master \
                 WHERE type = 'table' AND name = 'deletion_requests'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(table, 1);
        let index: i64 = db
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master \
                 WHERE type = 'index' AND name = 'idx_deletion_requests_active'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(index, 1);
    }

    #[test]
    fn deleting_photo_is_hidden_from_index_search_and_counts() {
        let db = Db::open_in_memory().unwrap();
        let id = seed_photo(&db);
        db.add_derivative(id, "/photos/a.webp", "webp", 16, 10, 8, "p", "keep")
            .unwrap();
        db.connection()
            .execute(
                "UPDATE photos SET status = 'deleting' WHERE id = ?1",
                params![id],
            )
            .unwrap();
        assert!(db.photo_index(PhotoSort::Newest).unwrap().is_empty());
        assert_eq!(db.count_photos().unwrap(), 0);
        let stats = db.stats().unwrap();
        assert_eq!(stats.photo_count, 0);
        assert_eq!(stats.compressed_count, 0);
        assert_eq!(stats.derivative_bytes, 0);
        assert!(db.compressed_photo_ids().unwrap().is_empty());
        assert!(
            db.search_photos(&PhotoFilter::default())
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn rebind_candidate_skips_photos_with_active_deletion() {
        let db = Db::open_in_memory().unwrap();
        let id = seed_photo(&db);
        db.set_photo_blake3(id, "hash").unwrap();
        let photo = db.photo_by_id(id).unwrap().unwrap();
        db.request_deletion(&[id], DeletionReason::Manual, false)
            .unwrap();
        let mut moved = photo_to_new(&photo);
        moved.abs_path = "/photos/moved.png".to_string();
        moved.rel_path = "moved.png".to_string();
        moved.blake3 = Some("hash".to_string());
        assert!(db.find_rebind_candidate(&moved).unwrap().is_none());
        assert!(!db.rebind_photo(id, &moved).unwrap());
    }

    #[test]
    fn upsert_preserves_deleting_status_while_a_request_is_active() {
        let db = Db::open_in_memory().unwrap();
        let id = seed_photo(&db);
        insert_deletion_request(&db, id, "manual", "pending");
        db.connection()
            .execute(
                "UPDATE photos SET status = 'deleting' WHERE id = ?1",
                params![id],
            )
            .unwrap();
        let photo = db.photo_by_id(id).unwrap().unwrap();
        db.upsert_photo(&photo_to_new(&photo)).unwrap();
        assert_eq!(
            db.photo_by_id(id).unwrap().unwrap().status,
            PhotoStatus::Deleting
        );
        assert_eq!(db.count_photos().unwrap(), 0);
    }

    #[test]
    fn upsert_preserves_replaced_display_while_auto_deletion_is_active() {
        let db = Db::open_in_memory().unwrap();
        let id = seed_photo(&db);
        let derivative = db
            .add_derivative(id, "/photos/a_webp.webp", "webp", 16, 10, 8, "p", "replace")
            .unwrap();
        db.set_replaced(id, derivative).unwrap();
        insert_deletion_request(&db, id, "auto_replace", "pending");
        let photo = db.photo_by_id(id).unwrap().unwrap();
        db.upsert_photo(&photo_to_new(&photo)).unwrap();
        let photo = db.photo_by_id(id).unwrap().unwrap();
        assert_eq!(photo.status, PhotoStatus::Replaced);
        assert_eq!(photo.display_derivative_id, Some(derivative));
    }

    #[test]
    fn set_replaced_does_not_unhide_a_deleting_photo() {
        let db = Db::open_in_memory().unwrap();
        let id = seed_photo(&db);
        let derivative = db
            .add_derivative(id, "/photos/a_webp.webp", "webp", 16, 10, 8, "p", "replace")
            .unwrap();
        insert_deletion_request(&db, id, "manual", "pending");
        db.connection()
            .execute(
                "UPDATE photos SET status = 'deleting' WHERE id = ?1",
                params![id],
            )
            .unwrap();
        assert!(db.set_replaced(id, derivative).unwrap());
        let photo = db.photo_by_id(id).unwrap().unwrap();
        assert_eq!(photo.status, PhotoStatus::Deleting);
        assert_eq!(photo.display_derivative_id, Some(derivative));
        assert_eq!(db.count_photos().unwrap(), 0);
    }

    #[test]
    fn manual_deletion_request_hides_photo_and_is_idempotent() {
        let db = Db::open_in_memory().unwrap();
        let id = seed_photo(&db);
        let first = db
            .request_deletion(&[id], DeletionReason::Manual, false)
            .unwrap();
        let second = db
            .request_deletion(&[id], DeletionReason::Manual, false)
            .unwrap();
        assert_eq!(first, second);
        assert_eq!(
            db.photo_by_id(id).unwrap().unwrap().status,
            PhotoStatus::Deleting
        );
        let request = db.active_deletion_request(id).unwrap().unwrap();
        assert_eq!(request.reason, DeletionReason::Manual);
        assert_eq!(request.status, DeletionStatus::Pending);
    }

    #[test]
    fn auto_replace_request_keeps_the_photo_visible() {
        let db = Db::open_in_memory().unwrap();
        let id = seed_photo(&db);
        db.request_deletion(&[id], DeletionReason::AutoReplace, false)
            .unwrap();
        assert_eq!(
            db.photo_by_id(id).unwrap().unwrap().status,
            PhotoStatus::Present
        );
        assert_eq!(db.count_photos().unwrap(), 1);
    }

    #[test]
    fn manual_request_upgrades_an_active_auto_request() {
        let db = Db::open_in_memory().unwrap();
        let id = seed_photo(&db);
        db.request_deletion(&[id], DeletionReason::AutoReplace, false)
            .unwrap();
        db.request_deletion(&[id], DeletionReason::Manual, false)
            .unwrap();
        let request = db.active_deletion_request(id).unwrap().unwrap();
        assert_eq!(request.reason, DeletionReason::Manual);
        assert_eq!(
            db.photo_by_id(id).unwrap().unwrap().status,
            PhotoStatus::Deleting
        );
    }

    #[test]
    fn unknown_deletion_reason_is_preserved_and_cannot_be_requested() {
        let db = Db::open_in_memory().unwrap();
        let id = seed_photo(&db);
        db.connection()
            .execute(
                "INSERT INTO deletion_requests(photo_id, reason, status, force, requested_at) \
                 VALUES (?1, 'future_reason', 'pending', 0, ?2)",
                params![id, now_string()],
            )
            .unwrap();

        let pending = db.pending_deletion_requests(10).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].reason, DeletionReason::Unknown);
        assert_ne!(pending[0].reason, DeletionReason::Manual);
        assert!(
            db.request_deletion(&[id], DeletionReason::Unknown, false)
                .is_err()
        );
    }

    #[test]
    fn enqueue_rejects_photos_with_active_deletion() {
        let db = Db::open_in_memory().unwrap();
        let id = seed_photo(&db);
        db.save_profile(&profile("p", "P")).unwrap();
        db.request_deletion(&[id], DeletionReason::Manual, false)
            .unwrap();
        assert!(
            !db.enqueue_job_for_existing_profile(id, "p", "keep")
                .unwrap()
        );
        assert!(db.enqueue_job(id, "p", "keep").is_err());
        assert_eq!(db.count_active_jobs().unwrap(), 0);
    }

    #[test]
    fn deletion_job_summary_counts_each_status() {
        let db = Db::open_in_memory().unwrap();
        let id = seed_photo(&db);
        db.save_profile(&profile("p", "P")).unwrap();
        db.enqueue_job(id, "p", "keep").unwrap();
        let waiting = db.enqueue_job(id, "p", "keep").unwrap();
        db.update_job(&waiting, QueueStatus::Waiting, 0.0, None, None)
            .unwrap();
        let failed = db.enqueue_job(id, "p", "keep").unwrap();
        db.update_job(&failed, QueueStatus::Failed, 0.0, Some("boom"), None)
            .unwrap();
        let summary = db.deletion_job_summary(&[id]).unwrap();
        assert_eq!(summary.compression.pending, 1);
        assert_eq!(summary.compression.waiting, 1);
        assert_eq!(summary.compression.failed, 1);
        assert_eq!(summary.send.total(), 0);
        assert_eq!(summary.affected_photos, 1);
        assert!(summary.needs_warning());

        let other = seed_other_photo(&db);
        let summary = db.deletion_job_summary(&[id, other]).unwrap();
        assert_eq!(
            summary.affected_photos, 1,
            "photos without jobs are excluded"
        );
    }

    #[test]
    fn pending_deletion_requests_skip_requests_with_active_jobs() {
        let db = Db::open_in_memory().unwrap();
        let root = db.add_root("/photos", "reference").unwrap();
        let blocked = db
            .upsert_photo(&sample_photo(root, "/photos/blocked.png", 1_700_000_000))
            .unwrap();
        let ready = db
            .upsert_photo(&sample_photo(root, "/photos/ready.png", 1_700_000_001))
            .unwrap();
        let unknown = db
            .upsert_photo(&sample_photo(root, "/photos/unknown.png", 1_700_000_002))
            .unwrap();
        db.save_profile(&profile("p", "P")).unwrap();

        db.enqueue_job(blocked, "p", "keep").unwrap();
        db.enqueue_job(unknown, "p", "keep").unwrap();
        let blocked_request = db
            .request_deletion(&[blocked], DeletionReason::Manual, false)
            .unwrap()[0];
        let ready_request = db
            .request_deletion(&[ready], DeletionReason::Manual, false)
            .unwrap()[0];
        db.connection()
            .execute(
                "INSERT INTO deletion_requests(photo_id, reason, status, force, requested_at) \
                 VALUES (?1, 'future_reason', 'pending', 0, ?2)",
                params![unknown, now_string()],
            )
            .unwrap();

        let pending = db.pending_deletion_requests(10).unwrap();
        assert_eq!(pending.len(), 2);
        assert!(pending.iter().any(|request| request.id == ready_request));
        assert!(
            pending
                .iter()
                .any(|request| request.reason == DeletionReason::Unknown),
            "unknown reasons must be returned even while jobs are active"
        );
        assert!(!pending.iter().any(|request| request.id == blocked_request));
    }

    #[test]
    fn pending_deletion_requests_prefer_manual_over_older_backlog() {
        let db = Db::open_in_memory().unwrap();
        let root = db.add_root("/photos", "reference").unwrap();
        let auto_old = db
            .upsert_photo(&sample_photo(root, "/photos/auto-old.png", 1_700_000_000))
            .unwrap();
        let auto_new = db
            .upsert_photo(&sample_photo(root, "/photos/auto-new.png", 1_700_000_001))
            .unwrap();
        let manual_earlier = db
            .upsert_photo(&sample_photo(
                root,
                "/photos/manual-earlier.png",
                1_700_000_002,
            ))
            .unwrap();
        let manual_later = db
            .upsert_photo(&sample_photo(
                root,
                "/photos/manual-later.png",
                1_700_000_003,
            ))
            .unwrap();

        let auto_old_request = db
            .request_deletion(&[auto_old], DeletionReason::AutoReplace, false)
            .unwrap()[0];
        let auto_new_request = db
            .request_deletion(&[auto_new], DeletionReason::AutoReplace, false)
            .unwrap()[0];
        let manual_earlier_request = db
            .request_deletion(&[manual_earlier], DeletionReason::Manual, false)
            .unwrap()[0];
        let manual_later_request = db
            .request_deletion(&[manual_later], DeletionReason::Manual, false)
            .unwrap()[0];

        for (request, requested_at) in [
            (auto_old_request, "2026-10-21T00:00:01Z"),
            (auto_new_request, "2026-10-21T00:00:02Z"),
            (manual_earlier_request, "2026-10-21T00:00:03Z"),
            (manual_later_request, "2026-10-21T00:00:04Z"),
        ] {
            db.connection()
                .execute(
                    "UPDATE deletion_requests SET requested_at = ?1 WHERE id = ?2",
                    params![requested_at, request],
                )
                .unwrap();
        }

        // A manual request must enter the row window even when older backlog
        // already fills it.
        let window = db.pending_deletion_requests(2).unwrap();
        let ids = |requests: &[DeletionRequest]| {
            requests
                .iter()
                .map(|request| request.id)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            ids(&window),
            vec![manual_earlier_request, manual_later_request],
            "manual requests must preempt an older auto_replace backlog"
        );

        // Both groups keep requested_at order.
        let pending = db.pending_deletion_requests(4).unwrap();
        assert_eq!(
            ids(&pending),
            vec![
                manual_earlier_request,
                manual_later_request,
                auto_old_request,
                auto_new_request
            ]
        );
    }

    #[test]
    fn claim_deletion_requires_no_active_jobs() {
        let db = Db::open_in_memory().unwrap();
        let id = seed_photo(&db);
        db.save_profile(&profile("p", "P")).unwrap();
        db.enqueue_job(id, "p", "keep").unwrap();
        let request = db
            .request_deletion(&[id], DeletionReason::Manual, false)
            .unwrap()[0];
        assert!(db.claim_deletion(request).unwrap().is_none());
        let job = db.next_pending_job().unwrap().unwrap();
        db.update_job(&job.id, QueueStatus::Completed, 1.0, None, None)
            .unwrap();
        assert!(db.claim_deletion(request).unwrap().is_some());
        assert_eq!(
            db.active_deletion_request(id).unwrap().unwrap().status,
            DeletionStatus::Running
        );
    }

    #[test]
    fn claim_deletion_blocks_failed_jobs_unless_forced() {
        let db = Db::open_in_memory().unwrap();
        let id = seed_photo(&db);
        db.save_profile(&profile("p", "P")).unwrap();
        let job = db.enqueue_job(id, "p", "keep").unwrap();
        db.update_job(&job, QueueStatus::Failed, 0.0, Some("boom"), None)
            .unwrap();
        let request = db
            .request_deletion(&[id], DeletionReason::Manual, false)
            .unwrap()[0];
        assert!(db.claim_deletion(request).unwrap().is_none());
        assert!(db.force_deletion(request).unwrap());
        assert!(db.claim_deletion(request).unwrap().is_some());
    }

    #[test]
    fn force_deletion_cancels_queued_but_not_processing_jobs() {
        let db = Db::open_in_memory().unwrap();
        let id = seed_photo(&db);
        db.save_profile(&profile("p", "P")).unwrap();
        let processing = db.enqueue_job(id, "p", "keep").unwrap();
        let pending = db.enqueue_job(id, "p", "keep").unwrap();
        let started = db.next_pending_job().unwrap().unwrap();
        assert_eq!(started.id, processing);
        let request = db
            .request_deletion(&[id], DeletionReason::Manual, false)
            .unwrap()[0];
        db.force_deletion(request).unwrap();
        let jobs = db.list_jobs(10).unwrap();
        let by_id = |id: &str| jobs.iter().find(|job| job.id == id).unwrap();
        assert_eq!(by_id(&pending).status, QueueStatus::Cancelled);
        assert_eq!(by_id(&processing).status, QueueStatus::Processing);
        assert!(
            db.claim_deletion(request).unwrap().is_none(),
            "the processing job must block the forced claim"
        );
        db.update_job(&processing, QueueStatus::Completed, 1.0, None, None)
            .unwrap();
        assert!(db.claim_deletion(request).unwrap().is_some());
    }

    #[test]
    fn cancel_deletion_restores_visible_status_until_running() {
        let db = Db::open_in_memory().unwrap();
        let id = seed_photo(&db);
        let request = db
            .request_deletion(&[id], DeletionReason::Manual, false)
            .unwrap()[0];
        assert!(db.cancel_deletion(request).unwrap());
        assert_eq!(
            db.photo_by_id(id).unwrap().unwrap().status,
            PhotoStatus::Present
        );
        assert!(db.active_deletion_request(id).unwrap().is_none());
        let second = db
            .request_deletion(&[id], DeletionReason::Manual, false)
            .unwrap()[0];
        assert!(db.claim_deletion(second).unwrap().is_some());
        assert!(!db.cancel_deletion(second).unwrap());
    }

    #[test]
    fn claim_deletion_returns_the_upgraded_request() {
        let db = Db::open_in_memory().unwrap();
        let id = seed_photo(&db);
        let request = db
            .request_deletion(&[id], DeletionReason::AutoReplace, false)
            .unwrap()[0];
        db.request_deletion(&[id], DeletionReason::Manual, false)
            .unwrap();
        let claimed = db.claim_deletion(request).unwrap().unwrap();
        assert_eq!(claimed.reason, DeletionReason::Manual);
        assert_eq!(claimed.status, DeletionStatus::Running);
    }

    #[test]
    fn block_job_failed_uses_the_current_force_flag() {
        let db = Db::open_in_memory().unwrap();
        let id = seed_photo(&db);
        db.save_profile(&profile("p", "P")).unwrap();
        let job = db.enqueue_job(id, "p", "keep").unwrap();
        db.update_job(&job, QueueStatus::Failed, 0.0, Some("boom"), None)
            .unwrap();
        let request = db
            .request_deletion(&[id], DeletionReason::Manual, false)
            .unwrap()[0];
        assert!(db.block_deletion_if_jobs_failed(request).unwrap());
        assert_eq!(
            db.active_deletion_request(id).unwrap().unwrap().status,
            DeletionStatus::Blocked
        );
        assert!(db.force_deletion(request).unwrap());
        assert!(!db.block_deletion_if_jobs_failed(request).unwrap());
        assert!(db.claim_deletion(request).unwrap().is_some());
    }

    #[test]
    fn block_unknown_blocks_a_still_unknown_reason() {
        let db = Db::open_in_memory().unwrap();
        let id = seed_photo(&db);
        db.connection()
            .execute(
                "INSERT INTO deletion_requests(photo_id, reason, status, force, requested_at) \
                 VALUES (?1, 'future_reason', 'pending', 0, '2026-10-21T00:00:00Z')",
                params![id],
            )
            .unwrap();
        let request = db.active_deletion_request(id).unwrap().unwrap();
        assert_eq!(request.reason, DeletionReason::Unknown);
        assert!(db.block_deletion_if_unknown(request.id).unwrap());
        let blocked = db.active_deletion_request(id).unwrap().unwrap();
        assert_eq!(blocked.status, DeletionStatus::Blocked);
        assert_eq!(blocked.blocked_reason.as_deref(), Some("unknown_reason"));
    }

    #[test]
    fn block_unknown_skips_a_request_upgraded_to_manual() {
        let db = Db::open_in_memory().unwrap();
        let id = seed_photo(&db);
        db.connection()
            .execute(
                "INSERT INTO deletion_requests(photo_id, reason, status, force, requested_at) \
                 VALUES (?1, 'future_reason', 'pending', 0, '2026-10-21T00:00:00Z')",
                params![id],
            )
            .unwrap();
        let request = db.active_deletion_request(id).unwrap().unwrap();
        // The UI upgrades the same request to manual after the executor's
        // snapshot was taken; the conditional block must not touch it.
        db.request_deletion(&[id], DeletionReason::Manual, false)
            .unwrap();
        assert!(!db.block_deletion_if_unknown(request.id).unwrap());
        let current = db.active_deletion_request(id).unwrap().unwrap();
        assert_eq!(current.reason, DeletionReason::Manual);
        assert_eq!(current.status, DeletionStatus::Pending);
        assert!(db.claim_deletion(request.id).unwrap().is_some());
    }

    #[test]
    fn reset_running_deletions_makes_requests_retryable() {
        let db = Db::open_in_memory().unwrap();
        let id = seed_photo(&db);
        let request = db
            .request_deletion(&[id], DeletionReason::Manual, false)
            .unwrap()[0];
        assert!(db.claim_deletion(request).unwrap().is_some());
        assert_eq!(db.reset_running_deletions().unwrap(), 1);
        let reset = db.active_deletion_request(id).unwrap().unwrap();
        assert_eq!(reset.status, DeletionStatus::Pending);
        assert!(
            reset.started_at.is_some(),
            "restart must keep the partial-execution marker"
        );
        assert!(
            !db.cancel_deletion(request).unwrap(),
            "cancel must refuse after a started deletion"
        );
    }

    #[test]
    fn cancel_deletion_rejects_auto_replace_requests() {
        let db = Db::open_in_memory().unwrap();
        let id = seed_photo(&db);
        let request = db
            .request_deletion(&[id], DeletionReason::AutoReplace, false)
            .unwrap()[0];
        assert!(!db.cancel_deletion(request).unwrap());
        assert_eq!(
            db.active_deletion_request(id).unwrap().unwrap().status,
            DeletionStatus::Pending
        );
    }

    #[test]
    fn manual_request_is_rejected_while_a_request_is_running() {
        let db = Db::open_in_memory().unwrap();
        let id = seed_photo(&db);
        let request = db
            .request_deletion(&[id], DeletionReason::AutoReplace, false)
            .unwrap()[0];
        assert!(db.claim_deletion(request).unwrap().is_some());
        let error = db
            .request_deletion(&[id], DeletionReason::Manual, false)
            .unwrap_err();
        assert!(error.to_string().contains("running"));
        assert_eq!(
            db.active_deletion_request(id).unwrap().unwrap().reason,
            DeletionReason::AutoReplace
        );
    }

    #[test]
    fn cancel_deletion_refuses_after_execution_started() {
        let db = Db::open_in_memory().unwrap();
        let id = seed_photo(&db);
        let request = db
            .request_deletion(&[id], DeletionReason::Manual, false)
            .unwrap()[0];
        assert!(db.claim_deletion(request).unwrap().is_some());
        db.block_deletion(request, "file_error", Some("permission denied"))
            .unwrap();
        assert!(!db.cancel_deletion(request).unwrap());
        assert_eq!(
            db.active_deletion_request(id).unwrap().unwrap().status,
            DeletionStatus::Blocked
        );
    }

    #[test]
    fn resume_blocked_deletion_waits_for_all_jobs() {
        let db = Db::open_in_memory().unwrap();
        let id = seed_photo(&db);
        db.save_profile(&profile("p", "P")).unwrap();
        let failed = db.enqueue_job(id, "p", "keep").unwrap();
        db.update_job(&failed, QueueStatus::Failed, 0.0, Some("boom"), None)
            .unwrap();
        let request = db
            .request_deletion(&[id], DeletionReason::Manual, false)
            .unwrap()[0];
        db.block_deletion(request, "job_failed", None).unwrap();
        assert_eq!(db.resume_blocked_deletions_for_photo(id).unwrap(), 0);

        db.retry_job(&failed).unwrap();
        let retried = db.next_pending_job().unwrap().unwrap();
        db.update_job(&retried.id, QueueStatus::Completed, 1.0, None, None)
            .unwrap();
        assert_eq!(db.resume_blocked_deletions_for_photo(id).unwrap(), 1);
        assert_eq!(
            db.active_deletion_request(id).unwrap().unwrap().status,
            DeletionStatus::Pending
        );
    }

    #[test]
    fn retry_is_rejected_while_deletion_is_running() {
        let db = Db::open_in_memory().unwrap();
        let id = seed_photo(&db);
        db.save_profile(&profile("p", "P")).unwrap();
        let failed = db.enqueue_job(id, "p", "keep").unwrap();
        db.update_job(&failed, QueueStatus::Failed, 0.0, Some("boom"), None)
            .unwrap();
        let request = db
            .request_deletion(&[id], DeletionReason::Manual, false)
            .unwrap()[0];
        db.force_deletion(request).unwrap();
        assert!(db.claim_deletion(request).unwrap().is_some());
        assert!(!db.retry_job(&failed).unwrap());
        assert_eq!(db.retry_failed_jobs().unwrap(), 0);
        assert_eq!(
            db.list_jobs(10)
                .unwrap()
                .into_iter()
                .find(|job| job.id == failed)
                .unwrap()
                .status,
            QueueStatus::Failed
        );
    }

    #[test]
    fn migration_adds_captured_id_index() {
        let db = Db::open_in_memory().unwrap();
        let version: i64 = db
            .conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, 8);

        let recorded: i64 = db
            .conn
            .query_row(
                "SELECT version FROM schema_migrations WHERE version = 3",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(recorded, 3);

        let sql: String = db
            .conn
            .query_row(
                "SELECT sql FROM sqlite_master \
                 WHERE type = 'index' AND name = 'idx_photos_captured_id'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let normalized = sql
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase();
        assert!(
            normalized.contains("on photos(captured_at_utc desc, id desc)"),
            "{sql}"
        );
        assert!(
            normalized.contains("where status in ('present', 'replaced')"),
            "{sql}"
        );
    }

    #[test]
    fn migration_adds_retry_count_and_daemon_tables() {
        let db = Db::open_in_memory().unwrap();
        let version: i64 = db
            .conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, 8);
        let retry_column: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('compression_jobs') WHERE name = 'retry_count'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(retry_column, 1);
        let tables: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' \
                 AND name IN ('daemon_errors', 'daemon_commands')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(tables, 2);
    }

    #[test]
    fn migration_v5_adds_display_derivative_and_visible_index() {
        let db = Db::open_in_memory().unwrap();
        let column: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('photos') \
                 WHERE name = 'display_derivative_id'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(column, 1);

        let sql: String = db
            .conn
            .query_row(
                "SELECT sql FROM sqlite_master \
                 WHERE type = 'index' AND name = 'idx_photos_captured_id'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let normalized = sql
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase();
        assert!(
            normalized.contains("status in ('present', 'replaced')"),
            "{sql}"
        );
    }

    #[test]
    fn migration_upgrades_version_3_database_with_existing_jobs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("library.db");
        seed_version_3_database(&path, None);

        let db = Db::open(&path).unwrap();
        let version: i64 = db
            .conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, 8);

        let jobs = db.list_jobs(10).unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].id, "job-1");
        assert_eq!(jobs[0].status, QueueStatus::Failed);
        assert_eq!(jobs[0].retry_count, 0);

        let tables: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' \
                 AND name IN ('daemon_errors', 'daemon_commands')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(tables, 2);
    }

    #[test]
    fn migration_v4_recovers_from_interrupted_upgrade() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("library.db");
        // The ALTER committed but user_version stayed at 3 and the daemon
        // tables are missing, as if the process died mid-upgrade.
        seed_version_3_database(&path, Some(2));

        let db = Db::open(&path).unwrap();
        let version: i64 = db
            .conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, 8);
        assert_eq!(db.list_jobs(10).unwrap()[0].retry_count, 2);

        let tables: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' \
                 AND name IN ('daemon_errors', 'daemon_commands')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(tables, 2);
    }

    #[test]
    fn photo_index_orders_newest_first_and_uses_local_month() {
        let db = Db::open_in_memory().unwrap();
        let root = db.add_root("/photos", "reference").unwrap();
        let older = db
            .upsert_photo(&photo_at(
                root,
                "/photos/older.png",
                1_700_000_000,
                "2025-04-30 09:00:00",
            ))
            .unwrap();
        let newer = db
            .upsert_photo(&photo_at(
                root,
                "/photos/newer.png",
                1_700_100_000,
                "2025-05-01 08:00:00",
            ))
            .unwrap();
        let missing = db
            .upsert_photo(&photo_at(
                root,
                "/photos/missing.png",
                1_700_200_000,
                "2025-06-01 08:00:00",
            ))
            .unwrap();
        db.mark_missing(missing).unwrap();

        let entries = db.photo_index(PhotoSort::Newest).unwrap();
        assert_eq!(
            entries.iter().map(|entry| entry.id).collect::<Vec<_>>(),
            vec![newer, older]
        );
        assert_eq!(entries[0].captured_month, 202505);
        assert_eq!(entries[1].captured_month, 202504);
    }

    #[test]
    fn photo_index_supports_world_sort() {
        let db = Db::open_in_memory().unwrap();
        let root = db.add_root("/photos", "reference").unwrap();
        let alpha = db
            .upsert_photo(&photo_at(
                root,
                "/photos/alpha.png",
                1_700_000_000,
                "2025-04-30 09:00:00",
            ))
            .unwrap();
        let beta = db
            .upsert_photo(&photo_at(
                root,
                "/photos/beta.png",
                1_700_000_100,
                "2025-04-30 10:00:00",
            ))
            .unwrap();
        for (photo_id, world_name) in [(alpha, "Alpha"), (beta, "Beta")] {
            db.set_photo_context(&PhotoContext {
                photo_id,
                session_id: None,
                world_id: None,
                world_name: Some(world_name.to_string()),
                instance_id: None,
                link_method: "manual".to_string(),
                confidence: 1.0,
                author: None,
                linked_at: now_string(),
            })
            .unwrap();
        }

        let entries = db.photo_index(PhotoSort::World).unwrap();
        assert_eq!(
            entries.iter().map(|entry| entry.id).collect::<Vec<_>>(),
            vec![alpha, beta]
        );
    }

    #[test]
    fn photo_index_falls_back_to_utc_month_for_bad_local_time() {
        let db = Db::open_in_memory().unwrap();
        let root = db.add_root("/photos", "reference").unwrap();
        // 2025-05-01 00:00:00 UTC
        db.upsert_photo(&photo_at(root, "/photos/bad-local.png", 1_746_057_600, ""))
            .unwrap();
        let entries = db.photo_index(PhotoSort::Newest).unwrap();
        assert_eq!(entries[0].captured_month, 202505);
    }

    #[test]
    fn photo_index_text_returns_more_than_300_matches() {
        let db = Db::open_in_memory().unwrap();
        let root = db.add_root("/photos", "reference").unwrap();
        for index in 0..305 {
            db.upsert_photo(&photo_at(
                root,
                &format!("/photos/{index}.png"),
                1_700_000_000 + index,
                "2025-04-30 09:00:00",
            ))
            .unwrap();
        }
        let entries = db.photo_index_text(&PhotoTextFilter::default()).unwrap();
        assert_eq!(entries.len(), 305);
    }

    #[test]
    fn photo_index_text_applies_favorite_filter() {
        let db = Db::open_in_memory().unwrap();
        let root = db.add_root("/photos", "reference").unwrap();
        let favorite = db
            .upsert_photo(&photo_at(
                root,
                "/photos/favorite.png",
                1_700_000_100,
                "2025-04-30 09:00:00",
            ))
            .unwrap();
        db.upsert_photo(&photo_at(
            root,
            "/photos/plain.png",
            1_700_000_000,
            "2025-04-30 08:00:00",
        ))
        .unwrap();
        db.set_favorite(favorite, true).unwrap();

        let entries = db
            .photo_index_text(&PhotoTextFilter {
                favorite: true,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(
            entries.iter().map(|entry| entry.id).collect::<Vec<_>>(),
            vec![favorite]
        );
    }

    #[test]
    fn set_note_round_trips_and_clears_with_none() {
        let db = Db::open_in_memory().unwrap();
        let root = db.add_root("/photos", "reference").unwrap();
        let id = db
            .upsert_photo(&photo_at(
                root,
                "/photos/note.png",
                1_700_000_000,
                "2025-04-30 09:00:00",
            ))
            .unwrap();

        assert_eq!(db.photo_by_id(id).unwrap().unwrap().note, None);
        db.set_note(id, Some("first note")).unwrap();
        assert_eq!(
            db.photo_by_id(id).unwrap().unwrap().note.as_deref(),
            Some("first note")
        );
        db.set_note(id, None).unwrap();
        assert_eq!(db.photo_by_id(id).unwrap().unwrap().note, None);
    }

    #[test]
    fn removing_a_tag_drops_the_photo_from_tag_search() {
        let db = Db::open_in_memory().unwrap();
        let root = db.add_root("/photos", "reference").unwrap();
        let id = db
            .upsert_photo(&photo_at(
                root,
                "/photos/tagged.png",
                1_700_000_000,
                "2025-04-30 09:00:00",
            ))
            .unwrap();
        db.set_photo_tags(id, &["keep".to_string(), "remove".to_string()])
            .unwrap();

        let filter = PhotoTextFilter {
            tag: Some("remove".to_string()),
            ..Default::default()
        };
        assert_eq!(db.photo_index_text(&filter).unwrap().len(), 1);

        db.set_photo_tags(id, &["keep".to_string()]).unwrap();
        assert!(db.photo_index_text(&filter).unwrap().is_empty());
        assert_eq!(db.photo_tags(id).unwrap(), vec!["keep".to_string()]);
    }

    #[test]
    fn photos_by_ids_preserves_order_and_skips_missing_ids() {
        let db = Db::open_in_memory().unwrap();
        let root = db.add_root("/photos", "reference").unwrap();
        let mut ids = Vec::new();
        for index in 0..501 {
            ids.push(
                db.upsert_photo(&photo_at(
                    root,
                    &format!("/photos/{index}.png"),
                    1_700_000_000 + index,
                    "2025-04-30 09:00:00",
                ))
                .unwrap(),
            );
        }

        let photos = db.photos_by_ids(&ids).unwrap();
        assert_eq!(photos.len(), 501);
        assert_eq!(photos.first().map(|photo| photo.id), Some(ids[0]));
        assert_eq!(photos.last().map(|photo| photo.id), Some(ids[500]));

        // The reversed request crosses the 500-id chunk boundary; every row
        // must come back in exactly the requested order.
        let reversed: Vec<i64> = ids.iter().rev().copied().collect();
        let photos = db.photos_by_ids(&reversed).unwrap();
        assert_eq!(
            photos.iter().map(|photo| photo.id).collect::<Vec<_>>(),
            reversed
        );

        let missing = db.photos_by_ids(&[ids[500], ids[0], -1]).unwrap();
        assert_eq!(
            missing.iter().map(|photo| photo.id).collect::<Vec<_>>(),
            vec![ids[500], ids[0]]
        );
    }

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
        assert_eq!(states["/photos/0500.png"].status, PhotoStatus::Missing);
    }

    /// Seeds 100k synthetic rows in one transaction and measures the shipped
    /// index and page queries. Ignored by default because it is a timing
    /// benchmark, not a correctness gate:
    ///
    /// `cargo test -p vrc-photo-core photo_index_scales_to_100k -- --ignored --nocapture`
    #[test]
    #[ignore = "100k timing benchmark"]
    fn photo_index_scales_to_100k() {
        use std::time::{Duration, Instant};

        let db = Db::open_in_memory().unwrap();
        let root = db.add_root("/photos", "reference").unwrap();
        let base = 1_600_000_000i64;
        let seed_start = Instant::now();
        {
            let tx = db.connection().unchecked_transaction().unwrap();
            {
                let mut stmt = tx
                    .prepare(
                        "INSERT INTO photos(root_id, rel_path, abs_path, file_name, \
                         captured_at_utc, captured_at_local, tz_offset_sec, width, height, \
                         file_size, mtime, status, created_at, updated_at) \
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0, 3840, 2160, 12400000, ?5, \
                         'present', '2026-01-01T00:00:00+00:00', '2026-01-01T00:00:00+00:00')",
                    )
                    .unwrap();
                for index in 0..100_000i64 {
                    let captured_at = base + index;
                    let local = chrono::DateTime::<chrono::Utc>::from_timestamp(captured_at, 0)
                        .unwrap()
                        .format("%Y-%m-%d %H:%M:%S")
                        .to_string();
                    let rel_path = format!("2026/{index:06}.png");
                    let abs_path = format!("/photos/{rel_path}");
                    stmt.execute(params![
                        root,
                        rel_path,
                        abs_path,
                        format!("{index:06}.png"),
                        captured_at,
                        local
                    ])
                    .unwrap();
                }
            }
            // The shipped query LEFT JOINs `photo_context`; seed every other
            // row so the benchmark measures the production join instead of an
            // all-NULL shortcut.
            tx.execute(
                "INSERT INTO photo_context(photo_id, world_name, link_method, confidence, linked_at) \
                 SELECT id, 'Bench World ' || (id % 100), 'log', 1.0, \
                 '2026-01-01T00:00:00+00:00' FROM photos WHERE id % 2 = 0",
                [],
            )
            .unwrap();
            tx.commit().unwrap();
        }
        eprintln!("seed 100k: {:?}", seed_start.elapsed());
        let context_count: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM photo_context", [], |row| row.get(0))
            .unwrap();
        assert_eq!(context_count, 50_000);

        let start = Instant::now();
        let entries = db.photo_index(PhotoSort::Newest).unwrap();
        let index_elapsed = start.elapsed();
        assert_eq!(entries.len(), 100_000);
        eprintln!("photo_index 100k: {index_elapsed:?}");
        assert!(
            index_elapsed < Duration::from_secs(2),
            "photo_index took {index_elapsed:?}"
        );

        let ids: Vec<i64> = entries[50_000..50_256]
            .iter()
            .map(|entry| entry.id)
            .collect();
        let start = Instant::now();
        let photos = db.photos_by_ids(&ids).unwrap();
        let page_elapsed = start.elapsed();
        eprintln!("photos_by_ids 256: {page_elapsed:?}");
        assert_eq!(photos.len(), 256);
        assert!(
            page_elapsed < Duration::from_millis(200),
            "photos_by_ids took {page_elapsed:?}"
        );

        // The search screen builds the same full snapshot with a text filter;
        // an empty filter is the worst case (all 100k rows match).
        let start = Instant::now();
        let matches = db.photo_index_text(&PhotoTextFilter::default()).unwrap();
        eprintln!("photo_index_text 100k: {:?}", start.elapsed());
        assert_eq!(matches.len(), 100_000);
    }

    #[test]
    fn connections_use_a_large_page_cache_and_memory_temp_store() {
        let db = Db::open_in_memory().unwrap();
        let cache_size: i64 = db
            .conn
            .pragma_query_value(None, "cache_size", |row| row.get(0))
            .unwrap();
        assert_eq!(cache_size, -65_536);
        let temp_store: i64 = db
            .conn
            .pragma_query_value(None, "temp_store", |row| row.get(0))
            .unwrap();
        assert_eq!(temp_store, 2);
    }

    #[test]
    fn database_path_is_exposed_for_read_only_workers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("library.db");
        let db = Db::open(&path).unwrap();
        assert_eq!(db.path(), Some(path.as_path()));
        assert_eq!(Db::open_in_memory().unwrap().path(), None);
    }

    #[test]
    fn migrates_and_upserts_photo() {
        let db = Db::open_in_memory().unwrap();
        let root = db.add_root("/photos", "reference").unwrap();
        let id = db
            .upsert_photo(&sample_photo(root, "/photos/a.png", 100))
            .unwrap();
        assert!(id > 0);
        let photo = db.photo_by_id(id).unwrap().unwrap();
        assert_eq!(photo.width, 3840);
        assert_eq!(photo.status, PhotoStatus::Present);

        let id2 = db
            .upsert_photo(&sample_photo(root, "/photos/a.png", 100))
            .unwrap();
        assert_eq!(id, id2);
        assert_eq!(db.count_photos().unwrap(), 1);
    }

    #[test]
    fn links_context_and_players() {
        let db = Db::open_in_memory().unwrap();
        let root = db.add_root("/photos", "reference").unwrap();
        let photo_id = db
            .upsert_photo(&sample_photo(root, "/photos/a.png", 100))
            .unwrap();
        let world = db
            .upsert_world("wrld_test", Some("Test World"), Some("log"))
            .unwrap();
        let session = db
            .create_session("output_log.txt", 90, world, Some("123~region(jp)"))
            .unwrap();
        db.upsert_session_player(session, "usr_a", "Alice", 95)
            .unwrap();

        let ctx = PhotoContext {
            photo_id,
            session_id: Some(session),
            world_id: Some(world),
            world_name: Some("Test World".to_string()),
            instance_id: Some("123~region(jp)".to_string()),
            link_method: "log".to_string(),
            confidence: 1.0,
            author: None,
            linked_at: now_string(),
        };
        db.set_photo_context(&ctx).unwrap();
        db.set_photo_players(
            photo_id,
            &[PhotoPlayer {
                user_id: "usr_a".to_string(),
                display_name: "Alice".to_string(),
            }],
        )
        .unwrap();

        let stored = db.photo_context(photo_id).unwrap().unwrap();
        assert_eq!(stored.world_name.as_deref(), Some("Test World"));
        assert_eq!(db.photo_players(photo_id).unwrap()[0].display_name, "Alice");

        let filter = PhotoFilter {
            world_ids: vec![world],
            ..Default::default()
        };
        assert_eq!(db.search_photos(&filter).unwrap().len(), 1);
        assert_eq!(db.count_search(&filter).unwrap(), 1);
    }

    #[test]
    fn queue_roundtrip() {
        let db = Db::open_in_memory().unwrap();
        let root = db.add_root("/photos", "reference").unwrap();
        let photo = db
            .upsert_photo(&sample_photo(root, "/photos/a.png", 100))
            .unwrap();
        db.enqueue_job(photo, "profile", "keep").unwrap();
        let job = db.next_pending_job().unwrap().unwrap();
        assert_eq!(job.status, QueueStatus::Processing);
        db.update_job(
            &job.id,
            QueueStatus::Completed,
            1.0,
            None,
            Some("/photos/compressed/a.webp"),
        )
        .unwrap();
        assert_eq!(db.count_jobs(QueueStatus::Completed).unwrap(), 1);
    }

    #[test]
    fn next_pending_photo_jobs_claims_the_oldest_photos_jobs() {
        let db = Db::open_in_memory().unwrap();
        let root = db.add_root("/photos", "reference").unwrap();
        let first = db
            .upsert_photo(&sample_photo(root, "/photos/a.png", 100))
            .unwrap();
        let second = db
            .upsert_photo(&sample_photo(root, "/photos/b.png", 200))
            .unwrap();
        db.save_profile(&profile("p", "P")).unwrap();
        let first_a = db.enqueue_job(first, "p", "keep").unwrap();
        let first_b = db.enqueue_job(first, "p", "keep").unwrap();
        let waiting = db
            .enqueue_job_with_status(first, "p", "keep", QueueStatus::Waiting)
            .unwrap();
        let second_a = db.enqueue_job(second, "p", "keep").unwrap();

        let batch = db.next_pending_photo_jobs().unwrap();
        assert_eq!(
            batch.iter().map(|job| job.id.as_str()).collect::<Vec<_>>(),
            vec![first_a.as_str(), first_b.as_str()]
        );
        assert!(
            batch
                .iter()
                .all(|job| job.status == QueueStatus::Processing)
        );
        assert!(batch.iter().all(|job| job.started_at.is_some()));

        let next = db.next_pending_photo_jobs().unwrap();
        assert_eq!(next.len(), 1);
        assert_eq!(next[0].id, second_a);
        assert!(db.next_pending_photo_jobs().unwrap().is_empty());

        let waiting = db
            .list_jobs(10)
            .unwrap()
            .into_iter()
            .find(|job| job.id == waiting)
            .unwrap();
        assert_eq!(waiting.status, QueueStatus::Waiting);
    }

    #[test]
    fn photo_has_forced_deletion_tracks_the_force_flag() {
        let db = Db::open_in_memory().unwrap();
        let id = seed_photo(&db);
        let request = db
            .request_deletion(&[id], DeletionReason::Manual, false)
            .unwrap()[0];

        assert!(!db.photo_has_forced_deletion(id).unwrap());
        assert!(db.force_deletion(request).unwrap());
        assert!(db.photo_has_forced_deletion(id).unwrap());
        db.block_deletion(request, "file_error", None).unwrap();
        assert!(db.photo_has_forced_deletion(id).unwrap());
    }

    #[test]
    fn fail_job_if_active_leaves_terminal_jobs_alone() {
        let db = Db::open_in_memory().unwrap();
        let id = seed_photo(&db);
        db.save_profile(&profile("p", "P")).unwrap();
        let first = db.enqueue_job(id, "p", "keep").unwrap();
        let second = db.enqueue_job(id, "p", "keep").unwrap();
        let third = db.enqueue_job(id, "p", "keep").unwrap();
        let batch = db.next_pending_photo_jobs().unwrap();
        assert_eq!(batch.len(), 3);
        db.update_job(&first, QueueStatus::Completed, 1.0, None, None)
            .unwrap();
        db.update_job(&second, QueueStatus::Cancelled, 0.0, None, None)
            .unwrap();

        assert_eq!(
            db.fail_job_if_active(&third, "boom").unwrap(),
            Some(QueueStatus::Failed)
        );
        assert_eq!(
            db.fail_job_if_active(&first, "late").unwrap(),
            Some(QueueStatus::Completed)
        );
        assert_eq!(
            db.fail_job_if_active(&second, "late").unwrap(),
            Some(QueueStatus::Cancelled)
        );
        assert_eq!(db.fail_job_if_active("missing", "late").unwrap(), None);

        let jobs = db.list_jobs(10).unwrap();
        let by_id = |id: &str| jobs.iter().find(|job| job.id == id).unwrap();
        assert_eq!(by_id(&first).status, QueueStatus::Completed);
        assert!(by_id(&first).error.is_none());
        assert_eq!(by_id(&second).status, QueueStatus::Cancelled);
        assert_eq!(by_id(&third).status, QueueStatus::Failed);
        assert_eq!(by_id(&third).error.as_deref(), Some("boom"));
    }

    #[test]
    fn manual_job_keeps_its_output_directory() {
        let db = Db::open_in_memory().unwrap();
        let root = db.add_root("/photos", "reference").unwrap();
        let photo = db
            .upsert_photo(&sample_photo(root, "/photos/a.png", 100))
            .unwrap();
        db.enqueue_job_with_output_dir(photo, "profile", "keep", Some("/exports"))
            .unwrap();
        let job = db.next_pending_job().unwrap().unwrap();
        assert_eq!(job.output_dir.as_deref(), Some("/exports"));
        assert_eq!(job.policy, "keep");
    }

    #[test]
    fn automatic_jobs_leave_the_output_directory_unset() {
        let db = Db::open_in_memory().unwrap();
        let root = db.add_root("/photos", "reference").unwrap();
        let photo = db
            .upsert_photo(&sample_photo(root, "/photos/a.png", 100))
            .unwrap();
        db.enqueue_job(photo, "profile", "keep").unwrap();
        let job = db.next_pending_job().unwrap().unwrap();
        assert!(job.output_dir.is_none());
    }

    #[test]
    fn waiting_jobs_are_not_dequeued_until_released() {
        let db = Db::open_in_memory().unwrap();
        let root = db.add_root("/photos", "reference").unwrap();
        let photo = db
            .upsert_photo(&sample_photo(root, "/photos/a.png", 100))
            .unwrap();
        let job = db
            .enqueue_job_with_status(photo, "p", "replace", QueueStatus::Waiting)
            .unwrap();

        assert_eq!(db.count_jobs(QueueStatus::Waiting).unwrap(), 1);
        assert_eq!(db.count_active_jobs().unwrap(), 1);
        assert!(db.next_pending_job().unwrap().is_none());

        assert_eq!(db.release_waiting_jobs("keep").unwrap(), 1);
        assert_eq!(db.count_jobs(QueueStatus::Waiting).unwrap(), 0);
        assert_eq!(db.count_jobs(QueueStatus::Pending).unwrap(), 1);
        let released = db.next_pending_job().unwrap().unwrap();
        assert_eq!(released.id, job);
        assert_eq!(released.policy, "keep");
        assert_eq!(db.release_waiting_jobs("keep").unwrap(), 0);
    }

    #[test]
    fn waiting_job_blocks_profile_deletion() {
        let db = Db::open_in_memory().unwrap();
        db.save_profile(&profile("p", "P")).unwrap();
        db.save_profile(&profile("q", "Q")).unwrap();
        let root = db.add_root("/photos", "reference").unwrap();
        let photo = db
            .upsert_photo(&sample_photo(root, "/photos/a.png", 100))
            .unwrap();
        let job = db
            .enqueue_job_with_status(photo, "p", "keep", QueueStatus::Waiting)
            .unwrap();

        assert_eq!(db.active_job_count_for_profile("p").unwrap(), 1);
        assert!(matches!(db.delete_profile("p"), Err(CoreError::Invalid(_))));

        db.update_job(&job, QueueStatus::Completed, 1.0, None, None)
            .unwrap();
        db.delete_profile("p").unwrap();
    }

    #[test]
    fn retry_job_resets_failed_job_and_enforces_cap() {
        let db = Db::open_in_memory().unwrap();
        let root = db.add_root("/photos", "reference").unwrap();
        let photo = db
            .upsert_photo(&sample_photo(root, "/photos/a.png", 100))
            .unwrap();
        let job = db.enqueue_job(photo, "p", "keep").unwrap();
        db.next_pending_job().unwrap();
        db.update_job(&job, QueueStatus::Failed, 0.0, Some("boom"), None)
            .unwrap();

        for expected in 1..=MAX_JOB_RETRIES {
            assert!(db.retry_job(&job).unwrap());
            let row = &db.list_jobs(1).unwrap()[0];
            assert_eq!(row.status, QueueStatus::Pending);
            assert_eq!(row.retry_count, expected);
            assert!(row.error.is_none());
            assert!(row.started_at.is_none());
            assert!(row.finished_at.is_none());
            db.next_pending_job().unwrap();
            db.update_job(&job, QueueStatus::Failed, 0.0, Some("boom"), None)
                .unwrap();
        }

        assert!(!db.retry_job(&job).unwrap());
        assert_eq!(db.list_jobs(1).unwrap()[0].retry_count, MAX_JOB_RETRIES);
    }

    #[test]
    fn retry_failed_jobs_returns_only_eligible_rows() {
        let db = Db::open_in_memory().unwrap();
        let root = db.add_root("/photos", "reference").unwrap();
        let photo = db
            .upsert_photo(&sample_photo(root, "/photos/a.png", 100))
            .unwrap();
        let first = db.enqueue_job(photo, "p", "keep").unwrap();
        let capped = db.enqueue_job(photo, "p", "keep").unwrap();
        let completed = db.enqueue_job(photo, "p", "keep").unwrap();
        for job in [&first, &capped, &completed] {
            db.next_pending_job().unwrap();
            db.update_job(job, QueueStatus::Failed, 0.0, Some("boom"), None)
                .unwrap();
        }
        db.connection()
            .execute(
                "UPDATE compression_jobs SET retry_count = ?1 WHERE id = ?2",
                params![MAX_JOB_RETRIES, capped],
            )
            .unwrap();
        db.update_job(&completed, QueueStatus::Completed, 1.0, None, None)
            .unwrap();

        assert_eq!(db.retry_failed_jobs().unwrap(), 1);
        assert_eq!(db.count_jobs(QueueStatus::Pending).unwrap(), 1);
        assert_eq!(
            db.list_jobs(10)
                .unwrap()
                .into_iter()
                .find(|row| row.id == first)
                .unwrap()
                .retry_count,
            1
        );
    }

    #[test]
    fn list_jobs_by_status_filters_and_limits() {
        let db = Db::open_in_memory().unwrap();
        let root = db.add_root("/photos", "reference").unwrap();
        let photo = db
            .upsert_photo(&sample_photo(root, "/photos/a.png", 100))
            .unwrap();
        let failed = db.enqueue_job(photo, "p", "keep").unwrap();
        db.enqueue_job(photo, "p", "keep").unwrap();
        db.update_job(&failed, QueueStatus::Failed, 0.0, Some("boom"), None)
            .unwrap();

        let rows = db.list_jobs_by_status(QueueStatus::Failed, 10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, failed);
        assert!(
            db.list_jobs_by_status(QueueStatus::Pending, 10)
                .unwrap()
                .len()
                == 1
        );
    }

    #[test]
    fn daemon_error_upsert_increments_attempts_and_clear_removes_it() {
        let db = Db::open_in_memory().unwrap();
        db.record_daemon_error(DaemonOperation::Scan, "cannot scan", Some("/missing"))
            .unwrap();
        db.record_daemon_error(DaemonOperation::Scan, "still missing", None)
            .unwrap();

        let errors = db.daemon_errors().unwrap();
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].operation, DaemonOperation::Scan);
        assert_eq!(errors[0].message, "still missing");
        assert_eq!(errors[0].detail, None);
        assert_eq!(errors[0].attempts, 2);

        db.clear_daemon_error(DaemonOperation::Scan).unwrap();
        assert!(db.daemon_errors().unwrap().is_empty());
    }

    #[test]
    fn daemon_commands_deduplicate_and_drain_in_order() {
        let db = Db::open_in_memory().unwrap();
        assert!(db.request_daemon_command(DaemonCommand::Rescan).unwrap());
        assert!(!db.request_daemon_command(DaemonCommand::Rescan).unwrap());
        assert!(db.request_daemon_command(DaemonCommand::Relog).unwrap());

        assert_eq!(
            db.take_daemon_commands().unwrap(),
            vec![DaemonCommand::Rescan, DaemonCommand::Relog]
        );
        assert!(db.take_daemon_commands().unwrap().is_empty());
        assert!(db.request_daemon_command(DaemonCommand::Relink).unwrap());
    }

    #[test]
    fn daemon_scan_root_commands_deduplicate_per_path() {
        let db = Db::open_in_memory().unwrap();
        assert!(db.request_daemon_scan_root("/photos/imported").unwrap());
        assert!(!db.request_daemon_scan_root("/photos/imported").unwrap());
        assert!(db.request_daemon_scan_root("/photos/other").unwrap());

        assert_eq!(
            db.take_daemon_commands().unwrap(),
            vec![
                DaemonCommand::ScanRoot {
                    path: "/photos/imported".into()
                },
                DaemonCommand::ScanRoot {
                    path: "/photos/other".into()
                },
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

    #[test]
    fn unknown_daemon_rows_are_ignored() {
        let db = Db::open_in_memory().unwrap();
        db.connection()
            .execute(
                "INSERT INTO daemon_errors(operation, message, detail, occurred_at, attempts) \
                 VALUES ('future-op', 'boom', NULL, '2026-10-21T00:00:00+00:00', 1)",
                [],
            )
            .unwrap();
        db.record_daemon_error(DaemonOperation::Log, "log failed", Some("detail"))
            .unwrap();

        let errors = db.daemon_errors().unwrap();
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].operation, DaemonOperation::Log);
        assert_eq!(errors[0].detail.as_deref(), Some("detail"));
        assert_eq!(errors[0].attempts, 1);

        db.connection()
            .execute(
                "INSERT INTO daemon_commands(command, created_at) VALUES ('future-command', 'now')",
                [],
            )
            .unwrap();
        assert!(db.request_daemon_command(DaemonCommand::Rescan).unwrap());
        assert_eq!(
            db.take_daemon_commands().unwrap(),
            vec![DaemonCommand::Rescan]
        );
        assert!(db.take_daemon_commands().unwrap().is_empty());
    }

    #[test]
    fn ensure_default_profiles_is_idempotent() {
        let db = Db::open_in_memory().unwrap();
        let first = db.ensure_default_profiles().unwrap();
        assert_eq!(first.len(), 3);

        let order: Vec<&str> = first.iter().map(|profile| profile.name.as_str()).collect();
        assert_eq!(order, ["無劣化webp", "discord用webp", "高画質webp"]);

        let by_name = |name: &str| first.iter().find(|profile| profile.name == name).unwrap();
        assert!(by_name("無劣化webp").auto_enabled);
        assert!(by_name("無劣化webp").lossless);
        assert!(!by_name("discord用webp").auto_enabled);
        assert!(by_name("discord用webp").clipboard);
        assert_eq!(by_name("discord用webp").width, 2560);
        assert_eq!(by_name("discord用webp").height, 1440);
        assert_eq!(
            by_name("discord用webp").output_dir.as_deref(),
            Some("discord")
        );
        assert_eq!(by_name("discord用webp").suffix, "_discord");
        assert_eq!(by_name("高画質webp").quality, 90);
        assert_eq!(by_name("高画質webp").output_dir.as_deref(), Some("webp"));

        let first_ids: Vec<String> = first.iter().map(|profile| profile.id.clone()).collect();
        let second = db.ensure_default_profiles().unwrap();
        assert_eq!(second.len(), 3);
        let second_ids: Vec<String> = second.iter().map(|profile| profile.id.clone()).collect();
        assert_eq!(first_ids, second_ids);
        assert_eq!(db.list_profiles().unwrap().len(), 3);
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
    fn ensure_default_profiles_rolls_back_partial_seed() {
        let db = Db::open_in_memory().unwrap();
        db.connection()
            .execute_batch(
                "CREATE TRIGGER fail_second_profile BEFORE INSERT ON compression_profiles \
                 WHEN (SELECT COUNT(*) FROM compression_profiles) = 1 \
                 BEGIN SELECT RAISE(ABORT, 'simulated write failure'); END;",
            )
            .unwrap();

        assert!(db.ensure_default_profiles().is_err());
        assert_eq!(db.list_profiles().unwrap().len(), 0);
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
            .query_row("SELECT COUNT(*) FROM compression_profiles", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn save_profile_assigns_and_preserves_sort_order() {
        let db = Db::open_in_memory().unwrap();
        for (id, name) in [("c", "C"), ("a", "A"), ("b", "B")] {
            db.save_profile(&profile(id, name)).unwrap();
        }

        let ids = || {
            db.list_profiles()
                .unwrap()
                .into_iter()
                .map(|profile| profile.id)
                .collect::<Vec<_>>()
        };
        assert_eq!(ids(), vec!["c", "a", "b"]);

        let mut edited = profile("a", "A renamed");
        edited.quality = 44;
        db.save_profile(&edited).unwrap();

        assert_eq!(ids(), vec!["c", "a", "b"]);
        let stored = db.list_profiles().unwrap();
        assert_eq!(stored[1].name, "A renamed");
        assert_eq!(stored[1].quality, 44);

        // Deleting the middle row leaves a gap. The next insert must use
        // MAX(sort_order) + 1, not COUNT(*), so "A0" goes after "B".
        db.delete_profile("a").unwrap();
        db.save_profile(&profile("d", "A0")).unwrap();
        assert_eq!(ids(), vec!["c", "b", "d"]);
    }

    #[test]
    fn save_profile_renames_the_name_column_and_json_together() {
        let db = Db::open_in_memory().unwrap();
        let mut saved = profile("p", "Old name");
        db.save_profile(&saved).unwrap();

        saved.name = "Archive".to_string();
        db.save_profile(&saved).unwrap();

        let (column, json): (String, String) = db
            .connection()
            .query_row(
                "SELECT name, json FROM compression_profiles WHERE id = 'p'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(column, "Archive");
        let stored: CompressionProfile = serde_json::from_str(&json).unwrap();
        assert_eq!(stored.name, "Archive");
        assert_eq!(db.list_profiles().unwrap()[0].name, "Archive");
    }

    #[test]
    fn renamed_default_profile_survives_reopen_and_reseeding() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("library.db");
        {
            let db = Db::open(&path).unwrap();
            let mut renamed = db.ensure_default_profiles().unwrap()[0].clone();
            renamed.name = "アーカイブ用".to_string();
            db.save_profile(&renamed).unwrap();
        }

        let db = Db::open(&path).unwrap();
        let profiles = db.ensure_default_profiles().unwrap();
        assert_eq!(profiles[0].name, "アーカイブ用");
        assert_eq!(profiles.len(), 3);
    }

    #[test]
    fn profiles_may_share_a_name() {
        let db = Db::open_in_memory().unwrap();
        db.save_profile(&profile("a", "同じ名前")).unwrap();
        db.save_profile(&profile("b", "同じ名前")).unwrap();

        let stored = db.list_profiles().unwrap();
        assert_eq!(stored.len(), 2);
        assert_eq!(stored[0].name, stored[1].name);
    }

    #[test]
    fn reorder_profiles_persists_and_rolls_back_bad_lists() {
        let db = Db::open_in_memory().unwrap();
        for (id, name) in [("c", "C"), ("a", "A"), ("b", "B")] {
            db.save_profile(&profile(id, name)).unwrap();
        }

        let ids = || {
            db.list_profiles()
                .unwrap()
                .into_iter()
                .map(|profile| profile.id)
                .collect::<Vec<_>>()
        };
        let order = |values: &[&str]| values.iter().map(|id| id.to_string()).collect::<Vec<_>>();

        // A legacy database where every row is zero sorts by name.
        db.connection()
            .execute("UPDATE compression_profiles SET sort_order = 0", [])
            .unwrap();
        assert_eq!(ids(), vec!["a", "b", "c"]);

        db.reorder_profiles(&order(&["c", "a", "b"])).unwrap();
        assert_eq!(ids(), vec!["c", "a", "b"]);

        assert!(matches!(
            db.reorder_profiles(&order(&["c", "a"])),
            Err(CoreError::Invalid(_))
        ));
        assert_eq!(ids(), vec!["c", "a", "b"]);

        assert!(matches!(
            db.reorder_profiles(&order(&["a", "a", "b"])),
            Err(CoreError::Invalid(_))
        ));
        assert_eq!(ids(), vec!["c", "a", "b"]);

        assert!(matches!(
            db.reorder_profiles(&order(&["c", "a", "missing"])),
            Err(CoreError::NotFound(_))
        ));
        assert_eq!(ids(), vec!["c", "a", "b"]);
    }

    #[test]
    fn reorder_profiles_survives_reopen_with_legacy_zero_order() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("library.db");

        {
            let db = Db::open(&path).unwrap();
            for (id, name) in [("c", "C"), ("a", "A"), ("b", "B")] {
                db.save_profile(&profile(id, name)).unwrap();
            }
            db.connection()
                .execute("UPDATE compression_profiles SET sort_order = 0", [])
                .unwrap();
            let ids: Vec<String> = db
                .list_profiles()
                .unwrap()
                .into_iter()
                .map(|profile| profile.id)
                .collect();
            assert_eq!(ids, vec!["a", "b", "c"]);

            db.reorder_profiles(&["c".to_string(), "a".to_string(), "b".to_string()])
                .unwrap();
        }

        let db = Db::open(&path).unwrap();
        let ids: Vec<String> = db
            .list_profiles()
            .unwrap()
            .into_iter()
            .map(|profile| profile.id)
            .collect();
        assert_eq!(ids, vec!["c", "a", "b"]);
    }

    #[test]
    fn duplicate_profile_inserts_named_copy_after_source() {
        let db = Db::open_in_memory().unwrap();
        for (id, name) in [("a", "A"), ("b", "B"), ("c", "C")] {
            db.save_profile(&profile(id, name)).unwrap();
        }
        let source = db
            .list_profiles()
            .unwrap()
            .into_iter()
            .find(|profile| profile.id == "b")
            .unwrap();

        let copy = db.duplicate_profile("b").unwrap();
        assert_ne!(copy.id, source.id);
        assert_eq!(copy.name, "B のコピー");
        assert!(!copy.auto_enabled);
        assert_eq!(copy.format, source.format);
        assert_eq!(copy.quality, source.quality);

        let stored = db.list_profiles().unwrap();
        let ids: Vec<&str> = stored.iter().map(|profile| profile.id.as_str()).collect();
        assert_eq!(ids, vec!["a", "b", copy.id.as_str(), "c"]);
        assert!(!stored[2].auto_enabled);

        let mut edited = copy.clone();
        edited.quality = 31;
        db.save_profile(&edited).unwrap();
        let stored = db.list_profiles().unwrap();
        assert_eq!(stored[1].quality, source.quality);
        assert_eq!(stored[2].quality, 31);

        let second = db.duplicate_profile("b").unwrap();
        assert_eq!(second.name, "B のコピー 2");
        let stored = db.list_profiles().unwrap();
        let ids: Vec<&str> = stored.iter().map(|profile| profile.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["a", "b", second.id.as_str(), copy.id.as_str(), "c"]
        );

        assert!(matches!(
            db.duplicate_profile("missing"),
            Err(CoreError::NotFound(_))
        ));
    }

    #[test]
    fn duplicate_profile_caps_generated_names_at_the_ui_limit() {
        let db = Db::open_in_memory().unwrap();
        let source_name = "あ".repeat(MAX_PROFILE_NAME_CHARS);
        db.save_profile(&profile("long", &source_name)).unwrap();

        let first = db.duplicate_profile("long").unwrap();
        assert_eq!(first.name.chars().count(), MAX_PROFILE_NAME_CHARS);
        assert!(first.name.ends_with(" のコピー"));

        let second = db.duplicate_profile("long").unwrap();
        assert_eq!(second.name.chars().count(), MAX_PROFILE_NAME_CHARS);
        assert!(second.name.ends_with(" のコピー 2"));
    }

    #[test]
    fn failed_profile_save_restores_the_previous_clipboard_flag() {
        let db = Db::open_in_memory().unwrap();
        let mut first = profile("first", "First");
        first.clipboard = true;
        db.save_profile(&first).unwrap();

        db.connection()
            .execute_batch(
                "CREATE TRIGGER fail_broken_profile BEFORE INSERT ON compression_profiles \
                 WHEN NEW.id = 'broken' BEGIN SELECT RAISE(ABORT, 'simulated write failure'); END;",
            )
            .unwrap();

        let mut broken = profile("broken", "Broken");
        broken.clipboard = true;
        assert!(db.save_profile(&broken).is_err());

        let stored = db.list_profiles().unwrap();
        assert_eq!(stored.len(), 1);
        assert!(stored[0].clipboard);
    }

    #[test]
    fn saving_a_clipboard_profile_clears_the_previous_one() {
        let db = Db::open_in_memory().unwrap();
        let mut first = profile("first", "First");
        first.clipboard = true;
        db.save_profile(&first).unwrap();
        let mut second = profile("second", "Second");
        second.clipboard = true;
        db.save_profile(&second).unwrap();

        let stored = db.list_profiles().unwrap();
        assert_eq!(stored.iter().filter(|profile| profile.clipboard).count(), 1);
        assert!(
            stored
                .iter()
                .find(|profile| profile.id == "second")
                .unwrap()
                .clipboard
        );
    }

    #[test]
    fn duplicate_profile_clears_the_clipboard_flag_on_the_copy() {
        let db = Db::open_in_memory().unwrap();
        let mut source = profile("source", "Source");
        source.clipboard = true;
        db.save_profile(&source).unwrap();

        let copy = db.duplicate_profile("source").unwrap();
        assert!(!copy.clipboard);

        let stored = db.list_profiles().unwrap();
        assert!(
            stored
                .iter()
                .find(|profile| profile.id == "source")
                .unwrap()
                .clipboard
        );
        assert!(
            !stored
                .iter()
                .find(|profile| profile.id == copy.id)
                .unwrap()
                .clipboard
        );
        assert_eq!(stored.iter().filter(|profile| profile.clipboard).count(), 1);
    }

    #[test]
    fn duplicate_profile_normalizes_legacy_zero_order() {
        let db = Db::open_in_memory().unwrap();
        for (id, name) in [("a", "A"), ("b", "B"), ("c", "C")] {
            db.save_profile(&profile(id, name)).unwrap();
        }
        db.connection()
            .execute("UPDATE compression_profiles SET sort_order = 0", [])
            .unwrap();

        let copy = db.duplicate_profile("b").unwrap();
        let stored = db.list_profiles().unwrap();
        let ids: Vec<&str> = stored.iter().map(|profile| profile.id.as_str()).collect();
        assert_eq!(ids, vec!["a", "b", copy.id.as_str(), "c"]);
    }

    #[test]
    fn delete_profile_blocks_active_jobs_and_keeps_history() {
        let db = Db::open_in_memory().unwrap();
        db.save_profile(&profile("p", "P")).unwrap();
        db.save_profile(&profile("q", "Q")).unwrap();
        let root = db.add_root("/photos", "reference").unwrap();
        let photo = db
            .upsert_photo(&sample_photo(root, "/photos/a.png", 100))
            .unwrap();
        let job = db.enqueue_job(photo, "p", "keep").unwrap();

        assert_eq!(db.list_profiles().unwrap().len(), 2);
        assert_eq!(db.active_job_count_for_profile("p").unwrap(), 1);
        assert!(matches!(db.delete_profile("p"), Err(CoreError::Invalid(_))));
        assert_eq!(db.list_profiles().unwrap().len(), 2);

        db.update_job(&job, QueueStatus::Processing, 0.1, None, None)
            .unwrap();
        assert_eq!(db.active_job_count_for_profile("p").unwrap(), 1);
        assert!(db.delete_profile("p").is_err());

        db.update_job(
            &job,
            QueueStatus::Completed,
            1.0,
            None,
            Some("/photos/compressed/a.webp"),
        )
        .unwrap();
        db.add_derivative(
            photo,
            "/photos/compressed/a.webp",
            "webp",
            16,
            10,
            10,
            "p",
            "keep",
        )
        .unwrap();
        let failed = db.enqueue_job(photo, "p", "keep").unwrap();
        db.update_job(&failed, QueueStatus::Failed, 0.0, Some("boom"), None)
            .unwrap();
        assert_eq!(db.active_job_count_for_profile("p").unwrap(), 0);

        db.delete_profile("p").unwrap();
        assert_eq!(db.list_profiles().unwrap().len(), 1);
        assert_eq!(db.list_profiles().unwrap()[0].id, "q");
        assert_eq!(db.list_jobs(10).unwrap().len(), 2);
        let derivatives = db.derivatives_for_photo(photo).unwrap();
        assert_eq!(derivatives.len(), 1);
        assert_eq!(derivatives[0].profile_id, "p");

        assert!(matches!(db.delete_profile("q"), Err(CoreError::Invalid(_))));
        assert_eq!(db.list_profiles().unwrap().len(), 1);
        assert!(matches!(
            db.delete_profile("missing"),
            Err(CoreError::NotFound(_))
        ));
    }

    #[test]
    fn guarded_enqueue_skips_missing_profiles() {
        let db = Db::open_in_memory().unwrap();
        let root = db.add_root("/photos", "reference").unwrap();
        let photo = db
            .upsert_photo(&sample_photo(root, "/photos/a.png", 100))
            .unwrap();

        assert!(
            !db.enqueue_job_for_existing_profile(photo, "missing", "keep")
                .unwrap()
        );
        assert_eq!(db.count_jobs(QueueStatus::Pending).unwrap(), 0);

        db.save_profile(&profile("p", "P")).unwrap();
        assert!(
            db.enqueue_job_for_existing_profile(photo, "p", "keep")
                .unwrap()
        );
        assert_eq!(db.count_jobs(QueueStatus::Pending).unwrap(), 1);
    }

    #[test]
    fn replaced_photo_stays_visible_and_stats_use_derivative_size() {
        let db = Db::open_in_memory().unwrap();
        let root = db.add_root("/photos", "reference").unwrap();
        let id = db
            .upsert_photo(&sample_photo(root, "/photos/a.png", 100))
            .unwrap();
        let derivative_id = db
            .add_derivative(
                id,
                "/photos/compressed/a.webp",
                "webp",
                16,
                10,
                1_234,
                "p",
                "replace",
            )
            .unwrap();

        assert!(db.set_replaced(id, derivative_id).unwrap());

        let photo = db.photo_by_id(id).unwrap().unwrap();
        assert_eq!(photo.status, PhotoStatus::Replaced);
        assert_eq!(
            photo.display_path.as_deref(),
            Some("/photos/compressed/a.webp")
        );
        assert_eq!(photo.effective_path(), "/photos/compressed/a.webp");
        assert_eq!(photo.effective_file_size(), 1_234);
        assert_eq!(photo.effective_dimensions(), (16, 10));

        assert_eq!(db.count_photos().unwrap(), 1);
        assert_eq!(db.photo_index(PhotoSort::Newest).unwrap().len(), 1);
        assert_eq!(
            db.photo_index_text(&PhotoTextFilter::default())
                .unwrap()
                .len(),
            1
        );

        let stats = db.stats().unwrap();
        assert_eq!(stats.photo_count, 1);
        assert_eq!(stats.total_bytes, 1_234);
        assert_eq!(stats.derivative_bytes, 1_234);
    }

    #[test]
    fn upserting_a_restored_original_clears_the_display_derivative() {
        let db = Db::open_in_memory().unwrap();
        let root = db.add_root("/photos", "reference").unwrap();
        let id = db
            .upsert_photo(&sample_photo(root, "/photos/a.png", 100))
            .unwrap();
        let derivative_id = db
            .add_derivative(
                id,
                "/photos/compressed/a.webp",
                "webp",
                16,
                10,
                1_234,
                "p",
                "replace",
            )
            .unwrap();
        assert!(db.set_replaced(id, derivative_id).unwrap());

        db.upsert_photo(&sample_photo(root, "/photos/a.png", 100))
            .unwrap();

        let photo = db.photo_by_id(id).unwrap().unwrap();
        assert_eq!(photo.status, PhotoStatus::Present);
        assert!(photo.display_derivative_id.is_none());
        assert!(photo.display_path.is_none());
        assert_eq!(photo.effective_path(), "/photos/a.png");
        assert_eq!(db.stats().unwrap().total_bytes, 12_400_000);
    }

    #[test]
    fn library_revision_starts_at_zero_and_is_monotonic() {
        let db = Db::open_in_memory().unwrap();
        assert_eq!(db.library_revision().unwrap(), 0);
        assert_eq!(db.bump_library_revision().unwrap(), 1);
        assert_eq!(db.bump_library_revision().unwrap(), 2);
        assert_eq!(db.library_revision().unwrap(), 2);
    }

    #[test]
    fn rebinding_a_photo_preserves_its_id_and_user_metadata() {
        let db = Db::open_in_memory().unwrap();
        let root = db.add_root("/photos", "reference").unwrap();
        let id = db
            .upsert_photo(&sample_photo(root, "/photos/a.png", 100))
            .unwrap();
        db.set_favorite(id, true).unwrap();
        db.set_note(id, Some("keep me")).unwrap();
        db.set_photo_tags(id, &["night".to_string()]).unwrap();

        let mut moved = sample_photo(root, "/photos/2026/a.png", 999);
        moved.captured_at_local = "2030-01-01 00:00:00".to_string();
        moved.width = 1;
        moved.height = 1;
        moved.blake3 = Some("hash-a".to_string());
        assert!(db.rebind_photo(id, &moved).unwrap());

        let photo = db.photo_by_id(id).unwrap().unwrap();
        assert_eq!(photo.abs_path, "/photos/2026/a.png");
        assert_eq!(photo.rel_path, "photos/2026/a.png");
        assert_eq!(photo.status, PhotoStatus::Present);
        assert_eq!(photo.captured_at_utc, 100);
        assert_eq!(photo.captured_at_local, "2026-10-21 21:14:03");
        assert_eq!((photo.width, photo.height), (3840, 2160));
        assert!(photo.is_favorite);
        assert_eq!(photo.note.as_deref(), Some("keep me"));
        assert_eq!(photo.blake3.as_deref(), Some("hash-a"));
        assert_eq!(db.photo_tags(id).unwrap(), vec!["night".to_string()]);
        assert_eq!(db.count_photos().unwrap(), 1);
    }

    #[test]
    fn rebind_candidate_prefers_a_unique_content_hash() {
        let db = Db::open_in_memory().unwrap();
        let root = db.add_root("/photos", "reference").unwrap();
        let id = db
            .upsert_photo(&sample_photo(root, "/photos/a.png", 100))
            .unwrap();
        let mut moved = sample_photo(root, "/photos/2026/a.png", 100);
        moved.blake3 = Some("same-content".to_string());
        db.connection()
            .execute(
                "UPDATE photos SET blake3 = 'same-content' WHERE id = ?1",
                params![id],
            )
            .unwrap();

        let candidate = db.find_rebind_candidate(&moved).unwrap().unwrap();
        assert_eq!(candidate.id, id);

        moved.blake3 = Some("different-content".to_string());
        assert!(db.find_rebind_candidate(&moved).unwrap().is_none());
    }

    #[test]
    fn duplicate_hashes_are_reported_without_merging_rows() {
        let db = Db::open_in_memory().unwrap();
        let root = db.add_root("/photos", "reference").unwrap();
        let mut first = sample_photo(root, "/photos/a.png", 100);
        first.blake3 = Some("same-content".to_string());
        let first_id = db.upsert_photo(&first).unwrap();
        let mut copy = sample_photo(root, "/photos/b.png", 101);
        copy.blake3 = Some("same-content".to_string());
        let copy_id = db.upsert_photo(&copy).unwrap();
        let mut other = sample_photo(root, "/photos/c.png", 102);
        other.blake3 = Some("other-content".to_string());
        db.upsert_photo(&other).unwrap();

        assert_eq!(db.duplicate_photo_ids().unwrap(), vec![first_id, copy_id]);
        assert_eq!(db.count_photos().unwrap(), 3);
    }
}
