//! Local memory store (S02): the single storage seam everything else sits on.
//!
//! [`store::MemoryStore`] owns exactly one SQLite file
//! (`app_data_dir/memory.db`, WAL). The schema is structurally text/metadata
//! only — R011/R012 extract-and-discard is enforced at the storage layer and
//! pinned by tests, so no frame data can ever land on disk here. Ingestion
//! (T03), semantic recall (T02), the IPC surface (T04), S03 tool-calling and
//! the S04 memory view all consume this module without new store logic.
//!
//! Failure taxonomy mirrors [`crate::llm::LlmError`]: every failure maps to a
//! kind-tagged [`MemoryError`] (`db` / `not-found` / `invalid-input`) with
//! camelCase fields — the error half of the memory IPC contract.

pub mod chat_ingest;
pub mod commands;
pub mod embed;
pub mod ingest;
pub mod store;

pub use chat_ingest::{ChatIngestState, ChatIngestStatus};
pub use embed::{search, Embedder, OpenAiEmbedder, SearchMode, SearchOutcome};
pub use ingest::{IngestState, IngestStatus};
pub use store::{MemoryRecord, MemorySource, MemoryStore, NewMemory};

use std::sync::{Arc, OnceLock};

use serde::Serialize;

use crate::llm::guard::{GuardState, GuardedEmbedder};

/// File name of the single memory database under `app_data_dir`.
pub const DB_FILE_NAME: &str = "memory.db";

/// Managed memory state (lib.rs): the store handle plus the ingestion status
/// surface. The store is installed by [`ingest::spawn`] in `setup()` — the
/// db path needs the app data dir, which a pre-setup `.manage()` cannot
/// resolve — so `store()` is `None` until then (and stays `None` if the open
/// failed, which `memory_status` (T04) reports visibly).
pub struct MemoryState {
    store: OnceLock<Arc<MemoryStore>>,
    ingest: Arc<IngestState>,
    /// Chat-exchange ingestion health + retained-retry queue (M008 S01) —
    /// sibling of `ingest`, fed by the chat command's post-reply spawn.
    /// `store()` returning `None` means chat ingest silently no-ops.
    chat_ingest: Arc<ChatIngestState>,
    /// Search embedder, built lazily on the first `memory_search` against
    /// the router's (fixed-per-run) endpoint and reused after — one reqwest
    /// pool per run, not per search. Always the guarded wrapper (M003 S02):
    /// this accessor is the only embedder construction path production code
    /// can reach.
    embedder: OnceLock<Arc<dyn Embedder>>,
    /// Shared privacy-guard telemetry the guarded embedder records into.
    guard: Arc<GuardState>,
    /// The chat session new exchanges append to (computer-control I3).
    /// 0 = none yet — the first exchange creates one lazily; the
    /// `chat_new_session` IPC starts a fresh one on demand.
    current_chat_session: std::sync::atomic::AtomicI64,
}

impl Default for MemoryState {
    fn default() -> Self {
        Self::new(Arc::new(GuardState::new()))
    }
}

impl MemoryState {
    /// `guard` is the app-shared guard telemetry (lib.rs); tests that don't
    /// care use `MemoryState::default()`.
    pub fn new(guard: Arc<GuardState>) -> Self {
        Self {
            store: OnceLock::new(),
            ingest: Arc::new(IngestState::new()),
            chat_ingest: Arc::new(ChatIngestState::new()),
            embedder: OnceLock::new(),
            guard,
            current_chat_session: std::sync::atomic::AtomicI64::new(0),
        }
    }

    /// The session new exchanges append to, creating one lazily on first
    /// use. `None` when the store is unavailable or creation failed (logged
    /// — the exchange simply goes unrecorded, never a chat failure).
    pub fn ensure_chat_session(&self, store: &MemoryStore, now_ms: i64) -> Option<i64> {
        use std::sync::atomic::Ordering;
        let current = self.current_chat_session.load(Ordering::SeqCst);
        if current > 0 {
            return Some(current);
        }
        match store.chat_session_create(now_ms) {
            Ok(id) => {
                self.current_chat_session.store(id, Ordering::SeqCst);
                log::info!("memory: chat session {id} started (lazy)");
                Some(id)
            }
            Err(e) => {
                log::warn!("memory: chat session create failed: {e:?}");
                None
            }
        }
    }

    /// Point subsequent exchanges at a session (the `chat_new_session` IPC).
    pub fn set_current_chat_session(&self, id: i64) {
        self.current_chat_session
            .store(id, std::sync::atomic::Ordering::SeqCst);
    }

    /// Install the opened store, exactly once. Returns `false` when one is
    /// already installed (a duplicate-spawn wiring bug — the caller logs it;
    /// never a panic).
    pub fn init_store(&self, store: Arc<MemoryStore>) -> bool {
        self.store.set(store).is_ok()
    }

    /// The store, once `setup()` opened it. `None` means memory is
    /// unavailable this run (open failed or setup has not run).
    pub fn store(&self) -> Option<Arc<MemoryStore>> {
        self.store.get().cloned()
    }

    /// The ingestion health surface — shared with the ingest loop.
    pub fn ingest(&self) -> Arc<IngestState> {
        self.ingest.clone()
    }

    /// The chat-ingestion health surface — shared with the chat command's
    /// fire-and-forget ingest spawns (T03) and `memory_status` (T04).
    pub fn chat_ingest(&self) -> Arc<ChatIngestState> {
        self.chat_ingest.clone()
    }

    /// The shared search embedder, constructed against `endpoint` on first
    /// use. The endpoint is fixed for the run (the router caches it at
    /// boot), so first-caller-wins is correct here. The raw
    /// [`OpenAiEmbedder`] is wrapped in [`GuardedEmbedder`] at this single
    /// construction site (M003 S02), so every embedding consumer — search,
    /// nudge memory context, S03 tool loop — inherits the fail-closed guard.
    pub fn embedder(&self, endpoint: &str) -> Arc<dyn Embedder> {
        self.embedder
            .get_or_init(|| {
                Arc::new(GuardedEmbedder::new(
                    Arc::new(OpenAiEmbedder::new(endpoint)),
                    self.guard.clone(),
                ))
            })
            .clone()
    }
}

/// The memory failure taxonomy (R006). Serialized with a `kind` tag
/// (`db` / `not-found` / `invalid-input`) and camelCase fields — this JSON
/// shape is the error half of the memory IPC contract with the UI (S04).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(
    tag = "kind",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase"
)]
pub enum MemoryError {
    /// SQLite refused or failed the operation (I/O, corruption, constraint,
    /// bad state). Carries the underlying detail for logs and surfaces.
    Db { detail: String },
    /// The addressed row does not exist — carries the id that missed.
    NotFound { id: i64 },
    /// The caller supplied input the store rejects (e.g. an empty summary).
    InvalidInput { detail: String },
}

impl MemoryError {
    /// Stable machine-readable name, mirroring the serde `kind` tag. Used in
    /// error logs so grep for `db` / `not-found` / `invalid-input` works.
    pub fn kind(&self) -> &'static str {
        match self {
            MemoryError::Db { .. } => "db",
            MemoryError::NotFound { .. } => "not-found",
            MemoryError::InvalidInput { .. } => "invalid-input",
        }
    }
}

impl std::fmt::Display for MemoryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MemoryError::Db { detail } => write!(f, "memory db failure: {detail}"),
            MemoryError::NotFound { id } => write!(f, "memory {id} not found"),
            MemoryError::InvalidInput { detail } => {
                write!(f, "invalid memory input: {detail}")
            }
        }
    }
}

impl std::error::Error for MemoryError {}

impl From<rusqlite::Error> for MemoryError {
    fn from(e: rusqlite::Error) -> Self {
        MemoryError::Db {
            detail: e.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_json_shape_is_the_ipc_contract() {
        // S04 matches on `kind` and reads camelCase fields; a change here is
        // a breaking IPC change.
        let db = MemoryError::Db {
            detail: "disk I/O error".into(),
        };
        let v = serde_json::to_value(&db).unwrap();
        assert_eq!(v["kind"], "db");
        assert_eq!(v["detail"], "disk I/O error");

        let not_found = MemoryError::NotFound { id: 42 };
        let v = serde_json::to_value(&not_found).unwrap();
        assert_eq!(v["kind"], "not-found");
        assert_eq!(v["id"], 42);

        let invalid = MemoryError::InvalidInput {
            detail: "summary is empty".into(),
        };
        let v = serde_json::to_value(&invalid).unwrap();
        assert_eq!(v["kind"], "invalid-input");
        assert_eq!(v["detail"], "summary is empty");
    }

    #[test]
    fn error_kind_mirrors_serde_tag() {
        assert_eq!(
            MemoryError::Db {
                detail: String::new()
            }
            .kind(),
            "db"
        );
        assert_eq!(MemoryError::NotFound { id: 1 }.kind(), "not-found");
        assert_eq!(
            MemoryError::InvalidInput {
                detail: String::new()
            }
            .kind(),
            "invalid-input"
        );
    }

    #[test]
    fn error_display_names_the_failure() {
        assert!(MemoryError::NotFound { id: 7 }.to_string().contains('7'));
        assert!(MemoryError::Db {
            detail: "locked".into()
        }
        .to_string()
        .contains("locked"));
    }

    #[test]
    fn rusqlite_errors_map_to_db_kind() {
        let err: MemoryError = rusqlite::Error::InvalidQuery.into();
        assert_eq!(err.kind(), "db");
    }

    // --- M003 S02: the embedder accessor is the guarded construction path ---

    #[tokio::test]
    async fn memory_state_embedder_is_guarded_fail_closed_on_external_endpoint() {
        // The pinned Low-confidence condition must be blocked with the typed
        // guard error before any socket write — TEST-NET-1 has no listener,
        // so an actual connect attempt would surface as `offline` instead.
        let guard = Arc::new(GuardState::new());
        let state = MemoryState::new(guard.clone());
        let embedder = state.embedder("http://192.0.2.1:9");
        let err = embedder
            .embed(&["credit card: 4111 1111 1111 1112".to_string()])
            .await
            .unwrap_err();
        assert_eq!(err.kind(), "guard-blocked");
        assert_eq!(
            guard.blocked_count(),
            1,
            "the shared GuardState must record the block"
        );
    }

    #[test]
    fn memory_state_embedder_is_shared_across_calls() {
        // First-caller-wins caching must survive the guard wrap: one reqwest
        // pool per run, not per search.
        let state = MemoryState::default();
        let a = state.embedder("http://localhost:1234");
        let b = state.embedder("http://localhost:1234");
        assert!(Arc::ptr_eq(&a, &b));
        assert_eq!(a.endpoint(), "http://localhost:1234");
    }
}
