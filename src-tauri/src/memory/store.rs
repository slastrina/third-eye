//! [`MemoryStore`]: CRUD + FTS5 keyword search over one local SQLite file.
//!
//! All methods are synchronous and issue short bounded queries — sync IPC
//! commands (T04) run on Tauri's command thread pool, and the async search
//! path (T02) calls them inline. The connection sits behind a `Mutex`, so
//! the store is `Send + Sync` and shareable via `tauri::State`.
//!
//! Structural invariants (pinned by tests):
//! - text/metadata columns only — declared types are INTEGER or TEXT, never
//!   a byte-array type, so no frame data can be stored here (R011/R012)
//! - `embedding` is JSON text of an f32 array, internal-only: it never
//!   appears in the serialized [`MemoryRecord`] IPC shape

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;

use super::MemoryError;

/// Provenance of a stored memory — a closed lowercase vocabulary ("watcher"
/// / "chat"). S03's source labels build on these exact stored strings, so
/// the set only grows deliberately. Unknown stored values degrade to
/// [`MemorySource::Watcher`] with a warning rather than failing the row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum MemorySource {
    Watcher,
    Chat,
    /// Explicitly stored on the user's request (the `remember` tool,
    /// 2026-07-31) — "remember that my name is Alex". The strongest
    /// provenance: the user chose these words.
    Told,
}

impl MemorySource {
    /// The exact string stored in the `source` column.
    pub fn as_str(self) -> &'static str {
        match self {
            MemorySource::Watcher => "watcher",
            MemorySource::Chat => "chat",
            MemorySource::Told => "told",
        }
    }

    /// Lenient parse of a stored value: unknown strings degrade to
    /// `Watcher` with a warning so one odd row never breaks a read path.
    pub fn from_str_lenient(value: &str) -> Self {
        match value {
            "watcher" => MemorySource::Watcher,
            "chat" => MemorySource::Chat,
            "told" => MemorySource::Told,
            other => {
                log::warn!("memory: unknown source {other:?}, defaulting to watcher");
                MemorySource::Watcher
            }
        }
    }
}

/// One stored memory as consumed over IPC (S04) and by tool-calling (S03).
/// camelCase on the wire; the embedding is deliberately absent — it is a
/// search-internal detail, not part of the record contract.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryRecord {
    pub id: i64,
    pub summary: String,
    /// App names active during the observed span (JSON array in the db).
    pub apps: Vec<String>,
    pub span_start_ms: i64,
    pub span_end_ms: i64,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    /// Where this memory came from ("watcher" / "chat" / "told" on the wire).
    pub source: MemorySource,
    /// One of [`CATEGORIES`] — the browse facet (memory v2, 2026-08-01).
    pub category: String,
    /// ≤5 lowercase keywords: derived mechanically from the summary at
    /// insert, or supplied explicitly by `remember`.
    pub tags: Vec<String>,
    /// Permanent — pruning never touches a pinned memory.
    pub pinned: bool,
    /// Explicit expiry; `None` follows the global retention window.
    pub expires_at_ms: Option<i64>,
}

/// One cached machine-inventory entry (computer-control I1): a GUI app
/// bundle (`kind: "app"`) or a PATH executable (`kind: "cli"`). Serialized
/// camelCase for the `inventory_search` IPC and the find_programs tool.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InventoryEntry {
    pub name: String,
    pub path: String,
    pub kind: String,
    pub refreshed_at_ms: i64,
}

/// One chat session's list row (computer-control I3). Serialized camelCase
/// for the `chat_sessions` IPC.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatSessionSummary {
    pub id: i64,
    pub started_at_ms: i64,
    pub last_at_ms: i64,
    /// First user line, truncated — derivable, never invented.
    pub title: String,
    pub message_count: usize,
}

/// One transcript line of a stored chat session.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatSessionMessage {
    pub role: String,
    pub text: String,
    pub at_ms: i64,
}

/// One transcript line matching a free-text search, with its session — the
/// `chat_history_search` tool's row (the model quoting what the user
/// actually asked before needs the message, not just the session).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MatchedChatMessage {
    pub session_id: i64,
    pub role: String,
    pub text: String,
    pub at_ms: i64,
}

/// What one inventory scan produces per program found.
#[derive(Debug, Clone, PartialEq)]
pub struct NewInventoryEntry {
    pub name: String,
    pub path: String,
    pub kind: String,
    pub refreshed_at_ms: i64,
}

/// Input for [`MemoryStore::insert`] — everything the distiller (T03)
/// produces for one memory. `embedding` is optional: keyword-only rows are
/// valid and get embedded lazily (T02).
#[derive(Debug, Clone, PartialEq)]
pub struct NewMemory {
    pub summary: String,
    pub apps: Vec<String>,
    pub span_start_ms: i64,
    pub span_end_ms: i64,
    pub embedding: Option<Vec<f32>>,
    pub source: MemorySource,
    /// Normalized against [`CATEGORIES`] at insert (unknown → "other").
    pub category: String,
    /// Empty means "derive from the summary" ([`derive_tags`]).
    pub tags: Vec<String>,
    pub pinned: bool,
    pub expires_at_ms: Option<i64>,
}

/// The fixed category taxonomy (memory v2): a CLOSED set so browse chips
/// stay renderable and search predictable. The distiller and `remember`
/// both normalize into it; anything else lands in "other".
pub const CATEGORIES: &[&str] = &[
    "development",
    "browsing",
    "communication",
    "writing",
    "media",
    "shopping",
    "reference",
    "personal",
    "system",
    "other",
];

/// Lenient category normalization: trims/lowercases; unknown → "other".
pub fn normalize_category(raw: &str) -> String {
    let lower = raw.trim().to_lowercase();
    if CATEGORIES.contains(&lower.as_str()) {
        lower
    } else {
        "other".into()
    }
}

/// Words too common to be tags — shared with the knowledge graph's edge
/// scoring (the graph imports THIS list).
pub const TAG_STOPWORDS: &[&str] = &[
    "about",
    "after",
    "again",
    "along",
    "around",
    "back",
    "been",
    "before",
    "being",
    "between",
    "browsing",
    "changes",
    "chat",
    "checked",
    "code",
    "computer",
    "conversation",
    "discussed",
    "doing",
    "file",
    "files",
    "from",
    "have",
    "info",
    "information",
    "into",
    "just",
    "looked",
    "looking",
    "more",
    "new",
    "online",
    "opened",
    "over",
    "page",
    "pages",
    "read",
    "reading",
    "reviewed",
    "screen",
    "searched",
    "searching",
    "session",
    "some",
    "that",
    "their",
    "then",
    "there",
    "these",
    "they",
    "this",
    "time",
    "used",
    "user",
    "users",
    "using",
    "viewed",
    "view",
    "were",
    "window",
    "with",
    "work",
    "worked",
    "working",
];

/// Mechanical tag derivation: the summary's distinctive lowercase words
/// (≥4 chars, stopworded, deduped, first-appearance order), capped at 5.
/// Deterministic — no model in the loop, works for every source.
pub fn derive_tags(summary: &str) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    summary
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.chars().count() >= 4 && !TAG_STOPWORDS.contains(w))
        .filter(|w| seen.insert(w.to_string()))
        .take(5)
        .map(str::to_string)
        .collect()
}

/// The storage seam: exactly one SQLite file (WAL), text/metadata columns
/// only. See module docs for the invariants.
pub struct MemoryStore {
    conn: Mutex<Connection>,
    /// On-disk location, surfaced by `memory_status` (T04). `None` for
    /// in-memory test stores.
    path: Option<PathBuf>,
}

/// Idempotent schema. Declared column types are INTEGER/TEXT only — the
/// structural test below pins both the exact column set and the type
/// allowlist. The FTS index is external-content over `memories.summary`,
/// kept in sync by triggers so every write path (including `wipe`) updates
/// it for free.
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS memories (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    summary       TEXT NOT NULL,
    apps          TEXT NOT NULL DEFAULT '[]',
    span_start_ms INTEGER NOT NULL,
    span_end_ms   INTEGER NOT NULL,
    embedding     TEXT,
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    source        TEXT NOT NULL DEFAULT 'watcher',
    category      TEXT NOT NULL DEFAULT 'other',
    tags          TEXT NOT NULL DEFAULT '[]',
    pinned        INTEGER NOT NULL DEFAULT 0,
    expires_at_ms INTEGER
);
CREATE VIRTUAL TABLE IF NOT EXISTS memories_fts USING fts5(
    summary,
    tags,
    category,
    content='memories',
    content_rowid='id'
);
CREATE TRIGGER IF NOT EXISTS memories_fts_ai AFTER INSERT ON memories BEGIN
    INSERT INTO memories_fts(rowid, summary, tags, category)
    VALUES (new.id, new.summary, new.tags, new.category);
END;
CREATE TRIGGER IF NOT EXISTS memories_fts_ad AFTER DELETE ON memories BEGIN
    INSERT INTO memories_fts(memories_fts, rowid, summary, tags, category)
    VALUES ('delete', old.id, old.summary, old.tags, old.category);
END;
CREATE TRIGGER IF NOT EXISTS memories_fts_au AFTER UPDATE OF summary, tags, category ON memories BEGIN
    INSERT INTO memories_fts(memories_fts, rowid, summary, tags, category)
    VALUES ('delete', old.id, old.summary, old.tags, old.category);
    INSERT INTO memories_fts(rowid, summary, tags, category)
    VALUES (new.id, new.summary, new.tags, new.category);
END;
CREATE TABLE IF NOT EXISTS inventory (
    id             INTEGER PRIMARY KEY AUTOINCREMENT,
    name           TEXT NOT NULL,
    path           TEXT NOT NULL,
    kind           TEXT NOT NULL,
    refreshed_at_ms INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS chat_sessions (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    started_at_ms INTEGER NOT NULL,
    last_at_ms    INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS chat_session_messages (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id INTEGER NOT NULL REFERENCES chat_sessions(id) ON DELETE CASCADE,
    role       TEXT NOT NULL,
    text       TEXT NOT NULL,
    at_ms      INTEGER NOT NULL
);
";

/// One graph-builder input row: the record plus its private embedding.
pub type GraphSourceRow = (MemoryRecord, Option<Vec<f32>>);

const RECORD_COLUMNS: &str = "id, summary, apps, span_start_ms, span_end_ms, created_at_ms, \
                              updated_at_ms, source, category, tags, pinned, expires_at_ms";

/// Additive migration for pre-M008 databases created before the `source`
/// column existed: `CREATE TABLE IF NOT EXISTS` skips their existing table,
/// so the column is added here. `ALTER TABLE ... ADD COLUMN` with a constant
/// default is the one cheap SQLite shape (no table rewrite) and is safe
/// under WAL. Idempotent: fresh databases already have the column from
/// `SCHEMA`, and the `table_info` probe skips the ALTER entirely.
fn migrate_source_column(conn: &Connection) -> rusqlite::Result<()> {
    let mut stmt = conn.prepare("PRAGMA table_info(memories)")?;
    let has_source = stmt
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<Result<Vec<_>, _>>()?
        .iter()
        .any(|name| name == "source");
    if !has_source {
        conn.execute_batch(
            "ALTER TABLE memories ADD COLUMN source TEXT NOT NULL DEFAULT 'watcher'",
        )?;
        log::info!("memory: migrated db — added source column (default 'watcher')");
    }
    Ok(())
}

/// Memory v2 migration (2026-08-01): add category/tags/pinned/expires
/// columns, backfill mechanical tags, and rebuild the FTS index to cover
/// tags + category. Ordering is load-bearing: SQLite compiles trigger
/// bodies LAZILY, so the new triggers (created by SCHEMA before the
/// columns existed) must be dropped BEFORE the backfill touches `tags`,
/// or their 'delete' rows corrupt an index that never held those docs
/// ("database disk image is malformed"). The rebuild uses FTS5's own
/// 'rebuild' command — the canonical external-content resync, which also
/// heals pre-M008 rows that were never indexed at all.
fn migrate_memory_v2(conn: &Connection) -> rusqlite::Result<()> {
    let columns: Vec<String> = conn
        .prepare("PRAGMA table_info(memories)")?
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<Result<Vec<_>, _>>()?;
    let needs_columns = !columns.iter().any(|c| c == "category");
    let fts_columns: Vec<String> = conn
        .prepare("PRAGMA table_info(memories_fts)")?
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<Result<Vec<_>, _>>()?;
    let needs_fts = !fts_columns.iter().any(|c| c == "tags");
    if !needs_columns && !needs_fts {
        return Ok(());
    }
    // Triggers and index go first — nothing may fire mid-upgrade.
    conn.execute_batch(
        "DROP TRIGGER IF EXISTS memories_fts_ai;
         DROP TRIGGER IF EXISTS memories_fts_ad;
         DROP TRIGGER IF EXISTS memories_fts_au;
         DROP TABLE IF EXISTS memories_fts;",
    )?;
    if needs_columns {
        conn.execute_batch(
            "ALTER TABLE memories ADD COLUMN category TEXT NOT NULL DEFAULT 'other';
             ALTER TABLE memories ADD COLUMN tags TEXT NOT NULL DEFAULT '[]';
             ALTER TABLE memories ADD COLUMN pinned INTEGER NOT NULL DEFAULT 0;
             ALTER TABLE memories ADD COLUMN expires_at_ms INTEGER;",
        )?;
        log::info!("memory: migrated db — added category/tags/pinned/expires columns");
    }
    // Backfill mechanical tags for rows that predate v2 (trigger-free zone).
    let pending: Vec<(i64, String)> = conn
        .prepare("SELECT id, summary FROM memories WHERE tags = '[]'")?
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<Result<Vec<_>, _>>()?;
    for (id, summary) in &pending {
        let tags = serde_json::to_string(&derive_tags(summary)).unwrap_or_else(|_| "[]".into());
        if tags != "[]" {
            conn.execute(
                "UPDATE memories SET tags = ?1 WHERE id = ?2",
                params![tags, id],
            )?;
        }
    }
    if !pending.is_empty() {
        log::info!("memory: backfilled tags for {} row(s)", pending.len());
    }
    // Recreate the index + triggers, then resync from the content table.
    conn.execute_batch(SCHEMA)?;
    conn.execute(
        "INSERT INTO memories_fts(memories_fts) VALUES('rebuild')",
        [],
    )?;
    log::info!("memory: rebuilt FTS index over summary + tags + category");
    Ok(())
}

/// Shared connection init for both open paths: pragmas, idempotent schema,
/// then the additive `source` migration for pre-M008 files.
fn init_connection(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch("PRAGMA foreign_keys=ON;")?;
    conn.execute_batch(SCHEMA)?;
    migrate_source_column(conn)?;
    migrate_memory_v2(conn)
}

impl MemoryStore {
    /// Open (creating if needed) the single memory db at `path`. Creates
    /// parent directories, switches to WAL, enables foreign keys, and runs
    /// the schema idempotently. The path is logged so the on-disk location
    /// is always greppable at startup.
    pub fn open(path: &Path) -> Result<Self, MemoryError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| MemoryError::Db {
                detail: format!("create {parent:?}: {e}"),
            })?;
        }
        let conn = Connection::open(path).map_err(|e| MemoryError::Db {
            detail: format!("open {path:?}: {e}"),
        })?;
        // journal_mode returns the resulting mode as a row; query it rather
        // than execute so the statement is consumed either way.
        let mode: String = conn.query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))?;
        init_connection(&conn)?;
        log::info!(
            "memory: db open at {} (journal_mode={mode})",
            path.display()
        );
        Ok(Self {
            conn: Mutex::new(conn),
            path: Some(path.to_path_buf()),
        })
    }

    /// In-memory store for tests: same schema and pragmas, no file.
    pub fn open_in_memory() -> Result<Self, MemoryError> {
        let conn = Connection::open_in_memory()?;
        init_connection(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
            path: None,
        })
    }

    /// The on-disk db path, or `None` for an in-memory store.
    pub fn db_path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().expect("memory store mutex poisoned")
    }

    /// Insert one memory, returning the stored record. Rejects blank
    /// summaries (`invalid-input`) — an unsearchable memory is a bug in the
    /// caller, not data.
    pub fn insert(&self, new: NewMemory) -> Result<MemoryRecord, MemoryError> {
        validate_summary(&new.summary)?;
        let apps_json =
            serde_json::to_string(&new.apps).map_err(|e| MemoryError::InvalidInput {
                detail: format!("apps: {e}"),
            })?;
        let embedding_json = new
            .embedding
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|e| MemoryError::InvalidInput {
                detail: format!("embedding: {e}"),
            })?;
        let now = now_ms();
        let category = normalize_category(&new.category);
        let tags = if new.tags.is_empty() {
            derive_tags(&new.summary)
        } else {
            new.tags
                .iter()
                .map(|t| t.trim().to_lowercase())
                .filter(|t| !t.is_empty())
                .take(5)
                .collect()
        };
        let tags_json = serde_json::to_string(&tags).map_err(|e| MemoryError::InvalidInput {
            detail: format!("tags: {e}"),
        })?;
        let conn = self.lock();
        conn.execute(
            "INSERT INTO memories
                (summary, apps, span_start_ms, span_end_ms, embedding,
                 created_at_ms, updated_at_ms, source, category, tags, pinned, expires_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                new.summary,
                apps_json,
                new.span_start_ms,
                new.span_end_ms,
                embedding_json,
                now,
                new.source.as_str(),
                category,
                tags_json,
                new.pinned as i64,
                new.expires_at_ms
            ],
        )?;
        let id = conn.last_insert_rowid();
        log::info!("memory: inserted row {id}");
        Self::get_on(&conn, id)
    }

    /// Newest-first page of records.
    pub fn list(&self, limit: usize, offset: usize) -> Result<Vec<MemoryRecord>, MemoryError> {
        let conn = self.lock();
        let mut stmt = conn.prepare(&format!(
            "SELECT {RECORD_COLUMNS} FROM memories
             ORDER BY created_at_ms DESC, id DESC LIMIT ?1 OFFSET ?2"
        ))?;
        let rows = stmt
            .query_map(params![limit as i64, offset as i64], row_to_record)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Newest memories with their stored embeddings — the graph builder's
    /// input. Embeddings stay inside the process (R-rule: they never cross
    /// IPC); the graph command reduces them to edge weights.
    pub fn list_for_graph(&self, limit: usize) -> Result<Vec<GraphSourceRow>, MemoryError> {
        let conn = self.lock();
        let mut stmt = conn.prepare(&format!(
            "SELECT {RECORD_COLUMNS}, embedding FROM memories
             ORDER BY created_at_ms DESC, id DESC LIMIT ?1"
        ))?;
        let rows = stmt
            .query_map(params![limit as i64], |row| {
                let record = row_to_record(row)?;
                let embedding_json: Option<String> = row.get(8)?;
                Ok((record, embedding_json))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows
            .into_iter()
            .map(|(record, embedding_json)| {
                let embedding = embedding_json
                    .and_then(|json| serde_json::from_str::<Vec<f32>>(&json).ok())
                    .filter(|e| !e.is_empty());
                (record, embedding)
            })
            .collect())
    }

    /// Enforce retention (memory v2 — the previously-unenforced window):
    /// delete un-pinned rows whose explicit expiry has passed, or, when a
    /// global window is set, whose creation fell outside it. Pinned rows
    /// are untouchable. Returns how many rows went.
    pub fn prune_expired(
        &self,
        now_ms: i64,
        global_window_ms: Option<i64>,
    ) -> Result<usize, MemoryError> {
        let conn = self.lock();
        let mut deleted = conn.execute(
            "DELETE FROM memories
             WHERE pinned = 0 AND expires_at_ms IS NOT NULL AND expires_at_ms <= ?1",
            params![now_ms],
        )?;
        if let Some(window) = global_window_ms {
            deleted += conn.execute(
                "DELETE FROM memories
                 WHERE pinned = 0 AND expires_at_ms IS NULL AND created_at_ms < ?1",
                params![now_ms.saturating_sub(window)],
            )?;
        }
        Ok(deleted)
    }

    /// Pin (permanent) or unpin one memory.
    pub fn set_pinned(&self, id: i64, pinned: bool) -> Result<MemoryRecord, MemoryError> {
        let conn = self.lock();
        let changed = conn.execute(
            "UPDATE memories SET pinned = ?1, updated_at_ms = ?2 WHERE id = ?3",
            params![pinned as i64, now_ms(), id],
        )?;
        if changed == 0 {
            return Err(MemoryError::NotFound { id });
        }
        Self::get_on(&conn, id)
    }

    /// Set (or clear, back to the global window) one memory's explicit expiry.
    pub fn set_expiry(
        &self,
        id: i64,
        expires_at_ms: Option<i64>,
    ) -> Result<MemoryRecord, MemoryError> {
        let conn = self.lock();
        let changed = conn.execute(
            "UPDATE memories SET expires_at_ms = ?1, updated_at_ms = ?2 WHERE id = ?3",
            params![expires_at_ms, now_ms(), id],
        )?;
        if changed == 0 {
            return Err(MemoryError::NotFound { id });
        }
        Self::get_on(&conn, id)
    }

    pub fn get(&self, id: i64) -> Result<MemoryRecord, MemoryError> {
        Self::get_on(&self.lock(), id)
    }

    fn get_on(conn: &Connection, id: i64) -> Result<MemoryRecord, MemoryError> {
        conn.query_row(
            &format!("SELECT {RECORD_COLUMNS} FROM memories WHERE id = ?1"),
            params![id],
            row_to_record,
        )
        .optional()?
        .ok_or(MemoryError::NotFound { id })
    }

    /// Replace a memory's summary. Bumps `updated_at_ms` and clears the
    /// stored embedding (stale after an edit, D022 — T02 re-embeds lazily).
    pub fn update_summary(&self, id: i64, summary: &str) -> Result<MemoryRecord, MemoryError> {
        validate_summary(summary)?;
        let now = now_ms();
        let conn = self.lock();
        // Derived metadata goes stale with the text it came from: the
        // embedding clears (re-embedded lazily) and mechanical tags
        // re-derive from the NEW summary.
        let tags = serde_json::to_string(&derive_tags(summary)).unwrap_or_else(|_| "[]".into());
        let changed = conn.execute(
            "UPDATE memories
             SET summary = ?1, embedding = NULL, tags = ?2, updated_at_ms = ?3
             WHERE id = ?4",
            params![summary, tags, now, id],
        )?;
        if changed == 0 {
            return Err(MemoryError::NotFound { id });
        }
        log::info!("memory: updated row {id} (embedding cleared)");
        Self::get_on(&conn, id)
    }

    pub fn delete(&self, id: i64) -> Result<(), MemoryError> {
        let changed = self
            .lock()
            .execute("DELETE FROM memories WHERE id = ?1", params![id])?;
        if changed == 0 {
            return Err(MemoryError::NotFound { id });
        }
        log::info!("memory: deleted row {id}");
        Ok(())
    }

    /// Delete every memory, returning how many rows were removed.
    pub fn wipe(&self) -> Result<usize, MemoryError> {
        let removed = self.lock().execute("DELETE FROM memories", [])?;
        log::info!("memory: wiped {removed} rows");
        Ok(removed)
    }

    /// Replace the whole machine inventory in one transaction (computer-
    /// control I1): the refresh loop's atomic wipe+refill, so readers never
    /// see a half-scanned cache. Returns how many entries landed.
    pub fn inventory_replace(&self, entries: &[NewInventoryEntry]) -> Result<usize, MemoryError> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM inventory", [])?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO inventory (name, path, kind, refreshed_at_ms)
                 VALUES (?1, ?2, ?3, ?4)",
            )?;
            for entry in entries {
                stmt.execute(params![
                    entry.name,
                    entry.path,
                    entry.kind,
                    entry.refreshed_at_ms
                ])?;
            }
        }
        tx.commit()?;
        log::info!("inventory: cache replaced with {} entries", entries.len());
        Ok(entries.len())
    }

    /// Case-insensitive name-substring search over the inventory, GUI apps
    /// ranked before CLI tools, then by name. An empty query lists from the
    /// top (bounded by `limit`).
    pub fn inventory_search(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<InventoryEntry>, MemoryError> {
        let pattern = format!("%{}%", query.trim());
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT name, path, kind, refreshed_at_ms FROM inventory
             WHERE name LIKE ?1 COLLATE NOCASE
             ORDER BY (kind = 'app') DESC, name COLLATE NOCASE
             LIMIT ?2",
        )?;
        let rows = stmt
            .query_map(params![pattern, limit as i64], |row| {
                Ok(InventoryEntry {
                    name: row.get(0)?,
                    path: row.get(1)?,
                    kind: row.get(2)?,
                    refreshed_at_ms: row.get(3)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Inventory health: `(apps, cli tools, last refresh ms)` — the last
    /// refresh is `None` until a first scan lands.
    pub fn inventory_counts(&self) -> Result<(usize, usize, Option<i64>), MemoryError> {
        let conn = self.lock();
        let apps: i64 = conn.query_row(
            "SELECT COUNT(*) FROM inventory WHERE kind = 'app'",
            [],
            |row| row.get(0),
        )?;
        let tools: i64 = conn.query_row(
            "SELECT COUNT(*) FROM inventory WHERE kind = 'cli'",
            [],
            |row| row.get(0),
        )?;
        let last: Option<i64> =
            conn.query_row("SELECT MAX(refreshed_at_ms) FROM inventory", [], |row| {
                row.get(0)
            })?;
        Ok((apps as usize, tools as usize, last))
    }

    /// Start a new chat session (computer-control I3); returns its id.
    pub fn chat_session_create(&self, now_ms: i64) -> Result<i64, MemoryError> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO chat_sessions (started_at_ms, last_at_ms) VALUES (?1, ?1)",
            params![now_ms],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// Append one completed exchange (user ask + assistant answer) to a
    /// session and bump its last-activity stamp. Raw transcript only — the
    /// distilled-recall path is chat_ingest's, unchanged.
    pub fn chat_append_exchange(
        &self,
        session_id: i64,
        user: &str,
        assistant: &str,
        now_ms: i64,
    ) -> Result<(), MemoryError> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        // Existence check first (SQLite does not enforce the REFERENCES
        // clause without a pragma): a missing session is typed not-found
        // and nothing is written.
        let changed = tx.execute(
            "UPDATE chat_sessions SET last_at_ms = ?2 WHERE id = ?1",
            params![session_id, now_ms],
        )?;
        if changed == 0 {
            return Err(MemoryError::NotFound { id: session_id });
        }
        tx.execute(
            "INSERT INTO chat_session_messages (session_id, role, text, at_ms)
             VALUES (?1, 'user', ?2, ?3)",
            params![session_id, user, now_ms],
        )?;
        tx.execute(
            "INSERT INTO chat_session_messages (session_id, role, text, at_ms)
             VALUES (?1, 'assistant', ?2, ?3)",
            params![session_id, assistant, now_ms],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Newest-first session summaries. `title` is the first user line
    /// (truncated to 80 chars) — honest and derivable, never invented.
    pub fn chat_sessions(&self, limit: usize) -> Result<Vec<ChatSessionSummary>, MemoryError> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT s.id, s.started_at_ms, s.last_at_ms,
                    (SELECT text FROM chat_session_messages
                     WHERE session_id = s.id AND role = 'user'
                     ORDER BY id LIMIT 1),
                    (SELECT COUNT(*) FROM chat_session_messages WHERE session_id = s.id)
             FROM chat_sessions s
             ORDER BY s.last_at_ms DESC, s.id DESC LIMIT ?1",
        )?;
        let rows = stmt
            .query_map(params![limit as i64], |row| {
                let raw_title: Option<String> = row.get(3)?;
                let title = raw_title
                    .map(|t| {
                        let t = t.trim().to_string();
                        if t.chars().count() > 80 {
                            format!("{}…", t.chars().take(80).collect::<String>())
                        } else {
                            t
                        }
                    })
                    .unwrap_or_else(|| "(empty chat)".into());
                Ok(ChatSessionSummary {
                    id: row.get(0)?,
                    started_at_ms: row.get(1)?,
                    last_at_ms: row.get(2)?,
                    title,
                    message_count: row.get::<_, i64>(4)? as usize,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Sessions whose title or ANY transcript line matches `query`
    /// (case-insensitive substring), newest-activity-first — the Chats tab's
    /// search across stored transcripts (the distilled recall layer already
    /// has memory_search; this covers the raw text).
    pub fn chat_sessions_matching(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<ChatSessionSummary>, MemoryError> {
        let pattern = format!("%{}%", query.trim());
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT s.id, s.started_at_ms, s.last_at_ms,
                    (SELECT text FROM chat_session_messages
                     WHERE session_id = s.id AND role = 'user'
                     ORDER BY id LIMIT 1),
                    (SELECT COUNT(*) FROM chat_session_messages WHERE session_id = s.id)
             FROM chat_sessions s
             WHERE EXISTS (SELECT 1 FROM chat_session_messages m
                           WHERE m.session_id = s.id
                             AND m.text LIKE ?1 COLLATE NOCASE)
             ORDER BY s.last_at_ms DESC, s.id DESC LIMIT ?2",
        )?;
        let rows = stmt
            .query_map(params![pattern, limit as i64], |row| {
                let raw_title: Option<String> = row.get(3)?;
                let title = raw_title
                    .map(|t| {
                        let t = t.trim().to_string();
                        if t.chars().count() > 80 {
                            format!("{}…", t.chars().take(80).collect::<String>())
                        } else {
                            t
                        }
                    })
                    .unwrap_or_else(|| "(empty chat)".into());
                Ok(ChatSessionSummary {
                    id: row.get(0)?,
                    started_at_ms: row.get(1)?,
                    last_at_ms: row.get(2)?,
                    title,
                    message_count: row.get::<_, i64>(4)? as usize,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// One session's transcript in order.
    /// Transcript lines whose text matches `query` (case-insensitive LIKE,
    /// same containment contract as [`Self::chat_sessions_matching`]),
    /// newest first — the model-facing recall path over past conversations.
    pub fn chat_messages_matching(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<MatchedChatMessage>, MemoryError> {
        let pattern = format!("%{}%", query.trim());
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT session_id, role, text, at_ms FROM chat_session_messages
             WHERE text LIKE ?1 ORDER BY id DESC LIMIT ?2",
        )?;
        let rows = stmt
            .query_map(params![pattern, limit as i64], |row| {
                Ok(MatchedChatMessage {
                    session_id: row.get(0)?,
                    role: row.get(1)?,
                    text: row.get(2)?,
                    at_ms: row.get(3)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Delete one stored session and its transcript. Typed not-found when
    /// the id never existed — a purge button must not silently no-op.
    pub fn chat_session_delete(&self, id: i64) -> Result<(), MemoryError> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        tx.execute(
            "DELETE FROM chat_session_messages WHERE session_id = ?1",
            params![id],
        )?;
        let deleted = tx.execute("DELETE FROM chat_sessions WHERE id = ?1", params![id])?;
        if deleted == 0 {
            return Err(MemoryError::NotFound { id });
        }
        tx.commit()?;
        Ok(())
    }

    /// Delete EVERY stored session and transcript; returns how many
    /// sessions went. The distilled memories derived from chats are NOT
    /// touched — they have their own wipe.
    pub fn chat_sessions_wipe(&self) -> Result<usize, MemoryError> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM chat_session_messages", [])?;
        let deleted = tx.execute("DELETE FROM chat_sessions", [])?;
        tx.commit()?;
        Ok(deleted)
    }

    pub fn chat_session_messages(
        &self,
        session_id: i64,
    ) -> Result<Vec<ChatSessionMessage>, MemoryError> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT role, text, at_ms FROM chat_session_messages
             WHERE session_id = ?1 ORDER BY id",
        )?;
        let rows = stmt
            .query_map(params![session_id], |row| {
                Ok(ChatSessionMessage {
                    role: row.get(0)?,
                    text: row.get(1)?,
                    at_ms: row.get(2)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Retention enforcement (memoryRetention follow-up): delete memories
    /// whose creation predates `cutoff_ms`, returning how many were removed.
    /// Keyed on `created_at_ms` — when the memory entered the store — not the
    /// observed span, so an old span distilled recently survives its full
    /// retention window like any other new memory.
    pub fn prune_created_before(&self, cutoff_ms: i64) -> Result<usize, MemoryError> {
        let removed = self.lock().execute(
            "DELETE FROM memories WHERE created_at_ms < ?1",
            params![cutoff_ms],
        )?;
        if removed > 0 {
            log::info!("memory: retention pruned {removed} rows older than {cutoff_ms}");
        }
        Ok(removed)
    }

    pub fn count(&self) -> Result<usize, MemoryError> {
        let n: i64 = self
            .lock()
            .query_row("SELECT COUNT(*) FROM memories", [], |row| row.get(0))?;
        Ok(n as usize)
    }

    /// Structural introspection of the `memories` table: `(column name,
    /// declared type)` in definition order, via `PRAGMA table_info`. Exposed so
    /// the R011/R023 persistence proof (an integration test, no access to the
    /// private connection) can pin that the store has no coordinate column and
    /// only ever declares INTEGER/TEXT types — the same invariant the in-crate
    /// `schema_is_text_and_metadata_only` unit test asserts.
    pub fn column_info(&self) -> Result<Vec<(String, String)>, MemoryError> {
        let conn = self.lock();
        let mut stmt = conn.prepare("PRAGMA table_info(memories)")?;
        let cols = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(1)?, row.get::<_, String>(2)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(cols)
    }

    /// FTS5 bm25 keyword search: best (lowest bm25) first. User text is
    /// never spliced into the MATCH grammar — each whitespace token is
    /// double-quoted (inner quotes escaped) so operators like `AND`, `-` or
    /// stray `"` are plain terms, and the whole expression is bound as a
    /// parameter. A query with no tokens returns no hits.
    pub fn search_keyword(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<(MemoryRecord, f64)>, MemoryError> {
        let Some(match_expr) = fts_match_expr(query) else {
            return Ok(Vec::new());
        };
        let conn = self.lock();
        let mut stmt = conn.prepare(&format!(
            "SELECT {}, bm25(memories_fts) AS score
             FROM memories_fts
             JOIN memories m ON m.id = memories_fts.rowid
             WHERE memories_fts MATCH ?1
             ORDER BY score LIMIT ?2",
            RECORD_COLUMNS
                .split(", ")
                .map(|c| format!("m.{c}"))
                .collect::<Vec<_>>()
                .join(", ")
        ))?;
        let rows = stmt
            .query_map(params![match_expr, limit as i64], |row| {
                Ok((row_to_record(row)?, row.get::<_, f64>("score")?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Oldest-first `(id, summary)` of rows with no stored embedding — the
    /// lazy backfill queue for semantic search (T02). Bounded by `limit` so
    /// one search never embeds the whole store at once.
    pub fn unembedded_rows(&self, limit: usize) -> Result<Vec<(i64, String)>, MemoryError> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, summary FROM memories WHERE embedding IS NULL
             ORDER BY id LIMIT ?1",
        )?;
        let rows = stmt
            .query_map(params![limit as i64], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Attach an embedding to an existing row (lazy backfill write-back).
    /// Rejects empty vectors — an unrankable embedding is a caller bug.
    pub fn set_embedding(&self, id: i64, embedding: &[f32]) -> Result<(), MemoryError> {
        if embedding.is_empty() {
            return Err(MemoryError::InvalidInput {
                detail: "embedding is empty".into(),
            });
        }
        let json = serde_json::to_string(embedding).map_err(|e| MemoryError::InvalidInput {
            detail: format!("embedding: {e}"),
        })?;
        let changed = self.lock().execute(
            "UPDATE memories SET embedding = ?1 WHERE id = ?2",
            params![json, id],
        )?;
        if changed == 0 {
            return Err(MemoryError::NotFound { id });
        }
        log::debug!("memory: stored embedding for row {id}");
        Ok(())
    }

    /// All rows that carry an embedding, as `(id, vector)` — the corpus the
    /// semantic ranker (T02) scores with cosine similarity. Rows whose
    /// stored JSON fails to parse are skipped with a warning rather than
    /// failing the whole search.
    pub fn embedded_rows(&self) -> Result<Vec<(i64, Vec<f32>)>, MemoryError> {
        let conn = self.lock();
        let mut stmt =
            conn.prepare("SELECT id, embedding FROM memories WHERE embedding IS NOT NULL")?;
        let rows = stmt
            .query_map([], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<Result<Vec<(i64, String)>, _>>()?;
        Ok(rows
            .into_iter()
            .filter_map(|(id, json)| match serde_json::from_str::<Vec<f32>>(&json) {
                Ok(vec) => Some((id, vec)),
                Err(e) => {
                    log::warn!("memory: row {id} has unparseable embedding, skipping: {e}");
                    None
                }
            })
            .collect())
    }
}

fn validate_summary(summary: &str) -> Result<(), MemoryError> {
    if summary.trim().is_empty() {
        return Err(MemoryError::InvalidInput {
            detail: "summary is empty".into(),
        });
    }
    Ok(())
}

/// Milliseconds since the Unix epoch. Saturates at 0 if the clock is set
/// before 1970 rather than panicking.
fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Build an FTS5 MATCH expression that treats the user's text as plain
/// terms: each whitespace token becomes a quoted string with inner quotes
/// doubled, so no input can hit the query grammar.
fn fts_match_expr(query: &str) -> Option<String> {
    let tokens: Vec<String> = query
        .split_whitespace()
        .map(|t| format!("\"{}\"", t.replace('"', "\"\"")))
        .collect();
    if tokens.is_empty() {
        None
    } else {
        Some(tokens.join(" "))
    }
}

fn row_to_record(row: &rusqlite::Row<'_>) -> rusqlite::Result<MemoryRecord> {
    let apps_json: String = row.get(2)?;
    Ok(MemoryRecord {
        id: row.get(0)?,
        summary: row.get(1)?,
        // A corrupt apps cell degrades to an empty list rather than making
        // the row unreadable.
        apps: serde_json::from_str(&apps_json).unwrap_or_default(),
        span_start_ms: row.get(3)?,
        span_end_ms: row.get(4)?,
        created_at_ms: row.get(5)?,
        updated_at_ms: row.get(6)?,
        source: MemorySource::from_str_lenient(&row.get::<_, String>(7)?),
        category: row.get::<_, String>(8).unwrap_or_else(|_| "other".into()),
        tags: row
            .get::<_, String>(9)
            .ok()
            .and_then(|json| serde_json::from_str(&json).ok())
            .unwrap_or_default(),
        pinned: row.get::<_, i64>(10).unwrap_or(0) != 0,
        expires_at_ms: row.get::<_, Option<i64>>(11).unwrap_or(None),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem(summary: &str) -> NewMemory {
        NewMemory {
            summary: summary.into(),
            apps: vec!["TestApp".into()],
            span_start_ms: 1_000,
            span_end_ms: 2_000,
            embedding: None,
            source: MemorySource::Watcher,
            category: "other".into(),
            tags: Vec::new(),
            pinned: false,
            expires_at_ms: None,
        }
    }

    fn store() -> MemoryStore {
        MemoryStore::open_in_memory().unwrap()
    }

    #[test]
    fn open_records_db_path_and_in_memory_has_none() {
        assert_eq!(store().db_path(), None);

        let path = std::env::temp_dir().join(format!(
            "third-eye-store-path-test-{}.db",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let s = MemoryStore::open(&path).unwrap();
        assert_eq!(s.db_path(), Some(path.as_path()));
        drop(s);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn chat_session_round_trip_with_titles_and_ordering() {
        let s = store();
        let a = s.chat_session_create(1_000).unwrap();
        let b = s.chat_session_create(2_000).unwrap();
        s.chat_append_exchange(a, "what's my ip?", "203.0.113.7", 3_000)
            .unwrap();
        s.chat_append_exchange(b, &"x".repeat(120), "long question answer", 4_000)
            .unwrap();
        // Newest-activity-first; titles derive from the first user line, the
        // long one truncated with a marker; empty sessions get honest copy.
        let c = s.chat_session_create(5_000).unwrap();
        let sessions = s.chat_sessions(10).unwrap();
        assert_eq!(
            sessions.iter().map(|x| x.id).collect::<Vec<_>>(),
            vec![c, b, a]
        );
        assert_eq!(sessions[2].title, "what's my ip?");
        assert!(sessions[1].title.ends_with('…'));
        assert_eq!(sessions[0].title, "(empty chat)");
        assert_eq!(sessions[2].message_count, 2);
        // Transcript in order.
        let transcript = s.chat_session_messages(a).unwrap();
        assert_eq!(transcript.len(), 2);
        assert_eq!(transcript[0].role, "user");
        assert_eq!(transcript[1].text, "203.0.113.7");
        // Appending to a missing session is typed not-found.
        assert!(matches!(
            s.chat_append_exchange(999, "q", "a", 6_000),
            Err(MemoryError::NotFound { id: 999 })
        ));
    }

    #[test]
    fn chat_sessions_matching_searches_transcript_text() {
        let s = store();
        let a = s.chat_session_create(1_000).unwrap();
        let b = s.chat_session_create(2_000).unwrap();
        s.chat_append_exchange(a, "how do I prune tomatoes?", "Cut the suckers.", 3_000)
            .unwrap();
        s.chat_append_exchange(b, "what's my ip", "203.0.113.7", 4_000)
            .unwrap();
        // Matches assistant text too, case-insensitive; misses honestly.
        let hits = s.chat_sessions_matching("SUCKERS", 10).unwrap();
        assert_eq!(hits.iter().map(|x| x.id).collect::<Vec<_>>(), vec![a]);
        assert!(s.chat_sessions_matching("lasagne", 10).unwrap().is_empty());
        // A broad match returns newest-activity-first.
        let all = s.chat_sessions_matching("", 10).unwrap();
        assert_eq!(all.iter().map(|x| x.id).collect::<Vec<_>>(), vec![b, a]);
    }

    #[test]
    fn chat_session_delete_removes_transcript_and_wipe_clears_all() {
        let s = store();
        let a = s.chat_session_create(1_000).unwrap();
        let b = s.chat_session_create(2_000).unwrap();
        s.chat_append_exchange(a, "q1", "a1", 3_000).unwrap();
        s.chat_append_exchange(b, "q2", "a2", 4_000).unwrap();

        s.chat_session_delete(a).unwrap();
        assert!(s.chat_session_messages(a).unwrap().is_empty());
        assert_eq!(s.chat_sessions(10).unwrap().len(), 1);
        // Deleting again is a typed not-found, not a silent no-op.
        assert!(matches!(
            s.chat_session_delete(a),
            Err(MemoryError::NotFound { id }) if id == a
        ));

        assert_eq!(s.chat_sessions_wipe().unwrap(), 1);
        assert!(s.chat_sessions(10).unwrap().is_empty());
        assert!(s.chat_messages_matching("q2", 10).unwrap().is_empty());
    }

    #[test]
    fn chat_messages_matching_returns_the_lines_newest_first() {
        let s = store();
        let a = s.chat_session_create(1_000).unwrap();
        let b = s.chat_session_create(2_000).unwrap();
        s.chat_append_exchange(a, "find me a carbonara recipe", "Here is one.", 3_000)
            .unwrap();
        s.chat_append_exchange(b, "a recipe for lasagne please", "Sure.", 4_000)
            .unwrap();
        s.chat_append_exchange(b, "what's my ip", "203.0.113.7", 5_000)
            .unwrap();
        // Case-insensitive, matches the message text itself, newest first,
        // each row carrying its session.
        let hits = s.chat_messages_matching("RECIPE", 10).unwrap();
        assert_eq!(
            hits.iter()
                .map(|m| (m.session_id, m.text.as_str()))
                .collect::<Vec<_>>(),
            vec![
                (b, "a recipe for lasagne please"),
                (a, "find me a carbonara recipe"),
            ]
        );
        assert_eq!(hits[0].role, "user");
        assert_eq!(hits[0].at_ms, 4_000);
        // Limit pages from the newest side; a miss is honestly empty.
        assert_eq!(s.chat_messages_matching("recipe", 1).unwrap().len(), 1);
        assert!(s.chat_messages_matching("gnocchi", 10).unwrap().is_empty());
    }

    #[test]
    fn inventory_replace_search_counts_round_trip() {
        let s = store();
        let entry = |name: &str, kind: &str| NewInventoryEntry {
            name: name.into(),
            path: format!("/x/{name}"),
            kind: kind.into(),
            refreshed_at_ms: 42,
        };
        s.inventory_replace(&[
            entry("Google Chrome", "app"),
            entry("chromedriver", "cli"),
            entry("ffmpeg", "cli"),
        ])
        .unwrap();
        // Case-insensitive substring, apps ranked before tools.
        let hits = s.inventory_search("CHROME", 10).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].name, "Google Chrome");
        assert_eq!(hits[0].kind, "app");
        assert_eq!(s.inventory_counts().unwrap(), (1, 2, Some(42)));
        // Replace is atomic wipe+refill — old rows vanish.
        s.inventory_replace(&[entry("ffmpeg", "cli")]).unwrap();
        assert_eq!(s.inventory_counts().unwrap(), (0, 1, Some(42)));
        assert!(s.inventory_search("chrome", 10).unwrap().is_empty());
    }

    #[test]
    fn prune_created_before_removes_only_expired_rows() {
        let s = store();
        let old = s.insert(mem("expired memory")).unwrap();
        let kept = s.insert(mem("fresh memory")).unwrap();
        // Cutoff strictly between the two creation times can't be arranged
        // with real clocks, so backdate the first row directly.
        s.lock()
            .execute(
                "UPDATE memories SET created_at_ms = 1 WHERE id = ?1",
                params![old.id],
            )
            .unwrap();
        let removed = s.prune_created_before(kept.created_at_ms).unwrap();
        assert_eq!(removed, 1);
        let survivors = s.list(10, 0).unwrap();
        assert_eq!(survivors.len(), 1);
        assert_eq!(survivors[0].id, kept.id);
        // A cutoff at/below every creation time removes nothing (forever
        // maps to never calling this at all — see memory_retention_window_ms).
        assert_eq!(s.prune_created_before(0).unwrap(), 0);
    }

    #[test]
    fn crud_round_trip() {
        let s = store();
        let rec = s.insert(mem("wrote the memory store schema")).unwrap();
        assert!(rec.id > 0);
        assert_eq!(rec.summary, "wrote the memory store schema");
        assert_eq!(rec.apps, vec!["TestApp".to_string()]);
        assert_eq!(rec.span_start_ms, 1_000);
        assert_eq!(rec.span_end_ms, 2_000);
        assert!(rec.created_at_ms > 0);
        assert_eq!(rec.created_at_ms, rec.updated_at_ms);

        let fetched = s.get(rec.id).unwrap();
        assert_eq!(fetched, rec);

        s.delete(rec.id).unwrap();
        assert_eq!(
            s.get(rec.id).unwrap_err(),
            MemoryError::NotFound { id: rec.id }
        );
        assert_eq!(s.count().unwrap(), 0);
    }

    #[test]
    fn list_is_newest_first_and_pages() {
        let s = store();
        // Same created_at_ms is possible within one test run — id DESC is
        // the tiebreak, so insertion order still reverses deterministically.
        let ids: Vec<i64> = (1..=5)
            .map(|i| s.insert(mem(&format!("memory number {i}"))).unwrap().id)
            .collect();
        let page1 = s.list(2, 0).unwrap();
        assert_eq!(
            page1.iter().map(|r| r.id).collect::<Vec<_>>(),
            vec![ids[4], ids[3]]
        );
        let page2 = s.list(2, 2).unwrap();
        assert_eq!(
            page2.iter().map(|r| r.id).collect::<Vec<_>>(),
            vec![ids[2], ids[1]]
        );
        let rest = s.list(10, 4).unwrap();
        assert_eq!(rest.iter().map(|r| r.id).collect::<Vec<_>>(), vec![ids[0]]);
    }

    #[test]
    fn wipe_empties_the_store() {
        let s = store();
        for i in 0..3 {
            s.insert(mem(&format!("memory {i}"))).unwrap();
        }
        assert_eq!(s.wipe().unwrap(), 3);
        assert_eq!(s.count().unwrap(), 0);
        assert!(s.list(10, 0).unwrap().is_empty());
        // FTS side emptied too (triggers fired).
        assert!(s.search_keyword("memory", 10).unwrap().is_empty());
    }

    #[test]
    fn update_bumps_timestamp_and_clears_embedding() {
        let s = store();
        let rec = s
            .insert(NewMemory {
                embedding: Some(vec![0.1, 0.2]),
                ..mem("draft summary")
            })
            .unwrap();
        assert_eq!(s.embedded_rows().unwrap().len(), 1);

        std::thread::sleep(std::time::Duration::from_millis(5));
        let updated = s.update_summary(rec.id, "final summary").unwrap();
        assert_eq!(updated.summary, "final summary");
        assert!(updated.updated_at_ms > rec.updated_at_ms);
        assert_eq!(updated.created_at_ms, rec.created_at_ms);
        // Embedding is stale after an edit and must be gone (D022).
        assert!(s.embedded_rows().unwrap().is_empty());
    }

    #[test]
    fn missing_ids_are_not_found() {
        let s = store();
        assert_eq!(s.get(99).unwrap_err(), MemoryError::NotFound { id: 99 });
        assert_eq!(s.delete(99).unwrap_err(), MemoryError::NotFound { id: 99 });
        assert_eq!(
            s.update_summary(99, "x").unwrap_err(),
            MemoryError::NotFound { id: 99 }
        );
    }

    #[test]
    fn blank_summaries_are_invalid_input() {
        let s = store();
        for bad in ["", "   ", "\n\t "] {
            assert_eq!(s.insert(mem(bad)).unwrap_err().kind(), "invalid-input");
        }
        let rec = s.insert(mem("real summary")).unwrap();
        assert_eq!(
            s.update_summary(rec.id, "  ").unwrap_err().kind(),
            "invalid-input"
        );
        // Failed update left the row untouched.
        assert_eq!(s.get(rec.id).unwrap().summary, "real summary");
    }

    #[test]
    fn v2_fields_round_trip_and_tags_derive_mechanically() {
        let s = store();
        let rec = s
            .insert(NewMemory {
                summary: "Compared lasagna ragu recipes on RecipeTin Eats".into(),
                apps: vec!["Chrome".into()],
                span_start_ms: 1,
                span_end_ms: 2,
                embedding: None,
                source: MemorySource::Watcher,
                category: "Browsing".into(),
                tags: Vec::new(),
                pinned: false,
                expires_at_ms: None,
            })
            .unwrap();
        assert_eq!(rec.category, "browsing", "category normalizes");
        assert!(rec.tags.contains(&"lasagna".to_string()), "{:?}", rec.tags);
        assert!(rec.tags.len() <= 5);
        // Explicit tags win; unknown category degrades to other.
        let told = s
            .insert(NewMemory {
                summary: "The user's name is Alex".into(),
                apps: Vec::new(),
                span_start_ms: 1,
                span_end_ms: 2,
                embedding: None,
                source: MemorySource::Told,
                category: "identity".into(),
                tags: vec!["Name".into(), "  ".into()],
                pinned: true,
                expires_at_ms: None,
            })
            .unwrap();
        assert_eq!(told.category, "other");
        assert_eq!(told.tags, vec!["name".to_string()]);
        assert!(told.pinned);
    }

    #[test]
    fn prune_expired_honors_pins_explicit_expiries_and_the_global_window() {
        let s = store();
        let base = NewMemory {
            summary: "expiring soon note".into(),
            apps: Vec::new(),
            span_start_ms: 1,
            span_end_ms: 2,
            embedding: None,
            source: MemorySource::Watcher,
            category: "other".into(),
            tags: Vec::new(),
            pinned: false,
            expires_at_ms: Some(1_000),
        };
        let expired = s.insert(base.clone()).unwrap();
        let pinned = s
            .insert(NewMemory {
                summary: "pinned forever note".into(),
                pinned: true,
                ..base.clone()
            })
            .unwrap();
        let standard = s
            .insert(NewMemory {
                summary: "standard window note".into(),
                expires_at_ms: None,
                ..base.clone()
            })
            .unwrap();
        // Explicit expiry passed → gone; pinned with the same expiry → kept.
        let deleted = s.prune_expired(2_000, None).unwrap();
        assert_eq!(deleted, 1);
        assert!(s.get(expired.id).is_err());
        assert!(s.get(pinned.id).is_ok());
        assert!(s.get(standard.id).is_ok());
        // Global window: standard rows older than the window go; pinned stays.
        let far_future = standard.created_at_ms + 10_000;
        let deleted = s.prune_expired(far_future, Some(1)).unwrap();
        assert_eq!(deleted, 1);
        assert!(s.get(standard.id).is_err());
        assert!(s.get(pinned.id).is_ok());
    }

    #[test]
    fn keyword_search_matches_explicit_tags_and_category() {
        let s = store();
        s.insert(NewMemory {
            summary: "The user's name is Alex".into(),
            apps: Vec::new(),
            span_start_ms: 1,
            span_end_ms: 2,
            embedding: None,
            source: MemorySource::Told,
            category: "personal".into(),
            tags: vec!["identity".into()],
            pinned: true,
            expires_at_ms: None,
        })
        .unwrap();
        // "identity" appears ONLY as a tag; "personal" only as the category.
        assert_eq!(s.search_keyword("identity", 10).unwrap().len(), 1);
        assert_eq!(s.search_keyword("personal", 10).unwrap().len(), 1);
    }

    #[test]
    fn keyword_search_ranks_topical_row_first() {
        let s = store();
        s.insert(mem(
            "debugged the tokio runtime deadlock in the watcher loop",
        ))
        .unwrap();
        let rust = s
            .insert(mem("refactored rust error taxonomy for the llm module"))
            .unwrap();
        s.insert(mem("planned the quarterly team offsite agenda"))
            .unwrap();

        let hits = s.search_keyword("rust error taxonomy", 10).unwrap();
        assert!(!hits.is_empty());
        assert_eq!(hits[0].0.id, rust.id);
        // bm25 scores are more negative for better matches; ordering is
        // ascending so the list is best-first.
        for pair in hits.windows(2) {
            assert!(pair[0].1 <= pair[1].1);
        }
    }

    #[test]
    fn fts_stays_in_sync_after_update_and_delete() {
        let s = store();
        let rec = s.insert(mem("notes about zebras")).unwrap();
        s.update_summary(rec.id, "notes about quokkas").unwrap();
        assert!(s.search_keyword("zebras", 10).unwrap().is_empty());
        assert_eq!(s.search_keyword("quokkas", 10).unwrap().len(), 1);

        s.delete(rec.id).unwrap();
        assert!(s.search_keyword("quokkas", 10).unwrap().is_empty());
    }

    #[test]
    fn hostile_query_strings_never_error() {
        let s = store();
        s.insert(mem("a perfectly normal memory")).unwrap();
        // FTS5 grammar operators, unbalanced quotes, SQL injection shapes,
        // and empty input — all must be treated as plain terms (Q7).
        for hostile in [
            "AND",
            "OR NOT",
            "-negated",
            "\"unbalanced",
            "a\"b\"\"c",
            "col:value",
            "(paren*",
            "'; DROP TABLE memories; --",
            "NEAR(a b)",
            "",
            "   ",
            "^caret",
        ] {
            let result = s.search_keyword(hostile, 10);
            assert!(result.is_ok(), "query {hostile:?} errored: {result:?}");
        }
        // The store survived intact.
        assert_eq!(s.count().unwrap(), 1);
    }

    #[test]
    fn schema_is_text_and_metadata_only() {
        // Structural R011/R012 pin: exact column set, and every declared
        // type is on the INTEGER/TEXT allowlist — no byte-array column type
        // can ever appear here.
        let s = store();
        let conn = s.lock();
        let mut stmt = conn.prepare("PRAGMA table_info(memories)").unwrap();
        let cols: Vec<(String, String)> = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(1)?, row.get::<_, String>(2)?))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();

        let names: Vec<&str> = cols.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "id",
                "summary",
                "apps",
                "span_start_ms",
                "span_end_ms",
                "embedding",
                "created_at_ms",
                "updated_at_ms",
                "source",
                "category",
                "tags",
                "pinned",
                "expires_at_ms"
            ]
        );
        for (name, ty) in &cols {
            assert!(
                matches!(ty.to_uppercase().as_str(), "INTEGER" | "TEXT"),
                "column {name} has disallowed declared type {ty:?}"
            );
        }
    }

    #[test]
    fn open_creates_parent_dirs_and_sets_wal() {
        let dir = std::env::temp_dir()
            .join(format!("third-eye-memory-test-{}", std::process::id()))
            .join("nested");
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
        let db_path = dir.join("memory.db");

        let s = MemoryStore::open(&db_path).unwrap();
        assert!(db_path.exists());
        let mode: String = s
            .lock()
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap();
        assert_eq!(mode.to_lowercase(), "wal");
        drop(s);

        // Re-open is idempotent (schema already present) and data persists.
        let s2 = MemoryStore::open(&db_path).unwrap();
        s2.insert(mem("persisted across opens")).unwrap();
        drop(s2);
        let s3 = MemoryStore::open(&db_path).unwrap();
        assert_eq!(s3.count().unwrap(), 1);
        drop(s3);
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    #[test]
    fn record_ipc_shape_is_camel_case_without_embedding() {
        // Field-set pin (like TextObservation's): S03/S04 read exactly these
        // keys, and the embedding must never leak over IPC.
        let s = store();
        let rec = s
            .insert(NewMemory {
                embedding: Some(vec![1.0]),
                ..mem("shape pin")
            })
            .unwrap();
        let v = serde_json::to_value(&rec).unwrap();
        let mut keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec![
                "apps",
                "category",
                "createdAtMs",
                "expiresAtMs",
                "id",
                "pinned",
                "source",
                "spanEndMs",
                "spanStartMs",
                "summary",
                "tags",
                "updatedAtMs"
            ]
        );
    }

    /// The current schema minus `source` — the exact pre-M008 CREATE TABLE,
    /// used to fabricate an old-format db the migration must upgrade.
    const PRE_M008_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS memories (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    summary       TEXT NOT NULL,
    apps          TEXT NOT NULL DEFAULT '[]',
    span_start_ms INTEGER NOT NULL,
    span_end_ms   INTEGER NOT NULL,
    embedding     TEXT,
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL
);
";

    fn pre_m008_db(path: &Path) {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(PRE_M008_SCHEMA).unwrap();
        conn.execute(
            "INSERT INTO memories
                (summary, apps, span_start_ms, span_end_ms, created_at_ms, updated_at_ms)
             VALUES ('pre-migration row', '[\"OldApp\"]', 1, 2, 3, 3)",
            [],
        )
        .unwrap();
    }

    #[test]
    fn migration_is_idempotent_across_reopens() {
        let path = std::env::temp_dir().join(format!(
            "third-eye-migration-idempotent-{}.db",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let s = MemoryStore::open(&path).unwrap();
        let cols_first = s.column_info().unwrap();
        drop(s);
        // Second open re-runs SCHEMA + migration probe — must be a no-op.
        let s2 = MemoryStore::open(&path).unwrap();
        assert_eq!(s2.column_info().unwrap(), cols_first);
        drop(s2);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn pre_m008_db_migrates_and_rows_default_to_watcher() {
        let path = std::env::temp_dir().join(format!(
            "third-eye-migration-fixture-{}.db",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        pre_m008_db(&path);

        let s = MemoryStore::open(&path).unwrap();
        let rows = s.list(10, 0).unwrap();
        assert_eq!(rows.len(), 1, "pre-migration row must survive the ALTER");
        assert_eq!(rows[0].summary, "pre-migration row");
        assert_eq!(rows[0].source, MemorySource::Watcher);
        drop(s);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn fresh_and_migrated_schemas_are_identical() {
        let path = std::env::temp_dir().join(format!(
            "third-eye-migration-equiv-{}.db",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        pre_m008_db(&path);

        let migrated = MemoryStore::open(&path).unwrap();
        let fresh = MemoryStore::open_in_memory().unwrap();
        assert_eq!(
            migrated.column_info().unwrap(),
            fresh.column_info().unwrap(),
            "ALTER-migrated column layout must match a fresh CREATE TABLE"
        );
        drop(migrated);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn chat_source_round_trips_and_serializes_lowercase() {
        let s = store();
        let rec = s
            .insert(NewMemory {
                source: MemorySource::Chat,
                ..mem("asked to search for rust")
            })
            .unwrap();
        assert_eq!(rec.source, MemorySource::Chat);
        assert_eq!(s.get(rec.id).unwrap().source, MemorySource::Chat);
        let v = serde_json::to_value(&rec).unwrap();
        assert_eq!(v["source"], "chat");
    }

    #[test]
    fn unknown_stored_source_degrades_to_watcher() {
        let s = store();
        let rec = s.insert(mem("row with odd provenance")).unwrap();
        s.lock()
            .execute(
                "UPDATE memories SET source = 'mystery' WHERE id = ?1",
                params![rec.id],
            )
            .unwrap();
        assert_eq!(s.get(rec.id).unwrap().source, MemorySource::Watcher);
    }

    #[test]
    fn unembedded_rows_is_oldest_first_and_bounded() {
        let s = store();
        let a = s.insert(mem("first without vector")).unwrap();
        let b = s.insert(mem("second without vector")).unwrap();
        s.insert(NewMemory {
            embedding: Some(vec![1.0]),
            ..mem("has a vector")
        })
        .unwrap();

        let rows = s.unembedded_rows(10).unwrap();
        assert_eq!(
            rows,
            vec![
                (a.id, "first without vector".into()),
                (b.id, "second without vector".into())
            ]
        );
        assert_eq!(s.unembedded_rows(1).unwrap().len(), 1);
    }

    #[test]
    fn set_embedding_backfills_and_validates() {
        let s = store();
        let rec = s.insert(mem("backfill me")).unwrap();
        s.set_embedding(rec.id, &[0.5, -1.0]).unwrap();
        assert_eq!(s.embedded_rows().unwrap(), vec![(rec.id, vec![0.5, -1.0])]);
        assert!(s.unembedded_rows(10).unwrap().is_empty());

        assert_eq!(
            s.set_embedding(rec.id, &[]).unwrap_err().kind(),
            "invalid-input"
        );
        assert_eq!(
            s.set_embedding(999, &[1.0]).unwrap_err(),
            MemoryError::NotFound { id: 999 }
        );
    }

    #[test]
    fn embedded_rows_returns_only_rows_with_vectors() {
        let s = store();
        s.insert(mem("no vector here")).unwrap();
        let with = s
            .insert(NewMemory {
                embedding: Some(vec![0.5, -0.25, 1.0]),
                ..mem("vector attached")
            })
            .unwrap();
        let rows = s.embedded_rows().unwrap();
        assert_eq!(rows, vec![(with.id, vec![0.5, -0.25, 1.0])]);
    }
}
