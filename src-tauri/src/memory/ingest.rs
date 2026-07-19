//! Ingestion pipeline (S02 T03): watcher observations → stored memories.
//!
//! Subscribes to [`crate::watcher::WatcherState`]'s observation broadcast
//! (the S01 seam), deterministically skips near-duplicate consecutive
//! observations, buffers the rest (bounded, drop-oldest), and distills each
//! full batch into 1–3 short text summaries via the thin lane — pinned
//! through [`ModelRouter::lane_client`], so distillation never rides the
//! lane the user is chatting on. Summaries land in [`MemoryStore`] with the
//! batch's app set and time span; embeddings backfill lazily at search time.
//!
//! Failure contract (R006): a distillation failure never crashes the loop
//! and never drops data silently — the buffer is retained for retry on the
//! next accepted observation, and the typed [`LlmError`] stays queryable on
//! [`IngestStatus`] (via `memory_status`, T04) until a distillation
//! succeeds and clears it.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use tauri::Manager;
use tokio::sync::broadcast::{self, error::RecvError};

use crate::llm::router::{ModelRouter, THIN_LANE};
use crate::llm::{ChatMessage, ChatRequest, LlmError};
use crate::watcher::TextObservation;

use super::store::{MemoryStore, NewMemory};
use super::{MemoryState, DB_FILE_NAME};

/// Buffered observations that trigger a distillation. At the watcher's ~5s
/// cadence a batch spans roughly 40s of active screen time.
pub const BATCH_SIZE: usize = 8;

/// Buffer bound (drop-oldest). While distillation keeps failing, at most
/// this many observations are retained for retry — memory stays flat no
/// matter how long LM Studio is down.
pub const BUFFER_CAP: usize = 64;

/// Word-set Jaccard similarity at or above this marks two consecutive
/// observations near-duplicates — a mostly-static screen produces one
/// buffered observation, not one per tick.
const NEAR_DUP_THRESHOLD: f64 = 0.85;

/// Per-observation cap on text forwarded to the distiller, so one dense
/// screen cannot blow the thin model's context (Q6: prompt size is bounded
/// by `BUFFER_CAP * SNIPPET_MAX_CHARS`).
const SNIPPET_MAX_CHARS: usize = 1500;

/// A batch distills into at most this many summaries, whatever the model
/// replies with.
const MAX_SUMMARIES_PER_BATCH: usize = 3;

/// Shared ingestion health, mutated only by the ingest loop and read by
/// `memory_status` (T04). Health-as-value: never an error, safe to poll.
pub struct IngestState {
    buffered: AtomicUsize,
    distilled_count: AtomicU64,
    last_distill_at_ms: Mutex<Option<i64>>,
    last_error: Mutex<Option<LlmError>>,
}

impl Default for IngestState {
    fn default() -> Self {
        Self {
            buffered: AtomicUsize::new(0),
            distilled_count: AtomicU64::new(0),
            last_distill_at_ms: Mutex::new(None),
            last_error: Mutex::new(None),
        }
    }
}

impl IngestState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Snapshot for `memory_status` — camelCase JSON, part of the S04 IPC
    /// contract.
    pub fn status(&self) -> IngestStatus {
        IngestStatus {
            buffered: self.buffered.load(Ordering::SeqCst),
            distilled_count: self.distilled_count.load(Ordering::SeqCst),
            last_distill_at_ms: *self.last_distill_at_ms.lock().unwrap(),
            last_error: self.last_error.lock().unwrap().clone(),
        }
    }

    fn set_buffered(&self, n: usize) {
        self.buffered.store(n, Ordering::SeqCst);
    }

    /// A distillation succeeded: count what was stored and clear any
    /// persisted failure — the error stays visible only until success.
    fn record_success(&self, stored: u64, at_ms: i64) {
        self.distilled_count.fetch_add(stored, Ordering::SeqCst);
        *self.last_distill_at_ms.lock().unwrap() = Some(at_ms);
        *self.last_error.lock().unwrap() = None;
    }

    fn record_failure(&self, err: LlmError) {
        *self.last_error.lock().unwrap() = Some(err);
    }
}

/// The ingest half of `memory_status` (T04). camelCase on the wire.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IngestStatus {
    pub buffered: usize,
    pub distilled_count: u64,
    pub last_distill_at_ms: Option<i64>,
    pub last_error: Option<LlmError>,
}

/// What [`IngestBuffer::push`] did with an observation — each variant gets
/// its own log line in the loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushOutcome {
    Accepted,
    /// Accepted, but the buffer was full and the oldest observation was
    /// dropped to make room.
    AcceptedDroppedOldest,
    /// Whitespace-only text — nothing to remember.
    SkippedEmpty,
    /// Near-duplicate of the newest buffered observation (static screen).
    SkippedDuplicate,
}

/// Dedup-on-entry, bounded, drop-oldest observation buffer. Pure and
/// synchronous so the dedupe and bounding behavior is unit-testable without
/// a runtime.
pub struct IngestBuffer {
    items: VecDeque<TextObservation>,
}

impl Default for IngestBuffer {
    fn default() -> Self {
        Self { items: VecDeque::new() }
    }
}

impl IngestBuffer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, obs: TextObservation) -> PushOutcome {
        if obs.text.trim().is_empty() {
            return PushOutcome::SkippedEmpty;
        }
        if let Some(last) = self.items.back() {
            if is_near_duplicate(&last.text, &obs.text) {
                return PushOutcome::SkippedDuplicate;
            }
        }
        let dropped = self.items.len() >= BUFFER_CAP;
        if dropped {
            self.items.pop_front();
        }
        self.items.push_back(obs);
        if dropped {
            PushOutcome::AcceptedDroppedOldest
        } else {
            PushOutcome::Accepted
        }
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn batch_ready(&self) -> bool {
        self.items.len() >= BATCH_SIZE
    }

    /// The whole retained buffer as one batch — after a failed distillation
    /// the retry covers everything still held, not just the newest slice.
    pub fn snapshot(&self) -> Vec<TextObservation> {
        self.items.iter().cloned().collect()
    }

    pub fn clear(&mut self) {
        self.items.clear();
    }
}

/// Deterministic near-duplicate test: Jaccard similarity over lowercased
/// word sets. No hashing seeds, no model calls — the same pair always
/// decides the same way.
pub fn is_near_duplicate(prev: &str, next: &str) -> bool {
    let a: std::collections::HashSet<String> =
        prev.split_whitespace().map(str::to_lowercase).collect();
    let b: std::collections::HashSet<String> =
        next.split_whitespace().map(str::to_lowercase).collect();
    if a.is_empty() && b.is_empty() {
        return true;
    }
    let intersection = a.intersection(&b).count();
    let union = a.len() + b.len() - intersection;
    intersection as f64 / union as f64 >= NEAR_DUP_THRESHOLD
}

/// The distillation prompt: one system instruction plus the batch as a
/// single user turn, each observation labeled with its app and truncated to
/// [`SNIPPET_MAX_CHARS`].
pub fn distill_messages(batch: &[TextObservation]) -> Vec<ChatMessage> {
    let mut body = String::new();
    for obs in batch {
        let app = obs.app_context.as_deref().unwrap_or("unknown app");
        body.push_str(&format!("[{app}]\n"));
        body.extend(obs.text.chars().take(SNIPPET_MAX_CHARS));
        body.push_str("\n---\n");
    }
    vec![
        ChatMessage::system(
            "You distill screen-activity observations into memory notes. \
             Reply with 1 to 3 lines. Each line is one standalone factual \
             sentence summarizing something the user was working on, \
             naming the concrete topic, project, or content. No preamble, \
             no numbering, no formatting — one summary per line.",
        ),
        ChatMessage::user(body),
    ]
}

/// Model reply → summaries: one per non-empty line, list markers stripped,
/// capped at [`MAX_SUMMARIES_PER_BATCH`].
pub fn parse_summaries(text: &str) -> Vec<String> {
    text.lines()
        .map(strip_list_marker)
        .filter(|l| !l.is_empty())
        .take(MAX_SUMMARIES_PER_BATCH)
        .map(String::from)
        .collect()
}

/// Tolerate models that number or bullet their lines despite instructions:
/// `- x`, `* x`, `• x`, `1. x`, `2) x` all yield `x`.
fn strip_list_marker(line: &str) -> &str {
    let l = line.trim().trim_start_matches(['-', '*', '•']).trim_start();
    let digits = l.chars().take_while(char::is_ascii_digit).count();
    if digits > 0 {
        if let Some(rest) =
            l[digits..].strip_prefix('.').or_else(|| l[digits..].strip_prefix(')'))
        {
            return rest.trim();
        }
    }
    l.trim_end()
}

/// Summaries → insertable rows: every summary of a batch shares the batch's
/// observed time span and its deduped, order-preserving app set. Embeddings
/// stay `None` — T02's search backfills them lazily.
pub fn batch_memories(batch: &[TextObservation], summaries: &[String]) -> Vec<NewMemory> {
    let span_start_ms = batch.iter().map(|o| o.captured_at).min().unwrap_or(0) as i64;
    let span_end_ms = batch.iter().map(|o| o.captured_at).max().unwrap_or(0) as i64;
    let mut apps: Vec<String> = Vec::new();
    for obs in batch {
        if let Some(app) = &obs.app_context {
            if !apps.contains(app) {
                apps.push(app.clone());
            }
        }
    }
    summaries
        .iter()
        .map(|summary| NewMemory {
            summary: summary.clone(),
            apps: apps.clone(),
            span_start_ms,
            span_end_ms,
            embedding: None,
        })
        .collect()
}

/// Open the store at `app_data_dir/memory.db`, install it on the managed
/// [`MemoryState`], and spawn the ingest loop over the watcher's broadcast.
/// Called once from `setup()` after `watcher::spawn_loop`. Every failure
/// path disables ingestion visibly (error log; the store stays `None` on
/// `MemoryState`, which `memory_status` reports) — never a panic.
pub fn spawn(app: &tauri::AppHandle) {
    let path = match app.path().app_data_dir() {
        Ok(dir) => dir.join(DB_FILE_NAME),
        Err(e) => {
            log::error!("memory: app data dir unresolved; ingestion disabled: {e}");
            return;
        }
    };
    let store = match MemoryStore::open(&path) {
        Ok(s) => Arc::new(s),
        Err(e) => {
            log::error!("memory: store open failed ({}); ingestion disabled: {e}", e.kind());
            return;
        }
    };
    let state = app.state::<MemoryState>();
    if !state.init_store(store.clone()) {
        log::error!("memory: store already initialized; duplicate ingest spawn ignored");
        return;
    }
    let ingest = state.ingest();
    let rx = app.state::<crate::watcher::WatcherState>().subscribe();
    let router = app.state::<crate::llm::commands::LlmState>().router();
    tauri::async_runtime::spawn(run_loop(rx, store, ingest, router));
}

/// The loop body: receive → dedupe/buffer → distill when a batch is ready.
/// Serial by design — at most one distillation is in flight, so a slow thin
/// model backpressures into the bounded buffer instead of piling up tasks.
/// Exits only when the observation channel closes (app shutdown).
pub async fn run_loop(
    mut rx: broadcast::Receiver<TextObservation>,
    store: Arc<MemoryStore>,
    ingest: Arc<IngestState>,
    router: Arc<ModelRouter>,
) {
    let mut buffer = IngestBuffer::new();
    loop {
        match rx.recv().await {
            Ok(obs) => {
                match buffer.push(obs) {
                    PushOutcome::Accepted => {}
                    PushOutcome::AcceptedDroppedOldest => log::warn!(
                        "memory: ingest buffer full ({BUFFER_CAP}); dropped oldest observation"
                    ),
                    PushOutcome::SkippedEmpty => {
                        log::debug!("memory: ingest skipped empty observation")
                    }
                    PushOutcome::SkippedDuplicate => log::debug!(
                        "memory: ingest dedupe skip (near-duplicate of newest buffered)"
                    ),
                }
                ingest.set_buffered(buffer.len());
                if buffer.batch_ready() {
                    distill_and_store(&mut buffer, &store, &ingest, &router).await;
                }
            }
            Err(RecvError::Lagged(n)) => {
                log::warn!("memory: ingest lagged behind the watcher; skipped {n} observations");
            }
            Err(RecvError::Closed) => {
                log::info!("memory: observation channel closed; ingest loop exiting");
                break;
            }
        }
    }
}

/// One distillation attempt over the whole retained buffer. Success stores
/// the summaries, clears the buffer, and clears any persisted error;
/// failure logs the typed kind, keeps the buffer for retry, and persists
/// the error on [`IngestState`] until a success clears it (R006).
async fn distill_and_store(
    buffer: &mut IngestBuffer,
    store: &Arc<MemoryStore>,
    ingest: &Arc<IngestState>,
    router: &Arc<ModelRouter>,
) {
    let batch = buffer.snapshot();
    // Snapshot per batch so an S07 runtime re-pin applies to the next
    // distillation without restarting the loop.
    let (model, client) = match router.lane_client(THIN_LANE) {
        Ok(lane) => lane,
        Err(e) => {
            log::error!("memory: ingest thin lane unavailable: {e}");
            return;
        }
    };
    log::info!(
        "memory: distillation start: {} observations via lane={THIN_LANE} model={model}",
        batch.len()
    );
    let messages = distill_messages(&batch);
    match client.stream_chat(&ChatRequest::new(messages), &|_| {}).await {
        Ok(outcome) => {
            let summaries = parse_summaries(&outcome.text);
            if summaries.is_empty() {
                // An empty-but-successful reply is a model quality issue,
                // not an LlmError; retrying the same batch would loop
                // forever, so drop it visibly instead.
                log::warn!(
                    "memory: distillation returned no parseable summaries \
                     ({} chars); dropping batch of {}",
                    outcome.text.len(),
                    batch.len()
                );
            }
            let mut stored = 0u64;
            for memory in batch_memories(&batch, &summaries) {
                match store.insert(memory) {
                    Ok(_) => stored += 1,
                    Err(e) => {
                        log::error!("memory: ingest insert failed ({}): {e}", e.kind())
                    }
                }
            }
            log::info!(
                "memory: distillation done: {stored} summaries stored via \
                 lane={THIN_LANE} model={model}"
            );
            buffer.clear();
            ingest.set_buffered(0);
            ingest.record_success(stored, now_ms());
        }
        Err(err) => {
            log::error!(
                "memory: distillation failed ({}): {err}; retaining {} buffered observations",
                err.kind(),
                batch.len()
            );
            ingest.record_failure(err);
        }
    }
}

/// Milliseconds since the Unix epoch — `IngestStatus.last_distill_at_ms`.
fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::router::Lane;
    use crate::llm::{LlmClient, LlmHealth, StreamOutcome, TokenSink};
    use crate::watcher::WatcherState;
    use async_trait::async_trait;

    fn obs(text: &str, app: Option<&str>, at: u64) -> TextObservation {
        TextObservation {
            text: text.into(),
            app_context: app.map(Into::into),
            captured_at: at,
        }
    }

    /// Distinct-enough texts: consecutive pairs fall well under the
    /// near-duplicate threshold.
    fn distinct_text(i: usize) -> String {
        format!("distinct topic {i}: working on file number {i} in module {i}")
    }

    /// Scripted lane client: fails its first `fail_first` calls with a typed
    /// offline error, then replies with `reply`. Records every call.
    struct ScriptedClient {
        reply: String,
        fail_first: usize,
        calls: AtomicUsize,
        last_messages: Mutex<Vec<ChatMessage>>,
    }

    impl ScriptedClient {
        fn ok(reply: &str) -> Arc<Self> {
            Arc::new(Self {
                reply: reply.into(),
                fail_first: 0,
                calls: AtomicUsize::new(0),
                last_messages: Mutex::new(Vec::new()),
            })
        }

        fn failing_then(fail_first: usize, reply: &str) -> Arc<Self> {
            Arc::new(Self {
                reply: reply.into(),
                fail_first,
                calls: AtomicUsize::new(0),
                last_messages: Mutex::new(Vec::new()),
            })
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl LlmClient for ScriptedClient {
        fn endpoint(&self) -> &str {
            "http://mock.invalid"
        }

        async fn stream_chat(
            &self,
            request: &ChatRequest,
            _on_token: TokenSink<'_>,
        ) -> Result<StreamOutcome, LlmError> {
            *self.last_messages.lock().unwrap() = request.messages.clone();
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            if call < self.fail_first {
                return Err(LlmError::Offline {
                    endpoint: self.endpoint().into(),
                    detail: "connection refused".into(),
                });
            }
            Ok(StreamOutcome { text: self.reply.clone(), token_count: 1, tool_calls: Vec::new() })
        }

        async fn health(&self) -> LlmHealth {
            LlmHealth { online: true, endpoint: self.endpoint().into() }
        }
    }

    fn thin_router(client: Arc<ScriptedClient>) -> Arc<ModelRouter> {
        Arc::new(ModelRouter::new(vec![Lane::new(
            THIN_LANE,
            Some("thin-test".into()),
            client,
        )]))
    }

    // --- dedupe ---

    #[test]
    fn identical_and_reordered_texts_are_near_duplicates() {
        assert!(is_near_duplicate("editing ingest.rs in zed", "editing ingest.rs in zed"));
        // Word-set comparison: order does not matter.
        assert!(is_near_duplicate("editing ingest.rs in zed", "in zed editing ingest.rs"));
    }

    #[test]
    fn small_change_on_a_static_screen_is_a_near_duplicate() {
        // A realistic mostly-static screen: plenty of shared text, one
        // changed token (a ticking counter).
        let a = "inbox 42 unread messages meeting notes budget review friday deadline \
                 project roadmap quarterly planning team sync agenda action items followup";
        let b = "inbox 43 unread messages meeting notes budget review friday deadline \
                 project roadmap quarterly planning team sync agenda action items followup";
        assert!(is_near_duplicate(a, b));
    }

    #[test]
    fn different_screens_are_not_near_duplicates() {
        assert!(!is_near_duplicate(&distinct_text(1), &distinct_text(2)));
        assert!(!is_near_duplicate("rust compiler output", "browser article about whales"));
    }

    #[test]
    fn near_duplicate_is_deterministic_and_symmetric() {
        let a = "one two three four five six seven eight nine ten";
        let b = "one two three four five six seven eight nine changed";
        for _ in 0..3 {
            assert_eq!(is_near_duplicate(a, b), is_near_duplicate(b, a));
        }
    }

    // --- buffer ---

    #[test]
    fn buffer_accepts_distinct_skips_duplicates_and_empties() {
        let mut buf = IngestBuffer::new();
        assert_eq!(buf.push(obs(&distinct_text(0), None, 1)), PushOutcome::Accepted);
        assert_eq!(buf.push(obs(&distinct_text(0), None, 2)), PushOutcome::SkippedDuplicate);
        assert_eq!(buf.push(obs("   \n\t ", None, 3)), PushOutcome::SkippedEmpty);
        assert_eq!(buf.push(obs(&distinct_text(1), None, 4)), PushOutcome::Accepted);
        assert_eq!(buf.len(), 2);
    }

    #[test]
    fn buffer_drops_oldest_at_cap() {
        let mut buf = IngestBuffer::new();
        for i in 0..BUFFER_CAP {
            assert_eq!(buf.push(obs(&distinct_text(i), None, i as u64)), PushOutcome::Accepted);
        }
        assert_eq!(
            buf.push(obs(&distinct_text(BUFFER_CAP), None, BUFFER_CAP as u64)),
            PushOutcome::AcceptedDroppedOldest
        );
        assert_eq!(buf.len(), BUFFER_CAP);
        let snapshot = buf.snapshot();
        assert_eq!(snapshot.first().unwrap().text, distinct_text(1), "oldest must be gone");
        assert_eq!(snapshot.last().unwrap().text, distinct_text(BUFFER_CAP));
    }

    #[test]
    fn batch_ready_at_batch_size() {
        let mut buf = IngestBuffer::new();
        for i in 0..BATCH_SIZE - 1 {
            buf.push(obs(&distinct_text(i), None, i as u64));
        }
        assert!(!buf.batch_ready());
        buf.push(obs(&distinct_text(BATCH_SIZE), None, 99));
        assert!(buf.batch_ready());
    }

    // --- prompt + parsing ---

    #[test]
    fn distill_messages_label_apps_and_truncate_long_text() {
        let long = "x".repeat(SNIPPET_MAX_CHARS * 2);
        let batch =
            vec![obs("short text", Some("Zed"), 1), obs(&long, None, 2)];
        let messages = distill_messages(&batch);
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, crate::llm::Role::System);
        let body = &messages[1].content;
        assert!(body.contains("[Zed]"), "app label missing: {body}");
        assert!(body.contains("[unknown app]"), "fallback label missing");
        assert!(body.contains("short text"));
        assert!(
            body.len() < SNIPPET_MAX_CHARS + 200,
            "long observation must be truncated (got {} chars)",
            body.len()
        );
    }

    #[test]
    fn parse_summaries_strips_markers_and_caps_at_three() {
        let reply = "- User edited the ingest pipeline in Rust.\n\
                     * User read tokio broadcast docs.\n\
                     1. User reviewed a SQLite schema.\n\
                     2) User answered email about budgets.\n";
        let summaries = parse_summaries(reply);
        assert_eq!(
            summaries,
            vec![
                "User edited the ingest pipeline in Rust.",
                "User read tokio broadcast docs.",
                "User reviewed a SQLite schema.",
            ]
        );
    }

    #[test]
    fn parse_summaries_keeps_plain_lines_and_drops_blanks() {
        let summaries = parse_summaries("\nFirst summary line.\n\n   \nSecond summary line.\n");
        assert_eq!(summaries, vec!["First summary line.", "Second summary line."]);
    }

    #[test]
    fn parse_summaries_of_empty_reply_is_empty() {
        assert!(parse_summaries("").is_empty());
        assert!(parse_summaries("  \n - \n").is_empty());
    }

    // --- batch → rows ---

    #[test]
    fn batch_memories_share_span_and_deduped_apps() {
        let batch = vec![
            obs("a", Some("Zed"), 300),
            obs("b", Some("Safari"), 100),
            obs("c", Some("Zed"), 200),
            obs("d", None, 250),
        ];
        let summaries = vec!["One.".to_string(), "Two.".to_string()];
        let rows = batch_memories(&batch, &summaries);
        assert_eq!(rows.len(), 2);
        for row in &rows {
            assert_eq!(row.span_start_ms, 100);
            assert_eq!(row.span_end_ms, 300);
            assert_eq!(row.apps, vec!["Zed", "Safari"], "apps deduped, order preserved");
            assert_eq!(row.embedding, None, "embeddings backfill lazily (T02)");
        }
        assert_eq!(rows[0].summary, "One.");
        assert_eq!(rows[1].summary, "Two.");
    }

    // --- status shape ---

    #[test]
    fn ingest_status_serializes_camel_case() {
        // memory_status (T04) and the S04 UI read exactly these keys; a
        // change here is a breaking IPC change.
        let state = IngestState::new();
        state.set_buffered(3);
        state.record_failure(LlmError::Offline {
            endpoint: "http://x:1".into(),
            detail: "down".into(),
        });
        let v = serde_json::to_value(state.status()).unwrap();
        assert_eq!(v["buffered"], 3);
        assert_eq!(v["distilledCount"], 0);
        assert!(v["lastDistillAtMs"].is_null());
        assert_eq!(v["lastError"]["kind"], "offline");

        state.record_success(2, 1234);
        let v = serde_json::to_value(state.status()).unwrap();
        assert_eq!(v["distilledCount"], 2);
        assert_eq!(v["lastDistillAtMs"], 1234);
        assert!(v["lastError"].is_null(), "success must clear the persisted error");
    }

    // --- loop end-to-end (mock lane, real store) ---

    #[tokio::test]
    async fn loop_distills_a_full_batch_into_the_store() {
        let watcher = WatcherState::new();
        let rx = watcher.subscribe();
        let store = Arc::new(MemoryStore::open_in_memory().unwrap());
        let ingest = Arc::new(IngestState::new());
        let client = ScriptedClient::ok(
            "User debugged the ingest pipeline in Rust.\nUser read tokio broadcast docs.",
        );
        let task =
            tokio::spawn(run_loop(rx, store.clone(), ingest.clone(), thin_router(client.clone())));

        for i in 0..BATCH_SIZE {
            watcher.publish(obs(&distinct_text(i), Some("Zed"), 1000 + i as u64));
        }
        drop(watcher); // closes the channel → the loop drains and exits
        task.await.unwrap();

        assert_eq!(client.calls(), 1, "one full batch → one distillation");
        assert_eq!(store.count().unwrap(), 2);
        let status = ingest.status();
        assert_eq!(status.distilled_count, 2);
        assert_eq!(status.buffered, 0);
        assert!(status.last_error.is_none());
        assert!(status.last_distill_at_ms.is_some());
        // The prompt actually carried the batch.
        let sent = client.last_messages.lock().unwrap().clone();
        assert!(sent[1].content.contains(&distinct_text(0)));
        assert!(sent[1].content.contains(&distinct_text(BATCH_SIZE - 1)));
    }

    #[tokio::test]
    async fn distill_failure_retains_buffer_and_persists_typed_error() {
        let watcher = WatcherState::new();
        let rx = watcher.subscribe();
        let store = Arc::new(MemoryStore::open_in_memory().unwrap());
        let ingest = Arc::new(IngestState::new());
        let client = ScriptedClient::failing_then(usize::MAX, "never");
        let task =
            tokio::spawn(run_loop(rx, store.clone(), ingest.clone(), thin_router(client.clone())));

        for i in 0..BATCH_SIZE {
            watcher.publish(obs(&distinct_text(i), None, i as u64));
        }
        drop(watcher);
        task.await.unwrap();

        assert_eq!(client.calls(), 1);
        assert_eq!(store.count().unwrap(), 0, "nothing stored on failure");
        let status = ingest.status();
        assert_eq!(status.buffered, BATCH_SIZE, "buffer retained for retry");
        assert_eq!(status.distilled_count, 0);
        assert_eq!(
            status.last_error.as_ref().map(|e| e.kind()),
            Some("offline"),
            "typed error must be queryable until a success clears it"
        );
    }

    #[tokio::test]
    async fn retry_after_failure_succeeds_and_clears_the_error() {
        let watcher = WatcherState::new();
        let rx = watcher.subscribe();
        let store = Arc::new(MemoryStore::open_in_memory().unwrap());
        let ingest = Arc::new(IngestState::new());
        let client = ScriptedClient::failing_then(1, "User recovered from an LM Studio outage.");
        let task =
            tokio::spawn(run_loop(rx, store.clone(), ingest.clone(), thin_router(client.clone())));

        // Batch fills → first distillation fails; the next accepted
        // observation retries over the retained (now larger) buffer.
        for i in 0..=BATCH_SIZE {
            watcher.publish(obs(&distinct_text(i), None, i as u64));
        }
        drop(watcher);
        task.await.unwrap();

        assert_eq!(client.calls(), 2, "fail once, retry once");
        assert_eq!(store.count().unwrap(), 1);
        let status = ingest.status();
        assert_eq!(status.buffered, 0);
        assert_eq!(status.distilled_count, 1);
        assert!(status.last_error.is_none(), "success must clear the persisted error");
        // The retry batch covered the retained observations, not just the new one.
        let sent = client.last_messages.lock().unwrap().clone();
        assert!(sent[1].content.contains(&distinct_text(0)));
    }

    #[tokio::test]
    async fn distillation_stays_pinned_to_thin_while_active_lane_is_heavy() {
        use crate::llm::router::HEAVY_LANE;
        let thin = ScriptedClient::ok("User worked in the thin lane.");
        let heavy = ScriptedClient::ok("wrong lane");
        let router = Arc::new(ModelRouter::new(vec![
            Lane::new(THIN_LANE, Some("thin-test".into()), thin.clone()),
            Lane::new(HEAVY_LANE, Some("heavy-test".into()), heavy.clone()),
        ]));
        router.set_active(HEAVY_LANE).unwrap();

        let watcher = WatcherState::new();
        let rx = watcher.subscribe();
        let store = Arc::new(MemoryStore::open_in_memory().unwrap());
        let ingest = Arc::new(IngestState::new());
        let task = tokio::spawn(run_loop(rx, store.clone(), ingest.clone(), router));
        for i in 0..BATCH_SIZE {
            watcher.publish(obs(&distinct_text(i), None, i as u64));
        }
        drop(watcher);
        task.await.unwrap();

        assert_eq!(thin.calls(), 1, "distillation must ride the thin lane");
        assert_eq!(heavy.calls(), 0, "the user's active lane must never see ingest traffic");
        assert_eq!(store.count().unwrap(), 1);
    }

    #[tokio::test]
    async fn empty_reply_drops_the_batch_without_error() {
        // A successful-but-unparseable reply must not retry forever: the
        // batch is dropped visibly and no error is persisted (it is a model
        // quality issue, not an LlmError).
        let watcher = WatcherState::new();
        let rx = watcher.subscribe();
        let store = Arc::new(MemoryStore::open_in_memory().unwrap());
        let ingest = Arc::new(IngestState::new());
        let client = ScriptedClient::ok("   \n  ");
        let task =
            tokio::spawn(run_loop(rx, store.clone(), ingest.clone(), thin_router(client.clone())));
        for i in 0..BATCH_SIZE {
            watcher.publish(obs(&distinct_text(i), None, i as u64));
        }
        drop(watcher);
        task.await.unwrap();

        assert_eq!(store.count().unwrap(), 0);
        let status = ingest.status();
        assert_eq!(status.buffered, 0, "batch dropped, not retained");
        assert!(status.last_error.is_none());
    }
}
