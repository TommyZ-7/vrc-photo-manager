use std::collections::HashMap;
use std::path::{Path, PathBuf};

use rusqlite::types::Value;
use rusqlite::{Connection, OpenFlags, OptionalExtension, Row, params, params_from_iter};

use crate::error::Result;
use crate::model::{
    CompressionJob, CompressionProfile, Derivative, LibraryRoot, LibraryStats, NewPhoto, Photo,
    PhotoContext, PhotoFilter, PhotoIndexEntry, PhotoPlayer, PhotoSort, PhotoStatus,
    PhotoTextFilter, QueueStatus, Session, SessionPlayer, World,
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

const PHOTO_SELECT: &str = "p.id, p.root_id, p.rel_path, p.abs_path, p.file_name, \
    p.captured_at_utc, p.captured_at_local, p.tz_offset_sec, p.width, p.height, \
    p.file_size, p.mtime, p.blake3, p.status, p.is_favorite, p.note, \
    c.world_name, c.instance_id, c.link_method, c.confidence, c.author";

const PHOTO_FROM: &str = " FROM photos p LEFT JOIN photo_context c ON c.photo_id = p.id ";

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
        // WAL is not available for in-memory databases; ignore that case.
        let _ = conn.pragma_update(None, "journal_mode", "WAL");
        let _ = conn.pragma_update(None, "synchronous", "NORMAL");
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
        Ok(())
    }

    pub fn connection(&self) -> &Connection {
        &self.conn
    }

    // ---------- library roots ----------

    pub fn add_root(&self, path: &str, kind: &str) -> Result<i64> {
        let id = self.conn.query_row(
            "INSERT INTO library_roots(path, kind, created_at) VALUES (?1, ?2, ?3) \
             ON CONFLICT(path) DO UPDATE SET kind = excluded.kind RETURNING id",
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
                status = 'present', updated_at = excluded.updated_at \
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
             WHERE p.status = 'present' ORDER BY {order}"
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
            "SELECT {PHOTO_SELECT}{PHOTO_FROM} WHERE p.status = 'present' \
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
             AND p.status = 'present' ORDER BY p.captured_at_utc DESC LIMIT ?1"
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

    pub fn count_photos(&self) -> Result<i64> {
        let count = self.conn.query_row(
            "SELECT COUNT(*) FROM photos WHERE status = 'present'",
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

    pub fn stats(&self) -> Result<LibraryStats> {
        let (photo_count, total_bytes): (i64, i64) = self.conn.query_row(
            "SELECT COUNT(*), COALESCE(SUM(file_size), 0) FROM photos WHERE status = 'present'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let compressed_count: i64 = self.conn.query_row(
            "SELECT COUNT(DISTINCT photo_id) FROM derivatives",
            [],
            |row| row.get(0),
        )?;
        Ok(LibraryStats {
            photo_count,
            total_bytes: total_bytes.max(0) as u64,
            compressed_count,
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
        Self::insert_profile(&self.conn, profile)
    }

    fn insert_profile(conn: &Connection, profile: &CompressionProfile) -> Result<()> {
        let json = serde_json::to_string(profile)?;
        conn.execute(
            "INSERT INTO compression_profiles(id, name, enabled, auto_enabled, json, sort_order) \
             VALUES (?1, ?2, ?3, ?4, ?5, 0) \
             ON CONFLICT(id) DO UPDATE SET name = excluded.name, enabled = excluded.enabled, \
                auto_enabled = excluded.auto_enabled, json = excluded.json",
            params![
                profile.id,
                profile.name,
                profile.enabled as i64,
                profile.auto_enabled as i64,
                json,
            ],
        )?;
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
        for profile in CompressionProfile::defaults() {
            Self::insert_profile(&transaction, &profile)?;
        }
        transaction.commit()?;
        self.list_profiles()
    }

    pub fn delete_profile(&self, id: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM compression_profiles WHERE id = ?1",
            params![id],
        )?;
        Ok(())
    }

    // ---------- compression jobs ----------

    pub fn enqueue_job(&self, photo_id: i64, profile_id: &str, policy: &str) -> Result<String> {
        let id = uuid::Uuid::new_v4().to_string();
        self.conn.execute(
            "INSERT INTO compression_jobs(id, photo_id, profile_id, status, progress, policy, created_at) \
             VALUES (?1, ?2, ?3, 'pending', 0, ?4, ?5)",
            params![id, photo_id, profile_id, policy, now_string()],
        )?;
        Ok(id)
    }

    pub fn next_pending_job(&self) -> Result<Option<CompressionJob>> {
        let job = self
            .conn
            .query_row(
                "UPDATE compression_jobs SET status = 'processing', started_at = ?1 \
                 WHERE id = (SELECT id FROM compression_jobs WHERE status = 'pending' \
                    ORDER BY created_at LIMIT 1) \
                 RETURNING id, photo_id, profile_id, status, progress, error, output_path, policy, \
                    created_at, started_at, finished_at",
                params![now_string()],
                row_to_job,
            )
            .optional()?;
        Ok(job)
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
                created_at, started_at, finished_at FROM compression_jobs \
             ORDER BY created_at DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit.max(1)], row_to_job)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn count_jobs(&self, status: QueueStatus) -> Result<i64> {
        let count = self.conn.query_row(
            "SELECT COUNT(*) FROM compression_jobs WHERE status = ?1",
            params![status.as_str()],
            |row| row.get(0),
        )?;
        Ok(count)
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
        let mut stmt = self
            .conn
            .prepare("SELECT DISTINCT photo_id FROM derivatives")?;
        let rows = stmt.query_map([], |row| row.get(0))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn derivative_bytes(&self) -> Result<u64> {
        let bytes: i64 = self.conn.query_row(
            "SELECT COALESCE(SUM(file_size), 0) FROM derivatives",
            [],
            |row| row.get(0),
        )?;
        Ok(bytes.max(0) as u64)
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
    let mut clauses = vec!["p.status = 'present'".to_string()];
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
    let mut clauses = vec!["p.status = 'present'".to_string()];
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
        world_name: row.get(16)?,
        instance_id: row.get(17)?,
        link_method: row.get(18)?,
        confidence: row.get(19)?,
        author: row.get(20)?,
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
        created_at: row.get(8)?,
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
    use crate::model::{NewPhoto, PhotoFilter, PhotoPlayer};

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

    #[test]
    fn migration_adds_captured_id_index() {
        let db = Db::open_in_memory().unwrap();
        let count: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master \
                 WHERE type = 'index' AND name = 'idx_photos_captured_id'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
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
        let missing = db.photos_by_ids(&[ids[500], ids[0], -1]).unwrap();
        assert_eq!(
            missing.iter().map(|photo| photo.id).collect::<Vec<_>>(),
            vec![ids[500], ids[0]]
        );
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
            tx.commit().unwrap();
        }
        eprintln!("seed 100k: {:?}", seed_start.elapsed());

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
    fn ensure_default_profiles_is_idempotent() {
        let db = Db::open_in_memory().unwrap();
        let first = db.ensure_default_profiles().unwrap();
        assert_eq!(first.len(), 4);

        let mut names: Vec<&str> = first.iter().map(|profile| profile.name.as_str()).collect();
        names.sort_unstable();
        assert_eq!(
            names,
            [
                "AVIF 容量優先",
                "JPEG XL ロスレス",
                "JPEG 互換用",
                "WebP 高品質"
            ]
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
}
