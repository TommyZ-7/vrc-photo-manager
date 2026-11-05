use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use rusqlite::types::Value;
use rusqlite::{
    Connection, OpenFlags, OptionalExtension, Row, Transaction, TransactionBehavior, params,
    params_from_iter,
};
use vrc_photon_ai_protocol::{
    Catalog, ModelState, TagBranch, TaggingErrorCode, TaggingJobStatus, TaggingStage,
};

use crate::ai::{
    AI_PLUGIN_ID, AiTagDefinition, AiTagThreshold, MAX_TAGGING_RETRIES, NewPhotoAi, PhotoAi,
    PhotoAiTag, PhotoAiTagView, PluginInstallation, PluginState, TaggingJob,
};
use crate::error::{CoreError, Result};
use crate::model::{
    AiPhotoFilter, CompressionJob, CompressionProfile, DaemonCommand, DaemonError, DaemonOperation,
    DeletionBlockers, DeletionCounts, DeletionJobSummary, DeletionReason, DeletionRequest,
    DeletionStatus, Derivative, JobStatusCounts, LibraryRoot, LibraryStats, MAX_JOB_RETRIES,
    MAX_PROFILE_NAME_CHARS, NewPhoto, Photo, PhotoContext, PhotoFilter, PhotoIndexEntry,
    PhotoPlayer, PhotoSort, PhotoStatus, PhotoTextFilter, QueueStatus, SendCompletion,
    SendErrorCode, SendHistoryEntry, SendKind, SendOrigin, SendOutboxEntry, SendSource, SendStatus,
    Session, SessionPlayer, World,
};
use crate::world_name::{
    NOT_FOUND_RETRY_SECONDS, REQUEST_INTERVAL_SECONDS, WorldNameFailure, WorldNameLookup,
    WorldNameLookupCounts, WorldNameLookupStatus, backoff_seconds,
};

/// Highest schema version this build understands. A database with a newer
/// `user_version` was written by a future release and must not be opened.
const SCHEMA_VERSION: i64 = 14;

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
CREATE INDEX IF NOT EXISTS idx_jobs_photo_profile
    ON compression_jobs(photo_id, profile_id);

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

const SCHEMA_V9_JOB_PHOTO_PROFILE_INDEX: &str = "CREATE INDEX IF NOT EXISTS idx_jobs_photo_profile \
     ON compression_jobs(photo_id, profile_id);";

const SCHEMA_V10_MESSAGE_KEY: &str = "ALTER TABLE daemon_errors ADD COLUMN message_key TEXT;";
const SCHEMA_V10_MESSAGE_ARGS: &str = "ALTER TABLE daemon_errors ADD COLUMN message_args TEXT;";

const SCHEMA_V11_SEND_OUTBOX: &str = r#"
CREATE TABLE IF NOT EXISTS send_outbox (
    id                 INTEGER PRIMARY KEY AUTOINCREMENT,
    photo_id           INTEGER REFERENCES photos(id),
    kind               TEXT NOT NULL,   -- photo | test
    origin             TEXT NOT NULL,   -- manual | auto
    status             TEXT NOT NULL,   -- waiting | pending | processing | sent | failed | cancelled
    source             TEXT,            -- original | profile | derivative
    profile_id         TEXT,
    destination        TEXT,
    attempts           INTEGER NOT NULL DEFAULT 0,
    next_attempt_at    INTEGER,
    discord_message_id TEXT,
    discord_channel_id TEXT,
    discord_guild_id   TEXT,
    error_code         TEXT,
    error_detail       TEXT,
    created_at         TEXT NOT NULL,
    started_at         TEXT,
    finished_at        TEXT
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_send_outbox_active_photo
    ON send_outbox(photo_id)
    WHERE photo_id IS NOT NULL
      AND status IN ('waiting', 'pending', 'processing');

CREATE INDEX IF NOT EXISTS idx_send_outbox_due
    ON send_outbox(status, next_attempt_at, id);

CREATE INDEX IF NOT EXISTS idx_send_outbox_photo
    ON send_outbox(photo_id);
"#;

const SCHEMA_V12_WORLD_NAME_LOOKUPS: &str = r#"
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
"#;

const SCHEMA_V13_AI_TAGGING: &str = r#"
CREATE TABLE IF NOT EXISTS plugin_installations (
    plugin_id TEXT NOT NULL,
    variant TEXT NOT NULL,
    version TEXT NOT NULL,
    protocol_version INTEGER NOT NULL,
    path TEXT NOT NULL,
    manifest_json TEXT NOT NULL,
    catalog_version TEXT,
    active INTEGER NOT NULL DEFAULT 0,
    installed_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    PRIMARY KEY (plugin_id, variant, version)
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_plugin_installations_one_active
    ON plugin_installations(plugin_id) WHERE active = 1;

CREATE TABLE IF NOT EXISTS plugin_state (
    plugin_id TEXT PRIMARY KEY,
    model_state TEXT NOT NULL,
    model_revision TEXT,
    model_bytes_downloaded INTEGER NOT NULL DEFAULT 0,
    model_bytes_total INTEGER,
    model_error TEXT,
    probe_json TEXT,
    updated_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS ai_tag_definitions (
    plugin_id TEXT NOT NULL,
    tag_key TEXT NOT NULL,
    branch TEXT NOT NULL,
    name TEXT NOT NULL,
    name_en TEXT,
    description TEXT NOT NULL,
    default_threshold REAL NOT NULL,
    sort_order INTEGER NOT NULL,
    catalog_version TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    PRIMARY KEY (plugin_id, tag_key)
);

CREATE TABLE IF NOT EXISTS ai_tag_thresholds (
    plugin_id TEXT NOT NULL,
    tag_key TEXT NOT NULL,
    threshold REAL NOT NULL,
    updated_at TEXT NOT NULL,
    PRIMARY KEY (plugin_id, tag_key)
);

CREATE TABLE IF NOT EXISTS photo_ai (
    photo_id INTEGER PRIMARY KEY REFERENCES photos(id) ON DELETE CASCADE,
    plugin_id TEXT NOT NULL,
    variant TEXT NOT NULL,
    sidecar_version TEXT NOT NULL,
    model_version TEXT NOT NULL,
    model_revision TEXT NOT NULL,
    catalog_version TEXT NOT NULL,
    source TEXT NOT NULL,
    presence_threshold REAL NOT NULL,
    character_present INTEGER NOT NULL,
    presence_probability REAL NOT NULL,
    people_min INTEGER,
    people_max INTEGER,
    people_median INTEGER,
    place_branch TEXT,
    place_probabilities_json TEXT,
    tagged_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS photo_ai_tags (
    photo_id INTEGER NOT NULL REFERENCES photos(id) ON DELETE CASCADE,
    plugin_id TEXT NOT NULL,
    tag_key TEXT NOT NULL,
    probability REAL NOT NULL,
    applied INTEGER NOT NULL,
    model_version TEXT NOT NULL,
    catalog_version TEXT NOT NULL,
    tagged_at TEXT NOT NULL,
    PRIMARY KEY (photo_id, plugin_id, tag_key)
);

CREATE TABLE IF NOT EXISTS tagging_jobs (
    id TEXT PRIMARY KEY,
    photo_id INTEGER NOT NULL REFERENCES photos(id) ON DELETE CASCADE,
    plugin_id TEXT NOT NULL,
    variant TEXT,
    status TEXT NOT NULL,
    progress REAL NOT NULL DEFAULT 0,
    stage TEXT,
    error TEXT,
    retry_count INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL,
    started_at TEXT,
    finished_at TEXT
);

CREATE INDEX IF NOT EXISTS idx_photo_ai_tags_applied
    ON photo_ai_tags(plugin_id, tag_key, applied, photo_id);
CREATE INDEX IF NOT EXISTS idx_photo_ai_people
    ON photo_ai(people_min, people_max);
CREATE INDEX IF NOT EXISTS idx_tagging_jobs_status
    ON tagging_jobs(status);
CREATE INDEX IF NOT EXISTS idx_tagging_jobs_claim
    ON tagging_jobs(status, created_at);
CREATE INDEX IF NOT EXISTS idx_tagging_jobs_photo
    ON tagging_jobs(photo_id, created_at);
CREATE INDEX IF NOT EXISTS idx_tagging_jobs_plugin_status
    ON tagging_jobs(plugin_id, status);
CREATE INDEX IF NOT EXISTS idx_tagging_jobs_active_list
    ON tagging_jobs(created_at DESC, id DESC)
    WHERE status IN ('waiting', 'pending', 'processing');
CREATE UNIQUE INDEX IF NOT EXISTS idx_tagging_jobs_active
    ON tagging_jobs(photo_id, plugin_id)
    WHERE status IN ('waiting', 'pending', 'processing');
"#;

const SCHEMA_V14_INDEXES: &str = r#"
CREATE INDEX IF NOT EXISTS idx_jobs_claim ON compression_jobs(status, created_at);
CREATE INDEX IF NOT EXISTS idx_jobs_created ON compression_jobs(created_at DESC);
CREATE INDEX IF NOT EXISTS idx_sessions_started ON sessions(started_at);
CREATE INDEX IF NOT EXISTS idx_sessions_ended ON sessions(ended_at) WHERE ended_at IS NOT NULL;
"#;

/// Sessions that end or start within an inclusive window around a capture.
/// `UNION` (not `UNION ALL`) collapses a zero-length session that matches both
/// ranges; the final ordering matches the caller's "at most two" contract.
const SESSIONS_NEAR_SQL: &str = "SELECT id, log_file, started_at, ended_at, world_id, instance_id \
     FROM sessions \
     WHERE started_at BETWEEN ?1 AND ?1 + ?2 \
     UNION \
     SELECT id, log_file, started_at, ended_at, world_id, instance_id \
     FROM sessions \
     WHERE ended_at IS NOT NULL AND ended_at BETWEEN ?1 - ?2 AND ?1 \
     ORDER BY started_at LIMIT 2";

const PHOTO_SELECT: &str = "p.id, p.root_id, p.rel_path, p.abs_path, p.file_name, \
    p.captured_at_utc, p.captured_at_local, p.tz_offset_sec, p.width, p.height, \
    p.file_size, p.mtime, p.blake3, p.status, p.is_favorite, p.note, \
    p.display_derivative_id, d.path, d.format, d.file_size, d.width, d.height, \
    c.world_name, c.instance_id, c.link_method, c.confidence, c.author, pw.world_id";

const PHOTO_FROM: &str = " FROM photos p \
    LEFT JOIN derivatives d ON d.id = p.display_derivative_id \
    LEFT JOIN photo_context c ON c.photo_id = p.id \
    LEFT JOIN worlds pw ON pw.id = c.world_id ";

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
        let version: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
        check_schema_version(version)?;
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
        check_schema_version(version)?;
        if version < 1 {
            self.migrate_v1()?;
        }
        if version < 2 {
            self.migrate_v2()?;
        }
        if version < 3 {
            self.migrate_v3()?;
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
        if version < 9 {
            self.migrate_v9()?;
        }
        if version < 10 {
            self.migrate_v10()?;
        }
        if version < 11 {
            self.migrate_v11()?;
        }
        if version < 12 {
            self.migrate_v12()?;
        }
        if version < 13 {
            self.migrate_v13()?;
        }
        if version < 14 {
            self.migrate_v14()?;
        }
        Ok(())
    }

    /// Applies schema version 1 in one immediate transaction. The write lock
    /// serializes concurrent UI/daemon startups, and the `IF NOT EXISTS` DDL
    /// makes the step safe to retry after an interrupted upgrade.
    fn migrate_v1(&self) -> Result<()> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        // Re-read under the write lock: another process may have completed V1
        // between the caller's version check and this transaction.
        let version: i64 = tx.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version >= 1 {
            tx.commit()?;
            return Ok(());
        }

        tx.execute_batch(SCHEMA_V1)?;
        tx.execute(
            "INSERT OR REPLACE INTO schema_migrations(version, applied_at) VALUES (1, ?1)",
            params![now_string()],
        )?;
        tx.pragma_update(None, "user_version", 1)?;
        tx.commit()?;
        Ok(())
    }

    /// Applies schema version 2 in one immediate transaction. The column
    /// check makes the step safe to retry after a crash between the ALTER
    /// and `user_version`, which the pre-transaction runner could leave
    /// behind as a duplicate column on the next open.
    fn migrate_v2(&self) -> Result<()> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let version: i64 = tx.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version >= 2 {
            tx.commit()?;
            return Ok(());
        }

        let has_author: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('photo_context') \
             WHERE name = 'author')",
            [],
            |row| row.get(0),
        )?;
        if !has_author {
            tx.execute_batch(SCHEMA_V2)?;
        }
        tx.execute(
            "INSERT OR REPLACE INTO schema_migrations(version, applied_at) VALUES (2, ?1)",
            params![now_string()],
        )?;
        tx.pragma_update(None, "user_version", 2)?;
        tx.commit()?;
        Ok(())
    }

    /// Applies schema version 3 in one immediate transaction. The index uses
    /// `IF NOT EXISTS`, so the step is safe to retry after an interrupted
    /// upgrade.
    fn migrate_v3(&self) -> Result<()> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let version: i64 = tx.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version >= 3 {
            tx.commit()?;
            return Ok(());
        }

        tx.execute_batch(SCHEMA_V3)?;
        tx.execute(
            "INSERT OR REPLACE INTO schema_migrations(version, applied_at) VALUES (3, ?1)",
            params![now_string()],
        )?;
        tx.pragma_update(None, "user_version", 3)?;
        tx.commit()?;
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

    /// Adds the `(photo_id, profile_id)` index used by the automatic
    /// compression dedup check. `IF NOT EXISTS` makes the step safe to retry
    /// after a crash between the DDL and `user_version`.
    fn migrate_v9(&self) -> Result<()> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let version: i64 = tx.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version >= 9 {
            tx.commit()?;
            return Ok(());
        }

        tx.execute_batch(SCHEMA_V9_JOB_PHOTO_PROFILE_INDEX)?;
        tx.execute(
            "INSERT OR REPLACE INTO schema_migrations(version, applied_at) VALUES (9, ?1)",
            params![now_string()],
        )?;
        tx.pragma_update(None, "user_version", 9)?;
        tx.commit()?;
        Ok(())
    }

    /// Adds the key/args columns used to render stored daemon errors in the
    /// current language. Additive and nullable, so old rows keep their
    /// `message` fallback.
    fn migrate_v10(&self) -> Result<()> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let version: i64 = tx.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version >= 10 {
            tx.commit()?;
            return Ok(());
        }

        for (column, sql) in [
            ("message_key", SCHEMA_V10_MESSAGE_KEY),
            ("message_args", SCHEMA_V10_MESSAGE_ARGS),
        ] {
            let exists: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM pragma_table_info('daemon_errors') \
                 WHERE name = ?1)",
                params![column],
                |row| row.get(0),
            )?;
            if !exists {
                tx.execute_batch(sql)?;
            }
        }
        tx.execute(
            "INSERT OR REPLACE INTO schema_migrations(version, applied_at) VALUES (10, ?1)",
            params![now_string()],
        )?;
        tx.pragma_update(None, "user_version", 10)?;
        tx.commit()?;
        Ok(())
    }

    /// Adds the durable Discord send outbox. The table and indexes use
    /// `IF NOT EXISTS`, so the step is safe to retry after a crash between the
    /// DDL and `user_version`.
    fn migrate_v11(&self) -> Result<()> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let version: i64 = tx.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version >= 11 {
            tx.commit()?;
            return Ok(());
        }

        tx.execute_batch(SCHEMA_V11_SEND_OUTBOX)?;
        tx.execute(
            "INSERT OR REPLACE INTO schema_migrations(version, applied_at) VALUES (11, ?1)",
            params![now_string()],
        )?;
        tx.pragma_update(None, "user_version", 11)?;
        tx.commit()?;
        Ok(())
    }

    /// Adds the world name lookup queue. The table and index use
    /// `IF NOT EXISTS`, so the step is safe to retry after a crash between the
    /// DDL and `user_version`.
    fn migrate_v12(&self) -> Result<()> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let version: i64 = tx.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version >= 12 {
            tx.commit()?;
            return Ok(());
        }

        tx.execute_batch(SCHEMA_V12_WORLD_NAME_LOOKUPS)?;
        tx.execute(
            "INSERT OR REPLACE INTO schema_migrations(version, applied_at) VALUES (12, ?1)",
            params![now_string()],
        )?;
        tx.pragma_update(None, "user_version", 12)?;
        tx.commit()?;
        Ok(())
    }

    /// Adds the AI tagging plugin, catalog, result, and job tables. The
    /// tables and indexes use `IF NOT EXISTS`, so the step is safe to retry
    /// after a crash between the DDL and `user_version`.
    fn migrate_v13(&self) -> Result<()> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let version: i64 = tx.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version >= 13 {
            tx.commit()?;
            return Ok(());
        }

        tx.execute_batch(SCHEMA_V13_AI_TAGGING)?;
        tx.execute(
            "INSERT OR REPLACE INTO schema_migrations(version, applied_at) VALUES (13, ?1)",
            params![now_string()],
        )?;
        tx.pragma_update(None, "user_version", 13)?;
        tx.commit()?;
        Ok(())
    }

    /// Adds the claim/sort indexes for compression jobs and session time
    /// lookups. `IF NOT EXISTS` makes the step safe to retry after a crash
    /// between the DDL and `user_version`.
    fn migrate_v14(&self) -> Result<()> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let version: i64 = tx.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version >= 14 {
            tx.commit()?;
            return Ok(());
        }

        tx.execute_batch(SCHEMA_V14_INDEXES)?;
        tx.execute(
            "INSERT OR REPLACE INTO schema_migrations(version, applied_at) VALUES (14, ?1)",
            params![now_string()],
        )?;
        tx.pragma_update(None, "user_version", 14)?;
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
                ) OR photos.status NOT IN ('present', 'replaced', 'missing', 'deleting') \
                THEN photos.status ELSE 'present' END, \
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
        let (where_sql, values) = build_text_filter(filter);
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
        let filter = PhotoTextFilter {
            world: world.map(str::to_string),
            player: player.map(str::to_string),
            tag: tag.map(str::to_string),
            favorite,
            from_utc,
            to_utc,
            ai: AiPhotoFilter::default(),
        };
        let (where_sql, mut values) = build_text_filter(&filter);

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
                file_size = ?5, mtime = ?6, blake3 = COALESCE(?7, blake3), \
                status = CASE WHEN status IN ('present', 'replaced', 'missing', 'deleting') \
                    THEN 'present' ELSE status END, \
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
        // Standalone calls take the write lock up front: with a deferred
        // transaction another process could write between the SELECT and the
        // UPDATE, turning our snapshot stale (SQLITE_BUSY_SNAPSHOT, which
        // busy_timeout does not retry) or racing the insert on a new world.
        // Calls already inside a caller-owned transaction (for example the
        // demo seed) must join it instead of nesting a second transaction.
        if self.conn.is_autocommit() {
            let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
            let id = self.upsert_world_inner(&tx, world_id, name, source)?;
            tx.commit()?;
            Ok(id)
        } else {
            self.upsert_world_inner(&self.conn, world_id, name, source)
        }
    }

    fn upsert_world_inner(
        &self,
        conn: &Connection,
        world_id: &str,
        name: Option<&str>,
        source: Option<&str>,
    ) -> Result<i64> {
        let now = now_string();
        let existing = conn
            .query_row(
                "SELECT id, name, name_source, alias FROM worlds WHERE world_id = ?1",
                params![world_id],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, Option<String>>(3)?,
                    ))
                },
            )
            .optional()?;

        let Some((id, old_name, old_source, alias)) = existing else {
            return conn
                .query_row(
                    "INSERT INTO worlds(world_id, name, name_source, first_seen, last_seen) \
                 VALUES (?1, ?2, ?3, ?4, ?4) RETURNING id",
                    params![world_id, name, source, now],
                    |row| row.get(0),
                )
                .map_err(Into::into);
        };

        // A missing name is always filled. An API name may be replaced by a
        // later log/XMP name; log/XMP names keep the first one seen.
        let replaces_api_name =
            old_source.as_deref() == Some("vrchat-api") && matches!(source, Some("log" | "xmp"));
        let stored_name = if old_name.is_none() || replaces_api_name {
            name.map(str::to_string).or_else(|| old_name.clone())
        } else {
            old_name.clone()
        };
        let stored_source = if name.is_some() && (old_name.is_none() || replaces_api_name) {
            source.map(str::to_string).or_else(|| old_source.clone())
        } else {
            old_source.clone()
        };
        conn.execute(
            "UPDATE worlds SET name = ?1, name_source = ?2, last_seen = ?3 WHERE id = ?4",
            params![stored_name, stored_source, now, id],
        )?;

        // Alias always wins for display. When an API name is replaced, photos
        // that already show that API name must follow the new name too.
        let effective_name = alias.as_deref().or(stored_name.as_deref());
        let updated = match effective_name {
            Some(effective_name) if replaces_api_name => conn.execute(
                "UPDATE photo_context SET world_name = ?1 \
                 WHERE world_id = ?2 AND (world_name IS NULL OR world_name = ?3)",
                params![effective_name, id, old_name],
            )?,
            Some(effective_name) => conn.execute(
                "UPDATE photo_context SET world_name = ?1 \
                 WHERE world_id = ?2 AND world_name IS NULL",
                params![effective_name, id],
            )?,
            None => 0,
        };

        if stored_name.is_some() && matches!(source, Some("log" | "xmp")) {
            self.mark_world_name_lookup_satisfied(id)?;
        }
        if updated > 0 {
            self.bump_library_revision()?;
        }
        Ok(id)
    }

    pub fn world_by_id(&self, id: i64) -> Result<Option<World>> {
        let world = self
            .conn
            .query_row(
                "SELECT id, world_id, name, name_source, alias FROM worlds WHERE id = ?1",
                params![id],
                row_to_world,
            )
            .optional()?;
        Ok(world)
    }

    pub fn world_by_world_id(&self, world_id: &str) -> Result<Option<World>> {
        let world = self
            .conn
            .query_row(
                "SELECT id, world_id, name, name_source, alias FROM worlds WHERE world_id = ?1",
                params![world_id],
                row_to_world,
            )
            .optional()?;
        Ok(world)
    }

    // ---------- world name lookups ----------

    /// Fills NULL `photo_context.world_name` rows from the stored world name,
    /// preferring the user alias. Returns the number of updated contexts.
    pub fn backfill_photo_context_world_names(&self) -> Result<usize> {
        let tx = self.conn.unchecked_transaction()?;
        let updated = tx.execute(
            "UPDATE photo_context SET world_name = ( \
                 SELECT COALESCE(w.alias, w.name) FROM worlds w \
                 WHERE w.id = photo_context.world_id \
             ) \
             WHERE world_name IS NULL AND world_id IS NOT NULL \
               AND EXISTS (SELECT 1 FROM worlds w \
                           WHERE w.id = photo_context.world_id \
                             AND COALESCE(w.alias, w.name) IS NOT NULL)",
            [],
        )?;
        if updated > 0 {
            self.bump_library_revision()?;
        }
        tx.commit()?;
        Ok(updated)
    }

    /// Queues a world for name resolution unless it is already queued.
    pub fn enqueue_world_name_lookup(&self, world_db_id: i64) -> Result<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO world_name_lookups(world_db_id, status, updated_at) \
             SELECT ?1, 'pending', ?2 \
             WHERE EXISTS (SELECT 1 FROM worlds WHERE id = ?1)",
            params![world_db_id, now_string()],
        )?;
        Ok(())
    }

    /// Queues every world referenced by a photo that has no name anywhere yet.
    /// Idempotent: existing lookup rows are never modified.
    pub fn enqueue_missing_world_name_lookups(&self) -> Result<usize> {
        let changed = self.conn.execute(
            "INSERT INTO world_name_lookups(world_db_id, status, updated_at) \
             SELECT w.id, 'pending', ?1 FROM worlds w \
             WHERE w.alias IS NULL AND w.name IS NULL \
               AND EXISTS (SELECT 1 FROM photo_context c \
                           WHERE c.world_id = w.id AND c.world_name IS NULL) \
               AND NOT EXISTS (SELECT 1 FROM world_name_lookups l \
                               WHERE l.world_db_id = w.id)",
            params![now_string()],
        )?;
        Ok(changed)
    }

    /// Oldest due lookup. `pending`, `retry`, and `not_found` rows are
    /// eligible; rows scheduled in the future are skipped.
    pub fn next_due_world_name_lookup(&self, now: i64) -> Result<Option<WorldNameLookup>> {
        let lookup = self
            .conn
            .query_row(
                "SELECT l.world_db_id, w.world_id, l.status, l.attempts, l.next_attempt_at \
                 FROM world_name_lookups l JOIN worlds w ON w.id = l.world_db_id \
                 WHERE l.status IN ('pending', 'retry', 'not_found') \
                   AND (l.next_attempt_at IS NULL OR l.next_attempt_at <= ?1) \
                 ORDER BY COALESCE(l.next_attempt_at, 0), l.world_db_id LIMIT 1",
                params![now],
                row_to_world_name_lookup,
            )
            .optional()?;
        Ok(lookup)
    }

    pub fn world_name_lookup(&self, world_db_id: i64) -> Result<Option<WorldNameLookup>> {
        let lookup = self
            .conn
            .query_row(
                "SELECT l.world_db_id, w.world_id, l.status, l.attempts, l.next_attempt_at \
                 FROM world_name_lookups l JOIN worlds w ON w.id = l.world_db_id \
                 WHERE l.world_db_id = ?1",
                params![world_db_id],
                row_to_world_name_lookup,
            )
            .optional()?;
        Ok(lookup)
    }

    /// Marks a queued lookup resolved because a log/XMP name already exists.
    pub fn mark_world_name_lookup_satisfied(&self, world_db_id: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE world_name_lookups SET status = 'resolved', next_attempt_at = NULL, \
                last_error = NULL, updated_at = ?1 \
             WHERE world_db_id = ?2 AND status IN ('pending', 'retry', 'not_found')",
            params![now_string(), world_db_id],
        )?;
        Ok(())
    }

    pub fn mark_world_name_lookup_resolved(&self, world_db_id: i64, now: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE world_name_lookups SET status = 'resolved', next_attempt_at = NULL, \
                last_error = NULL, last_attempt_at = ?1, updated_at = ?2 \
             WHERE world_db_id = ?3 AND status IN ('pending', 'retry', 'not_found')",
            params![now, now_string(), world_db_id],
        )?;
        Ok(())
    }

    /// Stores an API result without overriding a log/XMP name. NULL photo
    /// contexts are filled with the effective stored name either way.
    pub fn record_world_name_resolved(&self, world_db_id: i64, name: &str, now: i64) -> Result<()> {
        let Some(world) = self.world_by_id(world_db_id)? else {
            return Ok(());
        };
        self.upsert_world(&world.world_id, Some(name), Some("vrchat-api"))?;
        self.mark_world_name_lookup_resolved(world_db_id, now)?;
        Ok(())
    }

    /// Records the outcome of a failed attempt and schedules the next try
    /// according to the shared retry policy. 429s do not count as attempts
    /// and never schedule sooner than the global request interval.
    pub fn record_world_name_failure(
        &self,
        world_db_id: i64,
        failure: WorldNameFailure,
        now: i64,
    ) -> Result<()> {
        // Read-then-write must take the write lock up front, like
        // `upsert_world`: a deferred transaction can hit SQLITE_BUSY_SNAPSHOT
        // when the UI writes between our SELECT and UPDATE.
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let attempts = tx
            .query_row(
                "SELECT attempts FROM world_name_lookups WHERE world_db_id = ?1 \
                 AND status IN ('pending', 'retry', 'not_found')",
                params![world_db_id],
                |row| row.get::<_, i64>(0),
            )
            .optional()?;
        let Some(attempts) = attempts else {
            return Ok(());
        };

        let (status, attempts, next_attempt_at, last_error) = match failure {
            WorldNameFailure::RateLimited { retry_after } => (
                WorldNameLookupStatus::Retry,
                attempts,
                Some(now + retry_after.max(REQUEST_INTERVAL_SECONDS)),
                "http_429",
            ),
            WorldNameFailure::Retryable { code } => {
                let attempts = attempts + 1;
                (
                    WorldNameLookupStatus::Retry,
                    attempts,
                    Some(now + backoff_seconds(attempts)),
                    code,
                )
            }
            WorldNameFailure::NotFound => (
                WorldNameLookupStatus::NotFound,
                attempts + 1,
                Some(now + NOT_FOUND_RETRY_SECONDS),
                "not_found",
            ),
            WorldNameFailure::Invalid { code } => {
                (WorldNameLookupStatus::Failed, attempts + 1, None, code)
            }
        };

        tx.execute(
            "UPDATE world_name_lookups SET status = ?1, attempts = ?2, next_attempt_at = ?3, \
                last_attempt_at = ?4, last_error = ?5, updated_at = ?6 \
             WHERE world_db_id = ?7",
            params![
                status.as_str(),
                attempts,
                next_attempt_at,
                now,
                last_error,
                now_string(),
                world_db_id,
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn world_name_next_request_at(&self) -> Result<Option<i64>> {
        Ok(self
            .get_setting("world_name_next_request_at")?
            .and_then(|value| value.parse().ok()))
    }

    pub fn set_world_name_next_request_at(&self, timestamp: i64) -> Result<()> {
        self.set_setting("world_name_next_request_at", &timestamp.to_string())
    }

    pub fn world_name_lookup_counts(&self) -> Result<WorldNameLookupCounts> {
        let (pending, resolved, failed) = self.conn.query_row(
            "SELECT \
                COALESCE(SUM(CASE WHEN status IN ('pending', 'retry', 'not_found') \
                    THEN 1 ELSE 0 END), 0), \
                COALESCE(SUM(CASE WHEN status = 'resolved' THEN 1 ELSE 0 END), 0), \
                COALESCE(SUM(CASE WHEN status = 'failed' THEN 1 ELSE 0 END), 0) \
             FROM world_name_lookups",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        Ok(WorldNameLookupCounts {
            pending,
            resolved,
            failed,
        })
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

    /// Sessions that do not contain `timestamp` but end or start within
    /// `window_seconds` of it. At most two are returned: callers only need
    /// to distinguish none, exactly one, and ambiguous.
    pub fn sessions_near(&self, timestamp: i64, window_seconds: i64) -> Result<Vec<Session>> {
        let mut stmt = self.conn.prepare(SESSIONS_NEAR_SQL)?;
        let rows = stmt.query_map(params![timestamp, window_seconds], |row| {
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
        // A savepoint instead of a transaction, so the method also works
        // inside a caller's transaction (e.g. `in_transaction`).
        self.conn.execute_batch("SAVEPOINT set_photo_players")?;
        let result = (|| {
            self.conn.execute(
                "DELETE FROM photo_players WHERE photo_id = ?1",
                params![photo_id],
            )?;
            for player in players {
                if player.user_id.trim().is_empty() {
                    continue;
                }
                self.conn.execute(
                    "INSERT OR REPLACE INTO photo_players(photo_id, user_id, display_name) \
                     VALUES (?1, ?2, ?3)",
                    params![photo_id, player.user_id, player.display_name],
                )?;
            }
            Ok(())
        })();
        match result {
            Ok(()) => {
                self.conn.execute_batch("RELEASE set_photo_players")?;
                Ok(())
            }
            Err(error) => {
                let _ = self.conn.execute_batch("ROLLBACK TO set_photo_players");
                let _ = self.conn.execute_batch("RELEASE set_photo_players");
                Err(error)
            }
        }
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
        // A savepoint instead of a transaction, so the method also works
        // inside a caller's transaction (e.g. `in_transaction`).
        self.conn.execute_batch("SAVEPOINT set_photo_tags")?;
        let result = (|| {
            self.conn.execute(
                "DELETE FROM photo_tags WHERE photo_id = ?1",
                params![photo_id],
            )?;
            for name in tags {
                let name = name.trim();
                if name.is_empty() {
                    continue;
                }
                self.conn.execute(
                    "INSERT INTO tags(name) VALUES (?1) ON CONFLICT(name) DO NOTHING",
                    params![name],
                )?;
                let tag_id: i64 = self.conn.query_row(
                    "SELECT id FROM tags WHERE name = ?1",
                    params![name],
                    |row| row.get(0),
                )?;
                self.conn.execute(
                    "INSERT OR IGNORE INTO photo_tags(photo_id, tag_id) VALUES (?1, ?2)",
                    params![photo_id, tag_id],
                )?;
            }
            Ok(())
        })();
        match result {
            Ok(()) => {
                self.conn.execute_batch("RELEASE set_photo_tags")?;
                Ok(())
            }
            Err(error) => {
                let _ = self.conn.execute_batch("ROLLBACK TO set_photo_tags");
                let _ = self.conn.execute_batch("RELEASE set_photo_tags");
                Err(error)
            }
        }
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
        if profile.discord {
            Self::clear_other_discord_profiles(conn, &profile.id)?;
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

    /// Keeps the Discord target unique even when a caller other than the
    /// profile switch saves a flagged profile.
    fn clear_other_discord_profiles(conn: &Connection, keep_id: &str) -> Result<()> {
        let mut stmt = conn.prepare("SELECT id, json FROM compression_profiles WHERE id != ?1")?;
        let rows = stmt.query_map(params![keep_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut updates = Vec::new();
        for row in rows {
            let (id, json) = row?;
            let mut profile: CompressionProfile = serde_json::from_str(&json)?;
            if profile.discord {
                profile.discord = false;
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
            // The Discord target is unique for the same reason.
            discord: false,
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
    /// the executor removes only the original file. Queued and failed sends are
    /// cancelled in the same transaction: they carry no data-loss risk, while a
    /// `processing` upload is left to finish.
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
            tx.execute(
                "UPDATE send_outbox SET status = 'cancelled', finished_at = ?1 \
                 WHERE photo_id = ?2 AND status IN ('waiting', 'pending', 'failed')",
                params![now, photo_id],
            )?;
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

    /// Counts compression, send, and tagging work per status for the warning
    /// modal. Unknown compression statuses are reported as `unknown` and keep
    /// the photo affected: a status written by a newer version is never
    /// terminal. Unknown send and tagging statuses also count as pending.
    /// `affected_photos` is the union of distinct selected photos with
    /// non-terminal jobs, so a photo with several kinds of work counts once.
    pub fn deletion_job_summary(&self, photo_ids: &[i64]) -> Result<DeletionJobSummary> {
        let mut compression = JobStatusCounts::default();
        let mut send = JobStatusCounts::default();
        let mut tagging = JobStatusCounts::default();
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
                    QueueStatus::Unknown => compression.unknown += count,
                    QueueStatus::Completed | QueueStatus::Cancelled => {}
                }
            }

            let sql = format!(
                "SELECT status, COUNT(*) FROM send_outbox \
                 WHERE photo_id IN ({placeholders}) GROUP BY status"
            );
            let mut stmt = self.conn.prepare(&sql)?;
            let rows = stmt.query_map(params_from_iter(chunk.iter().copied()), |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })?;
            for row in rows {
                let (status, count) = row?;
                match SendStatus::parse(&status) {
                    SendStatus::Waiting => send.waiting += count,
                    SendStatus::Pending => send.pending += count,
                    SendStatus::Processing => send.processing += count,
                    SendStatus::Failed => send.failed += count,
                    SendStatus::Unknown => send.pending += count,
                    SendStatus::Sent | SendStatus::Cancelled => {}
                }
            }

            let sql = format!(
                "SELECT status, COUNT(*) FROM tagging_jobs \
                 WHERE photo_id IN ({placeholders}) GROUP BY status"
            );
            let mut stmt = self.conn.prepare(&sql)?;
            let rows = stmt.query_map(params_from_iter(chunk.iter().copied()), |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })?;
            for row in rows {
                let (status, count) = row?;
                match TaggingJobStatus::parse(&status) {
                    Some(TaggingJobStatus::Waiting) => tagging.waiting += count,
                    Some(TaggingJobStatus::Pending) => tagging.pending += count,
                    Some(TaggingJobStatus::Processing) => tagging.processing += count,
                    Some(TaggingJobStatus::Failed) => tagging.failed += count,
                    Some(TaggingJobStatus::Completed | TaggingJobStatus::Cancelled) => {}
                    None => tagging.pending += count,
                }
            }

            let union_params: Vec<i64> = chunk
                .iter()
                .copied()
                .chain(chunk.iter().copied())
                .chain(chunk.iter().copied())
                .collect();
            affected_photos += self.conn.query_row(
                &format!(
                    "SELECT COUNT(*) FROM ( \
                         SELECT photo_id FROM compression_jobs \
                         WHERE photo_id IN ({placeholders}) \
                           AND status NOT IN ('completed', 'cancelled') \
                         UNION \
                         SELECT photo_id FROM send_outbox \
                         WHERE photo_id IN ({placeholders}) \
                           AND status NOT IN ('sent', 'cancelled') \
                         UNION \
                         SELECT photo_id FROM tagging_jobs \
                         WHERE photo_id IN ({placeholders}) \
                           AND status NOT IN ('completed', 'cancelled') \
                     )"
                ),
                params_from_iter(union_params),
                |row| row.get::<_, i64>(0),
            )?;
        }
        Ok(DeletionJobSummary {
            compression,
            send,
            tagging,
            affected_photos,
        })
    }

    /// Counts work that still blocks a deletion. Active sends and tagging jobs
    /// block like active compression jobs. Failed sends do not, because they
    /// carry no data-loss risk and are cancelled when the request is recorded;
    /// failed compression and tagging jobs keep the request blocked.
    pub fn deletion_blockers(&self, photo_id: i64) -> Result<DeletionBlockers> {
        let blockers = self.conn.query_row(
            "SELECT \
                (SELECT COUNT(*) FROM compression_jobs \
                 WHERE photo_id = ?1 AND status IN ('waiting', 'pending', 'processing')) \
              + (SELECT COUNT(*) FROM send_outbox \
                 WHERE photo_id = ?1 AND status IN ('waiting', 'pending', 'processing')) \
              + (SELECT COUNT(*) FROM tagging_jobs \
                 WHERE photo_id = ?1 AND status IN ('waiting', 'pending', 'processing')), \
                (SELECT COUNT(*) FROM compression_jobs \
                 WHERE photo_id = ?1 AND status = 'failed') \
              + (SELECT COUNT(*) FROM tagging_jobs \
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
    /// `requested_at` order. Known reasons with an active compression job, an
    /// active send, or an active tagging job are skipped so they cannot starve
    /// ready requests behind the row limit, while unknown reasons are always
    /// returned so they can be blocked immediately. Requests with failed jobs
    /// stay in the list: the executor still needs to transition them to
    /// `blocked`.
    pub fn pending_deletion_requests(&self, limit: i64) -> Result<Vec<DeletionRequest>> {
        let sql = "SELECT dr.id, dr.photo_id, p.file_name, dr.reason, dr.status, dr.force, \
                dr.blocked_reason, dr.error, dr.requested_at, dr.started_at, dr.finished_at \
             FROM deletion_requests dr JOIN photos p ON p.id = dr.photo_id \
             WHERE dr.status = 'pending' \
               AND ( \
                   dr.reason NOT IN ('auto_replace', 'manual') \
                   OR ( \
                       dr.photo_id NOT IN ( \
                           SELECT j.photo_id FROM compression_jobs j \
                           WHERE j.status IN ('waiting', 'pending', 'processing') \
                       ) \
                       AND dr.photo_id NOT IN ( \
                           SELECT s.photo_id FROM send_outbox s \
                           WHERE s.photo_id IS NOT NULL \
                             AND s.status IN ('waiting', 'pending', 'processing') \
                       ) \
                       AND dr.photo_id NOT IN ( \
                           SELECT t.photo_id FROM tagging_jobs t \
                           WHERE t.status IN ('waiting', 'pending', 'processing') \
                       ) \
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
    /// between the snapshot and this call. Active sends and tagging jobs block
    /// the claim even when the request is forced; failed sends never do.
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
               AND NOT EXISTS ( \
                   SELECT 1 FROM send_outbox s \
                   WHERE s.photo_id = deletion_requests.photo_id \
                     AND s.status IN ('waiting', 'pending', 'processing') \
               ) \
               AND NOT EXISTS ( \
                   SELECT 1 FROM tagging_jobs t \
                   WHERE t.photo_id = deletion_requests.photo_id \
                     AND t.status IN ('waiting', 'pending', 'processing') \
               ) \
               AND ( \
                   force = 1 OR ( \
                       NOT EXISTS ( \
                           SELECT 1 FROM compression_jobs j \
                           WHERE j.photo_id = deletion_requests.photo_id \
                             AND j.status = 'failed' \
                       ) \
                       AND NOT EXISTS ( \
                           SELECT 1 FROM tagging_jobs t \
                           WHERE t.photo_id = deletion_requests.photo_id \
                             AND t.status = 'failed' \
                       ) \
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
               AND NOT EXISTS ( \
                   SELECT 1 FROM tagging_jobs t \
                   WHERE t.photo_id = deletion_requests.photo_id \
                     AND t.status IN ('waiting', 'pending', 'processing') \
               ) \
               AND ( \
                   EXISTS ( \
                       SELECT 1 FROM compression_jobs j \
                       WHERE j.photo_id = deletion_requests.photo_id \
                         AND j.status = 'failed' \
                   ) \
                   OR EXISTS ( \
                       SELECT 1 FROM tagging_jobs t \
                       WHERE t.photo_id = deletion_requests.photo_id \
                         AND t.status = 'failed' \
                   ) \
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
    /// or send still has to finish before `claim_deletion` can advance, so
    /// queued and failed sends are cancelled without touching `processing`.
    /// Tagging jobs are cancelled even while processing: a late sidecar result
    /// is rejected by `save_photo_ai_for_job`.
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
        tx.execute(
            "UPDATE send_outbox SET status = 'cancelled', finished_at = ?1 \
             WHERE photo_id = ?2 AND status IN ('waiting', 'pending', 'failed')",
            params![now, photo_id],
        )?;
        tx.execute(
            "UPDATE tagging_jobs SET status = 'cancelled', progress = 0, stage = NULL, \
                finished_at = ?1 \
             WHERE photo_id = ?2 AND status IN ('waiting', 'pending', 'processing')",
            params![now, photo_id],
        )?;
        tx.commit()?;
        Ok(true)
    }

    /// Cancels a still-pending manual request. Sends cancelled when the request
    /// was recorded stay cancelled: they are not restored.
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
               ) \
               AND NOT EXISTS ( \
                   SELECT 1 FROM tagging_jobs t \
                   WHERE t.photo_id = ?1 \
                     AND t.status IN ('waiting', 'pending', 'processing', 'failed') \
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

    /// Enqueues an automatic job unless the photo already has any job for the
    /// profile. Auto compression runs once per photo/profile; re-indexing must
    /// not recreate completed, failed, or cancelled work.
    pub fn enqueue_auto_job_for_existing_profile_with_status(
        &self,
        photo_id: i64,
        profile_id: &str,
        policy: &str,
        status: QueueStatus,
    ) -> Result<bool> {
        let id = uuid::Uuid::new_v4().to_string();
        let inserted = self.conn.execute(
            "INSERT INTO compression_jobs(id, photo_id, profile_id, status, progress, policy, output_dir, created_at) \
             SELECT ?1, ?2, ?3, ?4, 0, ?5, NULL, ?6 \
             WHERE EXISTS (SELECT 1 FROM compression_profiles WHERE id = ?3) \
               AND EXISTS (SELECT 1 FROM photos WHERE id = ?2 AND status <> 'deleting') \
               AND NOT EXISTS (SELECT 1 FROM deletion_requests \
                   WHERE photo_id = ?2 AND status IN ('pending', 'blocked', 'running')) \
               AND NOT EXISTS (SELECT 1 FROM compression_jobs \
                   WHERE photo_id = ?2 AND profile_id = ?3)",
            params![
                id,
                photo_id,
                profile_id,
                status.as_str(),
                policy,
                now_string()
            ],
        )?;
        Ok(inserted == 1)
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

    /// Columns read for every `compression_jobs` row, in `row_to_job` order.
    const JOB_COLUMNS: &str = "id, photo_id, profile_id, status, progress, error, \
        output_path, policy, output_dir, retry_count, created_at, started_at, finished_at";

    /// Lists the newest jobs first. `limit` is clamped to at least 1, so a
    /// non-positive limit returns the single newest job.
    pub fn list_jobs(&self, limit: i64) -> Result<Vec<CompressionJob>> {
        let sql = format!(
            "SELECT {} FROM compression_jobs ORDER BY created_at DESC LIMIT ?1",
            Self::JOB_COLUMNS
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params![limit.max(1)], row_to_job)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Lists the newest jobs with `status` first. `limit` is clamped to at
    /// least 1, so a non-positive limit returns the single newest match.
    pub fn list_jobs_by_status(
        &self,
        status: QueueStatus,
        limit: i64,
    ) -> Result<Vec<CompressionJob>> {
        let sql = format!(
            "SELECT {} FROM compression_jobs WHERE status = ?1 \
             ORDER BY created_at DESC LIMIT ?2",
            Self::JOB_COLUMNS
        );
        let mut stmt = self.conn.prepare(&sql)?;
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

    /// Assignments that return a failed job to the queue.
    const RETRY_JOB_SET: &str = "status = 'pending', progress = 0, error = NULL, \
        started_at = NULL, finished_at = NULL, retry_count = retry_count + 1";

    /// Guard that keeps a job queued while its photo is being deleted.
    const NO_RUNNING_DELETION_GUARD: &str = "NOT EXISTS (SELECT 1 FROM deletion_requests dr \
        WHERE dr.photo_id = compression_jobs.photo_id AND dr.status = 'running')";

    /// Returns a failed job to the queue once, incrementing its retry count.
    /// Returns false when the job is missing, not failed, or already at the
    /// retry cap.
    pub fn retry_job(&self, id: &str) -> Result<bool> {
        let sql = format!(
            "UPDATE compression_jobs SET {} WHERE id = ?1 AND status = 'failed' \
             AND retry_count < ?2 AND {}",
            Self::RETRY_JOB_SET,
            Self::NO_RUNNING_DELETION_GUARD
        );
        let changed = self.conn.execute(&sql, params![id, MAX_JOB_RETRIES])?;
        Ok(changed == 1)
    }

    /// Returns every eligible failed job to the queue and reports how many
    /// rows changed. Jobs at the retry cap are left untouched.
    pub fn retry_failed_jobs(&self) -> Result<usize> {
        let sql = format!(
            "UPDATE compression_jobs SET {} WHERE status = 'failed' \
             AND retry_count < ?1 AND {}",
            Self::RETRY_JOB_SET,
            Self::NO_RUNNING_DELETION_GUARD
        );
        let changed = self.conn.execute(&sql, params![MAX_JOB_RETRIES])?;
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

    /// Counts failed jobs that can still return to the queue. Callers use
    /// this aggregate — not a page of failed jobs — to decide whether a
    /// retry-all action exists, so the answer stays correct past the
    /// page size.
    pub fn count_retryable_failed_jobs(&self) -> Result<i64> {
        let count = self.conn.query_row(
            "SELECT COUNT(*) FROM compression_jobs \
             WHERE status = 'failed' AND retry_count < ?1",
            params![MAX_JOB_RETRIES],
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

    // ---------- send outbox ----------

    /// Columns read for every `send_outbox` row, in `row_to_send_outbox` order.
    const SEND_COLUMNS: &str = "id, photo_id, kind, origin, status, source, profile_id, \
        destination, attempts, next_attempt_at, discord_message_id, discord_channel_id, \
        discord_guild_id, error_code, error_detail, created_at, started_at, finished_at";

    /// Same columns qualified for the `send_history` join, which also reads
    /// `photo_name` and `world_name` at indexes 18 and 19.
    const SEND_COLUMNS_QUALIFIED: &str = "o.id, o.photo_id, o.kind, o.origin, o.status, \
        o.source, o.profile_id, o.destination, o.attempts, o.next_attempt_at, \
        o.discord_message_id, o.discord_channel_id, o.discord_guild_id, o.error_code, \
        o.error_detail, o.created_at, o.started_at, o.finished_at";

    /// Queues a photo send unless the photo is being deleted or another active
    /// send already exists. The partial `idx_send_outbox_active_photo` index is
    /// what dedupes active rows; `INSERT OR IGNORE` turns that conflict into a
    /// quiet `None`. Only `waiting` and `pending` rows may be enqueued.
    pub fn enqueue_send(
        &self,
        photo_id: i64,
        kind: SendKind,
        origin: SendOrigin,
        status: SendStatus,
    ) -> Result<Option<i64>> {
        if !matches!(status, SendStatus::Waiting | SendStatus::Pending) {
            return Err(CoreError::Invalid(format!(
                "send status '{}' cannot be enqueued",
                status.as_str()
            )));
        }
        self.conn.execute(
            "INSERT OR IGNORE INTO send_outbox(photo_id, kind, origin, status, created_at) \
             SELECT ?1, ?2, ?3, ?4, ?5 \
             WHERE EXISTS (SELECT 1 FROM photos WHERE id = ?1 AND status <> 'deleting')",
            params![
                photo_id,
                kind.as_str(),
                origin.as_str(),
                status.as_str(),
                now_string()
            ],
        )?;
        if self.conn.changes() == 0 {
            return Ok(None);
        }
        Ok(Some(self.conn.last_insert_rowid()))
    }

    /// Queues a webhook test send. Test rows have no photo, so they are never
    /// covered by the per-photo dedupe index.
    pub fn enqueue_test_send(&self) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO send_outbox(photo_id, kind, origin, status, created_at) \
             VALUES (NULL, 'test', 'manual', 'pending', ?1)",
            params![now_string()],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Atomically claims the oldest due pending send. `now` is epoch seconds;
    /// `next_attempt_at` is compared against it and left untouched.
    pub fn claim_next_send(&self, now: i64) -> Result<Option<SendOutboxEntry>> {
        let sql = format!(
            "UPDATE send_outbox SET status = 'processing', started_at = ?1 \
             WHERE id = ( \
                 SELECT id FROM send_outbox \
                 WHERE status = 'pending' \
                   AND (next_attempt_at IS NULL OR next_attempt_at <= ?2) \
                 ORDER BY id LIMIT 1 \
             ) \
             RETURNING {}",
            Self::SEND_COLUMNS
        );
        let entry = self
            .conn
            .query_row(&sql, params![now_string(), now], row_to_send_outbox)
            .optional()?;
        Ok(entry)
    }

    /// Delays the next claim of `id` until the given epoch second.
    pub fn set_send_next_attempt(&self, id: i64, next_attempt_at: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE send_outbox SET next_attempt_at = ?1 WHERE id = ?2",
            params![next_attempt_at, id],
        )?;
        Ok(())
    }

    pub fn send_entry_by_id(&self, id: i64) -> Result<Option<SendOutboxEntry>> {
        let sql = format!(
            "SELECT {} FROM send_outbox WHERE id = ?1",
            Self::SEND_COLUMNS
        );
        let entry = self
            .conn
            .query_row(&sql, params![id], row_to_send_outbox)
            .optional()?;
        Ok(entry)
    }

    /// Returns the active (`waiting` / `pending` / `processing`) row for a
    /// photo, if any. The partial UNIQUE index guarantees at most one.
    pub fn active_send_for_photo(&self, photo_id: i64) -> Result<Option<SendOutboxEntry>> {
        let sql = format!(
            "SELECT {} FROM send_outbox WHERE photo_id = ?1 \
             AND status IN ('waiting', 'pending', 'processing') ORDER BY id LIMIT 1",
            Self::SEND_COLUMNS
        );
        let entry = self
            .conn
            .query_row(&sql, params![photo_id], row_to_send_outbox)
            .optional()?;
        Ok(entry)
    }

    /// Counts work a user still expects to start: waiting and pending sends.
    pub fn count_pending_sends(&self) -> Result<i64> {
        let count = self.conn.query_row(
            "SELECT COUNT(*) FROM send_outbox WHERE status IN ('waiting', 'pending')",
            [],
            |row| row.get(0),
        )?;
        Ok(count)
    }

    /// Counts only sends held in automatic `waiting` mode.
    pub fn count_waiting_sends(&self) -> Result<i64> {
        let count = self.conn.query_row(
            "SELECT COUNT(*) FROM send_outbox WHERE status = 'waiting'",
            [],
            |row| row.get(0),
        )?;
        Ok(count)
    }

    /// Marks a claimed send as sent and stores the Discord identity produced by
    /// the upload. Rows that are not `processing` are left untouched.
    pub fn complete_send(&self, id: i64, completion: &SendCompletion) -> Result<()> {
        let source = completion.source.map(SendSource::as_str);
        self.conn.execute(
            "UPDATE send_outbox SET status = 'sent', source = ?1, profile_id = ?2, \
                destination = ?3, discord_message_id = ?4, discord_channel_id = ?5, \
                discord_guild_id = ?6, error_code = NULL, error_detail = NULL, \
                finished_at = ?7 \
             WHERE id = ?8 AND status = 'processing'",
            params![
                source,
                completion.profile_id,
                completion.destination,
                completion.message_id,
                completion.channel_id,
                completion.guild_id,
                now_string(),
                id,
            ],
        )?;
        Ok(())
    }

    /// Delays a send and records the last error without increasing `attempts`
    /// beyond what the caller computed.
    pub fn reschedule_send(
        &self,
        id: i64,
        attempts: i64,
        next_attempt_at: i64,
        error_code: SendErrorCode,
        error_detail: Option<&str>,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE send_outbox SET status = 'pending', attempts = ?1, \
                next_attempt_at = ?2, error_code = ?3, error_detail = ?4, \
                started_at = NULL \
             WHERE id = ?5",
            params![
                attempts,
                next_attempt_at,
                error_code.as_str(),
                error_detail,
                id,
            ],
        )?;
        Ok(())
    }

    /// Returns a claimed send to the queue without uploading. Used when the
    /// Discord integration was disabled between claim and upload.
    pub fn release_send(&self, id: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE send_outbox SET status = 'pending', started_at = NULL \
             WHERE id = ?1 AND status = 'processing'",
            params![id],
        )?;
        Ok(())
    }

    /// Marks a send as failed and stores the error plus whatever Discord
    /// identity was known. Pending rows are accepted: a send can be abandoned
    /// before it is ever claimed.
    pub fn fail_send(
        &self,
        id: i64,
        error_code: SendErrorCode,
        error_detail: Option<&str>,
        source: Option<SendSource>,
        profile_id: Option<&str>,
        destination: Option<&str>,
    ) -> Result<()> {
        let source = source.map(SendSource::as_str);
        self.conn.execute(
            "UPDATE send_outbox SET status = 'failed', error_code = ?1, \
                error_detail = ?2, source = ?3, profile_id = ?4, destination = ?5, \
                finished_at = ?6 \
             WHERE id = ?7",
            params![
                error_code.as_str(),
                error_detail,
                source,
                profile_id,
                destination,
                now_string(),
                id,
            ],
        )?;
        Ok(())
    }

    /// Returns a failed send to the queue with a clean slate. Refuses when the
    /// same photo already has another active row, since that would violate the
    /// partial `idx_send_outbox_active_photo` index.
    pub fn retry_send(&self, id: i64) -> Result<bool> {
        let changed = self.conn.execute(
            "UPDATE send_outbox SET status = 'pending', attempts = 0, \
                next_attempt_at = NULL, error_code = NULL, error_detail = NULL, \
                started_at = NULL, finished_at = NULL \
             WHERE id = ?1 AND status = 'failed' \
               AND (photo_id IS NULL OR NOT EXISTS ( \
                   SELECT 1 FROM send_outbox active \
                   WHERE active.photo_id = send_outbox.photo_id \
                     AND active.id <> send_outbox.id \
                     AND active.status IN ('waiting', 'pending', 'processing') \
               ))",
            params![id],
        )?;
        Ok(changed == 1)
    }

    /// Cancels a queued or failed send. Claimed (`processing`) rows are left
    /// alone so an upload in flight can finish.
    pub fn cancel_send(&self, id: i64) -> Result<bool> {
        let changed = self.conn.execute(
            "UPDATE send_outbox SET status = 'cancelled', finished_at = ?1 \
             WHERE id = ?2 AND status IN ('waiting', 'pending', 'failed')",
            params![now_string(), id],
        )?;
        Ok(changed == 1)
    }

    /// Cancels every automatic send still held in `waiting` mode.
    pub fn cancel_waiting_auto_sends(&self) -> Result<usize> {
        let changed = self.conn.execute(
            "UPDATE send_outbox SET status = 'cancelled', finished_at = ?1 \
             WHERE status = 'waiting' AND origin = 'auto'",
            params![now_string()],
        )?;
        Ok(changed)
    }

    /// Promotes every waiting send to pending so workers can pick it up.
    pub fn release_waiting_sends(&self) -> Result<usize> {
        let changed = self.conn.execute(
            "UPDATE send_outbox SET status = 'pending' WHERE status = 'waiting'",
            [],
        )?;
        Ok(changed)
    }

    /// Fails sends that were still `processing` when the process stopped.
    /// They are not retried automatically, so a possibly delivered message is
    /// not sent twice.
    pub fn reset_processing_sends(&self) -> Result<usize> {
        let changed = self.conn.execute(
            "UPDATE send_outbox SET status = 'failed', error_code = 'interrupted', \
                error_detail = NULL, finished_at = ?1 \
             WHERE status = 'processing'",
            params![now_string()],
        )?;
        Ok(changed)
    }

    /// Newest-first send history joined with the photo file name and world name.
    pub fn send_history(&self, limit: i64) -> Result<Vec<SendHistoryEntry>> {
        let sql = format!(
            "SELECT {}, p.file_name, c.world_name \
             FROM send_outbox o \
             LEFT JOIN photos p ON p.id = o.photo_id \
             LEFT JOIN photo_context c ON c.photo_id = o.photo_id \
             ORDER BY o.id DESC LIMIT ?1",
            Self::SEND_COLUMNS_QUALIFIED
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params![limit.max(0)], |row| {
            Ok(SendHistoryEntry {
                entry: row_to_send_outbox(row)?,
                photo_name: row.get(18)?,
                world_name: row.get(19)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Deletes every terminal send row and returns how many were removed.
    pub fn clear_send_history(&self) -> Result<usize> {
        let changed = self.conn.execute(
            "DELETE FROM send_outbox WHERE status IN ('sent', 'failed', 'cancelled')",
            [],
        )?;
        Ok(changed)
    }

    /// Deletes terminal send rows older than the newest `keep` ones. Active
    /// rows are never pruned.
    pub fn prune_send_history(&self, keep: i64) -> Result<usize> {
        let changed = self.conn.execute(
            "DELETE FROM send_outbox WHERE status IN ('sent', 'failed', 'cancelled') \
               AND id NOT IN ( \
                   SELECT id FROM send_outbox \
                   WHERE status IN ('sent', 'failed', 'cancelled') \
                   ORDER BY id DESC LIMIT ?1 \
               )",
            params![keep.max(0)],
        )?;
        Ok(changed)
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
        self.record_daemon_error_i18n(operation, message, None, None, detail)
    }

    /// Records a daemon failure with an optional catalog key and JSON args.
    /// `message` stays the fallback for older UIs and direct database readers.
    pub fn record_daemon_error_i18n(
        &self,
        operation: DaemonOperation,
        message: &str,
        message_key: Option<&str>,
        message_args: Option<&str>,
        detail: Option<&str>,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO daemon_errors(operation, message, detail, occurred_at, attempts, \
                message_key, message_args) \
             VALUES (?1, ?2, ?3, ?4, 1, ?5, ?6) \
             ON CONFLICT(operation) DO UPDATE SET message = excluded.message, \
                detail = excluded.detail, occurred_at = excluded.occurred_at, \
                attempts = daemon_errors.attempts + 1, \
                message_key = excluded.message_key, \
                message_args = excluded.message_args",
            params![
                operation.as_str(),
                message,
                detail,
                now_string(),
                message_key,
                message_args
            ],
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
            "SELECT operation, message, detail, occurred_at, attempts, message_key, message_args \
             FROM daemon_errors ORDER BY occurred_at DESC, operation",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, Option<String>>(6)?,
            ))
        })?;
        let mut errors = Vec::new();
        for row in rows {
            let (operation, message, detail, occurred_at, attempts, message_key, message_args) =
                row?;
            let Some(operation) = DaemonOperation::parse(&operation) else {
                continue;
            };
            errors.push(DaemonError {
                operation,
                message,
                detail,
                occurred_at,
                attempts,
                message_key,
                message_args,
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

    // ---------- AI plugins ----------

    /// Inserts or updates one plugin installation. When `install.active` is
    /// set, every other installation of the same plugin is deactivated in the
    /// same immediate transaction so the partial unique index always holds.
    pub fn upsert_plugin_installation(&self, install: &PluginInstallation) -> Result<()> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        if install.active {
            tx.execute(
                "UPDATE plugin_installations SET active = 0 \
                 WHERE plugin_id = ?1 AND active = 1",
                params![install.plugin_id],
            )?;
        }
        tx.execute(
            "INSERT INTO plugin_installations(plugin_id, variant, version, protocol_version, \
                path, manifest_json, catalog_version, active, installed_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10) \
             ON CONFLICT(plugin_id, variant, version) DO UPDATE SET \
                protocol_version = excluded.protocol_version, \
                path = excluded.path, \
                manifest_json = excluded.manifest_json, \
                catalog_version = excluded.catalog_version, \
                active = excluded.active, \
                updated_at = excluded.updated_at",
            params![
                install.plugin_id,
                install.variant,
                install.version,
                install.protocol_version,
                install.path,
                install.manifest_json,
                install.catalog_version,
                i64::from(install.active),
                install.installed_at,
                install.updated_at,
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn list_plugin_installations(&self, plugin_id: &str) -> Result<Vec<PluginInstallation>> {
        let mut stmt = self.conn.prepare(
            "SELECT plugin_id, variant, version, protocol_version, path, manifest_json, \
                catalog_version, active, installed_at, updated_at \
             FROM plugin_installations WHERE plugin_id = ?1 ORDER BY variant, version",
        )?;
        let rows = stmt.query_map(params![plugin_id], row_to_plugin_installation)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn active_plugin_installation(
        &self,
        plugin_id: &str,
    ) -> Result<Option<PluginInstallation>> {
        let install = self
            .conn
            .query_row(
                "SELECT plugin_id, variant, version, protocol_version, path, manifest_json, \
                    catalog_version, active, installed_at, updated_at \
                 FROM plugin_installations WHERE plugin_id = ?1 AND active = 1",
                params![plugin_id],
                row_to_plugin_installation,
            )
            .optional()?;
        Ok(install)
    }

    /// Marks one installation as active and clears the previous one
    /// atomically. `None` selects the highest semver installed for `variant`.
    /// Returns whether a matching installation was found.
    pub fn set_active_plugin_installation(
        &self,
        plugin_id: &str,
        variant: &str,
        version: Option<&str>,
    ) -> Result<bool> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let target = match version {
            Some(version) => tx
                .query_row(
                    "SELECT version FROM plugin_installations \
                     WHERE plugin_id = ?1 AND variant = ?2 AND version = ?3",
                    params![plugin_id, variant, version],
                    |row| row.get::<_, String>(0),
                )
                .optional()?,
            None => {
                let mut stmt = tx.prepare(
                    "SELECT version FROM plugin_installations \
                     WHERE plugin_id = ?1 AND variant = ?2",
                )?;
                let versions =
                    stmt.query_map(params![plugin_id, variant], |row| row.get::<_, String>(0))?;
                let mut best: Option<(semver::Version, String)> = None;
                for version in versions {
                    let version = version?;
                    let Ok(parsed) = semver::Version::parse(&version) else {
                        continue;
                    };
                    if best.as_ref().is_none_or(|(current, _)| parsed > *current) {
                        best = Some((parsed, version));
                    }
                }
                best.map(|(_, version)| version)
            }
        };
        let Some(target) = target else {
            tx.commit()?;
            return Ok(false);
        };

        tx.execute(
            "UPDATE plugin_installations SET active = 0 \
             WHERE plugin_id = ?1 AND active = 1",
            params![plugin_id],
        )?;
        tx.execute(
            "UPDATE plugin_installations SET active = 1 \
             WHERE plugin_id = ?1 AND variant = ?2 AND version = ?3",
            params![plugin_id, variant, target],
        )?;
        tx.commit()?;
        Ok(true)
    }

    pub fn remove_plugin_installation(
        &self,
        plugin_id: &str,
        variant: &str,
        version: &str,
    ) -> Result<()> {
        self.conn.execute(
            "DELETE FROM plugin_installations \
             WHERE plugin_id = ?1 AND variant = ?2 AND version = ?3",
            params![plugin_id, variant, version],
        )?;
        Ok(())
    }

    pub fn plugin_state(&self, plugin_id: &str) -> Result<Option<PluginState>> {
        let state = self
            .conn
            .query_row(
                "SELECT plugin_id, model_state, model_revision, model_bytes_downloaded, \
                    model_bytes_total, model_error, probe_json, updated_at \
                 FROM plugin_state WHERE plugin_id = ?1",
                params![plugin_id],
                row_to_plugin_state,
            )
            .optional()?;
        Ok(state)
    }

    pub fn set_plugin_model_state(
        &self,
        plugin_id: &str,
        state: ModelState,
        revision: Option<&str>,
        downloaded: i64,
        total: Option<i64>,
        error: Option<&str>,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO plugin_state(plugin_id, model_state, model_revision, \
                model_bytes_downloaded, model_bytes_total, model_error, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) \
             ON CONFLICT(plugin_id) DO UPDATE SET \
                model_state = excluded.model_state, \
                model_revision = excluded.model_revision, \
                model_bytes_downloaded = excluded.model_bytes_downloaded, \
                model_bytes_total = excluded.model_bytes_total, \
                model_error = excluded.model_error, \
                updated_at = excluded.updated_at",
            params![
                plugin_id,
                state.as_str(),
                revision,
                downloaded.max(0),
                total,
                error,
                now_string(),
            ],
        )?;
        Ok(())
    }

    /// Stores the raw `--probe` JSON, creating a `missing` model state row if
    /// the plugin has no state yet. The model columns are left untouched on
    /// update.
    pub fn set_plugin_probe_json(&self, plugin_id: &str, probe_json: Option<&str>) -> Result<()> {
        self.conn.execute(
            "INSERT INTO plugin_state(plugin_id, model_state, model_bytes_downloaded, \
                probe_json, updated_at) \
             VALUES (?1, ?2, 0, ?3, ?4) \
             ON CONFLICT(plugin_id) DO UPDATE SET \
                probe_json = excluded.probe_json, \
                updated_at = excluded.updated_at",
            params![
                plugin_id,
                ModelState::Missing.as_str(),
                probe_json,
                now_string()
            ],
        )?;
        Ok(())
    }

    /// Mirrors the plugin's catalog into the local definition table. User
    /// threshold overrides are never touched. If a tag's default threshold
    /// changed, applied flags are recomputed after the upserts.
    pub fn sync_ai_catalog(&self, plugin_id: &str, catalog: &Catalog) -> Result<()> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let mut defaults_changed = false;
        for (branch, definitions) in [
            (TagBranch::Outdoor, &catalog.outdoor),
            (TagBranch::Indoor, &catalog.indoor),
        ] {
            for (sort_order, definition) in definitions.iter().enumerate() {
                let previous: Option<f32> = tx
                    .query_row(
                        "SELECT default_threshold FROM ai_tag_definitions \
                         WHERE plugin_id = ?1 AND tag_key = ?2",
                        params![plugin_id, definition.key],
                        |row| row.get(0),
                    )
                    .optional()?;
                if previous.is_some_and(|previous| previous != definition.default_threshold) {
                    defaults_changed = true;
                }
                tx.execute(
                    "INSERT INTO ai_tag_definitions(plugin_id, tag_key, branch, name, name_en, \
                        description, default_threshold, sort_order, catalog_version, updated_at) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10) \
                     ON CONFLICT(plugin_id, tag_key) DO UPDATE SET \
                        branch = excluded.branch, \
                        name = excluded.name, \
                        name_en = excluded.name_en, \
                        description = excluded.description, \
                        default_threshold = excluded.default_threshold, \
                        sort_order = excluded.sort_order, \
                        catalog_version = excluded.catalog_version, \
                        updated_at = excluded.updated_at",
                    params![
                        plugin_id,
                        definition.key,
                        branch.as_str(),
                        definition.name,
                        definition.name_en,
                        definition.description,
                        definition.default_threshold,
                        sort_order as i64,
                        catalog.version,
                        now_string(),
                    ],
                )?;
            }
        }
        let updated = if defaults_changed {
            reapply_thresholds(&tx, plugin_id)?
        } else {
            0
        };
        tx.commit()?;
        if defaults_changed || updated > 0 {
            self.bump_library_revision()?;
        }
        Ok(())
    }

    pub fn ai_tag_definitions(&self, plugin_id: &str) -> Result<Vec<AiTagDefinition>> {
        let mut stmt = self.conn.prepare(
            "SELECT plugin_id, tag_key, branch, name, name_en, description, \
                default_threshold, sort_order, catalog_version, updated_at \
             FROM ai_tag_definitions WHERE plugin_id = ?1 \
             ORDER BY branch, sort_order, tag_key",
        )?;
        let rows = stmt.query_map(params![plugin_id], row_to_ai_tag_definition)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Applies a batch of tag-threshold overrides in one transaction.
    ///
    /// `Some(threshold)` upserts an override; `None` removes it so the
    /// catalog default applies again. After writing, `photo_ai_tags.applied`
    /// is recomputed once. Returns the number of re-evaluated tag rows.
    pub fn apply_ai_tag_thresholds(
        &self,
        plugin_id: &str,
        updates: &[(String, Option<f32>)],
    ) -> Result<usize> {
        for (_, threshold) in updates {
            if let Some(threshold) = threshold
                && (!threshold.is_finite() || !(0.0..=1.0).contains(threshold))
            {
                return Err(CoreError::Invalid(format!(
                    "AI tag threshold {threshold} is outside 0.0..=1.0"
                )));
            }
        }

        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        for (tag_key, threshold) in updates {
            match threshold {
                Some(threshold) => {
                    tx.execute(
                        "INSERT INTO ai_tag_thresholds(plugin_id, tag_key, threshold, updated_at) \
                         VALUES (?1, ?2, ?3, ?4) \
                         ON CONFLICT(plugin_id, tag_key) DO UPDATE SET \
                            threshold = excluded.threshold, \
                            updated_at = excluded.updated_at",
                        params![plugin_id, tag_key, *threshold, now_string()],
                    )?;
                }
                None => {
                    tx.execute(
                        "DELETE FROM ai_tag_thresholds WHERE plugin_id = ?1 AND tag_key = ?2",
                        params![plugin_id, tag_key],
                    )?;
                }
            }
        }
        let updated = reapply_thresholds(&tx, plugin_id)?;
        tx.commit()?;
        if updated > 0 {
            self.bump_library_revision()?;
        }
        Ok(updated)
    }

    pub fn set_ai_tag_threshold(
        &self,
        plugin_id: &str,
        tag_key: &str,
        threshold: f32,
    ) -> Result<()> {
        self.apply_ai_tag_thresholds(plugin_id, &[(tag_key.to_string(), Some(threshold))])
            .map(|_| ())
    }

    pub fn reset_ai_tag_threshold(&self, plugin_id: &str, tag_key: &str) -> Result<()> {
        self.apply_ai_tag_thresholds(plugin_id, &[(tag_key.to_string(), None)])
            .map(|_| ())
    }

    pub fn ai_tag_thresholds(&self, plugin_id: &str) -> Result<Vec<AiTagThreshold>> {
        let mut stmt = self.conn.prepare(
            "SELECT plugin_id, tag_key, threshold, updated_at \
             FROM ai_tag_thresholds WHERE plugin_id = ?1 ORDER BY tag_key",
        )?;
        let rows = stmt.query_map(params![plugin_id], row_to_ai_tag_threshold)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Recomputes `photo_ai_tags.applied` from the effective threshold
    /// (user override, else the catalog default) in one UPDATE. Returns the
    /// number of tag rows that were re-evaluated. The library revision
    /// advances after the commit when at least one row was touched.
    pub fn reapply_ai_tag_thresholds(&self, plugin_id: &str) -> Result<usize> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let updated = reapply_thresholds(&tx, plugin_id)?;
        tx.commit()?;
        if updated > 0 {
            self.bump_library_revision()?;
        }
        Ok(updated)
    }

    // ---------- AI results ----------

    /// Stores one tagging result for `photo_id`. The photo must still be in
    /// the library (`present` or `replaced`); missing, deleting, or unknown
    /// photos are skipped without writing anything. The judged tags replace
    /// the previous result for this plugin, and `applied` is derived from the
    /// effective threshold (user override, else catalog default). Manual tags
    /// are never touched. The library revision advances after the commit.
    pub fn save_photo_ai(
        &self,
        photo_id: i64,
        plugin_id: &str,
        result: &NewPhotoAi,
    ) -> Result<bool> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let saved = write_photo_ai(&tx, photo_id, plugin_id, result)?;
        tx.commit()?;
        if saved {
            self.bump_library_revision()?;
        }
        Ok(saved)
    }

    /// Stores a result only while its job is still `processing`. A cancelled,
    /// completed, failed, or missing job makes this a no-op so late sidecar
    /// results are never written after a cancellation.
    pub fn save_photo_ai_for_job(&self, job_id: &str, result: &NewPhotoAi) -> Result<bool> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let job: Option<(i64, String, String)> = tx
            .query_row(
                "SELECT photo_id, plugin_id, status FROM tagging_jobs WHERE id = ?1",
                params![job_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        let Some((photo_id, plugin_id, status)) = job else {
            tx.commit()?;
            return Ok(false);
        };
        if status != TaggingJobStatus::Processing.as_str() {
            tx.commit()?;
            return Ok(false);
        }
        let saved = write_photo_ai(&tx, photo_id, &plugin_id, result)?;
        tx.commit()?;
        if saved {
            self.bump_library_revision()?;
        }
        Ok(saved)
    }

    pub fn photo_ai(&self, photo_id: i64) -> Result<Option<PhotoAi>> {
        let ai = self
            .conn
            .query_row(
                "SELECT photo_id, plugin_id, variant, sidecar_version, model_version, \
                    model_revision, catalog_version, source, presence_threshold, \
                    character_present, presence_probability, people_min, people_max, \
                    people_median, place_branch, place_probabilities_json, tagged_at \
                 FROM photo_ai WHERE photo_id = ?1",
                params![photo_id],
                row_to_photo_ai,
            )
            .optional()?;
        Ok(ai)
    }

    pub fn photo_ai_tags(&self, photo_id: i64, applied_only: bool) -> Result<Vec<PhotoAiTag>> {
        let applied_clause = if applied_only { " AND applied = 1" } else { "" };
        let sql = format!(
            "SELECT photo_id, plugin_id, tag_key, probability, applied, model_version, \
                catalog_version, tagged_at FROM photo_ai_tags \
             WHERE photo_id = ?1{applied_clause} ORDER BY tag_key"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params![photo_id], row_to_photo_ai_tag)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Returns every judged tag with its catalog display names. Tags that are
    /// no longer in the catalog fall back to the raw key.
    pub fn photo_ai_tags_with_names(&self, photo_id: i64) -> Result<Vec<PhotoAiTagView>> {
        let mut stmt = self.conn.prepare(
            "SELECT t.tag_key, COALESCE(d.name, t.tag_key), d.name_en, t.probability, \
                t.applied \
             FROM photo_ai_tags t \
             LEFT JOIN ai_tag_definitions d \
               ON d.plugin_id = t.plugin_id AND d.tag_key = t.tag_key \
             WHERE t.photo_id = ?1 \
             ORDER BY COALESCE(d.branch, ''), COALESCE(d.sort_order, 9223372036854775807), \
                t.tag_key",
        )?;
        let rows = stmt.query_map(params![photo_id], |row| {
            Ok(PhotoAiTagView {
                tag_key: row.get(0)?,
                name: row.get(1)?,
                name_en: row.get(2)?,
                probability: row.get(3)?,
                applied: row.get::<_, i64>(4)? != 0,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    // ---------- tagging jobs ----------

    /// Enqueues one job per photo for `plugin_id`. Only `waiting` (backlog)
    /// and `pending` (ready) are accepted. The partial active index dedupes
    /// when the photo already has an unfinished job; photos outside the
    /// library or pending deletion are skipped. Returns the inserted count.
    pub fn enqueue_tagging_jobs(
        &self,
        photo_ids: &[i64],
        plugin_id: &str,
        status: TaggingJobStatus,
    ) -> Result<usize> {
        if !matches!(
            status,
            TaggingJobStatus::Waiting | TaggingJobStatus::Pending
        ) {
            return Err(CoreError::Invalid(format!(
                "cannot enqueue a tagging job as {}",
                status.as_str()
            )));
        }
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let now = now_string();
        let mut inserted = 0;
        for photo_id in photo_ids {
            inserted += tx.execute(
                "INSERT OR IGNORE INTO tagging_jobs \
                    (id, photo_id, plugin_id, status, progress, created_at) \
                 SELECT ?1, p.id, ?2, ?3, 0, ?4 FROM photos p \
                 WHERE p.id = ?5 AND p.status IN ('present', 'replaced') \
                   AND NOT EXISTS ( \
                       SELECT 1 FROM deletion_requests dr \
                       WHERE dr.photo_id = p.id AND dr.status IN ('pending', 'blocked', 'running') \
                   )",
                params![
                    uuid::Uuid::new_v4().to_string(),
                    plugin_id,
                    status.as_str(),
                    now,
                    photo_id,
                ],
            )?;
        }
        tx.commit()?;
        Ok(inserted)
    }

    /// Queues every visible photo without an AI result as `waiting`, newest
    /// first. Re-running it is idempotent: the active index keeps existing
    /// waiting/pending/processing rows from being duplicated. The whole
    /// backlog is one ordered `INSERT ... SELECT`, so a large library does not
    /// pay for a per-photo round trip inside the foreground transaction.
    pub fn enqueue_tagging_backlog(&self, plugin_id: &str) -> Result<usize> {
        let inserted = self.conn.execute(
            "INSERT OR IGNORE INTO tagging_jobs \
                (id, photo_id, plugin_id, status, progress, created_at) \
             SELECT lower(hex(randomblob(4))) || '-' || lower(hex(randomblob(2))) || '-' || \
                    lower(hex(randomblob(2))) || '-' || lower(hex(randomblob(2))) || '-' || \
                    lower(hex(randomblob(6))), \
                    p.id, ?1, 'waiting', 0, ?2 \
             FROM photos p \
             WHERE p.status IN ('present', 'replaced') \
               AND NOT EXISTS (SELECT 1 FROM photo_ai a WHERE a.photo_id = p.id) \
               AND NOT EXISTS ( \
                   SELECT 1 FROM deletion_requests dr \
                   WHERE dr.photo_id = p.id AND dr.status IN ('pending', 'blocked', 'running') \
               ) \
             ORDER BY p.captured_at_utc DESC, p.id DESC",
            params![plugin_id, now_string()],
        )?;
        Ok(inserted)
    }

    /// Moves every `waiting` backlog job for the plugin to `pending`.
    pub fn release_waiting_tagging_jobs(&self, plugin_id: &str) -> Result<usize> {
        let changed = self.conn.execute(
            "UPDATE tagging_jobs SET status = 'pending', progress = 0, stage = NULL, \
                error = NULL, started_at = NULL, finished_at = NULL \
             WHERE plugin_id = ?1 AND status = 'waiting'",
            params![plugin_id],
        )?;
        Ok(changed)
    }

    /// Atomically claims the oldest pending job, records its start time, and
    /// stamps the active installation's variant when one exists.
    pub fn next_pending_tagging_job(&self) -> Result<Option<TaggingJob>> {
        let job = self
            .conn
            .query_row(
                "UPDATE tagging_jobs SET status = 'processing', started_at = ?1, \
                    progress = 0, stage = NULL, error = NULL, \
                    variant = COALESCE(( \
                        SELECT pi.variant FROM plugin_installations pi \
                        WHERE pi.plugin_id = tagging_jobs.plugin_id AND pi.active = 1 \
                    ), variant) \
                 WHERE id = ( \
                     SELECT id FROM tagging_jobs WHERE status = 'pending' \
                     ORDER BY created_at, rowid LIMIT 1 \
                 ) \
                 RETURNING id, photo_id, plugin_id, variant, status, progress, stage, error, \
                    retry_count, created_at, started_at, finished_at",
                params![now_string()],
                row_to_tagging_job,
            )
            .optional()?;
        Ok(job)
    }

    pub fn tagging_job(&self, job_id: &str) -> Result<Option<TaggingJob>> {
        let job = self
            .conn
            .query_row(
                &format!(
                    "SELECT {} FROM tagging_jobs WHERE id = ?1",
                    TAGGING_JOB_COLUMNS
                ),
                params![job_id],
                row_to_tagging_job,
            )
            .optional()?;
        Ok(job)
    }

    /// Records progress for a job that is still processing. Cancelled jobs
    /// keep their last visible state instead of moving backwards.
    pub fn update_tagging_progress(
        &self,
        job_id: &str,
        progress: f64,
        stage: TaggingStage,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE tagging_jobs SET progress = ?1, stage = ?2 \
             WHERE id = ?3 AND status = 'processing'",
            params![progress, stage.as_str(), job_id],
        )?;
        Ok(())
    }

    /// Marks a processing job completed. Terminal jobs are never overwritten.
    pub fn complete_tagging_job(&self, job_id: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE tagging_jobs SET status = 'completed', progress = 1, stage = NULL, \
                error = NULL, finished_at = ?1 \
             WHERE id = ?2 AND status = 'processing'",
            params![now_string(), job_id],
        )?;
        Ok(())
    }

    /// Marks a processing job failed with a host error code.
    pub fn fail_tagging_job(&self, job_id: &str, error: TaggingErrorCode) -> Result<()> {
        self.conn.execute(
            "UPDATE tagging_jobs SET status = 'failed', progress = 0, stage = NULL, \
                error = ?1, finished_at = ?2 \
             WHERE id = ?3 AND status = 'processing'",
            params![error.as_str(), now_string(), job_id],
        )?;
        Ok(())
    }

    /// Returns a processing job to `pending` while the plugin is not ready.
    /// The error stays visible for the UI, and `retry_count` is unchanged.
    pub fn defer_tagging_job(&self, job_id: &str, error: TaggingErrorCode) -> Result<()> {
        self.conn.execute(
            "UPDATE tagging_jobs SET status = 'pending', progress = 0, stage = NULL, \
                error = ?1, started_at = NULL, finished_at = NULL \
             WHERE id = ?2 AND status = 'processing'",
            params![error.as_str(), job_id],
        )?;
        Ok(())
    }

    /// Consumes one automatic retry while the job stays claimed (`processing`).
    ///
    /// The worker backs off without releasing the job, so a cancel during the
    /// delay still sees a processing row and a daemon restart leaves enough
    /// state for `resume_interrupted_tagging_jobs` to continue the same job.
    pub fn bump_tagging_retry(&self, job_id: &str) -> Result<bool> {
        let changed = self.conn.execute(
            "UPDATE tagging_jobs SET retry_count = retry_count + 1, progress = 0, \
                stage = NULL, error = NULL \
             WHERE id = ?1 AND status = 'processing' AND retry_count < ?2",
            params![job_id, MAX_TAGGING_RETRIES],
        )?;
        Ok(changed == 1)
    }

    /// Returns a failed job to the queue while it still has retries left and
    /// no newer job for the same photo/plugin is active.
    pub fn retry_tagging_job(&self, job_id: &str) -> Result<bool> {
        let changed = self.conn.execute(
            "UPDATE tagging_jobs SET status = 'pending', retry_count = retry_count + 1, \
                progress = 0, stage = NULL, error = NULL, started_at = NULL, finished_at = NULL \
             WHERE id = ?1 AND status = 'failed' AND retry_count < ?2 \
               AND NOT EXISTS ( \
                   SELECT 1 FROM tagging_jobs other \
                   WHERE other.photo_id = tagging_jobs.photo_id \
                     AND other.plugin_id = tagging_jobs.plugin_id \
                     AND other.id <> tagging_jobs.id \
                     AND other.status IN ('waiting', 'pending', 'processing') \
               )",
            params![job_id, MAX_TAGGING_RETRIES],
        )?;
        Ok(changed == 1)
    }

    /// Cancels an unfinished job. Returns false for terminal or missing jobs.
    pub fn cancel_tagging_job(&self, job_id: &str) -> Result<bool> {
        let changed = self.conn.execute(
            "UPDATE tagging_jobs SET status = 'cancelled', progress = 0, stage = NULL, \
                finished_at = ?1 \
             WHERE id = ?2 AND status IN ('waiting', 'pending', 'processing')",
            params![now_string(), job_id],
        )?;
        Ok(changed == 1)
    }

    /// Cancels every unfinished job for one photo, including one currently
    /// processing. Returns the number of cancelled rows.
    pub fn cancel_tagging_jobs_for_photo(&self, photo_id: i64) -> Result<usize> {
        let changed = self.conn.execute(
            "UPDATE tagging_jobs SET status = 'cancelled', progress = 0, stage = NULL, \
                finished_at = ?1 \
             WHERE photo_id = ?2 AND status IN ('waiting', 'pending', 'processing')",
            params![now_string(), photo_id],
        )?;
        Ok(changed)
    }

    /// Cancels every unfinished tagging job whose photo has a pending forced
    /// deletion. `pending_deletion_requests` hides those requests while their
    /// jobs are active, so the executor runs this before collecting work. One
    /// statement keeps the per-tick cost bounded even when the deletion backlog
    /// is large.
    pub fn cancel_tagging_for_forced_deletions(&self) -> Result<usize> {
        let changed = self.conn.execute(
            "UPDATE tagging_jobs SET status = 'cancelled', progress = 0, stage = NULL, \
                finished_at = ?1 \
             WHERE status IN ('waiting', 'pending', 'processing') \
               AND photo_id IN ( \
                   SELECT photo_id FROM deletion_requests \
                   WHERE force = 1 AND status = 'pending' \
               )",
            params![now_string()],
        )?;
        Ok(changed)
    }

    /// Cancels every unfinished job for one plugin. Returns the number of
    /// cancelled rows.
    pub fn cancel_tagging_jobs_for_plugin(&self, plugin_id: &str) -> Result<usize> {
        cancel_tagging_jobs_for_plugin(&self.conn, plugin_id)
    }

    /// Returns jobs left `processing` by a crash to the queue without
    /// consuming a retry.
    pub fn resume_interrupted_tagging_jobs(&self) -> Result<usize> {
        let changed = self.conn.execute(
            "UPDATE tagging_jobs SET status = 'pending', progress = 0, stage = NULL, \
                error = NULL, started_at = NULL, finished_at = NULL \
             WHERE status = 'processing'",
            [],
        )?;
        Ok(changed)
    }

    pub fn tagging_job_counts(&self, plugin_id: &str) -> Result<JobStatusCounts> {
        let mut counts = JobStatusCounts::default();
        let mut stmt = self.conn.prepare(
            "SELECT status, COUNT(*) FROM tagging_jobs WHERE plugin_id = ?1 GROUP BY status",
        )?;
        let rows = stmt.query_map(params![plugin_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?;
        for row in rows {
            let (status, count) = row?;
            match TaggingJobStatus::parse(&status) {
                Some(TaggingJobStatus::Waiting) => counts.waiting += count,
                Some(TaggingJobStatus::Pending) => counts.pending += count,
                Some(TaggingJobStatus::Processing) => counts.processing += count,
                Some(TaggingJobStatus::Failed) => counts.failed += count,
                Some(TaggingJobStatus::Completed | TaggingJobStatus::Cancelled) => {}
                None => counts.pending += count,
            }
        }
        Ok(counts)
    }

    /// Newest unfinished jobs, for the settings/integrations status card.
    /// `limit` is clamped to at least 1. The partial index keeps this bounded
    /// even when the backlog holds six figures of waiting rows, and the
    /// explicit `INDEXED BY` prevents SQLite from falling back to a temp
    /// B-tree over every active job.
    pub fn active_tagging_jobs(&self, limit: i64) -> Result<Vec<TaggingJob>> {
        let sql = format!(
            "SELECT {} FROM tagging_jobs INDEXED BY idx_tagging_jobs_active_list \
             WHERE status IN ('waiting', 'pending', 'processing') \
             ORDER BY created_at DESC, id DESC LIMIT ?1",
            TAGGING_JOB_COLUMNS
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params![limit.max(1)], row_to_tagging_job)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Active jobs for the runtime card, ordered so the jobs actually running
    /// are never pushed out by a large ready queue. Processing jobs come first
    /// (oldest start first), then pending jobs (oldest first). The waiting
    /// backlog is excluded: the card never shows waiting rows, so including
    /// them in the limited snapshot only hides real work.
    pub fn active_tagging_jobs_for_ui(&self, limit: i64) -> Result<Vec<TaggingJob>> {
        let limit = limit.max(1);
        let mut jobs = self.tagging_jobs_with_status(
            TaggingJobStatus::Processing,
            "COALESCE(started_at, created_at), created_at, rowid",
            limit,
        )?;
        let remaining = limit - i64::try_from(jobs.len()).unwrap_or(limit);
        if remaining > 0 {
            jobs.extend(self.tagging_jobs_with_status(
                TaggingJobStatus::Pending,
                "created_at, rowid",
                remaining,
            )?);
        }
        Ok(jobs)
    }

    fn tagging_jobs_with_status(
        &self,
        status: TaggingJobStatus,
        order: &str,
        limit: i64,
    ) -> Result<Vec<TaggingJob>> {
        let sql = format!(
            "SELECT {} FROM tagging_jobs WHERE status = ?1 ORDER BY {order} LIMIT ?2",
            TAGGING_JOB_COLUMNS
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params![status.as_str(), limit], row_to_tagging_job)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn tagging_jobs_for_photo(&self, photo_id: i64) -> Result<Vec<TaggingJob>> {
        let sql = format!(
            "SELECT {} FROM tagging_jobs WHERE photo_id = ?1 \
             ORDER BY created_at, rowid",
            TAGGING_JOB_COLUMNS
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params![photo_id], row_to_tagging_job)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Removes every AI result and catalog row for one plugin in a single
    /// transaction. Manual tags and other plugins' data are left untouched.
    /// Active jobs for the plugin are cancelled first so an in-flight sidecar
    /// result cannot recreate the rows after the delete. The library revision
    /// advances only when something was removed.
    pub fn delete_ai_data(&self, plugin_id: &str) -> Result<()> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        cancel_tagging_jobs_for_plugin(&tx, plugin_id)?;
        let mut removed = 0;
        for statement in [
            "DELETE FROM photo_ai_tags WHERE plugin_id = ?1",
            "DELETE FROM photo_ai WHERE plugin_id = ?1",
            "DELETE FROM ai_tag_definitions WHERE plugin_id = ?1",
            "DELETE FROM ai_tag_thresholds WHERE plugin_id = ?1",
        ] {
            removed += tx.execute(statement, params![plugin_id])?;
        }
        tx.commit()?;
        if removed > 0 {
            self.bump_library_revision()?;
        }
        Ok(())
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

    /// Runs `f` as a single transaction: commits when `f` succeeds and
    /// rolls back when it fails, so bulk writers never leave a
    /// half-written library behind.
    pub fn in_transaction<T>(&self, f: impl FnOnce(&Db) -> Result<T>) -> Result<T> {
        let tx = self.conn.unchecked_transaction()?;
        let value = f(self)?;
        tx.commit()?;
        Ok(value)
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

    pub fn delete_setting(&self, key: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM app_settings WHERE key = ?1", params![key])?;
        Ok(())
    }
}

/// Writes one AI result and its judged tags. Returns false without touching
/// the database when the photo is no longer in the library.
fn write_photo_ai(
    conn: &Connection,
    photo_id: i64,
    plugin_id: &str,
    result: &NewPhotoAi,
) -> Result<bool> {
    let visible: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM photos \
         WHERE id = ?1 AND status IN ('present', 'replaced'))",
        params![photo_id],
        |row| row.get(0),
    )?;
    if !visible {
        return Ok(false);
    }

    let now = now_string();
    conn.execute(
        "INSERT INTO photo_ai(photo_id, plugin_id, variant, sidecar_version, model_version, \
            model_revision, catalog_version, source, presence_threshold, character_present, \
            presence_probability, people_min, people_max, people_median, place_branch, \
            place_probabilities_json, tagged_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17) \
         ON CONFLICT(photo_id) DO UPDATE SET \
            plugin_id = excluded.plugin_id, \
            variant = excluded.variant, \
            sidecar_version = excluded.sidecar_version, \
            model_version = excluded.model_version, \
            model_revision = excluded.model_revision, \
            catalog_version = excluded.catalog_version, \
            source = excluded.source, \
            presence_threshold = excluded.presence_threshold, \
            character_present = excluded.character_present, \
            presence_probability = excluded.presence_probability, \
            people_min = excluded.people_min, \
            people_max = excluded.people_max, \
            people_median = excluded.people_median, \
            place_branch = excluded.place_branch, \
            place_probabilities_json = excluded.place_probabilities_json, \
            tagged_at = excluded.tagged_at",
        params![
            photo_id,
            plugin_id,
            result.variant,
            result.sidecar_version,
            result.model_version,
            result.model_revision,
            result.catalog_version,
            result.source,
            result.presence_threshold,
            i64::from(result.character_present),
            result.presence_probability,
            result.people_min.map(i64::from),
            result.people_max.map(i64::from),
            result.people_median.map(i64::from),
            result.place_branch,
            result.place_probabilities_json,
            now,
        ],
    )?;

    // Re-tagging may move the photo to another branch, so replace the whole
    // judged set for this plugin instead of only upserting the new keys.
    conn.execute(
        "DELETE FROM photo_ai_tags WHERE photo_id = ?1 AND plugin_id = ?2",
        params![photo_id, plugin_id],
    )?;
    for (tag_key, probability) in &result.tags {
        conn.execute(
            "INSERT INTO photo_ai_tags(photo_id, plugin_id, tag_key, probability, applied, \
                model_version, catalog_version, tagged_at) \
             VALUES (?1, ?2, ?3, ?4, CASE WHEN ?4 >= COALESCE( \
                (SELECT threshold FROM ai_tag_thresholds \
                 WHERE plugin_id = ?2 AND tag_key = ?3), \
                (SELECT default_threshold FROM ai_tag_definitions \
                 WHERE plugin_id = ?2 AND tag_key = ?3)) \
             THEN 1 ELSE 0 END, ?5, ?6, ?7)",
            params![
                photo_id,
                plugin_id,
                tag_key,
                probability,
                result.model_version,
                result.catalog_version,
                now,
            ],
        )?;
    }
    Ok(true)
}

const TAGGING_JOB_COLUMNS: &str = "id, photo_id, plugin_id, variant, status, progress, stage, \
    error, retry_count, created_at, started_at, finished_at";

fn cancel_tagging_jobs_for_plugin(conn: &Connection, plugin_id: &str) -> Result<usize> {
    let changed = conn.execute(
        "UPDATE tagging_jobs SET status = 'cancelled', progress = 0, stage = NULL, \
            finished_at = ?1 \
         WHERE plugin_id = ?2 AND status IN ('waiting', 'pending', 'processing')",
        params![now_string(), plugin_id],
    )?;
    Ok(changed)
}

fn row_to_photo_ai(row: &Row<'_>) -> rusqlite::Result<PhotoAi> {
    Ok(PhotoAi {
        photo_id: row.get(0)?,
        plugin_id: row.get(1)?,
        variant: row.get(2)?,
        sidecar_version: row.get(3)?,
        model_version: row.get(4)?,
        model_revision: row.get(5)?,
        catalog_version: row.get(6)?,
        source: row.get(7)?,
        presence_threshold: row.get(8)?,
        character_present: row.get::<_, i64>(9)? != 0,
        presence_probability: row.get(10)?,
        people_min: row
            .get::<_, Option<i64>>(11)?
            .map(|value| value.max(0) as u32),
        people_max: row
            .get::<_, Option<i64>>(12)?
            .map(|value| value.max(0) as u32),
        people_median: row
            .get::<_, Option<i64>>(13)?
            .map(|value| value.max(0) as u32),
        place_branch: row.get(14)?,
        place_probabilities_json: row.get(15)?,
        tagged_at: row.get(16)?,
    })
}

fn row_to_photo_ai_tag(row: &Row<'_>) -> rusqlite::Result<PhotoAiTag> {
    Ok(PhotoAiTag {
        photo_id: row.get(0)?,
        plugin_id: row.get(1)?,
        tag_key: row.get(2)?,
        probability: row.get(3)?,
        applied: row.get::<_, i64>(4)? != 0,
        model_version: row.get(5)?,
        catalog_version: row.get(6)?,
        tagged_at: row.get(7)?,
    })
}

fn row_to_tagging_job(row: &Row<'_>) -> rusqlite::Result<TaggingJob> {
    let status = row.get::<_, String>(4)?;
    Ok(TaggingJob {
        id: row.get(0)?,
        photo_id: row.get(1)?,
        plugin_id: row.get(2)?,
        variant: row.get(3)?,
        status: TaggingJobStatus::parse(&status).unwrap_or(TaggingJobStatus::Pending),
        progress: row.get(5)?,
        stage: row
            .get::<_, Option<String>>(6)?
            .and_then(|stage| TaggingStage::parse(&stage)),
        error: row.get(7)?,
        retry_count: row.get(8)?,
        created_at: row.get(9)?,
        started_at: row.get(10)?,
        finished_at: row.get(11)?,
    })
}

fn reapply_thresholds(conn: &Connection, plugin_id: &str) -> Result<usize> {
    let updated = conn.execute(
        "UPDATE photo_ai_tags SET applied = CASE WHEN probability >= COALESCE( \
            (SELECT t.threshold FROM ai_tag_thresholds t \
             WHERE t.plugin_id = photo_ai_tags.plugin_id \
               AND t.tag_key = photo_ai_tags.tag_key), \
            (SELECT d.default_threshold FROM ai_tag_definitions d \
             WHERE d.plugin_id = photo_ai_tags.plugin_id \
               AND d.tag_key = photo_ai_tags.tag_key)) \
         THEN 1 ELSE 0 END \
         WHERE plugin_id = ?1",
        params![plugin_id],
    )?;
    Ok(updated)
}

fn photo_order(sort: PhotoSort) -> &'static str {
    match sort {
        PhotoSort::Newest => "p.captured_at_utc DESC, p.id DESC",
        PhotoSort::Oldest => "p.captured_at_utc ASC, p.id ASC",
        PhotoSort::Largest => "p.file_size DESC, p.id DESC",
        PhotoSort::World => "c.world_name COLLATE NOCASE ASC, p.captured_at_utc DESC",
    }
}

fn push_ai_filter(clauses: &mut Vec<String>, values: &mut Vec<Value>, ai: &AiPhotoFilter) {
    if !ai.tags.is_empty() {
        let placeholders = vec!["?"; ai.tags.len()].join(",");
        clauses.push(format!(
            "EXISTS (SELECT 1 FROM photo_ai_tags at \
             WHERE at.photo_id = p.id AND at.plugin_id = ? AND at.applied = 1 \
             AND at.tag_key IN ({placeholders}))"
        ));
        values.push(Value::Text(AI_PLUGIN_ID.to_string()));
        values.extend(ai.tags.iter().map(|tag| Value::Text(tag.clone())));
    }

    if let Some(count) = ai.people_count {
        clauses.push(
            "EXISTS (SELECT 1 FROM photo_ai pa WHERE pa.photo_id = p.id \
             AND pa.plugin_id = ? AND pa.people_min <= ? AND pa.people_max >= ?)"
                .to_string(),
        );
        values.push(Value::Text(AI_PLUGIN_ID.to_string()));
        values.push(Value::Integer(i64::from(count)));
        values.push(Value::Integer(i64::from(count)));
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

    push_ai_filter(&mut clauses, &mut values, &filter.ai);

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

fn build_text_filter(filter: &PhotoTextFilter) -> (String, Vec<Value>) {
    let mut clauses = vec![VISIBLE_STATUS.to_string()];
    let mut values: Vec<Value> = Vec::new();

    if let Some(query) = filter
        .world
        .as_deref()
        .map(str::trim)
        .filter(|query| !query.is_empty())
    {
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
    if let Some(query) = filter
        .player
        .as_deref()
        .map(str::trim)
        .filter(|query| !query.is_empty())
    {
        let like = format!("%{query}%");
        clauses.push(
            "EXISTS (SELECT 1 FROM photo_players pp WHERE pp.photo_id = p.id \
             AND (pp.display_name LIKE ? OR pp.user_id LIKE ?))"
                .to_string(),
        );
        values.push(Value::Text(like.clone()));
        values.push(Value::Text(like));
    }
    if let Some(query) = filter
        .tag
        .as_deref()
        .map(str::trim)
        .filter(|query| !query.is_empty())
    {
        let like = format!("%{query}%");
        clauses.push(
            "EXISTS (SELECT 1 FROM photo_tags pt JOIN tags t ON t.id = pt.tag_id \
             WHERE pt.photo_id = p.id AND t.name LIKE ?)"
                .to_string(),
        );
        values.push(Value::Text(like));
    }

    push_ai_filter(&mut clauses, &mut values, &filter.ai);

    if filter.favorite {
        clauses.push("p.is_favorite = 1".to_string());
    }
    if let Some(from) = filter.from_utc {
        clauses.push("p.captured_at_utc >= ?".to_string());
        values.push(Value::Integer(from));
    }
    if let Some(to) = filter.to_utc {
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
        world_id: row.get(27)?,
    })
}

fn row_to_photo_index(row: &Row<'_>) -> rusqlite::Result<PhotoIndexEntry> {
    Ok(PhotoIndexEntry {
        id: row.get(0)?,
        captured_at_utc: row.get(1)?,
        captured_month: row.get(2)?,
    })
}

fn row_to_plugin_installation(row: &Row<'_>) -> rusqlite::Result<PluginInstallation> {
    Ok(PluginInstallation {
        plugin_id: row.get(0)?,
        variant: row.get(1)?,
        version: row.get(2)?,
        protocol_version: row.get(3)?,
        path: row.get(4)?,
        manifest_json: row.get(5)?,
        catalog_version: row.get(6)?,
        active: row.get::<_, i64>(7)? != 0,
        installed_at: row.get(8)?,
        updated_at: row.get(9)?,
    })
}

fn row_to_plugin_state(row: &Row<'_>) -> rusqlite::Result<PluginState> {
    let model_state: String = row.get(1)?;
    let model_state = ModelState::parse(&model_state).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            1,
            rusqlite::types::Type::Text,
            format!("unknown model state '{model_state}'").into(),
        )
    })?;
    Ok(PluginState {
        plugin_id: row.get(0)?,
        model_state,
        model_revision: row.get(2)?,
        model_bytes_downloaded: row.get(3)?,
        model_bytes_total: row.get(4)?,
        model_error: row.get(5)?,
        probe_json: row.get(6)?,
        updated_at: row.get(7)?,
    })
}

fn row_to_ai_tag_definition(row: &Row<'_>) -> rusqlite::Result<AiTagDefinition> {
    Ok(AiTagDefinition {
        plugin_id: row.get(0)?,
        tag_key: row.get(1)?,
        branch: row.get(2)?,
        name: row.get(3)?,
        name_en: row.get(4)?,
        description: row.get(5)?,
        default_threshold: row.get(6)?,
        sort_order: row.get(7)?,
        catalog_version: row.get(8)?,
        updated_at: row.get(9)?,
    })
}

fn row_to_ai_tag_threshold(row: &Row<'_>) -> rusqlite::Result<AiTagThreshold> {
    Ok(AiTagThreshold {
        plugin_id: row.get(0)?,
        tag_key: row.get(1)?,
        threshold: row.get(2)?,
        updated_at: row.get(3)?,
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

fn row_to_send_outbox(row: &Row<'_>) -> rusqlite::Result<SendOutboxEntry> {
    Ok(SendOutboxEntry {
        id: row.get(0)?,
        photo_id: row.get(1)?,
        kind: SendKind::parse(&row.get::<_, String>(2)?),
        origin: SendOrigin::parse(&row.get::<_, String>(3)?),
        status: SendStatus::parse(&row.get::<_, String>(4)?),
        source: row
            .get::<_, Option<String>>(5)?
            .map(|value| SendSource::parse(&value)),
        profile_id: row.get(6)?,
        destination: row.get(7)?,
        attempts: row.get(8)?,
        next_attempt_at: row.get(9)?,
        discord_message_id: row.get(10)?,
        discord_channel_id: row.get(11)?,
        discord_guild_id: row.get(12)?,
        error_code: row.get(13)?,
        error_detail: row.get(14)?,
        created_at: row.get(15)?,
        started_at: row.get(16)?,
        finished_at: row.get(17)?,
    })
}

fn row_to_world(row: &Row<'_>) -> rusqlite::Result<World> {
    Ok(World {
        id: row.get(0)?,
        world_id: row.get(1)?,
        name: row.get(2)?,
        name_source: row.get(3)?,
        alias: row.get(4)?,
    })
}

fn row_to_world_name_lookup(row: &Row<'_>) -> rusqlite::Result<WorldNameLookup> {
    let status: String = row.get(2)?;
    let status = WorldNameLookupStatus::parse(&status).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            2,
            rusqlite::types::Type::Text,
            format!("unknown world name lookup status '{status}'").into(),
        )
    })?;
    Ok(WorldNameLookup {
        world_db_id: row.get(0)?,
        world_id: row.get(1)?,
        status,
        attempts: row.get(3)?,
        next_attempt_at: row.get(4)?,
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

fn check_schema_version(version: i64) -> Result<()> {
    if version > SCHEMA_VERSION {
        return Err(CoreError::Invalid(format!(
            "database schema version {version} is newer than this build supports \
             (maximum {SCHEMA_VERSION}); update VRC PhotoN before opening it"
        )));
    }
    Ok(())
}

fn now_string() -> String {
    chrono::Utc::now().to_rfc3339()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::{NewPhotoAi, PluginInstallation};
    use crate::model::{
        DeletionReason, DeletionStatus, NewPhoto, PhotoFilter, PhotoPlayer, PhotoStatus,
    };
    use vrc_photon_ai_protocol::{
        Catalog, ModelState, TagDefinition, TaggingErrorCode, TaggingJobStatus, TaggingStage,
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

    const AI_PLUGIN: &str = "vrc-photon-autotag";

    fn ai_install(
        plugin_id: &str,
        variant: &str,
        version: &str,
        active: bool,
    ) -> PluginInstallation {
        PluginInstallation {
            plugin_id: plugin_id.to_string(),
            variant: variant.to_string(),
            version: version.to_string(),
            protocol_version: 1,
            path: format!("/plugins/{plugin_id}/{variant}/{version}"),
            manifest_json: format!(r#"{{"version":"{version}"}}"#),
            catalog_version: Some("2026-11-03.1".to_string()),
            active,
            installed_at: "2026-11-04T00:00:00+00:00".to_string(),
            updated_at: "2026-11-04T00:00:00+00:00".to_string(),
        }
    }

    fn tag(key: &str, name: &str, name_en: Option<&str>, default_threshold: f32) -> TagDefinition {
        TagDefinition {
            key: key.to_string(),
            name: name.to_string(),
            name_en: name_en.map(str::to_string),
            description: format!("{name} description"),
            default_threshold,
        }
    }

    fn ai_catalog(version: &str, sea_threshold: f32, room_threshold: f32) -> Catalog {
        Catalog {
            version: version.to_string(),
            outdoor: vec![
                tag("sea", "海", Some("Sea"), sea_threshold),
                tag("park", "公園", None, 0.5),
            ],
            indoor: vec![tag(
                "japanese_room",
                "和室",
                Some("Japanese room"),
                room_threshold,
            )],
        }
    }

    fn seed_ai_tag(
        db: &Db,
        photo_id: i64,
        plugin_id: &str,
        tag_key: &str,
        probability: f32,
        applied: bool,
    ) {
        db.connection()
            .execute(
                "INSERT INTO photo_ai_tags(photo_id, plugin_id, tag_key, probability, applied, \
                    model_version, catalog_version, tagged_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, 'd1-2B-Q8_0', '2026-11-03.1', '2026-11-04T00:00:00+00:00')",
                params![photo_id, plugin_id, tag_key, probability, i64::from(applied)],
            )
            .unwrap();
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
    fn interrupted_v1_migration_recovers() {
        // The V1 DDL committed but the crash hit before user_version moved:
        // the database still reports version 0 with the tables present.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("photos.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(SCHEMA_V1).unwrap();
        }
        let db = Db::open(&path).unwrap();
        let version: i64 = db
            .conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, 14);
    }

    #[test]
    fn interrupted_v2_migration_recovers() {
        // The V2 ALTER committed but user_version stayed at 1: the old
        // autocommit runner failed the next open with a duplicate column.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("photos.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(SCHEMA_V1).unwrap();
            conn.execute(
                "INSERT INTO schema_migrations(version, applied_at) VALUES (1, ?1)",
                params![now_string()],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO library_roots(path, kind, created_at) \
                 VALUES ('/photos', 'reference', ?1)",
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
            conn.execute_batch(SCHEMA_V2).unwrap();
            conn.execute(
                "INSERT INTO photo_context(photo_id, link_method, confidence, linked_at, author) \
                 VALUES (1, 'auto', 0, ?1, 'alice')",
                params![now_string()],
            )
            .unwrap();
            conn.pragma_update(None, "user_version", 1).unwrap();
        }
        let db = Db::open(&path).unwrap();
        let version: i64 = db
            .conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, 14);
        let author_columns: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('photo_context') WHERE name = 'author'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(author_columns, 1);
        let author: String = db
            .conn
            .query_row(
                "SELECT author FROM photo_context WHERE photo_id = 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(author, "alice");
        let recorded: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM schema_migrations WHERE version = 2",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(recorded, 1);
    }

    #[test]
    fn concurrent_open_of_a_fresh_database_succeeds() {
        // UI and daemon opening the same new database at once must both
        // succeed: the immediate transactions serialize the migrations.
        for round in 0..3 {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join(format!("photos-{round}.db"));
            let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
            let mut handles = Vec::new();
            for _ in 0..2 {
                let barrier = barrier.clone();
                let path = path.clone();
                handles.push(std::thread::spawn(move || {
                    barrier.wait();
                    Db::open(&path).map(|db| {
                        db.conn
                            .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
                    })
                }));
            }
            for handle in handles {
                let opened = handle.join().unwrap();
                assert_eq!(opened.unwrap().unwrap(), 14);
            }
        }
    }

    #[test]
    fn migration_v6_adds_job_output_dir() {
        let db = Db::open_in_memory().unwrap();
        let version: i64 = db
            .conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, 14);
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
        assert_eq!(version, 14);
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
    fn deletion_job_summary_keeps_unknown_status_distinct() {
        let db = Db::open_in_memory().unwrap();
        let id = seed_photo(&db);
        db.save_profile(&profile("p", "P")).unwrap();
        let job = db.enqueue_job(id, "p", "keep").unwrap();
        db.connection()
            .execute(
                "UPDATE compression_jobs SET status = 'future_status' WHERE id = ?1",
                params![job],
            )
            .unwrap();

        let summary = db.deletion_job_summary(&[id]).unwrap();
        assert_eq!(
            summary.compression.pending, 0,
            "unknown statuses must not be treated as pending"
        );
        assert_eq!(
            summary.compression.unknown, 1,
            "unknown statuses keep their own count"
        );
        assert_eq!(
            summary.affected_photos, 1,
            "the heading must not disagree with the body"
        );
    }

    #[test]
    fn deletion_blockers_count_active_sends_but_not_failed_sends() {
        let db = Db::open_in_memory().unwrap();
        let photo = seed_photo(&db);
        let id = db
            .enqueue_send(
                photo,
                SendKind::Photo,
                SendOrigin::Manual,
                SendStatus::Pending,
            )
            .unwrap()
            .unwrap();
        assert_eq!(db.deletion_blockers(photo).unwrap().active, 1);
        db.claim_next_send(100).unwrap();
        db.fail_send(id, SendErrorCode::Network, None, None, None, None)
            .unwrap();
        assert_eq!(db.deletion_blockers(photo).unwrap().active, 0);
    }

    #[test]
    fn deletion_summary_includes_send_statuses() {
        let db = Db::open_in_memory().unwrap();
        let waiting_photo = seed_photo(&db);
        let pending_photo = seed_other_photo(&db);
        db.enqueue_send(
            waiting_photo,
            SendKind::Photo,
            SendOrigin::Auto,
            SendStatus::Waiting,
        )
        .unwrap();
        db.enqueue_send(
            pending_photo,
            SendKind::Photo,
            SendOrigin::Manual,
            SendStatus::Pending,
        )
        .unwrap();
        let summary = db
            .deletion_job_summary(&[waiting_photo, pending_photo])
            .unwrap();
        assert_eq!(summary.send.waiting, 1);
        assert_eq!(summary.send.pending, 1);
        assert_eq!(
            summary.affected_photos, 2,
            "both photos have a pending send"
        );
    }

    #[test]
    fn deletion_summary_counts_a_photo_with_jobs_and_sends_once() {
        let db = Db::open_in_memory().unwrap();
        let both = seed_photo(&db);
        let send_only = seed_other_photo(&db);

        // Create and fail the send-only photo's row first so the claim below
        // cannot pick the combined photo's send.
        let failed = db
            .enqueue_send(
                send_only,
                SendKind::Photo,
                SendOrigin::Manual,
                SendStatus::Pending,
            )
            .unwrap()
            .unwrap();
        db.claim_next_send(100).unwrap();
        db.fail_send(failed, SendErrorCode::Network, None, None, None, None)
            .unwrap();

        db.save_profile(&profile("p", "P")).unwrap();
        db.enqueue_job(both, "p", "keep").unwrap();
        db.enqueue_send(
            both,
            SendKind::Photo,
            SendOrigin::Manual,
            SendStatus::Pending,
        )
        .unwrap();

        let summary = db.deletion_job_summary(&[both, send_only]).unwrap();
        assert_eq!(summary.compression.pending, 1);
        assert_eq!(summary.send.pending, 1);
        assert_eq!(summary.send.failed, 1);
        assert_eq!(
            summary.affected_photos, 2,
            "a photo with both kinds of work counts once, and a failed send alone counts"
        );
    }

    #[test]
    fn deletion_summary_counts_a_processing_send_as_affected() {
        let db = Db::open_in_memory().unwrap();
        let photo = seed_photo(&db);
        db.enqueue_send(
            photo,
            SendKind::Photo,
            SendOrigin::Manual,
            SendStatus::Pending,
        )
        .unwrap();
        let claimed = db.claim_next_send(100).unwrap().unwrap();
        assert_eq!(claimed.status, SendStatus::Processing);

        let summary = db.deletion_job_summary(&[photo]).unwrap();
        assert_eq!(summary.send.processing, 1);
        assert_eq!(summary.send.total(), 1);
        assert_eq!(
            summary.affected_photos, 1,
            "a photo with a claimed send is affected"
        );
        assert!(summary.needs_warning());
    }

    #[test]
    fn deletion_summary_excludes_photos_with_only_terminal_sends() {
        let db = Db::open_in_memory().unwrap();
        let sent_photo = seed_photo(&db);
        let cancelled_photo = seed_other_photo(&db);

        let sent = db
            .enqueue_send(
                sent_photo,
                SendKind::Photo,
                SendOrigin::Manual,
                SendStatus::Pending,
            )
            .unwrap()
            .unwrap();
        db.claim_next_send(100).unwrap();
        db.complete_send(
            sent,
            &SendCompletion {
                source: None,
                profile_id: None,
                destination: "D".into(),
                message_id: None,
                channel_id: None,
                guild_id: None,
            },
        )
        .unwrap();

        let cancelled = db
            .enqueue_send(
                cancelled_photo,
                SendKind::Photo,
                SendOrigin::Manual,
                SendStatus::Pending,
            )
            .unwrap()
            .unwrap();
        assert!(db.cancel_send(cancelled).unwrap());

        let summary = db
            .deletion_job_summary(&[sent_photo, cancelled_photo])
            .unwrap();
        assert_eq!(summary.send.total(), 0);
        assert_eq!(
            summary.affected_photos, 0,
            "sent and cancelled sends are terminal and leave the photo unaffected"
        );
        assert!(!summary.needs_warning());
    }

    #[test]
    fn deletion_summary_counts_an_unknown_send_status_as_affected() {
        let db = Db::open_in_memory().unwrap();
        let photo = seed_photo(&db);
        let send = db
            .enqueue_send(
                photo,
                SendKind::Photo,
                SendOrigin::Manual,
                SendStatus::Pending,
            )
            .unwrap()
            .unwrap();
        db.connection()
            .execute(
                "UPDATE send_outbox SET status = 'future_status' WHERE id = ?1",
                params![send],
            )
            .unwrap();

        let summary = db.deletion_job_summary(&[photo]).unwrap();
        assert_eq!(
            summary.send.pending, 1,
            "unknown send statuses parse as pending in the body"
        );
        assert_eq!(
            summary.affected_photos, 1,
            "a status written by a newer version is never terminal"
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
    fn pending_deletion_requests_skip_active_sends_and_ignore_test_rows() {
        let db = Db::open_in_memory().unwrap();
        let blocked = seed_photo(&db);
        let ready = seed_other_photo(&db);
        db.enqueue_test_send().unwrap();
        // auto_replace keeps the photo visible so a send can be queued after
        // the request is recorded.
        let blocked_request = db
            .request_deletion(&[blocked], DeletionReason::AutoReplace, false)
            .unwrap()[0];
        let ready_request = db
            .request_deletion(&[ready], DeletionReason::AutoReplace, false)
            .unwrap()[0];
        db.enqueue_send(
            blocked,
            SendKind::Photo,
            SendOrigin::Manual,
            SendStatus::Pending,
        )
        .unwrap();

        let pending = db.pending_deletion_requests(10).unwrap();
        assert!(pending.iter().any(|request| request.id == ready_request));
        assert!(
            !pending.iter().any(|request| request.id == blocked_request),
            "an active send must hide the request"
        );
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
    fn claim_deletion_will_not_start_while_a_send_is_processing() {
        let db = Db::open_in_memory().unwrap();
        let photo = seed_photo(&db);
        // auto_replace は写真を deleting にしないため、要求後に送信を積める。
        let request = db
            .request_deletion(&[photo], DeletionReason::AutoReplace, false)
            .unwrap()[0];
        db.enqueue_send(
            photo,
            SendKind::Photo,
            SendOrigin::Manual,
            SendStatus::Pending,
        )
        .unwrap();
        db.claim_next_send(100).unwrap();
        assert!(db.claim_deletion(request).unwrap().is_none());
    }

    #[test]
    fn claim_deletion_starts_after_an_in_flight_send_reaches_a_terminal_state() {
        let db = Db::open_in_memory().unwrap();
        let failed_photo = seed_photo(&db);
        let cancelled_photo = seed_other_photo(&db);
        // auto_replace は写真を deleting にしないため、要求後に送信を積める。
        let failed_request = db
            .request_deletion(&[failed_photo], DeletionReason::AutoReplace, false)
            .unwrap()[0];
        let cancelled_request = db
            .request_deletion(&[cancelled_photo], DeletionReason::AutoReplace, false)
            .unwrap()[0];
        let failed_send = db
            .enqueue_send(
                failed_photo,
                SendKind::Photo,
                SendOrigin::Manual,
                SendStatus::Pending,
            )
            .unwrap()
            .unwrap();
        let cancelled_send = db
            .enqueue_send(
                cancelled_photo,
                SendKind::Photo,
                SendOrigin::Manual,
                SendStatus::Pending,
            )
            .unwrap()
            .unwrap();
        db.claim_next_send(100).unwrap();
        assert!(db.claim_deletion(failed_request).unwrap().is_none());
        assert!(db.claim_deletion(cancelled_request).unwrap().is_none());

        db.fail_send(failed_send, SendErrorCode::Http401, None, None, None, None)
            .unwrap();
        assert!(db.claim_deletion(failed_request).unwrap().is_some());

        assert!(db.cancel_send(cancelled_send).unwrap());
        assert!(db.claim_deletion(cancelled_request).unwrap().is_some());
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
    fn manual_deletion_cancels_waiting_pending_and_failed_sends() {
        let db = Db::open_in_memory().unwrap();
        let waiting_photo = seed_photo(&db);
        let pending_photo = seed_other_photo(&db);
        let failed_root = db.add_root("/photos-c", "reference").unwrap();
        let failed_photo = db
            .upsert_photo(&photo_at(
                failed_root,
                "c.png",
                1_700_000_002,
                "2026-10-21 21:14:05",
            ))
            .unwrap();
        // 最初に failed 行を作って claim し、processing から failed にする。
        let failed = db
            .enqueue_send(
                failed_photo,
                SendKind::Photo,
                SendOrigin::Manual,
                SendStatus::Pending,
            )
            .unwrap()
            .unwrap();
        db.claim_next_send(100).unwrap();
        db.fail_send(failed, SendErrorCode::Http401, None, None, None, None)
            .unwrap();
        // その後で待機・実行待ちを積む。
        let waiting = db
            .enqueue_send(
                waiting_photo,
                SendKind::Photo,
                SendOrigin::Auto,
                SendStatus::Waiting,
            )
            .unwrap()
            .unwrap();
        let pending = db
            .enqueue_send(
                pending_photo,
                SendKind::Photo,
                SendOrigin::Manual,
                SendStatus::Pending,
            )
            .unwrap()
            .unwrap();
        db.request_deletion(
            &[waiting_photo, pending_photo, failed_photo],
            DeletionReason::Manual,
            false,
        )
        .unwrap();
        assert_eq!(
            db.send_entry_by_id(waiting).unwrap().unwrap().status,
            SendStatus::Cancelled
        );
        assert_eq!(
            db.send_entry_by_id(pending).unwrap().unwrap().status,
            SendStatus::Cancelled
        );
        assert_eq!(
            db.send_entry_by_id(failed).unwrap().unwrap().status,
            SendStatus::Cancelled
        );
    }

    #[test]
    fn force_deletion_leaves_a_processing_send_running() {
        let db = Db::open_in_memory().unwrap();
        let photo = seed_photo(&db);
        let request = db
            .request_deletion(&[photo], DeletionReason::AutoReplace, false)
            .unwrap()[0];
        let id = db
            .enqueue_send(
                photo,
                SendKind::Photo,
                SendOrigin::Manual,
                SendStatus::Pending,
            )
            .unwrap()
            .unwrap();
        db.claim_next_send(100).unwrap();
        db.force_deletion(request).unwrap();
        assert_eq!(
            db.send_entry_by_id(id).unwrap().unwrap().status,
            SendStatus::Processing
        );
        assert!(db.claim_deletion(request).unwrap().is_none());
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
    fn cancelling_a_deletion_does_not_restore_cancelled_sends() {
        let db = Db::open_in_memory().unwrap();
        let photo = seed_photo(&db);
        let send = db
            .enqueue_send(
                photo,
                SendKind::Photo,
                SendOrigin::Manual,
                SendStatus::Pending,
            )
            .unwrap()
            .unwrap();
        let request = db
            .request_deletion(&[photo], DeletionReason::Manual, false)
            .unwrap()[0];
        db.cancel_deletion(request).unwrap();
        assert_eq!(
            db.send_entry_by_id(send).unwrap().unwrap().status,
            SendStatus::Cancelled
        );
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
        assert_eq!(version, 14);

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
        assert_eq!(version, 14);
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
        assert_eq!(version, 14);

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
        assert_eq!(version, 14);
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
        let mut first_id = 0;
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
            if index == 0 {
                first_id = id;
            }
            if index == 500 {
                missing_id = id;
            }
            paths.push(path);
        }
        db.mark_missing(missing_id).unwrap();

        let states = db.photo_scan_states(&paths).unwrap();
        assert_eq!(states.len(), 501);
        let first = &states["/photos/0000.png"];
        assert_eq!(first.id, first_id);
        assert_eq!(first.file_size, 100);
        assert_eq!(first.mtime, 200);
        assert_eq!(first.blake3.as_deref(), Some("hash"));
        assert_eq!(first.status, PhotoStatus::Present);
        assert_eq!(first.display_derivative_id, None);
        assert_eq!(states["/photos/0500.png"].status, PhotoStatus::Missing);

        let derivative = db
            .add_derivative(
                first_id,
                "/photos/0000_webp.webp",
                "webp",
                4,
                3,
                10,
                "p",
                "replace",
            )
            .unwrap();
        db.set_replaced(first_id, derivative).unwrap();
        let states = db
            .photo_scan_states(&["/photos/0000.png".to_string()])
            .unwrap();
        let replaced = &states["/photos/0000.png"];
        assert_eq!(replaced.id, first_id);
        assert_eq!(replaced.display_derivative_id, Some(derivative));
        assert_eq!(replaced.status, PhotoStatus::Replaced);
    }

    /// Seeds 100k synthetic rows in one transaction and measures the shipped
    /// index and page queries. Ignored by default because it is a timing
    /// benchmark, not a correctness gate:
    ///
    /// `cargo test -p vrc-photon-core photo_index_scales_to_100k -- --ignored --nocapture`
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
            tx.execute(
                "INSERT INTO photo_ai(photo_id, plugin_id, variant, sidecar_version, \
                    model_version, model_revision, catalog_version, source, \
                    presence_threshold, character_present, presence_probability, \
                    people_min, people_max, people_median, place_branch, \
                    place_probabilities_json, tagged_at) \
                 SELECT id, ?1, 'bench', '1.0.0', 'd1-2B', 'rev', '2026-11-03.1', \
                    'original', 0.9, 1, 0.99, 2, 4, 3, 'outdoor', '{}', \
                    '2026-11-04T00:00:00+00:00' \
                 FROM photos WHERE id % 4 = 0",
                params![AI_PLUGIN_ID],
            )
            .unwrap();
            tx.execute(
                "INSERT INTO photo_ai_tags(photo_id, plugin_id, tag_key, probability, \
                    applied, model_version, catalog_version, tagged_at) \
                 SELECT photo_id, ?1, 'sea', 0.9, 1, 'd1-2B', '2026-11-03.1', \
                    '2026-11-04T00:00:00+00:00' \
                 FROM photo_ai WHERE plugin_id = ?1",
                params![AI_PLUGIN_ID],
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

        let ai_filter = PhotoTextFilter {
            ai: AiPhotoFilter {
                tags: vec!["sea".into()],
                people_count: Some(3),
            },
            ..Default::default()
        };
        let start = Instant::now();
        let ai_matches = db.photo_index_text(&ai_filter).unwrap();
        let ai_elapsed = start.elapsed();
        eprintln!("photo_index_text AI 100k: {ai_elapsed:?}");
        assert_eq!(ai_matches.len(), 25_000);
        assert!(
            ai_elapsed < Duration::from_secs(2),
            "photo_index_text AI took {ai_elapsed:?}"
        );
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
    fn open_sessions_returns_only_unfinished_sessions() {
        let db = Db::open_in_memory().unwrap();
        let world = db.upsert_world("wrld_test", None, None).unwrap();
        let open = db.create_session("open.txt", 10, world, None).unwrap();
        let closed = db.create_session("closed.txt", 20, world, None).unwrap();
        db.close_session(closed, 30).unwrap();

        let sessions = db.open_sessions().unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, open);
    }

    #[test]
    fn sessions_near_uses_inclusive_start_and_end_boundaries() {
        let cases = [
            ("ends at the timestamp", 900, Some(1000), true),
            ("ends at the lower bound", 900, Some(970), true),
            ("ends before the lower bound", 900, Some(969), false),
            ("starts at the timestamp", 1000, None, true),
            ("starts at the upper bound", 1030, None, true),
            ("starts after the upper bound", 1031, None, false),
            ("contains the timestamp", 900, Some(1100), false),
            ("is open before the timestamp", 900, None, false),
        ];

        for (label, started_at, ended_at, expected) in cases {
            let db = Db::open_in_memory().unwrap();
            let world = db.upsert_world("wrld_test", None, None).unwrap();
            let session = db
                .create_session("log.txt", started_at, world, None)
                .unwrap();
            if let Some(ended_at) = ended_at {
                db.close_session(session, ended_at).unwrap();
            }

            let near = db.sessions_near(1000, 30).unwrap().len() == 1;
            assert_eq!(near, expected, "{label}");
        }
    }

    #[test]
    fn sessions_near_deduplicates_a_zero_length_session_at_the_timestamp() {
        let db = Db::open_in_memory().unwrap();
        let world = db.upsert_world("wrld_test", None, None).unwrap();
        let session = db.create_session("log.txt", 1000, world, None).unwrap();
        db.close_session(session, 1000).unwrap();

        let near = db.sessions_near(1000, 30).unwrap();
        assert_eq!(near.len(), 1, "UNION must collapse both range matches");
        assert_eq!(near[0].id, session);
    }

    #[test]
    fn sessions_near_orders_by_start_and_returns_at_most_two() {
        let db = Db::open_in_memory().unwrap();
        let world = db.upsert_world("wrld_test", None, None).unwrap();
        for started_at in [1003, 1001, 1002, 1004] {
            db.create_session("log.txt", started_at, world, None)
                .unwrap();
        }

        let near = db.sessions_near(1000, 30).unwrap();
        assert_eq!(
            near.iter()
                .map(|session| session.started_at)
                .collect::<Vec<_>>(),
            vec![1001, 1002]
        );
    }

    #[test]
    fn log_mtime_reads_the_last_ingested_mtime() {
        let db = Db::open_in_memory().unwrap();
        assert_eq!(db.log_mtime("output_log.txt").unwrap(), None);
        db.set_log_offset("output_log.txt", 42, 1_700_000_123, 42, "1")
            .unwrap();
        assert_eq!(db.log_mtime("output_log.txt").unwrap(), Some(1_700_000_123));
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
    fn retry_resets_progress_and_rejects_non_failed_jobs() {
        let db = Db::open_in_memory().unwrap();
        let root = db.add_root("/photos", "reference").unwrap();
        let photo = db
            .upsert_photo(&sample_photo(root, "/photos/a.png", 100))
            .unwrap();
        let job = db.enqueue_job(photo, "p", "keep").unwrap();

        // A job that never failed cannot be retried, and neither can a
        // missing one.
        assert!(!db.retry_job(&job).unwrap());
        assert!(!db.retry_job("missing-job").unwrap());

        db.next_pending_job().unwrap();
        db.update_job(&job, QueueStatus::Failed, 0.5, Some("boom"), None)
            .unwrap();
        assert!(db.retry_job(&job).unwrap());
        let row = &db.list_jobs(1).unwrap()[0];
        assert_eq!(row.status, QueueStatus::Pending);
        assert_eq!(row.progress, 0.0);
        assert!(row.error.is_none());

        // The bulk retry resets progress the same way.
        db.next_pending_job().unwrap();
        db.update_job(&job, QueueStatus::Failed, 0.75, Some("boom again"), None)
            .unwrap();
        assert_eq!(db.retry_failed_jobs().unwrap(), 1);
        let row = &db.list_jobs(1).unwrap()[0];
        assert_eq!(row.status, QueueStatus::Pending);
        assert_eq!(row.progress, 0.0);
        assert!(row.error.is_none());
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
    fn count_retryable_failed_jobs_skips_capped_and_non_failed() {
        let db = Db::open_in_memory().unwrap();
        let root = db.add_root("/photos", "reference").unwrap();
        let photo = db
            .upsert_photo(&sample_photo(root, "/photos/a.png", 100))
            .unwrap();
        let first = db.enqueue_job(photo, "p", "keep").unwrap();
        let second = db.enqueue_job(photo, "p", "keep").unwrap();
        let capped = db.enqueue_job(photo, "p", "keep").unwrap();
        let completed = db.enqueue_job(photo, "p", "keep").unwrap();
        for job in [&first, &second, &capped, &completed] {
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

        assert_eq!(db.count_retryable_failed_jobs().unwrap(), 2);
        assert_eq!(db.retry_failed_jobs().unwrap(), 2);
        assert_eq!(db.count_retryable_failed_jobs().unwrap(), 0);
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
    fn list_jobs_by_status_clamps_non_positive_limits() {
        let db = Db::open_in_memory().unwrap();
        let root = db.add_root("/photos", "reference").unwrap();
        let photo = db
            .upsert_photo(&sample_photo(root, "/photos/a.png", 100))
            .unwrap();
        let first = db.enqueue_job(photo, "p", "keep").unwrap();
        let second = db.enqueue_job(photo, "p", "keep").unwrap();
        for job in [&first, &second] {
            db.update_job(job, QueueStatus::Failed, 0.0, Some("boom"), None)
                .unwrap();
        }

        // The documented contract: the limit never drops below 1.
        assert_eq!(
            db.list_jobs_by_status(QueueStatus::Failed, 0)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            db.list_jobs_by_status(QueueStatus::Failed, -5)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            db.list_jobs_by_status(QueueStatus::Failed, 2)
                .unwrap()
                .len(),
            2
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
    fn keyed_daemon_errors_round_trip() {
        let db = Db::open_in_memory().unwrap();
        db.record_daemon_error_i18n(
            DaemonOperation::Scan,
            "Photo scan failed",
            Some("daemon.scan_failed"),
            Some(r#"{"count":"3"}"#),
            Some("detail"),
        )
        .unwrap();

        let errors = db.daemon_errors().unwrap();
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].message_key.as_deref(), Some("daemon.scan_failed"));
        assert_eq!(errors[0].message_args.as_deref(), Some(r#"{"count":"3"}"#));
    }

    #[test]
    fn legacy_daemon_errors_have_no_key_or_args() {
        let db = Db::open_in_memory().unwrap();
        db.record_daemon_error(DaemonOperation::Log, "log failed", None)
            .unwrap();

        let errors = db.daemon_errors().unwrap();
        assert!(errors[0].message_key.is_none());
        assert!(errors[0].message_args.is_none());
    }

    #[test]
    fn v10_migration_preserves_pre_v10_daemon_errors() {
        let db = Db::open_in_memory().unwrap();
        db.connection()
            .execute_batch(
                "ALTER TABLE daemon_errors DROP COLUMN message_key;\
                 ALTER TABLE daemon_errors DROP COLUMN message_args;",
            )
            .unwrap();
        db.connection()
            .pragma_update(None, "user_version", 9)
            .unwrap();
        db.connection()
            .execute(
                "INSERT INTO daemon_errors(operation, message, detail, occurred_at, attempts) \
                 VALUES ('scan', 'legacy scan error', '/photos', \
                 '2026-10-31T00:00:00+00:00', 2)",
                [],
            )
            .unwrap();

        db.migrate_v10().unwrap();

        let errors = db.daemon_errors().unwrap();
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].message, "legacy scan error");
        assert_eq!(errors[0].detail.as_deref(), Some("/photos"));
        assert!(errors[0].message_key.is_none());
        assert!(errors[0].message_args.is_none());
    }

    #[test]
    fn migrate_v10_adds_columns_and_is_retryable() {
        let db = Db::open_in_memory().unwrap();
        db.connection()
            .pragma_update(None, "user_version", 9)
            .unwrap();
        db.migrate_v10().unwrap();
        db.migrate_v10().unwrap();

        let has_key: bool = db
            .connection()
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM pragma_table_info('daemon_errors') \
                 WHERE name = 'message_key')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let has_args: bool = db
            .connection()
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM pragma_table_info('daemon_errors') \
                 WHERE name = 'message_args')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(has_key);
        assert!(has_args);
    }

    #[test]
    fn migration_v11_adds_send_outbox() {
        let db = Db::open_in_memory().unwrap();
        let version: i64 = db
            .conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(version, 14);
        let exists: bool = db
            .conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='send_outbox')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(exists);
    }

    #[test]
    fn migration_v11_is_retryable() {
        let db = Db::open_in_memory().unwrap();
        db.conn.pragma_update(None, "user_version", 10).unwrap();
        db.migrate_v11().unwrap();
        db.migrate_v11().unwrap();
        let version: i64 = db
            .conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(version, 11);
    }

    #[test]
    fn migration_v12_adds_world_name_lookups() {
        let db = Db::open_in_memory().unwrap();
        let version: i64 = db
            .conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, 14);
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

    #[test]
    fn migrate_v13_adds_ai_tables_and_is_retryable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("library.db");
        let db = Db::open(&path).unwrap();
        db.connection()
            .pragma_update(None, "user_version", 12)
            .unwrap();
        assert!(db.migrate_v13().is_ok());
        assert!(db.migrate_v13().is_ok());
        let version: i64 = db
            .connection()
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, 13);
        assert!(db.active_plugin_installation(AI_PLUGIN).unwrap().is_none());

        for table in [
            "plugin_installations",
            "plugin_state",
            "ai_tag_definitions",
            "ai_tag_thresholds",
            "photo_ai",
            "photo_ai_tags",
            "tagging_jobs",
        ] {
            let exists: bool = db
                .connection()
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
                    params![table],
                    |row| row.get(0),
                )
                .unwrap();
            assert!(exists, "missing table {table}");
        }
        for index in [
            "idx_plugin_installations_one_active",
            "idx_photo_ai_tags_applied",
            "idx_photo_ai_people",
            "idx_tagging_jobs_status",
            "idx_tagging_jobs_claim",
            "idx_tagging_jobs_photo",
            "idx_tagging_jobs_plugin_status",
            "idx_tagging_jobs_active_list",
            "idx_tagging_jobs_active",
        ] {
            let exists: bool = db
                .connection()
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'index' AND name = ?1)",
                    params![index],
                    |row| row.get(0),
                )
                .unwrap();
            assert!(exists, "missing index {index}");
        }
    }

    #[test]
    fn migration_v13_upgrades_a_v12_database_and_preserves_photos() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("library.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE schema_migrations (version INTEGER PRIMARY KEY, applied_at TEXT NOT NULL);\
                 CREATE TABLE photos (id INTEGER PRIMARY KEY, abs_path TEXT NOT NULL);\
                 CREATE TABLE sessions (\
                     id INTEGER PRIMARY KEY, started_at INTEGER NOT NULL, ended_at INTEGER);\
                 CREATE TABLE compression_jobs (\
                     id TEXT PRIMARY KEY, status TEXT NOT NULL, created_at TEXT NOT NULL);\
                 INSERT INTO photos(id, abs_path) VALUES (42, '/photos/keep.png');\
                 PRAGMA user_version = 12;",
            )
            .unwrap();
        }

        let db = Db::open(&path).unwrap();
        let version: i64 = db
            .connection()
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, 14);
        let abs_path: String = db
            .connection()
            .query_row("SELECT abs_path FROM photos WHERE id = 42", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(abs_path, "/photos/keep.png");
        let jobs_table: bool = db
            .connection()
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master \
                 WHERE type = 'table' AND name = 'tagging_jobs')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(jobs_table);
    }

    #[test]
    fn migration_v13_recovers_from_partial_schema() {
        let db = Db::open_in_memory().unwrap();
        db.connection()
            .execute_batch("DROP TABLE photo_ai_tags")
            .unwrap();
        db.connection()
            .pragma_update(None, "user_version", 12)
            .unwrap();

        db.migrate_v13().unwrap();
        db.migrate_v13().unwrap();

        let version: i64 = db
            .conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, 13);
        let exists: bool = db
            .conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master \
                 WHERE type = 'table' AND name = 'photo_ai_tags')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(exists);
    }

    #[test]
    fn fresh_database_is_v14_and_has_the_claim_indexes() {
        let db = Db::open_in_memory().unwrap();
        let version: i64 = db
            .connection()
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, 14);
        for index in [
            "idx_jobs_claim",
            "idx_jobs_created",
            "idx_sessions_started",
            "idx_sessions_ended",
        ] {
            let exists: bool = db
                .connection()
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_master \
                     WHERE type = 'index' AND name = ?1)",
                    params![index],
                    |row| row.get(0),
                )
                .unwrap();
            assert!(exists, "missing index {index}");
        }
    }

    #[test]
    fn migration_v14_upgrades_a_v13_database() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("library.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE schema_migrations (\
                     version INTEGER PRIMARY KEY, applied_at TEXT NOT NULL);\
                 CREATE TABLE sessions (\
                     id INTEGER PRIMARY KEY, started_at INTEGER NOT NULL, ended_at INTEGER);\
                 CREATE TABLE compression_jobs (\
                     id TEXT PRIMARY KEY, status TEXT NOT NULL, created_at TEXT NOT NULL);\
                 PRAGMA user_version = 13;",
            )
            .unwrap();
        }

        let db = Db::open(&path).unwrap();
        let version: i64 = db
            .connection()
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, 14);
        for index in [
            "idx_jobs_claim",
            "idx_jobs_created",
            "idx_sessions_started",
            "idx_sessions_ended",
        ] {
            let exists: bool = db
                .connection()
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_master \
                     WHERE type = 'index' AND name = ?1)",
                    params![index],
                    |row| row.get(0),
                )
                .unwrap();
            assert!(exists, "missing index {index}");
        }
        let recorded: i64 = db
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM schema_migrations WHERE version = 14",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(recorded, 1);
    }

    #[test]
    fn migration_v14_is_retryable() {
        let db = Db::open_in_memory().unwrap();
        db.connection()
            .execute_batch(
                "DROP INDEX idx_jobs_claim;\
                 DROP INDEX idx_jobs_created;\
                 DROP INDEX idx_sessions_started;\
                 DROP INDEX idx_sessions_ended;",
            )
            .unwrap();
        db.connection()
            .pragma_update(None, "user_version", 13)
            .unwrap();

        db.migrate_v14().unwrap();
        db.migrate_v14().unwrap();

        let version: i64 = db
            .conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, 14);
    }

    #[test]
    fn open_rejects_a_database_from_a_newer_schema_version() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("library.db");
        let db = Db::open(&path).unwrap();
        db.connection()
            .pragma_update(None, "user_version", 15)
            .unwrap();
        drop(db);

        match Db::open(&path) {
            Err(CoreError::Invalid(message)) => {
                assert!(message.contains("15"), "{message}");
                assert!(message.contains("14"), "{message}");
            }
            Err(error) => panic!("expected a typed future-version error, got {error}"),
            Ok(_) => panic!("expected Db::open to reject schema version 15"),
        }
        match Db::open_read_only(&path) {
            Err(CoreError::Invalid(message)) => {
                assert!(message.contains("15"), "{message}");
                assert!(message.contains("14"), "{message}");
            }
            Err(error) => panic!("expected a typed future-version error, got {error}"),
            Ok(_) => panic!("expected Db::open_read_only to reject schema version 15"),
        }
    }

    #[test]
    fn next_pending_job_query_uses_the_claim_index() {
        let db = Db::open_in_memory().unwrap();
        let mut stmt = db
            .conn
            .prepare(
                "EXPLAIN QUERY PLAN SELECT id FROM compression_jobs \
                 WHERE status = 'pending' ORDER BY created_at, rowid LIMIT 1",
            )
            .unwrap();
        let plan = stmt
            .query_map([], |row| row.get::<_, String>(3))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
            .join("\n");
        assert!(plan.contains("idx_jobs_claim"), "{plan}");
    }

    #[test]
    fn sessions_near_query_uses_the_session_indexes() {
        let db = Db::open_in_memory().unwrap();
        let mut stmt = db
            .conn
            .prepare(&format!("EXPLAIN QUERY PLAN {SESSIONS_NEAR_SQL}"))
            .unwrap();
        let plan = stmt
            .query_map(params![1000, 30], |row| row.get::<_, String>(3))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
            .join("\n");
        assert!(plan.contains("idx_sessions_started"), "{plan}");
        assert!(plan.contains("idx_sessions_ended"), "{plan}");
    }

    #[test]
    fn active_installation_is_unique_per_plugin() {
        let db = Db::open_in_memory().unwrap();
        db.upsert_plugin_installation(&ai_install(AI_PLUGIN, "linux-vulkan", "1.0.0", true))
            .unwrap();
        db.upsert_plugin_installation(&ai_install(AI_PLUGIN, "linux-vulkan", "2.0.0", false))
            .unwrap();
        assert_eq!(
            db.active_plugin_installation(AI_PLUGIN)
                .unwrap()
                .unwrap()
                .version,
            "1.0.0"
        );

        // Upserting another install as active must clear the previous one
        // inside the same transaction instead of violating the unique index.
        db.upsert_plugin_installation(&ai_install(AI_PLUGIN, "linux-vulkan", "2.0.0", true))
            .unwrap();
        let active = db.active_plugin_installation(AI_PLUGIN).unwrap().unwrap();
        assert_eq!(active.version, "2.0.0");
        assert_eq!(
            db.list_plugin_installations(AI_PLUGIN)
                .unwrap()
                .iter()
                .filter(|install| install.active)
                .count(),
            1
        );

        // Active is unique per plugin, not per variant.
        db.upsert_plugin_installation(&ai_install(AI_PLUGIN, "windows-vulkan", "1.0.0", true))
            .unwrap();
        let active = db.active_plugin_installation(AI_PLUGIN).unwrap().unwrap();
        assert_eq!(active.variant, "windows-vulkan");
        assert_eq!(
            db.list_plugin_installations(AI_PLUGIN)
                .unwrap()
                .iter()
                .filter(|install| install.active)
                .count(),
            1
        );
    }

    #[test]
    fn plugin_installation_crud_roundtrip_and_active_selection() {
        let db = Db::open_in_memory().unwrap();
        for version in ["1.2.0", "1.9.9", "1.10.0"] {
            db.upsert_plugin_installation(&ai_install(AI_PLUGIN, "linux-vulkan", version, false))
                .unwrap();
        }
        let installations = db.list_plugin_installations(AI_PLUGIN).unwrap();
        assert_eq!(installations.len(), 3);
        let install = installations
            .iter()
            .find(|install| install.version == "1.10.0")
            .unwrap();
        assert_eq!(install.plugin_id, AI_PLUGIN);
        assert_eq!(install.variant, "linux-vulkan");
        assert_eq!(install.protocol_version, 1);
        assert_eq!(
            install.path,
            "/plugins/vrc-photon-autotag/linux-vulkan/1.10.0"
        );
        assert_eq!(install.manifest_json, r#"{"version":"1.10.0"}"#);
        assert_eq!(install.catalog_version.as_deref(), Some("2026-11-03.1"));
        assert!(db.active_plugin_installation(AI_PLUGIN).unwrap().is_none());

        // `None` picks the highest semantic version, not the lexicographic
        // maximum, for the requested variant.
        assert!(
            db.set_active_plugin_installation(AI_PLUGIN, "linux-vulkan", None)
                .unwrap()
        );
        let active = db.active_plugin_installation(AI_PLUGIN).unwrap().unwrap();
        assert_eq!(active.version, "1.10.0");

        assert!(
            db.set_active_plugin_installation(AI_PLUGIN, "linux-vulkan", Some("1.2.0"))
                .unwrap()
        );
        let active = db.active_plugin_installation(AI_PLUGIN).unwrap().unwrap();
        assert_eq!(active.version, "1.2.0");

        assert!(
            !db.set_active_plugin_installation(AI_PLUGIN, "linux-vulkan", Some("9.9.9"))
                .unwrap()
        );
        assert!(
            !db.set_active_plugin_installation(AI_PLUGIN, "windows-vulkan", None)
                .unwrap()
        );
        let active = db.active_plugin_installation(AI_PLUGIN).unwrap().unwrap();
        assert_eq!(active.version, "1.2.0");

        db.remove_plugin_installation(AI_PLUGIN, "linux-vulkan", "1.2.0")
            .unwrap();
        assert_eq!(db.list_plugin_installations(AI_PLUGIN).unwrap().len(), 2);
        assert!(db.active_plugin_installation(AI_PLUGIN).unwrap().is_none());
    }

    #[test]
    fn plugin_state_upserts_model_and_probe() {
        let db = Db::open_in_memory().unwrap();
        assert!(db.plugin_state(AI_PLUGIN).unwrap().is_none());

        db.set_plugin_probe_json(AI_PLUGIN, Some(r#"{"backend":"cpu"}"#))
            .unwrap();
        let state = db.plugin_state(AI_PLUGIN).unwrap().unwrap();
        assert_eq!(state.plugin_id, AI_PLUGIN);
        assert_eq!(state.model_state, ModelState::Missing);
        assert_eq!(state.model_bytes_downloaded, 0);
        assert_eq!(state.model_bytes_total, None);
        assert_eq!(state.probe_json.as_deref(), Some(r#"{"backend":"cpu"}"#));

        db.set_plugin_model_state(
            AI_PLUGIN,
            ModelState::Downloading,
            Some("rev-1"),
            512,
            Some(2048),
            None,
        )
        .unwrap();
        let state = db.plugin_state(AI_PLUGIN).unwrap().unwrap();
        assert_eq!(state.model_state, ModelState::Downloading);
        assert_eq!(state.model_revision.as_deref(), Some("rev-1"));
        assert_eq!(state.model_bytes_downloaded, 512);
        assert_eq!(state.model_bytes_total, Some(2048));
        assert_eq!(state.model_error, None);
        assert_eq!(state.probe_json.as_deref(), Some(r#"{"backend":"cpu"}"#));

        db.set_plugin_model_state(
            AI_PLUGIN,
            ModelState::Error,
            Some("rev-1"),
            512,
            Some(2048),
            Some("checksum mismatch"),
        )
        .unwrap();
        let state = db.plugin_state(AI_PLUGIN).unwrap().unwrap();
        assert_eq!(state.model_state, ModelState::Error);
        assert_eq!(state.model_error.as_deref(), Some("checksum mismatch"));

        // The model-state insert path must work without a prior probe row.
        db.set_plugin_model_state("other-plugin", ModelState::Ready, None, 0, None, None)
            .unwrap();
        let state = db.plugin_state("other-plugin").unwrap().unwrap();
        assert_eq!(state.model_state, ModelState::Ready);
        assert_eq!(state.probe_json, None);

        db.set_plugin_probe_json(AI_PLUGIN, None).unwrap();
        let state = db.plugin_state(AI_PLUGIN).unwrap().unwrap();
        assert_eq!(state.probe_json, None);
        assert_eq!(state.model_state, ModelState::Error);
    }

    #[test]
    fn catalog_sync_preserves_user_thresholds() {
        let db = Db::open_in_memory().unwrap();
        db.sync_ai_catalog(AI_PLUGIN, &ai_catalog("2026-11-03.1", 0.5, 0.5))
            .unwrap();
        let definitions = db.ai_tag_definitions(AI_PLUGIN).unwrap();
        assert_eq!(definitions.len(), 3);
        let sea = definitions
            .iter()
            .find(|definition| definition.tag_key == "sea")
            .unwrap();
        assert_eq!(sea.branch, "outdoor");
        assert_eq!(sea.sort_order, 0);
        assert_eq!(sea.default_threshold, 0.5);
        assert_eq!(sea.name_en.as_deref(), Some("Sea"));
        let room = definitions
            .iter()
            .find(|definition| definition.tag_key == "japanese_room")
            .unwrap();
        assert_eq!(room.branch, "indoor");
        assert_eq!(room.sort_order, 0);

        db.set_ai_tag_threshold(AI_PLUGIN, "sea", 0.8).unwrap();
        let photo = seed_photo(&db);
        seed_ai_tag(&db, photo, AI_PLUGIN, "sea", 0.75, true);

        db.sync_ai_catalog(AI_PLUGIN, &ai_catalog("2026-11-04.1", 0.7, 0.5))
            .unwrap();
        let definitions = db.ai_tag_definitions(AI_PLUGIN).unwrap();
        let sea = definitions
            .iter()
            .find(|definition| definition.tag_key == "sea")
            .unwrap();
        assert_eq!(sea.default_threshold, 0.7);
        assert_eq!(sea.catalog_version, "2026-11-04.1");

        let thresholds = db.ai_tag_thresholds(AI_PLUGIN).unwrap();
        assert_eq!(thresholds.len(), 1);
        assert_eq!(thresholds[0].tag_key, "sea");
        assert_eq!(thresholds[0].threshold, 0.8);

        // The user override (0.8) wins over the new default (0.7).
        let applied: i64 = db
            .connection()
            .query_row(
                "SELECT applied FROM photo_ai_tags \
                 WHERE photo_id = ?1 AND plugin_id = ?2 AND tag_key = 'sea'",
                params![photo, AI_PLUGIN],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(applied, 0);
    }

    #[test]
    fn catalog_sync_updates_defaults_and_reapplies() {
        let db = Db::open_in_memory().unwrap();
        db.sync_ai_catalog(AI_PLUGIN, &ai_catalog("2026-11-03.1", 0.5, 0.5))
            .unwrap();
        let photo = seed_photo(&db);
        seed_ai_tag(&db, photo, AI_PLUGIN, "sea", 0.6, true);
        seed_ai_tag(&db, photo, AI_PLUGIN, "park", 0.8, true);

        db.sync_ai_catalog(AI_PLUGIN, &ai_catalog("2026-11-04.1", 0.7, 0.5))
            .unwrap();

        let definitions = db.ai_tag_definitions(AI_PLUGIN).unwrap();
        let sea = definitions
            .iter()
            .find(|definition| definition.tag_key == "sea")
            .unwrap();
        assert_eq!(sea.default_threshold, 0.7);
        assert_eq!(sea.catalog_version, "2026-11-04.1");

        let applied = |tag_key: &str| -> i64 {
            db.connection()
                .query_row(
                    "SELECT applied FROM photo_ai_tags \
                     WHERE photo_id = ?1 AND plugin_id = ?2 AND tag_key = ?3",
                    params![photo, AI_PLUGIN, tag_key],
                    |row| row.get(0),
                )
                .unwrap()
        };
        assert_eq!(applied("sea"), 0);
        assert_eq!(applied("park"), 1);
        assert!(db.ai_tag_thresholds(AI_PLUGIN).unwrap().is_empty());
    }

    #[test]
    fn reapply_thresholds_updates_applied() {
        let db = Db::open_in_memory().unwrap();
        db.sync_ai_catalog(AI_PLUGIN, &ai_catalog("2026-11-03.1", 0.5, 0.5))
            .unwrap();
        let photo = seed_photo(&db);
        seed_ai_tag(&db, photo, AI_PLUGIN, "sea", 0.4, true);
        seed_ai_tag(&db, photo, AI_PLUGIN, "japanese_room", 0.6, false);
        db.connection()
            .execute(
                "INSERT INTO ai_tag_thresholds(plugin_id, tag_key, threshold, updated_at) \
                 VALUES (?1, 'japanese_room', 0.7, ?2)",
                params![AI_PLUGIN, "2026-11-04T00:00:00+00:00"],
            )
            .unwrap();

        assert_eq!(db.reapply_ai_tag_thresholds(AI_PLUGIN).unwrap(), 2);
        let applied = |plugin_id: &str, tag_key: &str| -> i64 {
            db.connection()
                .query_row(
                    "SELECT applied FROM photo_ai_tags \
                     WHERE photo_id = ?1 AND plugin_id = ?2 AND tag_key = ?3",
                    params![photo, plugin_id, tag_key],
                    |row| row.get(0),
                )
                .unwrap()
        };
        assert_eq!(applied(AI_PLUGIN, "sea"), 0);
        assert_eq!(applied(AI_PLUGIN, "japanese_room"), 0);

        seed_ai_tag(&db, photo, "other-plugin", "sea", 0.9, false);
        assert_eq!(db.reapply_ai_tag_thresholds(AI_PLUGIN).unwrap(), 2);
        assert_eq!(applied("other-plugin", "sea"), 0);
    }

    #[test]
    fn ai_tag_threshold_crud() {
        let db = Db::open_in_memory().unwrap();
        assert!(db.ai_tag_thresholds(AI_PLUGIN).unwrap().is_empty());

        db.set_ai_tag_threshold(AI_PLUGIN, "sea", 0.8).unwrap();
        let thresholds = db.ai_tag_thresholds(AI_PLUGIN).unwrap();
        assert_eq!(thresholds.len(), 1);
        assert_eq!(thresholds[0].plugin_id, AI_PLUGIN);
        assert_eq!(thresholds[0].tag_key, "sea");
        assert_eq!(thresholds[0].threshold, 0.8);

        db.set_ai_tag_threshold(AI_PLUGIN, "sea", 0.6).unwrap();
        db.set_ai_tag_threshold(AI_PLUGIN, "park", 0.3).unwrap();
        let thresholds = db.ai_tag_thresholds(AI_PLUGIN).unwrap();
        assert_eq!(thresholds.len(), 2);
        assert_eq!(
            thresholds
                .iter()
                .find(|threshold| threshold.tag_key == "sea")
                .unwrap()
                .threshold,
            0.6
        );

        db.reset_ai_tag_threshold(AI_PLUGIN, "sea").unwrap();
        let thresholds = db.ai_tag_thresholds(AI_PLUGIN).unwrap();
        assert_eq!(thresholds.len(), 1);
        assert_eq!(thresholds[0].tag_key, "park");

        // Resetting a missing override is a no-op.
        db.reset_ai_tag_threshold(AI_PLUGIN, "sea").unwrap();
        assert_eq!(db.ai_tag_thresholds(AI_PLUGIN).unwrap().len(), 1);

        assert!(matches!(
            db.set_ai_tag_threshold(AI_PLUGIN, "sea", 1.5),
            Err(CoreError::Invalid(_))
        ));
        assert!(matches!(
            db.set_ai_tag_threshold(AI_PLUGIN, "sea", f32::NAN),
            Err(CoreError::Invalid(_))
        ));
    }

    #[test]
    fn apply_ai_tag_thresholds_updates_resets_and_reapplies_once() {
        let db = Db::open_in_memory().unwrap();
        let photo = seed_photo(&db);
        install_test_catalog(&db);
        db.save_photo_ai(
            photo,
            AI_PLUGIN,
            &test_ai_result(&[("sea", 0.75), ("park", 0.75)]),
        )
        .unwrap();
        let revision = db.library_revision().unwrap();

        let updated = db
            .apply_ai_tag_thresholds(
                AI_PLUGIN,
                &[("sea".to_string(), Some(0.8)), ("park".to_string(), None)],
            )
            .unwrap();

        assert!(updated >= 2);
        let overrides = db.ai_tag_thresholds(AI_PLUGIN).unwrap();
        assert_eq!(overrides.len(), 1);
        assert_eq!(overrides[0].tag_key, "sea");
        let tags = db.photo_ai_tags(photo, false).unwrap();
        assert!(
            !tags
                .iter()
                .find(|tag| tag.tag_key == "sea")
                .unwrap()
                .applied
        );
        assert!(
            tags.iter()
                .find(|tag| tag.tag_key == "park")
                .unwrap()
                .applied
        );
        assert!(db.library_revision().unwrap() > revision);
    }

    #[test]
    fn apply_ai_tag_thresholds_rejects_out_of_range_values() {
        let db = Db::open_in_memory().unwrap();
        assert!(
            db.apply_ai_tag_thresholds(AI_PLUGIN, &[("sea".to_string(), Some(1.1))])
                .is_err()
        );
        assert!(
            db.apply_ai_tag_thresholds(AI_PLUGIN, &[("sea".to_string(), Some(f32::NAN))])
                .is_err()
        );
    }

    #[test]
    fn threshold_reapplication_bumps_library_revision() {
        let db = Db::open_in_memory().unwrap();
        db.sync_ai_catalog(AI_PLUGIN, &ai_catalog("2026-11-03.1", 0.5, 0.5))
            .unwrap();
        let photo = seed_photo(&db);
        db.save_photo_ai(photo, AI_PLUGIN, &test_ai_result(&[("sea", 0.6)]))
            .unwrap();

        let before = db.library_revision().unwrap();
        db.set_ai_tag_threshold(AI_PLUGIN, "sea", 0.7).unwrap();
        assert!(!db.photo_ai_tags(photo, false).unwrap()[0].applied);
        assert!(db.library_revision().unwrap() > before);

        let before = db.library_revision().unwrap();
        db.reset_ai_tag_threshold(AI_PLUGIN, "sea").unwrap();
        assert!(db.photo_ai_tags(photo, false).unwrap()[0].applied);
        assert!(db.library_revision().unwrap() > before);

        let before = db.library_revision().unwrap();
        db.sync_ai_catalog(AI_PLUGIN, &ai_catalog("2026-11-04.1", 0.7, 0.5))
            .unwrap();
        assert!(!db.photo_ai_tags(photo, false).unwrap()[0].applied);
        assert!(db.library_revision().unwrap() > before);

        // A catalog refresh that keeps the defaults does not reapply.
        let before = db.library_revision().unwrap();
        db.sync_ai_catalog(AI_PLUGIN, &ai_catalog("2026-11-05.1", 0.7, 0.5))
            .unwrap();
        assert_eq!(db.library_revision().unwrap(), before);
    }

    fn install_test_catalog(db: &Db) {
        db.sync_ai_catalog(AI_PLUGIN, &ai_catalog("2026-11-03.1", 0.5, 0.5))
            .unwrap();
    }

    fn test_ai_result(tags: &[(&str, f32)]) -> NewPhotoAi {
        NewPhotoAi {
            variant: "linux-cpu".to_string(),
            sidecar_version: "1.0.0".to_string(),
            model_version: "d1-2B-Q8_0".to_string(),
            model_revision: "rev-1".to_string(),
            catalog_version: "2026-11-03.1".to_string(),
            source: "original".to_string(),
            presence_threshold: 0.9,
            character_present: true,
            presence_probability: 0.99,
            people_min: Some(2),
            people_max: Some(3),
            people_median: Some(3),
            place_branch: Some("outdoor".to_string()),
            place_probabilities_json: Some(
                r#"{"indoor":0.1,"outdoor":0.8,"other":0.1}"#.to_string(),
            ),
            tags: tags
                .iter()
                .map(|(key, probability)| ((*key).to_string(), *probability))
                .collect(),
        }
    }

    fn seed_photos(db: &Db, count: usize) -> Vec<i64> {
        let root = db.add_root("/photos", "reference").unwrap();
        (0..count)
            .map(|index| {
                db.upsert_photo(&sample_photo(
                    root,
                    &format!("/photos/bulk-{index}.png"),
                    1_700_000_000 + index as i64,
                ))
                .unwrap()
            })
            .collect()
    }

    fn claim_tagging_job(db: &Db, photo_id: i64) -> TaggingJob {
        assert_eq!(
            db.enqueue_tagging_jobs(&[photo_id], AI_PLUGIN, TaggingJobStatus::Pending)
                .unwrap(),
            1
        );
        let job = db.next_pending_tagging_job().unwrap().unwrap();
        assert_eq!(job.photo_id, photo_id);
        job
    }

    fn query_plan(db: &Db, sql: &str) -> Vec<String> {
        query_plan_with_values(db, sql, Vec::new())
    }

    fn query_plan_with_values(db: &Db, sql: &str, values: Vec<Value>) -> Vec<String> {
        let mut stmt = db
            .connection()
            .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
            .unwrap();
        let rows = stmt
            .query_map(params_from_iter(values), |row| row.get::<_, String>(3))
            .unwrap();
        rows.collect::<rusqlite::Result<Vec<_>>>().unwrap()
    }

    #[test]
    fn ai_tags_never_touch_manual_tags() {
        let db = Db::open_in_memory().unwrap();
        let photo = seed_photo(&db);
        db.set_photo_tags(photo, &["manual".to_string()]).unwrap();
        install_test_catalog(&db);

        let result = NewPhotoAi {
            variant: "linux-cpu".into(),
            sidecar_version: "1.0.0".into(),
            model_version: "d1-2B".into(),
            model_revision: "rev".into(),
            catalog_version: "2026-11-03.1".into(),
            source: "original".into(),
            presence_threshold: 0.9,
            character_present: true,
            presence_probability: 0.99,
            people_min: Some(2),
            people_max: Some(3),
            people_median: Some(3),
            place_branch: Some("outdoor".into()),
            place_probabilities_json: Some(r#"{"indoor":0.1,"outdoor":0.8,"other":0.1}"#.into()),
            tags: vec![("sea".into(), 0.97), ("park".into(), 0.4)],
        };
        assert!(
            db.save_photo_ai(photo, "vrc-photon-autotag", &result)
                .unwrap()
        );
        assert_eq!(db.photo_tags(photo).unwrap(), vec!["manual".to_string()]);
        let tags = db.photo_ai_tags(photo, false).unwrap();
        assert_eq!(tags.len(), 2);
        assert!(
            tags.iter()
                .find(|tag| tag.tag_key == "sea")
                .unwrap()
                .applied
        );
        assert!(
            !tags
                .iter()
                .find(|tag| tag.tag_key == "park")
                .unwrap()
                .applied
        );
    }

    #[test]
    fn ai_tag_filter_matches_only_applied_tags_of_the_ai_plugin() {
        let db = Db::open_in_memory().unwrap();
        let sea = seed_photo(&db);
        install_test_catalog(&db);
        db.save_photo_ai(
            sea,
            AI_PLUGIN,
            &test_ai_result(&[("sea", 0.97), ("park", 0.42)]),
        )
        .unwrap();
        let other = seed_other_photo(&db);
        seed_ai_tag(&db, other, "other-plugin", "sea", 0.99, true);

        let sea_filter = PhotoTextFilter {
            ai: AiPhotoFilter {
                tags: vec!["sea".into()],
                people_count: None,
            },
            ..Default::default()
        };
        assert_eq!(
            db.photo_index_text(&sea_filter)
                .unwrap()
                .iter()
                .map(|entry| entry.id)
                .collect::<Vec<_>>(),
            vec![sea]
        );

        let unapplied = PhotoTextFilter {
            ai: AiPhotoFilter {
                tags: vec!["park".into()],
                people_count: None,
            },
            ..Default::default()
        };
        assert!(db.photo_index_text(&unapplied).unwrap().is_empty());
    }

    #[test]
    fn people_count_filter_matches_inclusive_intervals() {
        let db = Db::open_in_memory().unwrap();
        let range = seed_photo(&db);
        install_test_catalog(&db);
        let mut range_result = test_ai_result(&[]);
        range_result.people_min = Some(2);
        range_result.people_max = Some(4);
        db.save_photo_ai(range, AI_PLUGIN, &range_result).unwrap();

        let zero = seed_other_photo(&db);
        let mut zero_result = test_ai_result(&[]);
        zero_result.people_min = Some(0);
        zero_result.people_max = Some(0);
        db.save_photo_ai(zero, AI_PLUGIN, &zero_result).unwrap();

        let no_ai = seed_photos(&db, 1)[0];
        for (count, expected) in [
            (0, vec![zero]),
            (2, vec![range]),
            (3, vec![range]),
            (4, vec![range]),
            (5, vec![]),
        ] {
            let filter = PhotoTextFilter {
                ai: AiPhotoFilter {
                    tags: Vec::new(),
                    people_count: Some(count),
                },
                ..Default::default()
            };
            let mut ids = db
                .photo_index_text(&filter)
                .unwrap()
                .iter()
                .map(|entry| entry.id)
                .collect::<Vec<_>>();
            ids.sort_unstable();
            assert_eq!(ids, expected, "count={count}");
        }

        let zero_filter = PhotoTextFilter {
            ai: AiPhotoFilter {
                tags: Vec::new(),
                people_count: Some(0),
            },
            ..Default::default()
        };
        assert!(
            db.photo_index_text(&zero_filter)
                .unwrap()
                .iter()
                .all(|entry| entry.id != no_ai)
        );
    }

    #[test]
    fn photo_filter_ai_conditions_apply_to_search_photos_and_count() {
        let db = Db::open_in_memory().unwrap();
        let photo = seed_photo(&db);
        install_test_catalog(&db);
        db.save_photo_ai(photo, AI_PLUGIN, &test_ai_result(&[("sea", 0.9)]))
            .unwrap();
        seed_other_photo(&db);

        let filter = PhotoFilter {
            ai: AiPhotoFilter {
                tags: vec!["sea".into()],
                people_count: Some(3),
            },
            ..Default::default()
        };
        assert_eq!(db.count_search(&filter).unwrap(), 1);
        assert_eq!(
            db.search_photos(&filter)
                .unwrap()
                .iter()
                .map(|photo| photo.id)
                .collect::<Vec<_>>(),
            vec![photo]
        );
    }

    #[test]
    fn ai_filters_use_the_ai_indexes() {
        let db = Db::open_in_memory().unwrap();
        install_test_catalog(&db);
        let photos = seed_photos(&db, 500);
        for (index, photo_id) in photos.iter().enumerate() {
            // sea は 10 枚に 1 枚だけにして、インデックスが選択されるようにする。
            let tag = if index % 10 == 0 { "sea" } else { "park" };
            let mut result = test_ai_result(&[(tag, 0.9)]);
            result.people_min = Some((index % 5) as u32);
            result.people_max = Some(((index % 5) + 2) as u32);
            db.save_photo_ai(*photo_id, AI_PLUGIN, &result).unwrap();
        }
        db.connection().execute("ANALYZE", []).unwrap();

        let filter = PhotoTextFilter {
            ai: AiPhotoFilter {
                tags: vec!["sea".into()],
                people_count: Some(3),
            },
            ..Default::default()
        };
        let (where_sql, values) = build_text_filter(&filter);
        let plan = query_plan_with_values(
            &db,
            &format!("SELECT p.id {PHOTO_FROM} WHERE {where_sql}"),
            values,
        );
        assert!(
            plan.iter()
                .any(|line| line.contains("idx_photo_ai_tags_applied")),
            "plan: {plan:#?}"
        );
        // `photo_ai` is looked up by its INTEGER PRIMARY KEY (photo_id) for the
        // correlated EXISTS, so SQLite does not need idx_photo_ai_people here.
        assert!(
            plan.iter()
                .any(|line| line.contains("SEARCH pa") && line.contains("INTEGER PRIMARY KEY")),
            "plan: {plan:#?}"
        );
    }

    #[test]
    fn save_photo_ai_skips_missing_or_deleting_photos() {
        let db = Db::open_in_memory().unwrap();
        let photo = seed_photo(&db);
        install_test_catalog(&db);
        let result = test_ai_result(&[("sea", 0.97)]);
        let before = db.library_revision().unwrap();

        db.connection()
            .execute(
                "UPDATE photos SET status = 'missing' WHERE id = ?1",
                params![photo],
            )
            .unwrap();
        assert!(!db.save_photo_ai(photo, AI_PLUGIN, &result).unwrap());
        assert!(db.photo_ai(photo).unwrap().is_none());
        assert!(db.photo_ai_tags(photo, false).unwrap().is_empty());

        db.connection()
            .execute(
                "UPDATE photos SET status = 'deleting' WHERE id = ?1",
                params![photo],
            )
            .unwrap();
        assert!(!db.save_photo_ai(photo, AI_PLUGIN, &result).unwrap());
        assert!(db.photo_ai(photo).unwrap().is_none());
        assert_eq!(db.library_revision().unwrap(), before);

        db.connection()
            .execute(
                "UPDATE photos SET status = 'present' WHERE id = ?1",
                params![photo],
            )
            .unwrap();
        assert!(db.save_photo_ai(photo, AI_PLUGIN, &result).unwrap());
        assert!(db.library_revision().unwrap() > before);
    }

    #[test]
    fn saving_ai_result_replaces_the_previous_branch_tags() {
        let db = Db::open_in_memory().unwrap();
        let photo = seed_photo(&db);
        install_test_catalog(&db);

        assert!(
            db.save_photo_ai(
                photo,
                AI_PLUGIN,
                &test_ai_result(&[("sea", 0.9), ("park", 0.8)])
            )
            .unwrap()
        );
        assert_eq!(db.photo_ai_tags(photo, false).unwrap().len(), 2);

        let mut indoor = test_ai_result(&[("japanese_room", 0.6)]);
        indoor.place_branch = Some("indoor".to_string());
        assert!(db.save_photo_ai(photo, AI_PLUGIN, &indoor).unwrap());

        let tags = db.photo_ai_tags(photo, false).unwrap();
        assert_eq!(tags.len(), 1);
        assert_eq!(tags[0].tag_key, "japanese_room");
        let ai = db.photo_ai(photo).unwrap().unwrap();
        assert_eq!(ai.place_branch.as_deref(), Some("indoor"));
    }

    #[test]
    fn saved_ai_tags_follow_threshold_changes() {
        let db = Db::open_in_memory().unwrap();
        let photo = seed_photo(&db);
        install_test_catalog(&db);
        db.save_photo_ai(
            photo,
            AI_PLUGIN,
            &test_ai_result(&[("sea", 0.6), ("park", 0.4)]),
        )
        .unwrap();

        let applied = |tag_key: &str| -> bool {
            db.photo_ai_tags(photo, false)
                .unwrap()
                .into_iter()
                .find(|tag| tag.tag_key == tag_key)
                .unwrap()
                .applied
        };
        assert!(applied("sea"));
        assert!(!applied("park"));

        db.set_ai_tag_threshold(AI_PLUGIN, "sea", 0.7).unwrap();
        assert!(!applied("sea"));
        db.reset_ai_tag_threshold(AI_PLUGIN, "sea").unwrap();
        assert!(applied("sea"));
    }

    #[test]
    fn photo_ai_tags_with_names_join_the_catalog() {
        let db = Db::open_in_memory().unwrap();
        let photo = seed_photo(&db);
        install_test_catalog(&db);
        db.save_photo_ai(
            photo,
            AI_PLUGIN,
            &test_ai_result(&[("sea", 0.97), ("park", 0.4)]),
        )
        .unwrap();

        let views = db.photo_ai_tags_with_names(photo).unwrap();
        assert_eq!(views.len(), 2);
        let sea = views.iter().find(|view| view.tag_key == "sea").unwrap();
        assert_eq!(sea.name, "海");
        assert_eq!(sea.name_en.as_deref(), Some("Sea"));
        assert!((sea.probability - 0.97).abs() < f32::EPSILON);
        assert!(sea.applied);
        let park = views.iter().find(|view| view.tag_key == "park").unwrap();
        assert_eq!(park.name, "公園");
        assert_eq!(park.name_en, None);
        assert!(!park.applied);
    }

    #[test]
    fn ai_results_cascade_on_photo_delete() {
        let db = Db::open_in_memory().unwrap();
        let photo = seed_photo(&db);
        install_test_catalog(&db);
        db.save_photo_ai(photo, AI_PLUGIN, &test_ai_result(&[("sea", 0.97)]))
            .unwrap();
        assert!(db.photo_ai(photo).unwrap().is_some());
        assert_eq!(db.photo_ai_tags(photo, false).unwrap().len(), 1);

        db.connection()
            .execute("DELETE FROM photos WHERE id = ?1", params![photo])
            .unwrap();
        assert!(db.photo_ai(photo).unwrap().is_none());
        assert!(db.photo_ai_tags(photo, false).unwrap().is_empty());
    }

    #[test]
    fn tagging_jobs_dedupe_active_work_and_respect_waiting() {
        let db = Db::open_in_memory().unwrap();
        let photo = seed_photo(&db);
        assert_eq!(
            db.enqueue_tagging_jobs(&[photo], AI_PLUGIN, TaggingJobStatus::Pending)
                .unwrap(),
            1
        );
        assert_eq!(
            db.enqueue_tagging_jobs(&[photo], AI_PLUGIN, TaggingJobStatus::Pending)
                .unwrap(),
            0
        );
        assert_eq!(db.tagging_job_counts(AI_PLUGIN).unwrap().pending, 1);
        assert!(db.next_pending_tagging_job().unwrap().is_some());
        assert!(db.next_pending_tagging_job().unwrap().is_none());
    }

    #[test]
    fn tagging_enqueue_rejects_terminal_statuses() {
        let db = Db::open_in_memory().unwrap();
        let photo = seed_photo(&db);
        for status in [
            TaggingJobStatus::Processing,
            TaggingJobStatus::Completed,
            TaggingJobStatus::Failed,
            TaggingJobStatus::Cancelled,
        ] {
            assert!(matches!(
                db.enqueue_tagging_jobs(&[photo], AI_PLUGIN, status),
                Err(CoreError::Invalid(_))
            ));
        }
    }

    #[test]
    fn next_pending_tagging_job_claims_with_the_active_variant() {
        let db = Db::open_in_memory().unwrap();
        let photo = seed_photo(&db);
        db.enqueue_tagging_jobs(&[photo], AI_PLUGIN, TaggingJobStatus::Pending)
            .unwrap();
        let job = db.next_pending_tagging_job().unwrap().unwrap();
        assert_eq!(job.status, TaggingJobStatus::Processing);
        assert_eq!(job.variant, None);

        db.upsert_plugin_installation(&ai_install(AI_PLUGIN, "linux-vulkan", "1.0.0", true))
            .unwrap();
        let photo = seed_other_photo(&db);
        db.enqueue_tagging_jobs(&[photo], AI_PLUGIN, TaggingJobStatus::Pending)
            .unwrap();
        let job = db.next_pending_tagging_job().unwrap().unwrap();
        assert_eq!(job.variant.as_deref(), Some("linux-vulkan"));
    }

    #[test]
    fn backlog_enqueue_is_idempotent_and_newest_first() {
        let db = Db::open_in_memory().unwrap();
        let photos = seed_photos(&db, 1000);

        assert_eq!(db.enqueue_tagging_backlog(AI_PLUGIN).unwrap(), 1000);
        assert_eq!(db.enqueue_tagging_backlog(AI_PLUGIN).unwrap(), 0);
        assert_eq!(db.tagging_job_counts(AI_PLUGIN).unwrap().waiting, 1000);
        assert!(db.next_pending_tagging_job().unwrap().is_none());

        assert_eq!(db.release_waiting_tagging_jobs(AI_PLUGIN).unwrap(), 1000);
        assert_eq!(db.enqueue_tagging_backlog(AI_PLUGIN).unwrap(), 0);
        let mut claimed = Vec::new();
        for _ in 0..3 {
            claimed.push(db.next_pending_tagging_job().unwrap().unwrap().photo_id);
        }
        assert_eq!(
            claimed,
            photos.iter().rev().take(3).copied().collect::<Vec<_>>(),
            "the newest photos are claimed first"
        );
    }

    #[test]
    fn tagging_claim_and_active_queries_avoid_temp_btrees() {
        let db = Db::open_in_memory().unwrap();

        let claim = query_plan(
            &db,
            "SELECT id FROM tagging_jobs WHERE status = 'pending' \
             ORDER BY created_at, rowid LIMIT 1",
        );
        assert!(
            !claim.iter().any(|line| line.contains("TEMP B-TREE")),
            "claim query sorts the pending backlog: {claim:?}"
        );
        assert!(
            claim
                .iter()
                .any(|line| line.contains("idx_tagging_jobs_claim")),
            "claim query did not use idx_tagging_jobs_claim: {claim:?}"
        );

        let active = query_plan(
            &db,
            "SELECT id FROM tagging_jobs INDEXED BY idx_tagging_jobs_active_list \
             WHERE status IN ('waiting', 'pending', 'processing') \
             ORDER BY created_at DESC, id DESC LIMIT 10",
        );
        assert!(
            !active.iter().any(|line| line.contains("TEMP B-TREE")),
            "active query sorts the whole queue: {active:?}"
        );
        assert!(
            active
                .iter()
                .any(|line| line.contains("idx_tagging_jobs_active_list")),
            "active query did not use idx_tagging_jobs_active_list: {active:?}"
        );

        let counts = query_plan(
            &db,
            "SELECT status, COUNT(*) FROM tagging_jobs WHERE plugin_id = 'vrc-photon-autotag' \
             GROUP BY status",
        );
        assert!(
            counts
                .iter()
                .any(|line| line.contains("idx_tagging_jobs_plugin_status")),
            "status counts did not use idx_tagging_jobs_plugin_status: {counts:?}"
        );
    }

    #[test]
    fn release_waiting_moves_only_waiting_jobs_for_the_plugin() {
        let db = Db::open_in_memory().unwrap();
        let waiting = seed_photo(&db);
        let pending = seed_other_photo(&db);
        let other_plugin = seed_photos(&db, 3)[2];
        db.enqueue_tagging_jobs(&[waiting], AI_PLUGIN, TaggingJobStatus::Waiting)
            .unwrap();
        db.enqueue_tagging_jobs(&[pending], AI_PLUGIN, TaggingJobStatus::Pending)
            .unwrap();
        db.enqueue_tagging_jobs(&[other_plugin], "other-plugin", TaggingJobStatus::Waiting)
            .unwrap();

        assert_eq!(db.release_waiting_tagging_jobs(AI_PLUGIN).unwrap(), 1);
        let counts = db.tagging_job_counts(AI_PLUGIN).unwrap();
        assert_eq!(counts.waiting, 0);
        assert_eq!(counts.pending, 2);
        assert_eq!(db.tagging_job_counts("other-plugin").unwrap().waiting, 1);
    }

    #[test]
    fn ui_active_jobs_prioritize_processing_over_the_ready_queue() {
        let db = Db::open_in_memory().unwrap();
        let photos = seed_photos(&db, 25);
        assert_eq!(
            db.enqueue_tagging_jobs(&photos, AI_PLUGIN, TaggingJobStatus::Pending)
                .unwrap(),
            25
        );

        let mut claimed = Vec::new();
        for _ in 0..3 {
            claimed.push(db.next_pending_tagging_job().unwrap().unwrap());
        }

        let ui = db.active_tagging_jobs_for_ui(3).unwrap();
        assert_eq!(ui.len(), 3, "the three running jobs must fill the card");
        assert!(
            ui.iter()
                .all(|job| job.status == TaggingJobStatus::Processing)
        );
        let mut ui_ids: Vec<_> = ui.iter().map(|job| job.id.clone()).collect();
        let mut claimed_ids: Vec<_> = claimed.iter().map(|job| job.id.clone()).collect();
        ui_ids.sort();
        claimed_ids.sort();
        assert_eq!(ui_ids, claimed_ids);

        // A larger snapshot adds pending work after the running jobs, never
        // before them.
        let wider = db.active_tagging_jobs_for_ui(5).unwrap();
        assert_eq!(wider.len(), 5);
        assert!(
            wider[..3]
                .iter()
                .all(|job| job.status == TaggingJobStatus::Processing)
        );
        assert!(
            wider[3..]
                .iter()
                .all(|job| job.status == TaggingJobStatus::Pending)
        );
    }

    #[test]
    fn retry_caps_at_two() {
        let db = Db::open_in_memory().unwrap();
        let photo = seed_photo(&db);
        let job = claim_tagging_job(&db, photo);
        db.fail_tagging_job(&job.id, TaggingErrorCode::Internal)
            .unwrap();

        for expected_retry in 1..=MAX_TAGGING_RETRIES {
            assert!(db.retry_tagging_job(&job.id).unwrap());
            let retried = db.tagging_job(&job.id).unwrap().unwrap();
            assert_eq!(retried.status, TaggingJobStatus::Pending);
            assert_eq!(retried.retry_count, expected_retry);
            assert_eq!(retried.progress, 0.0);
            assert_eq!(retried.stage, None);
            assert_eq!(retried.started_at, None);
            assert_eq!(retried.error, None);

            let claimed = db.next_pending_tagging_job().unwrap().unwrap();
            assert_eq!(claimed.id, job.id);
            db.fail_tagging_job(&job.id, TaggingErrorCode::Internal)
                .unwrap();
        }

        assert!(!db.retry_tagging_job(&job.id).unwrap());
        let failed = db.tagging_job(&job.id).unwrap().unwrap();
        assert_eq!(failed.status, TaggingJobStatus::Failed);
        assert_eq!(failed.retry_count, MAX_TAGGING_RETRIES);
    }

    #[test]
    fn retry_failed_job_refuses_when_a_newer_active_job_exists() {
        let db = Db::open_in_memory().unwrap();
        let photo = seed_photo(&db);
        let job = claim_tagging_job(&db, photo);
        db.fail_tagging_job(&job.id, TaggingErrorCode::Internal)
            .unwrap();
        assert_eq!(
            db.enqueue_tagging_jobs(&[photo], AI_PLUGIN, TaggingJobStatus::Pending)
                .unwrap(),
            1
        );

        assert!(!db.retry_tagging_job(&job.id).unwrap());
        let failed = db.tagging_job(&job.id).unwrap().unwrap();
        assert_eq!(failed.status, TaggingJobStatus::Failed);
        assert_eq!(failed.retry_count, 0);
        assert_eq!(db.tagging_job_counts(AI_PLUGIN).unwrap().pending, 1);
    }

    #[test]
    fn defer_tagging_job_waits_without_counting_a_retry() {
        let db = Db::open_in_memory().unwrap();
        let photo = seed_photo(&db);
        let job = claim_tagging_job(&db, photo);
        db.update_tagging_progress(&job.id, 0.3, TaggingStage::PeopleCount)
            .unwrap();
        db.defer_tagging_job(&job.id, TaggingErrorCode::PluginNotReady)
            .unwrap();

        let deferred = db.tagging_job(&job.id).unwrap().unwrap();
        assert_eq!(deferred.status, TaggingJobStatus::Pending);
        assert_eq!(deferred.error.as_deref(), Some("plugin_not_ready"));
        assert_eq!(deferred.retry_count, 0);
        assert_eq!(deferred.progress, 0.0);
        assert_eq!(deferred.stage, None);
        assert_eq!(deferred.started_at, None);

        let claimed = db.next_pending_tagging_job().unwrap().unwrap();
        assert_eq!(claimed.status, TaggingJobStatus::Processing);
        assert_eq!(claimed.error, None);
    }

    #[test]
    fn cancel_processing_blocks_late_write() {
        let db = Db::open_in_memory().unwrap();
        let photo = seed_photo(&db);
        install_test_catalog(&db);
        let job = claim_tagging_job(&db, photo);
        db.update_tagging_progress(&job.id, 0.7, TaggingStage::PlaceTags)
            .unwrap();
        assert!(db.cancel_tagging_job(&job.id).unwrap());
        let cancelled = db.tagging_job(&job.id).unwrap().unwrap();
        assert_eq!(cancelled.status, TaggingJobStatus::Cancelled);
        assert_eq!(cancelled.progress, 0.0);
        assert_eq!(cancelled.stage, None);
        assert!(!db.cancel_tagging_job(&job.id).unwrap());

        assert!(
            !db.save_photo_ai_for_job(&job.id, &test_ai_result(&[("sea", 0.97)]))
                .unwrap()
        );
        assert!(db.photo_ai(photo).unwrap().is_none());

        db.complete_tagging_job(&job.id).unwrap();
        db.fail_tagging_job(&job.id, TaggingErrorCode::Internal)
            .unwrap();
        assert_eq!(
            db.tagging_job(&job.id).unwrap().unwrap().status,
            TaggingJobStatus::Cancelled
        );
    }

    #[test]
    fn save_photo_ai_for_job_requires_processing_status() {
        let db = Db::open_in_memory().unwrap();
        let photo = seed_photo(&db);
        install_test_catalog(&db);
        db.enqueue_tagging_jobs(&[photo], AI_PLUGIN, TaggingJobStatus::Pending)
            .unwrap();
        let job = db.tagging_jobs_for_photo(photo).unwrap()[0].id.clone();
        let result = test_ai_result(&[("sea", 0.97)]);
        let before = db.library_revision().unwrap();

        assert!(!db.save_photo_ai_for_job(&job, &result).unwrap());
        assert!(db.photo_ai(photo).unwrap().is_none());
        assert!(!db.save_photo_ai_for_job("missing-job", &result).unwrap());
        assert_eq!(db.library_revision().unwrap(), before);

        db.next_pending_tagging_job().unwrap().unwrap();
        assert!(db.save_photo_ai_for_job(&job, &result).unwrap());
        assert_eq!(db.photo_ai(photo).unwrap().unwrap().plugin_id, AI_PLUGIN);
        assert!(db.library_revision().unwrap() > before);
        db.complete_tagging_job(&job).unwrap();
        assert_eq!(
            db.tagging_job(&job).unwrap().unwrap().status,
            TaggingJobStatus::Completed
        );

        let missing_photo = seed_other_photo(&db);
        let missing_job = claim_tagging_job(&db, missing_photo);
        db.mark_missing(missing_photo).unwrap();
        assert!(!db.save_photo_ai_for_job(&missing_job.id, &result).unwrap());
        assert!(db.photo_ai(missing_photo).unwrap().is_none());
    }

    #[test]
    fn resume_processing_as_pending() {
        let db = Db::open_in_memory().unwrap();
        let first = seed_photo(&db);
        let second = seed_other_photo(&db);
        db.enqueue_tagging_jobs(&[first], AI_PLUGIN, TaggingJobStatus::Pending)
            .unwrap();
        db.enqueue_tagging_jobs(&[second], AI_PLUGIN, TaggingJobStatus::Pending)
            .unwrap();
        let claimed = db.next_pending_tagging_job().unwrap().unwrap();
        assert_eq!(claimed.photo_id, first);

        assert_eq!(db.resume_interrupted_tagging_jobs().unwrap(), 1);
        let resumed = db.tagging_job(&claimed.id).unwrap().unwrap();
        assert_eq!(resumed.status, TaggingJobStatus::Pending);
        assert_eq!(resumed.retry_count, 0);
        assert_eq!(resumed.started_at, None);
        assert_eq!(db.tagging_job_counts(AI_PLUGIN).unwrap().processing, 0);
        assert_eq!(db.tagging_job_counts(AI_PLUGIN).unwrap().pending, 2);

        let reclaimed = db.next_pending_tagging_job().unwrap().unwrap();
        assert_eq!(reclaimed.id, claimed.id);
        db.complete_tagging_job(&claimed.id).unwrap();
        assert_eq!(db.resume_interrupted_tagging_jobs().unwrap(), 0);
        assert_eq!(
            db.tagging_job(&claimed.id).unwrap().unwrap().status,
            TaggingJobStatus::Completed
        );
    }

    #[test]
    fn cancel_tagging_jobs_for_photo_only_touches_active_jobs() {
        let db = Db::open_in_memory().unwrap();
        let photo = seed_photo(&db);
        let completed = seed_other_photo(&db);
        let job = claim_tagging_job(&db, completed);
        db.complete_tagging_job(&job.id).unwrap();
        db.enqueue_tagging_jobs(&[photo], AI_PLUGIN, TaggingJobStatus::Pending)
            .unwrap();
        db.enqueue_tagging_jobs(&[photo], "other-plugin", TaggingJobStatus::Waiting)
            .unwrap();

        assert_eq!(db.cancel_tagging_jobs_for_photo(photo).unwrap(), 2);
        for job in db.tagging_jobs_for_photo(photo).unwrap() {
            assert_eq!(job.status, TaggingJobStatus::Cancelled);
        }
        assert_eq!(
            db.tagging_job(&job.id).unwrap().unwrap().status,
            TaggingJobStatus::Completed
        );
    }

    #[test]
    fn cancel_tagging_for_forced_deletions_only_touches_forced_pending_requests() {
        let db = Db::open_in_memory().unwrap();
        let forced = seed_photo(&db);
        let plain = seed_other_photo(&db);
        db.enqueue_tagging_jobs(&[forced], AI_PLUGIN, TaggingJobStatus::Pending)
            .unwrap();
        db.enqueue_tagging_jobs(&[plain], AI_PLUGIN, TaggingJobStatus::Pending)
            .unwrap();
        db.request_deletion(&[forced], DeletionReason::Manual, true)
            .unwrap();
        db.request_deletion(&[plain], DeletionReason::Manual, false)
            .unwrap();

        assert_eq!(db.cancel_tagging_for_forced_deletions().unwrap(), 1);
        assert_eq!(
            db.tagging_jobs_for_photo(forced).unwrap()[0].status,
            TaggingJobStatus::Cancelled
        );
        assert_eq!(
            db.tagging_jobs_for_photo(plain).unwrap()[0].status,
            TaggingJobStatus::Pending
        );
    }

    #[test]
    fn active_tagging_jobs_exclude_terminal_jobs() {
        let db = Db::open_in_memory().unwrap();
        let photos = seed_photos(&db, 3);
        for photo in photos {
            db.enqueue_tagging_jobs(&[photo], AI_PLUGIN, TaggingJobStatus::Pending)
                .unwrap();
        }
        assert_eq!(db.active_tagging_jobs(10).unwrap().len(), 3);
        assert_eq!(db.active_tagging_jobs(2).unwrap().len(), 2);

        let claimed = db.next_pending_tagging_job().unwrap().unwrap();
        db.complete_tagging_job(&claimed.id).unwrap();
        assert_eq!(db.active_tagging_jobs(10).unwrap().len(), 2);
    }

    #[test]
    fn delete_ai_data_removes_only_the_plugin_rows() {
        let db = Db::open_in_memory().unwrap();
        let photo = seed_photo(&db);
        install_test_catalog(&db);
        db.save_photo_ai(photo, AI_PLUGIN, &test_ai_result(&[("sea", 0.97)]))
            .unwrap();
        db.set_ai_tag_threshold(AI_PLUGIN, "sea", 0.8).unwrap();
        seed_ai_tag(&db, photo, "other-plugin", "sea", 0.9, true);

        db.delete_ai_data(AI_PLUGIN).unwrap();

        assert!(db.photo_ai(photo).unwrap().is_none());
        assert!(db.ai_tag_definitions(AI_PLUGIN).unwrap().is_empty());
        assert!(db.ai_tag_thresholds(AI_PLUGIN).unwrap().is_empty());
        let remaining = db.photo_ai_tags(photo, false).unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].plugin_id, "other-plugin");
    }

    #[test]
    fn delete_ai_data_cancels_active_jobs_and_blocks_late_results() {
        let db = Db::open_in_memory().unwrap();
        let processing_photo = seed_photo(&db);
        install_test_catalog(&db);
        let processing = claim_tagging_job(&db, processing_photo);
        let waiting_photo = seed_other_photo(&db);
        db.enqueue_tagging_jobs(&[waiting_photo], AI_PLUGIN, TaggingJobStatus::Waiting)
            .unwrap();
        let other_plugin_photo = seed_photos(&db, 3)[2];
        db.enqueue_tagging_jobs(
            &[other_plugin_photo],
            "other-plugin",
            TaggingJobStatus::Pending,
        )
        .unwrap();

        db.delete_ai_data(AI_PLUGIN).unwrap();

        let job = db.tagging_job(&processing.id).unwrap().unwrap();
        assert_eq!(job.status, TaggingJobStatus::Cancelled);
        assert_eq!(job.progress, 0.0);
        assert_eq!(job.stage, None);
        assert_eq!(
            db.tagging_jobs_for_photo(waiting_photo).unwrap()[0].status,
            TaggingJobStatus::Cancelled
        );
        assert_eq!(
            db.tagging_jobs_for_photo(other_plugin_photo).unwrap()[0].status,
            TaggingJobStatus::Pending
        );

        assert!(
            !db.save_photo_ai_for_job(&processing.id, &test_ai_result(&[("sea", 0.97)]))
                .unwrap()
        );
        assert!(db.photo_ai(processing_photo).unwrap().is_none());
        assert!(
            db.photo_ai_tags(processing_photo, false)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn deletion_summary_includes_tagging() {
        let db = Db::open_in_memory().unwrap();
        let photos = seed_photos(&db, 4);

        db.enqueue_tagging_jobs(&[photos[0]], AI_PLUGIN, TaggingJobStatus::Waiting)
            .unwrap();
        db.enqueue_tagging_jobs(&[photos[2]], AI_PLUGIN, TaggingJobStatus::Pending)
            .unwrap();
        let processing = db.next_pending_tagging_job().unwrap().unwrap();
        assert_eq!(processing.photo_id, photos[2]);
        db.enqueue_tagging_jobs(&[photos[3]], AI_PLUGIN, TaggingJobStatus::Pending)
            .unwrap();
        let failed = db.next_pending_tagging_job().unwrap().unwrap();
        assert_eq!(failed.photo_id, photos[3]);
        db.fail_tagging_job(&failed.id, TaggingErrorCode::Internal)
            .unwrap();
        db.enqueue_tagging_jobs(&[photos[1]], AI_PLUGIN, TaggingJobStatus::Pending)
            .unwrap();

        let summary = db.deletion_job_summary(&photos).unwrap();
        assert_eq!(summary.tagging.waiting, 1);
        assert_eq!(summary.tagging.pending, 1);
        assert_eq!(summary.tagging.processing, 1);
        assert_eq!(summary.tagging.failed, 1);
        assert_eq!(summary.affected_photos, 4);
        assert!(summary.needs_warning());
        assert_eq!(summary.compression.total(), 0);
        assert_eq!(summary.send.total(), 0);
    }

    #[test]
    fn deletion_summary_excludes_terminal_tagging_jobs() {
        let db = Db::open_in_memory().unwrap();
        let photo = seed_photo(&db);
        let job = claim_tagging_job(&db, photo);
        db.cancel_tagging_job(&job.id).unwrap();

        let summary = db.deletion_job_summary(&[photo]).unwrap();
        assert_eq!(summary.tagging.total(), 0);
        assert_eq!(summary.affected_photos, 0);
        assert!(!summary.needs_warning());
    }

    #[test]
    fn deletion_blockers_wait_for_tagging() {
        let db = Db::open_in_memory().unwrap();
        let photo = seed_photo(&db);
        db.enqueue_tagging_jobs(&[photo], AI_PLUGIN, TaggingJobStatus::Pending)
            .unwrap();
        assert_eq!(db.deletion_blockers(photo).unwrap().active, 1);

        let request = db
            .request_deletion(&[photo], DeletionReason::Manual, false)
            .unwrap()[0];
        assert!(db.claim_deletion(request).unwrap().is_none());
        let job = db.next_pending_tagging_job().unwrap().unwrap();
        db.cancel_tagging_job(&job.id).unwrap();
        assert_eq!(db.deletion_blockers(photo).unwrap().active, 0);
        assert!(db.claim_deletion(request).unwrap().is_some());
    }

    #[test]
    fn failed_tagging_blocks_a_non_forced_deletion() {
        let db = Db::open_in_memory().unwrap();
        let photo = seed_photo(&db);
        let job = claim_tagging_job(&db, photo);
        db.fail_tagging_job(&job.id, TaggingErrorCode::Internal)
            .unwrap();
        assert_eq!(db.deletion_blockers(photo).unwrap().failed, 1);

        let request = db
            .request_deletion(&[photo], DeletionReason::Manual, false)
            .unwrap()[0];
        assert!(db.claim_deletion(request).unwrap().is_none());
        assert!(db.block_deletion_if_jobs_failed(request).unwrap());
        let blocked = db.deletion_request_by_id(request).unwrap().unwrap();
        assert_eq!(blocked.blocked_reason.as_deref(), Some("job_failed"));

        assert!(db.force_deletion(request).unwrap());
        assert!(db.claim_deletion(request).unwrap().is_some());
    }

    #[test]
    fn force_delete_cancels_tagging_before_claiming() {
        let db = Db::open_in_memory().unwrap();
        let photo = seed_photo(&db);
        install_test_catalog(&db);
        let job = claim_tagging_job(&db, photo);
        assert_eq!(
            db.tagging_job(&job.id).unwrap().unwrap().status,
            TaggingJobStatus::Processing
        );

        let request = db
            .request_deletion(&[photo], DeletionReason::Manual, false)
            .unwrap()[0];
        assert!(db.claim_deletion(request).unwrap().is_none());
        assert!(db.force_deletion(request).unwrap());
        assert_eq!(
            db.tagging_job(&job.id).unwrap().unwrap().status,
            TaggingJobStatus::Cancelled
        );
        assert!(
            !db.save_photo_ai_for_job(&job.id, &test_ai_result(&[("sea", 0.97)]))
                .unwrap()
        );
        assert!(db.photo_ai(photo).unwrap().is_none());
        assert!(db.claim_deletion(request).unwrap().is_some());
    }

    #[test]
    fn enqueue_missing_world_name_lookups_queues_only_unresolved_referenced_worlds() {
        let db = Db::open_in_memory().unwrap();
        let root = db.add_root("/photos", "reference").unwrap();
        let photo_a = db.upsert_photo(&sample_photo(root, "a.png", 100)).unwrap();
        let photo_b = db.upsert_photo(&sample_photo(root, "b.png", 200)).unwrap();
        let photo_c = db.upsert_photo(&sample_photo(root, "c.png", 300)).unwrap();

        let unresolved = db.upsert_world("wrld_unresolved", None, None).unwrap();
        let known = db
            .upsert_world("wrld_known", Some("Known"), Some("log"))
            .unwrap();
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

    #[test]
    fn next_due_world_name_lookup_respects_schedule_and_not_found() {
        let db = Db::open_in_memory().unwrap();
        let pending = db.upsert_world("wrld_pending", None, None).unwrap();
        let retry = db.upsert_world("wrld_retry", None, None).unwrap();
        let not_found = db.upsert_world("wrld_404", None, None).unwrap();
        db.enqueue_world_name_lookup(pending).unwrap();
        db.enqueue_world_name_lookup(retry).unwrap();
        db.enqueue_world_name_lookup(not_found).unwrap();
        db.record_world_name_failure(retry, WorldNameFailure::Retryable { code: "http_5xx" }, 100)
            .unwrap();
        db.record_world_name_failure(not_found, WorldNameFailure::NotFound, 100)
            .unwrap();

        assert_eq!(
            db.next_due_world_name_lookup(100)
                .unwrap()
                .unwrap()
                .world_db_id,
            pending
        );
        assert_eq!(
            db.next_due_world_name_lookup(159)
                .unwrap()
                .unwrap()
                .world_db_id,
            pending
        );
        db.mark_world_name_lookup_resolved(pending, 159).unwrap();
        assert_eq!(
            db.next_due_world_name_lookup(160)
                .unwrap()
                .unwrap()
                .world_db_id,
            retry
        );
        assert_eq!(
            db.next_due_world_name_lookup(86_499)
                .unwrap()
                .unwrap()
                .world_db_id,
            retry
        );
        db.mark_world_name_lookup_resolved(retry, 86_499).unwrap();
        assert_eq!(
            db.next_due_world_name_lookup(86_500)
                .unwrap()
                .unwrap()
                .world_db_id,
            not_found
        );
    }

    #[test]
    fn failures_schedule_the_expected_backoff() {
        let db = Db::open_in_memory().unwrap();
        let world = db.upsert_world("wrld_test", None, None).unwrap();
        db.enqueue_world_name_lookup(world).unwrap();

        db.record_world_name_failure(
            world,
            WorldNameFailure::RateLimited { retry_after: 120 },
            1_000,
        )
        .unwrap();
        let row = db.world_name_lookup(world).unwrap().unwrap();
        assert_eq!(row.status, WorldNameLookupStatus::Retry);
        assert_eq!(row.attempts, 0);
        assert_eq!(row.next_attempt_at, Some(1_120));

        db.record_world_name_failure(
            world,
            WorldNameFailure::Retryable { code: "http_5xx" },
            2_000,
        )
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

        db.record_world_name_failure(
            world,
            WorldNameFailure::Invalid {
                code: "invalid_response",
            },
            4_000,
        )
        .unwrap();
        let row = db.world_name_lookup(world).unwrap().unwrap();
        assert_eq!(row.status, WorldNameLookupStatus::Failed);
        assert_eq!(row.next_attempt_at, None);
    }

    #[test]
    fn short_retry_after_never_schedules_sooner_than_the_global_interval() {
        let db = Db::open_in_memory().unwrap();
        let world = db.upsert_world("wrld_test", None, None).unwrap();
        db.enqueue_world_name_lookup(world).unwrap();

        db.record_world_name_failure(
            world,
            WorldNameFailure::RateLimited { retry_after: 30 },
            1_000,
        )
        .unwrap();
        let row = db.world_name_lookup(world).unwrap().unwrap();
        assert_eq!(row.status, WorldNameLookupStatus::Retry);
        assert_eq!(row.attempts, 0);
        assert_eq!(row.next_attempt_at, Some(1_060));
    }

    #[test]
    fn satisfied_and_resolved_lookups_clear_the_last_error() {
        let db = Db::open_in_memory().unwrap();
        let satisfied = db.upsert_world("wrld_satisfied", None, None).unwrap();
        let resolved = db.upsert_world("wrld_resolved", None, None).unwrap();
        db.enqueue_world_name_lookup(satisfied).unwrap();
        db.enqueue_world_name_lookup(resolved).unwrap();
        db.record_world_name_failure(
            satisfied,
            WorldNameFailure::Retryable { code: "http_5xx" },
            1_000,
        )
        .unwrap();
        db.record_world_name_failure(
            resolved,
            WorldNameFailure::Retryable { code: "network" },
            1_000,
        )
        .unwrap();

        db.mark_world_name_lookup_satisfied(satisfied).unwrap();
        db.mark_world_name_lookup_resolved(resolved, 1_060).unwrap();

        for world in [satisfied, resolved] {
            let row = db.world_name_lookup(world).unwrap().unwrap();
            assert_eq!(row.status, WorldNameLookupStatus::Resolved);
            assert_eq!(row.next_attempt_at, None);
            let last_error: Option<String> = db
                .conn
                .query_row(
                    "SELECT last_error FROM world_name_lookups WHERE world_db_id = ?1",
                    params![world],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(last_error, None);
        }
    }

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
        db.record_world_name_failure(
            failed,
            WorldNameFailure::Invalid {
                code: "invalid_response",
            },
            0,
        )
        .unwrap();

        let counts = db.world_name_lookup_counts().unwrap();
        assert_eq!(counts.pending, 2);
        assert_eq!(counts.resolved, 1);
        assert_eq!(counts.failed, 1);
    }

    #[test]
    fn upsert_world_joins_an_existing_transaction() {
        let db = Db::open_in_memory().unwrap();
        db.in_transaction(|db| {
            let world = db.upsert_world("wrld_tx", Some("Tx World"), Some("log"))?;
            assert_eq!(
                db.world_by_id(world)?.unwrap().name.as_deref(),
                Some("Tx World")
            );
            Ok(())
        })
        .unwrap();

        let world = db.world_by_world_id("wrld_tx").unwrap().unwrap();
        assert_eq!(world.name.as_deref(), Some("Tx World"));
    }

    #[test]
    fn backfill_photo_context_world_names_fills_nulls_with_alias_then_name() {
        let db = Db::open_in_memory().unwrap();
        let root = db.add_root("/photos", "reference").unwrap();
        let photo = db.upsert_photo(&sample_photo(root, "a.png", 100)).unwrap();
        let world = db
            .upsert_world("wrld_test", Some("World Name"), Some("log"))
            .unwrap();
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
            db.photo_context(photo)
                .unwrap()
                .unwrap()
                .world_name
                .as_deref(),
            Some("Alias Name")
        );
        assert!(db.library_revision().unwrap() > before);
        assert_eq!(db.backfill_photo_context_world_names().unwrap(), 0);
    }

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

        db.record_world_name_resolved(world, "API World", 1_000)
            .unwrap();
        let stored = db.world_by_id(world).unwrap().unwrap();
        assert_eq!(stored.name.as_deref(), Some("API World"));
        assert_eq!(stored.name_source.as_deref(), Some("vrchat-api"));
        assert_eq!(
            db.photo_context(null_photo)
                .unwrap()
                .unwrap()
                .world_name
                .as_deref(),
            Some("API World")
        );
        assert_eq!(
            db.photo_context(xmp_photo)
                .unwrap()
                .unwrap()
                .world_name
                .as_deref(),
            Some("XMP World")
        );
        assert_eq!(
            db.world_name_lookup(world).unwrap().unwrap().status,
            WorldNameLookupStatus::Resolved
        );

        db.record_world_name_resolved(world, "Ignored API World", 2_000)
            .unwrap();
        assert_eq!(
            db.world_by_id(world).unwrap().unwrap().name.as_deref(),
            Some("API World")
        );
    }

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
            db.photo_context(photo)
                .unwrap()
                .unwrap()
                .world_name
                .as_deref(),
            Some("Log World")
        );
    }

    #[test]
    fn log_name_overwrites_api_name_and_updates_contexts() {
        let db = Db::open_in_memory().unwrap();
        let root = db.add_root("/photos", "reference").unwrap();
        let photo = db.upsert_photo(&sample_photo(root, "a.png", 100)).unwrap();
        let world = db
            .upsert_world("wrld_test", Some("API World"), Some("vrchat-api"))
            .unwrap();
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

        db.upsert_world("wrld_test", Some("Log World"), Some("log"))
            .unwrap();
        let stored = db.world_by_id(world).unwrap().unwrap();
        assert_eq!(stored.name.as_deref(), Some("Log World"));
        assert_eq!(stored.name_source.as_deref(), Some("log"));
        assert_eq!(
            db.photo_context(photo)
                .unwrap()
                .unwrap()
                .world_name
                .as_deref(),
            Some("Log World")
        );
        assert_eq!(
            db.world_name_lookup(world).unwrap().unwrap().status,
            WorldNameLookupStatus::Resolved
        );

        db.upsert_world("wrld_test", Some("XMP World"), Some("xmp"))
            .unwrap();
        assert_eq!(
            db.world_by_id(world).unwrap().unwrap().name.as_deref(),
            Some("Log World")
        );
    }

    #[test]
    fn enqueue_send_dedupes_active_rows_and_allows_resend_after_terminal() {
        let db = Db::open_in_memory().unwrap();
        let photo = seed_photo(&db);
        let id = db
            .enqueue_send(
                photo,
                SendKind::Photo,
                SendOrigin::Manual,
                SendStatus::Pending,
            )
            .unwrap()
            .unwrap();
        assert!(
            db.enqueue_send(
                photo,
                SendKind::Photo,
                SendOrigin::Auto,
                SendStatus::Pending
            )
            .unwrap()
            .is_none()
        );
        assert!(db.cancel_send(id).unwrap());
        let again = db
            .enqueue_send(
                photo,
                SendKind::Photo,
                SendOrigin::Manual,
                SendStatus::Pending,
            )
            .unwrap();
        assert!(again.is_some());
    }

    #[test]
    fn enqueue_send_rejects_a_deleting_photo() {
        let db = Db::open_in_memory().unwrap();
        let photo = seed_photo(&db);
        db.request_deletion(&[photo], DeletionReason::Manual, false)
            .unwrap();
        assert!(
            db.enqueue_send(
                photo,
                SendKind::Photo,
                SendOrigin::Manual,
                SendStatus::Pending
            )
            .unwrap()
            .is_none()
        );
    }

    #[test]
    fn claim_next_send_respects_status_and_next_attempt() {
        let db = Db::open_in_memory().unwrap();
        let photo = seed_photo(&db);
        let id = db
            .enqueue_send(
                photo,
                SendKind::Photo,
                SendOrigin::Manual,
                SendStatus::Pending,
            )
            .unwrap()
            .unwrap();
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
    fn send_counts_and_active_lookup_follow_statuses() {
        let db = Db::open_in_memory().unwrap();
        let photo = seed_photo(&db);
        let other = seed_other_photo(&db);
        assert!(db.active_send_for_photo(photo).unwrap().is_none());

        let waiting = db
            .enqueue_send(
                photo,
                SendKind::Photo,
                SendOrigin::Auto,
                SendStatus::Waiting,
            )
            .unwrap()
            .unwrap();
        let pending = db
            .enqueue_send(
                other,
                SendKind::Photo,
                SendOrigin::Manual,
                SendStatus::Pending,
            )
            .unwrap()
            .unwrap();

        assert_eq!(db.count_waiting_sends().unwrap(), 1);
        assert_eq!(db.count_pending_sends().unwrap(), 2);
        assert_eq!(
            db.active_send_for_photo(photo).unwrap().unwrap().id,
            waiting
        );

        db.claim_next_send(100).unwrap();
        assert_eq!(db.count_waiting_sends().unwrap(), 1);
        assert_eq!(db.count_pending_sends().unwrap(), 1);
        assert_eq!(
            db.active_send_for_photo(other).unwrap().unwrap().id,
            pending
        );

        db.conn
            .execute(
                "UPDATE send_outbox SET status = 'sent' WHERE id = ?1",
                params![waiting],
            )
            .unwrap();
        assert!(db.active_send_for_photo(photo).unwrap().is_none());
    }

    #[test]
    fn send_status_parse_keeps_unknown_values_distinct() {
        assert_eq!(SendStatus::parse("future_state"), SendStatus::Unknown);
        assert_eq!(SendStatus::Unknown.as_str(), "unknown");
        assert!(!SendStatus::Unknown.is_terminal());
        assert_eq!(SendErrorCode::parse("http_413"), SendErrorCode::Http413);
        assert_eq!(SendErrorCode::parse("future_error"), SendErrorCode::Unknown);
    }

    #[test]
    fn enqueue_send_rejects_statuses_other_than_waiting_or_pending() {
        let db = Db::open_in_memory().unwrap();
        let photo = seed_photo(&db);
        for status in [
            SendStatus::Processing,
            SendStatus::Sent,
            SendStatus::Failed,
            SendStatus::Cancelled,
            SendStatus::Unknown,
        ] {
            assert!(matches!(
                db.enqueue_send(photo, SendKind::Photo, SendOrigin::Manual, status),
                Err(CoreError::Invalid(_))
            ));
        }
        assert_eq!(db.count_pending_sends().unwrap(), 0);
    }

    #[test]
    fn claim_next_send_claims_oldest_due_row_first() {
        let db = Db::open_in_memory().unwrap();
        let first = seed_photo(&db);
        let second = seed_other_photo(&db);
        let first_id = db
            .enqueue_send(
                first,
                SendKind::Photo,
                SendOrigin::Manual,
                SendStatus::Pending,
            )
            .unwrap()
            .unwrap();
        let second_id = db
            .enqueue_send(
                second,
                SendKind::Photo,
                SendOrigin::Manual,
                SendStatus::Pending,
            )
            .unwrap()
            .unwrap();
        assert_eq!(db.claim_next_send(100).unwrap().unwrap().id, first_id);
        assert_eq!(db.claim_next_send(100).unwrap().unwrap().id, second_id);
        assert!(db.claim_next_send(100).unwrap().is_none());
    }

    #[test]
    fn complete_send_stores_message_identity_and_source() {
        let db = Db::open_in_memory().unwrap();
        let photo = seed_photo(&db);
        let id = db
            .enqueue_send(
                photo,
                SendKind::Photo,
                SendOrigin::Manual,
                SendStatus::Pending,
            )
            .unwrap()
            .unwrap();
        db.claim_next_send(100).unwrap();
        db.complete_send(
            id,
            &SendCompletion {
                source: Some(SendSource::Original),
                profile_id: None,
                destination: "My server".into(),
                message_id: Some("42".into()),
                channel_id: Some("7".into()),
                guild_id: Some("9".into()),
            },
        )
        .unwrap();
        let entry = db.send_entry_by_id(id).unwrap().unwrap();
        assert_eq!(entry.status, SendStatus::Sent);
        assert_eq!(entry.discord_message_id.as_deref(), Some("42"));
        assert_eq!(entry.discord_channel_id.as_deref(), Some("7"));
        assert_eq!(entry.discord_guild_id.as_deref(), Some("9"));
        assert_eq!(entry.source, Some(SendSource::Original));
        assert_eq!(entry.destination.as_deref(), Some("My server"));
        assert!(entry.finished_at.is_some());
    }

    #[test]
    fn complete_send_leaves_unclaimed_rows_alone() {
        let db = Db::open_in_memory().unwrap();
        let photo = seed_photo(&db);
        let id = db
            .enqueue_send(
                photo,
                SendKind::Photo,
                SendOrigin::Manual,
                SendStatus::Pending,
            )
            .unwrap()
            .unwrap();
        db.complete_send(
            id,
            &SendCompletion {
                source: None,
                profile_id: None,
                destination: "D".into(),
                message_id: None,
                channel_id: None,
                guild_id: None,
            },
        )
        .unwrap();
        assert_eq!(
            db.send_entry_by_id(id).unwrap().unwrap().status,
            SendStatus::Pending
        );
    }

    #[test]
    fn fail_send_stores_error_and_identity() {
        let db = Db::open_in_memory().unwrap();
        let photo = seed_photo(&db);
        let id = db
            .enqueue_send(
                photo,
                SendKind::Photo,
                SendOrigin::Manual,
                SendStatus::Pending,
            )
            .unwrap()
            .unwrap();
        db.fail_send(
            id,
            SendErrorCode::Http401,
            Some("unauthorized"),
            Some(SendSource::Profile),
            Some("profile-a"),
            Some("My server"),
        )
        .unwrap();
        let entry = db.send_entry_by_id(id).unwrap().unwrap();
        assert_eq!(entry.status, SendStatus::Failed);
        assert_eq!(entry.error_code.as_deref(), Some("http_401"));
        assert_eq!(entry.error_detail.as_deref(), Some("unauthorized"));
        assert_eq!(entry.source, Some(SendSource::Profile));
        assert_eq!(entry.profile_id.as_deref(), Some("profile-a"));
        assert_eq!(entry.destination.as_deref(), Some("My server"));
        assert!(entry.finished_at.is_some());
    }

    #[test]
    fn reschedule_send_delays_and_records_the_last_error() {
        let db = Db::open_in_memory().unwrap();
        let photo = seed_photo(&db);
        let id = db
            .enqueue_send(
                photo,
                SendKind::Photo,
                SendOrigin::Manual,
                SendStatus::Pending,
            )
            .unwrap()
            .unwrap();
        db.claim_next_send(100).unwrap();
        db.reschedule_send(id, 2, 300, SendErrorCode::Network, Some("timeout"))
            .unwrap();
        let entry = db.send_entry_by_id(id).unwrap().unwrap();
        assert_eq!(entry.status, SendStatus::Pending);
        assert_eq!(entry.attempts, 2);
        assert_eq!(entry.next_attempt_at, Some(300));
        assert_eq!(entry.error_code.as_deref(), Some("network"));
        assert_eq!(entry.error_detail.as_deref(), Some("timeout"));
        assert_eq!(entry.started_at, None);
        assert!(db.claim_next_send(299).unwrap().is_none());
        assert_eq!(db.claim_next_send(300).unwrap().unwrap().id, id);
    }

    #[test]
    fn release_send_returns_a_claimed_row_to_the_queue() {
        let db = Db::open_in_memory().unwrap();
        let photo = seed_photo(&db);
        let id = db
            .enqueue_send(
                photo,
                SendKind::Photo,
                SendOrigin::Manual,
                SendStatus::Pending,
            )
            .unwrap()
            .unwrap();
        db.claim_next_send(100).unwrap();
        db.release_send(id).unwrap();
        let entry = db.send_entry_by_id(id).unwrap().unwrap();
        assert_eq!(entry.status, SendStatus::Pending);
        assert_eq!(entry.started_at, None);
        assert_eq!(db.claim_next_send(200).unwrap().unwrap().id, id);
    }

    #[test]
    fn retry_send_resets_attempts_and_errors_only_for_failed() {
        let db = Db::open_in_memory().unwrap();
        let photo = seed_photo(&db);
        let id = db
            .enqueue_send(
                photo,
                SendKind::Photo,
                SendOrigin::Manual,
                SendStatus::Pending,
            )
            .unwrap()
            .unwrap();
        db.claim_next_send(100).unwrap();
        db.reschedule_send(id, 3, 200, SendErrorCode::Network, Some("timeout"))
            .unwrap();
        db.claim_next_send(200).unwrap();
        db.fail_send(
            id,
            SendErrorCode::Http401,
            Some("unauthorized"),
            None,
            None,
            None,
        )
        .unwrap();
        assert!(db.retry_send(id).unwrap());
        let entry = db.send_entry_by_id(id).unwrap().unwrap();
        assert_eq!(entry.status, SendStatus::Pending);
        assert_eq!(entry.attempts, 0);
        assert_eq!(entry.next_attempt_at, None);
        assert_eq!(entry.error_code, None);
        assert_eq!(entry.error_detail, None);
        assert_eq!(entry.started_at, None);
        assert_eq!(entry.finished_at, None);
        assert!(!db.retry_send(id).unwrap());
    }

    #[test]
    fn cancel_send_refuses_processing_rows() {
        let db = Db::open_in_memory().unwrap();
        let photo = seed_photo(&db);
        let id = db
            .enqueue_send(
                photo,
                SendKind::Photo,
                SendOrigin::Manual,
                SendStatus::Pending,
            )
            .unwrap()
            .unwrap();
        db.claim_next_send(100).unwrap();
        assert!(!db.cancel_send(id).unwrap());
        assert_eq!(
            db.send_entry_by_id(id).unwrap().unwrap().status,
            SendStatus::Processing
        );
    }

    #[test]
    fn cancel_send_cancels_waiting_pending_and_failed_rows() {
        let db = Db::open_in_memory().unwrap();
        let photo = seed_photo(&db);
        let waiting = db
            .enqueue_send(
                photo,
                SendKind::Photo,
                SendOrigin::Auto,
                SendStatus::Waiting,
            )
            .unwrap()
            .unwrap();
        let pending = db.enqueue_test_send().unwrap();
        let failed = db.enqueue_test_send().unwrap();
        db.fail_send(failed, SendErrorCode::Unknown, None, None, None, None)
            .unwrap();

        for id in [waiting, pending, failed] {
            assert!(db.cancel_send(id).unwrap());
            let entry = db.send_entry_by_id(id).unwrap().unwrap();
            assert_eq!(entry.status, SendStatus::Cancelled);
            assert!(entry.finished_at.is_some());
        }
        assert!(!db.cancel_send(waiting).unwrap());
    }

    #[test]
    fn retry_send_refuses_when_another_active_row_exists() {
        let db = Db::open_in_memory().unwrap();
        let photo = seed_photo(&db);
        let failed = db
            .enqueue_send(
                photo,
                SendKind::Photo,
                SendOrigin::Manual,
                SendStatus::Pending,
            )
            .unwrap()
            .unwrap();
        db.claim_next_send(100).unwrap();
        db.fail_send(failed, SendErrorCode::Http401, None, None, None, None)
            .unwrap();
        let active = db
            .enqueue_send(
                photo,
                SendKind::Photo,
                SendOrigin::Manual,
                SendStatus::Pending,
            )
            .unwrap()
            .unwrap();
        assert!(!db.retry_send(failed).unwrap());
        assert_eq!(
            db.send_entry_by_id(failed).unwrap().unwrap().status,
            SendStatus::Failed
        );
        assert_eq!(
            db.send_entry_by_id(active).unwrap().unwrap().status,
            SendStatus::Pending
        );
    }

    #[test]
    fn reset_processing_sends_marks_interrupted_without_resending() {
        let db = Db::open_in_memory().unwrap();
        let photo = seed_photo(&db);
        let id = db
            .enqueue_send(
                photo,
                SendKind::Photo,
                SendOrigin::Manual,
                SendStatus::Pending,
            )
            .unwrap()
            .unwrap();
        db.claim_next_send(100).unwrap();
        assert_eq!(db.reset_processing_sends().unwrap(), 1);
        let entry = db.send_entry_by_id(id).unwrap().unwrap();
        assert_eq!(entry.status, SendStatus::Failed);
        assert_eq!(entry.error_code.as_deref(), Some("interrupted"));
        assert_eq!(entry.error_detail, None);
        assert!(entry.finished_at.is_some());
        assert!(db.claim_next_send(200).unwrap().is_none());
    }

    #[test]
    fn release_waiting_and_cancel_waiting_auto_sends() {
        let db = Db::open_in_memory().unwrap();
        let photo = seed_photo(&db);
        let waiting = db
            .enqueue_send(
                photo,
                SendKind::Photo,
                SendOrigin::Auto,
                SendStatus::Waiting,
            )
            .unwrap()
            .unwrap();
        assert_eq!(db.release_waiting_sends().unwrap(), 1);
        assert_eq!(
            db.send_entry_by_id(waiting).unwrap().unwrap().status,
            SendStatus::Pending
        );
        let photo2 = seed_other_photo(&db);
        db.enqueue_send(
            photo2,
            SendKind::Photo,
            SendOrigin::Auto,
            SendStatus::Waiting,
        )
        .unwrap();
        assert_eq!(db.cancel_waiting_auto_sends().unwrap(), 1);
        assert_eq!(
            db.send_entry_by_id(waiting).unwrap().unwrap().status,
            SendStatus::Pending
        );
    }

    #[test]
    fn send_history_joins_photo_and_world_and_prunes_to_cap() {
        let db = Db::open_in_memory().unwrap();
        let photo = seed_photo(&db);
        db.set_photo_context(&PhotoContext {
            photo_id: photo,
            session_id: None,
            world_id: None,
            world_name: Some("Test World".to_string()),
            instance_id: None,
            link_method: "manual".to_string(),
            confidence: 1.0,
            author: None,
            linked_at: now_string(),
        })
        .unwrap();
        for _ in 0..3 {
            let id = db
                .enqueue_send(
                    photo,
                    SendKind::Photo,
                    SendOrigin::Manual,
                    SendStatus::Pending,
                )
                .unwrap()
                .unwrap();
            db.claim_next_send(100).unwrap();
            db.complete_send(
                id,
                &SendCompletion {
                    source: Some(SendSource::Original),
                    profile_id: None,
                    destination: "D".into(),
                    message_id: Some(id.to_string()),
                    channel_id: None,
                    guild_id: None,
                },
            )
            .unwrap();
        }
        let history = db.send_history(10).unwrap();
        assert_eq!(history.len(), 3);
        assert_eq!(history[0].entry.discord_message_id.as_deref(), Some("3"));
        assert_eq!(history[0].photo_name.as_deref(), Some("a.png"));
        assert_eq!(history[0].world_name.as_deref(), Some("Test World"));
        assert_eq!(db.send_history(1).unwrap().len(), 1);
        assert_eq!(db.prune_send_history(2).unwrap(), 1);
        assert_eq!(db.send_history(10).unwrap().len(), 2);
        assert_eq!(db.clear_send_history().unwrap(), 2);
        assert!(db.send_history(10).unwrap().is_empty());
    }

    #[test]
    fn prune_and_clear_send_history_keep_active_rows() {
        let db = Db::open_in_memory().unwrap();
        let photo = seed_photo(&db);
        let waiting = db
            .enqueue_send(
                photo,
                SendKind::Photo,
                SendOrigin::Auto,
                SendStatus::Waiting,
            )
            .unwrap()
            .unwrap();
        for _ in 0..3 {
            let id = db.enqueue_test_send().unwrap();
            db.claim_next_send(100).unwrap();
            db.complete_send(
                id,
                &SendCompletion {
                    source: None,
                    profile_id: None,
                    destination: "D".into(),
                    message_id: None,
                    channel_id: None,
                    guild_id: None,
                },
            )
            .unwrap();
        }
        assert_eq!(db.prune_send_history(1).unwrap(), 2);
        assert_eq!(db.send_history(10).unwrap().len(), 2);
        assert_eq!(
            db.send_entry_by_id(waiting).unwrap().unwrap().status,
            SendStatus::Waiting
        );
        assert_eq!(db.clear_send_history().unwrap(), 1);
        assert!(db.send_entry_by_id(waiting).unwrap().is_some());
        assert_eq!(db.send_history(10).unwrap().len(), 1);
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
    fn migration_v9_adds_the_job_photo_profile_index() {
        let db = Db::open_in_memory().unwrap();
        let index_sql: String = db
            .conn
            .query_row(
                "SELECT sql FROM sqlite_master \
                 WHERE type = 'index' AND name = 'idx_jobs_photo_profile'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let normalized = index_sql
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase();
        assert!(
            normalized.contains("on compression_jobs(photo_id, profile_id)"),
            "{index_sql}"
        );

        // A crash after the DDL but before user_version must be safe to retry.
        db.conn.pragma_update(None, "user_version", 8).unwrap();
        db.migrate_v9().unwrap();
        let version: i64 = db
            .conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, 9);
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
        assert_eq!(
            db.list_profiles()
                .unwrap()
                .iter()
                .filter(|p| p.discord)
                .count(),
            1
        );
    }

    #[test]
    fn the_default_discord_preset_is_selected() {
        let db = Db::open_in_memory().unwrap();
        let profiles = db.ensure_default_profiles().unwrap();
        let discord = profiles.iter().find(|p| p.name == "discord用webp").unwrap();
        assert!(discord.discord);
        assert_eq!(profiles.iter().filter(|p| p.discord).count(), 1);
    }

    #[test]
    fn delete_setting_removes_only_the_requested_key() {
        let db = Db::open_in_memory().unwrap();
        db.set_setting("keep", "value").unwrap();
        db.set_setting("remove", "value").unwrap();
        db.delete_setting("remove").unwrap();
        assert_eq!(db.get_setting("remove").unwrap(), None);
        assert_eq!(db.get_setting("keep").unwrap().as_deref(), Some("value"));
        db.delete_setting("missing").unwrap();
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
    fn upsert_preserves_a_photo_status_written_by_a_newer_version() {
        let db = Db::open_in_memory().unwrap();
        let root = db.add_root("/photos", "reference").unwrap();
        let photo = db
            .upsert_photo(&sample_photo(root, "/photos/a.png", 100))
            .unwrap();
        db.connection()
            .execute(
                "UPDATE photos SET status = 'future_status' WHERE id = ?1",
                params![photo],
            )
            .unwrap();

        db.upsert_photo(&sample_photo(root, "/photos/a.png", 100))
            .unwrap();

        let status: String = db
            .connection()
            .query_row(
                "SELECT status FROM photos WHERE id = ?1",
                params![photo],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            status, "future_status",
            "an older scan must not rewrite a future status to present"
        );
        assert_eq!(
            db.photo_by_id(photo).unwrap().unwrap().status,
            PhotoStatus::Unknown
        );
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

    #[test]
    fn photo_query_includes_the_vrchat_world_id() {
        let db = Db::open_in_memory().unwrap();
        let root = db.add_root("/photos", "reference").unwrap();
        let photo = db.upsert_photo(&sample_photo(root, "a.png", 100)).unwrap();
        let world = db
            .upsert_world("wrld_test", Some("Test World"), Some("log"))
            .unwrap();
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
}
