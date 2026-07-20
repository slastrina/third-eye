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
    updated_at_ms INTEGER NOT NULL
);
CREATE VIRTUAL TABLE IF NOT EXISTS memories_fts USING fts5(
    summary,
    content='memories',
    content_rowid='id'
);
CREATE TRIGGER IF NOT EXISTS memories_fts_ai AFTER INSERT ON memories BEGIN
    INSERT INTO memories_fts(rowid, summary) VALUES (new.id, new.summary);
END;
CREATE TRIGGER IF NOT EXISTS memories_fts_ad AFTER DELETE ON memories BEGIN
    INSERT INTO memories_fts(memories_fts, rowid, summary)
        VALUES ('delete', old.id, old.summary);
END;
CREATE TRIGGER IF NOT EXISTS memories_fts_au AFTER UPDATE OF summary ON memories BEGIN
    INSERT INTO memories_fts(memories_fts, rowid, summary)
        VALUES ('delete', old.id, old.summary);
    INSERT INTO memories_fts(rowid, summary) VALUES (new.id, new.summary);
END;
";

const RECORD_COLUMNS: &str =
    "id, summary, apps, span_start_ms, span_end_ms, created_at_ms, updated_at_ms";

impl MemoryStore {
    /// Open (creating if needed) the single memory db at `path`. Creates
    /// parent directories, switches to WAL, enables foreign keys, and runs
    /// the schema idempotently. The path is logged so the on-disk location
    /// is always greppable at startup.
    pub fn open(path: &Path) -> Result<Self, MemoryError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| MemoryError::Db { detail: format!("create {parent:?}: {e}") })?;
        }
        let conn = Connection::open(path)
            .map_err(|e| MemoryError::Db { detail: format!("open {path:?}: {e}") })?;
        // journal_mode returns the resulting mode as a row; query it rather
        // than execute so the statement is consumed either way.
        let mode: String =
            conn.query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))?;
        conn.execute_batch("PRAGMA foreign_keys=ON;")?;
        conn.execute_batch(SCHEMA)?;
        log::info!("memory: db open at {} (journal_mode={mode})", path.display());
        Ok(Self { conn: Mutex::new(conn), path: Some(path.to_path_buf()) })
    }

    /// In-memory store for tests: same schema and pragmas, no file.
    pub fn open_in_memory() -> Result<Self, MemoryError> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch("PRAGMA foreign_keys=ON;")?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn: Mutex::new(conn), path: None })
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
        let apps_json = serde_json::to_string(&new.apps)
            .map_err(|e| MemoryError::InvalidInput { detail: format!("apps: {e}") })?;
        let embedding_json = new
            .embedding
            .as_ref()
            .map(|v| serde_json::to_string(v))
            .transpose()
            .map_err(|e| MemoryError::InvalidInput { detail: format!("embedding: {e}") })?;
        let now = now_ms();
        let conn = self.lock();
        conn.execute(
            "INSERT INTO memories
                (summary, apps, span_start_ms, span_end_ms, embedding,
                 created_at_ms, updated_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
            params![
                new.summary,
                apps_json,
                new.span_start_ms,
                new.span_end_ms,
                embedding_json,
                now
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
        let changed = conn.execute(
            "UPDATE memories
             SET summary = ?1, embedding = NULL, updated_at_ms = ?2
             WHERE id = ?3",
            params![summary, now, id],
        )?;
        if changed == 0 {
            return Err(MemoryError::NotFound { id });
        }
        log::info!("memory: updated row {id} (embedding cleared)");
        Self::get_on(&conn, id)
    }

    pub fn delete(&self, id: i64) -> Result<(), MemoryError> {
        let changed = self.lock().execute("DELETE FROM memories WHERE id = ?1", params![id])?;
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

    pub fn count(&self) -> Result<usize, MemoryError> {
        let n: i64 =
            self.lock().query_row("SELECT COUNT(*) FROM memories", [], |row| row.get(0))?;
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
            .query_map([], |row| Ok((row.get::<_, String>(1)?, row.get::<_, String>(2)?)))?
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
            return Err(MemoryError::InvalidInput { detail: "embedding is empty".into() });
        }
        let json = serde_json::to_string(embedding)
            .map_err(|e| MemoryError::InvalidInput { detail: format!("embedding: {e}") })?;
        let changed = self
            .lock()
            .execute("UPDATE memories SET embedding = ?1 WHERE id = ?2", params![json, id])?;
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
        return Err(MemoryError::InvalidInput { detail: "summary is empty".into() });
    }
    Ok(())
}

/// Milliseconds since the Unix epoch. Saturates at 0 if the clock is set
/// before 1970 rather than panicking.
fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
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
        }
    }

    fn store() -> MemoryStore {
        MemoryStore::open_in_memory().unwrap()
    }

    #[test]
    fn open_records_db_path_and_in_memory_has_none() {
        assert_eq!(store().db_path(), None);

        let path = std::env::temp_dir()
            .join(format!("third-eye-store-path-test-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let s = MemoryStore::open(&path).unwrap();
        assert_eq!(s.db_path(), Some(path.as_path()));
        drop(s);
        let _ = std::fs::remove_file(&path);
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
        assert_eq!(s.get(rec.id).unwrap_err(), MemoryError::NotFound { id: rec.id });
        assert_eq!(s.count().unwrap(), 0);
    }

    #[test]
    fn list_is_newest_first_and_pages() {
        let s = store();
        // Same created_at_ms is possible within one test run — id DESC is
        // the tiebreak, so insertion order still reverses deterministically.
        let ids: Vec<i64> =
            (1..=5).map(|i| s.insert(mem(&format!("memory number {i}"))).unwrap().id).collect();
        let page1 = s.list(2, 0).unwrap();
        assert_eq!(page1.iter().map(|r| r.id).collect::<Vec<_>>(), vec![ids[4], ids[3]]);
        let page2 = s.list(2, 2).unwrap();
        assert_eq!(page2.iter().map(|r| r.id).collect::<Vec<_>>(), vec![ids[2], ids[1]]);
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
            .insert(NewMemory { embedding: Some(vec![0.1, 0.2]), ..mem("draft summary") })
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
        assert_eq!(s.update_summary(99, "x").unwrap_err(), MemoryError::NotFound { id: 99 });
    }

    #[test]
    fn blank_summaries_are_invalid_input() {
        let s = store();
        for bad in ["", "   ", "\n\t "] {
            assert_eq!(s.insert(mem(bad)).unwrap_err().kind(), "invalid-input");
        }
        let rec = s.insert(mem("real summary")).unwrap();
        assert_eq!(s.update_summary(rec.id, "  ").unwrap_err().kind(), "invalid-input");
        // Failed update left the row untouched.
        assert_eq!(s.get(rec.id).unwrap().summary, "real summary");
    }

    #[test]
    fn keyword_search_ranks_topical_row_first() {
        let s = store();
        s.insert(mem("debugged the tokio runtime deadlock in the watcher loop")).unwrap();
        let rust = s.insert(mem("refactored rust error taxonomy for the llm module")).unwrap();
        s.insert(mem("planned the quarterly team offsite agenda")).unwrap();

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
            .query_map([], |row| Ok((row.get::<_, String>(1)?, row.get::<_, String>(2)?)))
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
                "updated_at_ms"
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
        let mode: String =
            s.lock().query_row("PRAGMA journal_mode", [], |row| row.get(0)).unwrap();
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
            .insert(NewMemory { embedding: Some(vec![1.0]), ..mem("shape pin") })
            .unwrap();
        let v = serde_json::to_value(&rec).unwrap();
        let mut keys: Vec<&str> =
            v.as_object().unwrap().keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec![
                "apps",
                "createdAtMs",
                "id",
                "spanEndMs",
                "spanStartMs",
                "summary",
                "updatedAtMs"
            ]
        );
    }

    #[test]
    fn unembedded_rows_is_oldest_first_and_bounded() {
        let s = store();
        let a = s.insert(mem("first without vector")).unwrap();
        let b = s.insert(mem("second without vector")).unwrap();
        s.insert(NewMemory { embedding: Some(vec![1.0]), ..mem("has a vector") }).unwrap();

        let rows = s.unembedded_rows(10).unwrap();
        assert_eq!(
            rows,
            vec![(a.id, "first without vector".into()), (b.id, "second without vector".into())]
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

        assert_eq!(s.set_embedding(rec.id, &[]).unwrap_err().kind(), "invalid-input");
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
