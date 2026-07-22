//! The memory IPC surface (T04): search/list/update/delete/wipe commands
//! plus `memory_status` health-as-value.
//!
//! Every command is a thin wrapper over [`MemoryStore`] / [`search`] —
//! no new store logic lives here (the S02 contract for S03 tool-calling and
//! the S04 memory view). Fallible commands reject with the kind-tagged
//! [`MemoryError`] JSON (`db` / `not-found` / `invalid-input`);
//! `memory_status` never rejects — a missing store is data
//! (`available: false`), not an error, so the UI can always poll it.
//!
//! Limits are clamped server-side (Q6): a hostile or buggy caller cannot
//! request an unbounded page out of SQLite.

use std::sync::Arc;

use serde::Serialize;
use tauri::{Manager, State};

use crate::llm::commands::LlmState;
use crate::llm::LlmClient;

use super::embed::{search, Embedder, SearchOutcome};
use super::store::{MemoryRecord, MemoryStore};
use super::{ChatIngestStatus, IngestStatus, MemoryError, MemoryState};

/// Search returns a short ranked shortlist by default — recall quality
/// drops off fast past the top handful and S03 feeds these to a small
/// model's context.
pub const DEFAULT_SEARCH_LIMIT: usize = 8;
/// Hard ceiling on one search's result set, whatever the caller asks for.
pub const MAX_SEARCH_LIMIT: usize = 50;
/// Default page size for `memory_list` (the S04 browse view).
pub const DEFAULT_LIST_LIMIT: usize = 100;
/// Hard ceiling on one list page — deeper browsing pages via `offset`.
pub const MAX_LIST_LIMIT: usize = 500;

/// `memory_status` payload — health-as-value, camelCase on the wire.
/// `available: false` means the store never opened this run (the open
/// failure was logged at startup); `store_error` carries a count/read
/// failure on an otherwise open store. Ingest health rides along so one
/// poll answers "is memory working?" end to end.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryStatus {
    pub available: bool,
    pub count: Option<usize>,
    pub db_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub store_error: Option<MemoryError>,
    pub ingest: IngestStatus,
    pub chat_ingest: ChatIngestStatus,
}

/// The one rejection every fallible command shares when the store never
/// opened (setup not run, or the open failed and was logged at startup).
fn unavailable() -> MemoryError {
    MemoryError::Db {
        detail: "memory store unavailable (open failed at startup or setup has not run)".into(),
    }
}

/// Resolve the store or reject with the shared `unavailable` error, logged
/// by kind so a refused command is visible in the logs, not just in the
/// caller's rejection.
fn require_store(state: &MemoryState) -> Result<Arc<MemoryStore>, MemoryError> {
    state.store().ok_or_else(|| {
        let err = unavailable();
        log::error!("memory: command refused ({}): {err}", err.kind());
        err
    })
}

/// Clamp a caller-supplied limit: absent → `default`, oversized → `max`.
fn clamp_limit(requested: Option<usize>, default: usize, max: usize) -> usize {
    requested.unwrap_or(default).min(max)
}

/// Assemble the status snapshot. Pure over its inputs so the never-rejects
/// contract is unit-testable without a Tauri app.
pub fn build_status(
    store: Option<Arc<MemoryStore>>,
    ingest: IngestStatus,
    chat_ingest: ChatIngestStatus,
) -> MemoryStatus {
    match store {
        None => MemoryStatus {
            available: false,
            count: None,
            db_path: None,
            store_error: None,
            ingest,
            chat_ingest,
        },
        Some(store) => {
            let db_path = store.db_path().map(|p| p.display().to_string());
            match store.count() {
                Ok(count) => MemoryStatus {
                    available: true,
                    count: Some(count),
                    db_path,
                    store_error: None,
                    ingest,
                    chat_ingest,
                },
                Err(err) => {
                    log::error!("memory: status count failed ({}): {err}", err.kind());
                    MemoryStatus {
                        available: true,
                        count: None,
                        db_path,
                        store_error: Some(err),
                        ingest,
                        chat_ingest,
                    }
                }
            }
        }
    }
}

/// Hybrid semantic/keyword recall (T02) over IPC. The [`SearchOutcome`]
/// carries the ranking mode and, on a degrade, the typed reason — embedding
/// failures never reject, only store failures do.
#[tauri::command]
pub async fn memory_search(
    memory: State<'_, MemoryState>,
    llm: State<'_, LlmState>,
    query: String,
    limit: Option<usize>,
) -> Result<SearchOutcome, MemoryError> {
    let store = require_store(&memory)?;
    let router = llm.router();
    let embedder: Arc<dyn Embedder> = memory.embedder(router.endpoint());
    let limit = clamp_limit(limit, DEFAULT_SEARCH_LIMIT, MAX_SEARCH_LIMIT);
    search(&store, embedder.as_ref(), &query, limit).await
}

/// Newest-first page of stored memories (the S04 browse view).
#[tauri::command]
pub fn memory_list(
    memory: State<'_, MemoryState>,
    limit: Option<usize>,
    offset: Option<usize>,
) -> Result<Vec<MemoryRecord>, MemoryError> {
    let store = require_store(&memory)?;
    store.list(clamp_limit(limit, DEFAULT_LIST_LIMIT, MAX_LIST_LIMIT), offset.unwrap_or(0))
}

/// Replace one memory's summary text. The store bumps `updated_at_ms`,
/// clears the stale embedding (re-embedded lazily on the next search) and
/// logs the mutation.
#[tauri::command]
pub fn memory_update(
    memory: State<'_, MemoryState>,
    id: i64,
    summary: String,
) -> Result<MemoryRecord, MemoryError> {
    require_store(&memory)?.update_summary(id, &summary)
}

/// Delete one memory by id (`not-found` when the id misses).
#[tauri::command]
pub fn memory_delete(memory: State<'_, MemoryState>, id: i64) -> Result<(), MemoryError> {
    require_store(&memory)?.delete(id)
}

/// Delete every stored memory, returning how many rows were removed —
/// the R012 "wipe it all" control.
#[tauri::command]
pub fn memory_wipe(memory: State<'_, MemoryState>) -> Result<usize, MemoryError> {
    require_store(&memory)?.wipe()
}

/// Result of a chat-memory toggle — the same health-as-value shape as
/// `capture::PrivacyStatus`: a persist failure is data the caller renders,
/// not a rejection. `error` serializes as an explicit `null` when absent so
/// the TS contract is `error: string | null`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatMemoryStatus {
    pub enabled: bool,
    pub error: Option<String>,
}

/// The one shared chat-memory applier (S03 T02): every entry point —
/// `set_chat_memory_enabled` IPC (`via = "ipc"`) now, tray later — funnels
/// through here so the enabled atomic has exactly one mutation site outside
/// tests (MEM053/MEM049, privacy-mode precedent). Persists to
/// settings.json; on persist failure the atomic is rolled back (an
/// unpersisted flip must never silently revert on restart) and the error
/// naming the persist path returns as data.
pub fn apply_chat_memory_enabled(
    app: &tauri::AppHandle,
    desired: bool,
    via: &str,
) -> ChatMemoryStatus {
    let state = app.state::<MemoryState>();
    let chat = state.chat_ingest();
    let previous = chat.enabled();
    chat.set_enabled(desired);
    match crate::config::save_chat_memory_enabled(app, desired) {
        Ok(()) => {
            log::info!("memory: chat memory enabled={desired} via={via}");
            ChatMemoryStatus { enabled: desired, error: None }
        }
        Err(e) => {
            chat.set_enabled(previous);
            log::error!("memory: {e}");
            ChatMemoryStatus { enabled: previous, error: Some(e) }
        }
    }
}

/// Set the chat-memory toggle from the UI (S03, R032). Never rejects — the
/// resulting [`ChatMemoryStatus`] carries any persist failure as data, same
/// contract as `set_privacy_mode`.
#[tauri::command]
pub fn set_chat_memory_enabled(app: tauri::AppHandle, enable: bool) -> ChatMemoryStatus {
    apply_chat_memory_enabled(&app, enable, "ipc")
}

/// Memory health snapshot — never rejects (health-as-value, R006). Safe to
/// poll from any surface.
#[tauri::command]
pub fn memory_status(memory: State<'_, MemoryState>) -> MemoryStatus {
    let status = build_status(memory.store(), memory.ingest().status(), memory.chat_ingest().status());
    log::debug!(
        "memory: status available={} count={:?} buffered={}",
        status.available,
        status.count,
        status.ingest.buffered
    );
    status
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::LlmError;
    use crate::memory::store::NewMemory;
    use async_trait::async_trait;

    fn empty_ingest() -> IngestStatus {
        IngestStatus {
            buffered: 0,
            distilled_count: 0,
            last_distill_at_ms: None,
            last_error: None,
        }
    }

    fn empty_chat_ingest() -> ChatIngestStatus {
        ChatIngestStatus { buffered: 0, ingested_count: 0, last_error: None, enabled: true }
    }

    fn seeded_store() -> Arc<MemoryStore> {
        let store = MemoryStore::open_in_memory().unwrap();
        store
            .insert(NewMemory {
                summary: "Debugged the tokio broadcast lag in the watcher loop".into(),
                apps: vec!["Zed".into()],
                span_start_ms: 1_000,
                span_end_ms: 2_000,
                embedding: None,
                source: crate::memory::store::MemorySource::Watcher,
            })
            .unwrap();
        Arc::new(store)
    }

    /// Embedder that always fails offline — forces the keyword degrade.
    struct DownEmbedder;

    #[async_trait]
    impl Embedder for DownEmbedder {
        fn endpoint(&self) -> &str {
            "http://localhost:0"
        }

        async fn embed(&self, _texts: &[String]) -> Result<Vec<Vec<f32>>, LlmError> {
            Err(LlmError::Offline { endpoint: self.endpoint().into(), detail: "down".into() })
        }
    }

    #[test]
    fn status_without_store_is_a_value_not_an_error() {
        let status = build_status(None, empty_ingest(), empty_chat_ingest());
        assert!(!status.available);
        assert_eq!(status.count, None);
        assert_eq!(status.db_path, None);
        assert_eq!(status.store_error, None);
    }

    #[test]
    fn status_with_store_reports_count() {
        let status = build_status(Some(seeded_store()), empty_ingest(), empty_chat_ingest());
        assert!(status.available);
        assert_eq!(status.count, Some(1));
        // In-memory stores have no file path; the on-disk path shape is
        // covered by the db_path accessor test in store.rs.
        assert_eq!(status.db_path, None);
        assert_eq!(status.store_error, None);
    }

    #[test]
    fn status_json_is_camel_case_with_nested_ingest() {
        // S04 reads these exact keys; a change here is a breaking IPC change.
        let status = build_status(Some(seeded_store()), empty_ingest(), empty_chat_ingest());
        let v = serde_json::to_value(&status).unwrap();
        assert_eq!(v["available"], true);
        assert_eq!(v["count"], 1);
        assert!(v["dbPath"].is_null());
        assert!(v.get("storeError").is_none(), "None storeError must be omitted");
        assert_eq!(v["ingest"]["buffered"], 0);
        assert_eq!(v["ingest"]["distilledCount"], 0);
        assert!(v["ingest"]["lastDistillAtMs"].is_null());
        assert!(v.get("chatIngest").is_some(), "chatIngest must ride alongside ingest");
    }

    #[test]
    fn chat_ingest_block_serializes_camel_case_with_expected_fields() {
        // Additive IPC contract: chatIngest carries the same health-as-value
        // shape as ingest, and its typed error surfaces when present.
        let chat = ChatIngestStatus {
            buffered: 2,
            ingested_count: 5,
            last_error: Some(LlmError::Offline {
                endpoint: "http://localhost:0".into(),
                detail: "down".into(),
            }),
            enabled: false,
        };
        let status = build_status(None, empty_ingest(), chat);
        let v = serde_json::to_value(&status).unwrap();
        assert_eq!(v["chatIngest"]["buffered"], 2);
        assert_eq!(v["chatIngest"]["ingestedCount"], 5);
        assert_eq!(v["chatIngest"]["lastError"]["kind"], "offline");
        // S03 additive field: the exact wire key is "enabled".
        assert_eq!(v["chatIngest"]["enabled"], false);
        assert!(v["chatIngest"].get("enabled").is_some(), "wire key must be \"enabled\"");
    }

    #[test]
    fn chat_memory_status_serializes_camel_case_with_explicit_null_error() {
        // The TS contract is `error: string | null` (mirrors PrivacyStatus):
        // None must serialize as an explicit null, never be omitted.
        let v = serde_json::to_value(ChatMemoryStatus { enabled: true, error: None }).unwrap();
        assert_eq!(v, serde_json::json!({ "enabled": true, "error": null }));
        let v = serde_json::to_value(ChatMemoryStatus {
            enabled: false,
            error: Some("failed to persist chatMemoryEnabled=false to /tmp/settings.json".into()),
        })
        .unwrap();
        assert_eq!(v["enabled"], false);
        assert!(v["error"].as_str().unwrap().contains("chatMemoryEnabled"));
    }

    #[test]
    fn chat_ingest_flip_then_rollback_restores_prior_value() {
        // The applier's rollback arm is AppHandle-bound; pin the state
        // mechanics it composes: set_enabled(desired) then, on persist
        // failure, set_enabled(previous) restores exactly the prior value
        // with counters untouched.
        let state = crate::memory::chat_ingest::ChatIngestState::new();
        let previous = state.enabled();
        assert!(previous, "default is ON (opt-out)");
        state.set_enabled(false);
        assert!(!state.enabled());
        state.set_enabled(previous);
        assert!(state.enabled(), "rollback restores the prior value");
        assert_eq!(state.status().ingested_count, 0, "counters untouched by the flip");
    }

    #[test]
    fn unavailable_error_is_db_kind() {
        let err = unavailable();
        assert_eq!(err.kind(), "db");
        let v = serde_json::to_value(&err).unwrap();
        assert_eq!(v["kind"], "db");
        assert!(v["detail"].as_str().unwrap().contains("unavailable"));
    }

    #[test]
    fn limits_clamp_to_default_and_max() {
        assert_eq!(clamp_limit(None, DEFAULT_SEARCH_LIMIT, MAX_SEARCH_LIMIT), 8);
        assert_eq!(clamp_limit(Some(3), DEFAULT_SEARCH_LIMIT, MAX_SEARCH_LIMIT), 3);
        assert_eq!(clamp_limit(Some(10_000), DEFAULT_SEARCH_LIMIT, MAX_SEARCH_LIMIT), 50);
        assert_eq!(clamp_limit(Some(0), DEFAULT_LIST_LIMIT, MAX_LIST_LIMIT), 0);
        assert_eq!(clamp_limit(Some(10_000), DEFAULT_LIST_LIMIT, MAX_LIST_LIMIT), 500);
    }

    #[tokio::test]
    async fn search_over_ipc_shape_degrades_visibly_when_embedder_is_down() {
        // The command body is `require_store` + clamp + this call; the
        // degrade contract it forwards is what S04 renders.
        let store = seeded_store();
        let outcome = search(&store, &DownEmbedder, "broadcast lag", 8).await.unwrap();
        let v = serde_json::to_value(&outcome).unwrap();
        assert_eq!(v["mode"], "keyword");
        assert_eq!(v["degradeReason"]["kind"], "offline");
        assert_eq!(v["results"][0]["summary"], store.get(1).unwrap().summary);
    }

    #[test]
    fn update_rejects_blank_summary_with_invalid_input() {
        let store = seeded_store();
        let err = store.update_summary(1, "   ").unwrap_err();
        assert_eq!(err.kind(), "invalid-input");
    }

    #[test]
    fn delete_missing_id_is_not_found() {
        let store = seeded_store();
        let err = store.delete(999).unwrap_err();
        assert_eq!(err, MemoryError::NotFound { id: 999 });
    }

    #[test]
    fn wipe_reports_removed_rows() {
        let store = seeded_store();
        assert_eq!(store.wipe().unwrap(), 1);
        assert_eq!(store.count().unwrap(), 0);
    }
}
