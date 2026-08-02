//! Chat exchange ingestion (M008 S01): completed chat exchanges → at most
//! one stored `source='chat'` memory each.
//!
//! Sibling of [`super::ingest`], deliberately NOT riding the watcher
//! observation broadcast (the nudge classifier subscribes there — chat text
//! would trigger out-of-context nudges). One completed request yields one
//! [`Exchange`] captured after the reply settles; distillation is
//! one-line-or-NOTHING on the thin lane, pinned through
//! [`ModelRouter::lane_client`] so it never rides the lane the user is
//! chatting on.
//!
//! Privacy contract (D029 mount): the exchange text passes through
//! [`crate::privacy::redact`] at capture — before an [`Exchange`] can exist
//! — exactly like `watcher::build_observation`. A redaction failure drops
//! the whole exchange (fail closed); this module offers no bypass path.
//!
//! Failure contract (R006): a distillation failure never reaches the reply
//! path and never drops data silently — the bounded queue is retained for
//! retry on the next completed exchange, and the typed [`LlmError`] stays
//! queryable on [`ChatIngestStatus`] (via `memory_status`, T04) until a
//! distillation succeeds and clears it.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

use crate::llm::router::{ModelRouter, THIN_LANE};
use crate::llm::toolloop::ToolEvent;
use crate::llm::{ChatMessage, ChatRequest, LlmClient, LlmError};
use crate::privacy::RedactionError;

use super::store::{MemorySource, MemoryStore, NewMemory};

/// Retained-retry queue bound (drop-oldest). While distillation keeps
/// failing, at most this many exchanges wait for retry — memory stays flat
/// no matter how long LM Studio is down.
pub const QUEUE_CAP: usize = 16;

/// Completed exchanges per rolling session summary (S02). The session
/// buffer is bounded here too (drop-oldest), so a stalled summary can
/// never grow memory past one session's worth of exchanges.
pub const SESSION_SUMMARY_THRESHOLD: usize = 5;

/// Cap on exchange text forwarded to the distiller, so one long reply
/// cannot blow the thin model's context (Q6: prompt size is bounded by
/// this constant per attempt).
const EXCHANGE_MAX_CHARS: usize = 6000;

/// Per-exchange cap inside the session summary prompt. Tighter than
/// [`EXCHANGE_MAX_CHARS`] because the session prompt carries up to
/// [`SESSION_SUMMARY_THRESHOLD`] exchanges at once (Q6: whole prompt is
/// bounded by `SESSION_SUMMARY_THRESHOLD * SESSION_EXCHANGE_MAX_CHARS`).
const SESSION_EXCHANGE_MAX_CHARS: usize = 2000;

/// The literal reply that marks an exchange too trivial to remember.
const NOTHING_TOKEN: &str = "NOTHING";

/// One completed chat exchange, ready to distill. Pure data, no handles.
/// `text` is the already-redacted composition of the user's ask, each tool
/// call paired with its verified outcome, and the final reply — composed
/// and redacted atomically in [`capture_exchange`], so raw (unredacted)
/// exchange content never outlives the capture call.
#[derive(Debug, Clone, PartialEq)]
pub struct Exchange {
    /// Redacted composed exchange text (ask + tool outcomes + reply).
    pub text: String,
    /// Capture timestamp (ms since epoch) — becomes the stored memory's
    /// span (a chat exchange is a point in time, not an observed span).
    pub captured_at_ms: i64,
}

/// Compose and redact one completed exchange. `None` means there is
/// nothing to remember (empty exchange) or redaction failed — the caller
/// drops the exchange either way, fail closed. Infallible on
/// model-controlled data: no unwraps, no panics — this sits adjacent to
/// the product's primary loop.
pub fn capture_exchange(user_ask: &str, events: &[ToolEvent], reply: &str) -> Option<Exchange> {
    capture_with_redactor(user_ask, events, reply, |text| {
        crate::privacy::redact(text).map(|outcome| outcome.text)
    })
}

/// [`capture_exchange`] with the redactor injected — the seam the
/// fail-closed unit test uses, since the real engine's error path is
/// operationally unreachable from input.
fn capture_with_redactor(
    user_ask: &str,
    events: &[ToolEvent],
    reply: &str,
    redact: impl Fn(&str) -> Result<String, RedactionError>,
) -> Option<Exchange> {
    if user_ask.trim().is_empty() && reply.trim().is_empty() && events.is_empty() {
        return None;
    }
    let composed = compose_exchange_text(user_ask, events, reply);
    match redact(&composed) {
        Ok(text) => {
            log::debug!("memory: chat exchange captured ({} chars)", text.len());
            Some(Exchange {
                text,
                captured_at_ms: now_ms(),
            })
        }
        Err(e) => {
            log::warn!(
                "memory: chat capture dropped exchange (redaction failed: {})",
                e.kind()
            );
            None
        }
    }
}

/// The pre-redaction composition: the ask, each tool call paired with its
/// result (ok / typed failure / no result when the run was cut short), and
/// the reply. Arguments JSON rides along verbatim — that is where the
/// typed search text lives for the recall demo.
fn compose_exchange_text(user_ask: &str, events: &[ToolEvent], reply: &str) -> String {
    let mut body = String::new();
    body.push_str("User asked: ");
    body.push_str(user_ask.trim());
    body.push('\n');
    for event in events {
        if let ToolEvent::Call(call) = event {
            let outcome = events.iter().find_map(|e| match e {
                ToolEvent::Result(r) if r.call_id == call.call.id => Some(r),
                _ => None,
            });
            let outcome_text = match outcome {
                Some(r) if r.ok => "ok".to_string(),
                Some(r) => {
                    format!("failed ({})", r.failure.as_deref().unwrap_or("unknown"))
                }
                None => "no result".to_string(),
            };
            body.push_str(&format!(
                "Tool {} called with arguments {} -> {}\n",
                call.call.name, call.call.arguments, outcome_text
            ));
        }
    }
    body.push_str("Assistant replied: ");
    body.push_str(reply.trim());
    body
}

/// The distillation prompt: one system instruction plus the (already
/// redacted, size-capped) exchange as a single user turn.
pub fn distill_messages(exchange: &Exchange) -> Vec<ChatMessage> {
    let body: String = exchange.text.chars().take(EXCHANGE_MAX_CHARS).collect();
    vec![
        ChatMessage::system(
            "You distill one completed chat exchange into a memory note. \
             Reply with exactly ONE line: a standalone factual sentence \
             saying what the user asked and what verifiably happened, \
             naming the concrete search terms, apps, or content involved. \
             If the exchange is trivial (a greeting, small talk, nothing \
             was done), reply with exactly the single word NOTHING. If the \
             exchange revealed a DURABLE fact about the user (identity, a \
             stable preference), start the line with ! so it is kept \
             permanently. No preamble, no numbering, no formatting.",
        ),
        ChatMessage::user(body),
    ]
}

/// Model reply → at most ONE summary. `None` means the model judged the
/// exchange trivial (the literal `NOTHING`) or replied with nothing
/// parseable — both are successes that store nothing, never retries
/// (retrying an empty parse loops forever; mirrors the watcher ingest's
/// drop policy).
/// Whether a distilled line carries the durable-fact pin marker; returns
/// the marker-stripped line alongside.
pub fn split_pin_marker(line: &str) -> (bool, String) {
    match line.trim().strip_prefix('!') {
        Some(rest) => (true, rest.trim().to_string()),
        None => (false, line.trim().to_string()),
    }
}

pub fn parse_distilled(text: &str) -> Option<String> {
    let first = super::ingest::parse_summaries(text).into_iter().next()?;
    if first
        .trim()
        .trim_end_matches('.')
        .eq_ignore_ascii_case(NOTHING_TOKEN)
    {
        return None;
    }
    Some(first)
}

/// The rolling session-summary prompt (S02): the buffered session's
/// exchanges as distinct labeled parts (`[exchange N]`) of ONE
/// conversation, so the single output line names the conversation's themes
/// and verified outcomes rather than one exchange's. Reuses S01's
/// one-line-or-NOTHING contract — the reply parses through
/// [`parse_distilled`] (cap 1, NOTHING/empty = success storing nothing).
/// Labeled-sections precedent: the watcher's
/// [`super::ingest::distill_messages`]. Each exchange's text is capped at
/// [`SESSION_EXCHANGE_MAX_CHARS`], bounding the whole prompt at
/// `SESSION_SUMMARY_THRESHOLD * SESSION_EXCHANGE_MAX_CHARS` body chars.
pub fn compose_session_summary_prompt(exchanges: &[Exchange]) -> Vec<ChatMessage> {
    let mut body = String::new();
    for (i, exchange) in exchanges.iter().enumerate() {
        body.push_str(&format!("[exchange {}]\n", i + 1));
        body.extend(exchange.text.chars().take(SESSION_EXCHANGE_MAX_CHARS));
        body.push_str("\n---\n");
    }
    vec![
        ChatMessage::system(
            "You summarize one chat session between a user and an \
             assistant. The exchanges below are consecutive parts of ONE \
             conversation, oldest first. Reply with exactly ONE line: a \
             standalone factual sentence naming the conversation's main \
             themes and what verifiably happened, with the concrete \
             topics, apps, or content involved. If the whole session is \
             trivial (greetings, small talk, nothing was done), reply \
             with exactly the single word NOTHING. No preamble, no \
             numbering, no formatting.",
        ),
        ChatMessage::user(body),
    ]
}

/// Shared chat-ingestion health plus the bounded retained-retry queue.
/// Health-as-value: never an error, safe to poll from `memory_status`.
pub struct ChatIngestState {
    /// The user's chat-memory toggle (S03, R032). Read by the pre-capture
    /// gate in `llm::commands` on every completed exchange; mutated only by
    /// the T02 applier (plus startup restore). Arc + atomic is sufficient —
    /// no lock (research constraint): the gate tolerates seeing either side
    /// of a concurrent flip.
    enabled: AtomicBool,
    ingested_count: AtomicU64,
    last_error: Mutex<Option<LlmError>>,
    /// Fired with the stored count after an ingest stores >0 memories —
    /// setup installs the tray's "noted" flash here (guard-notifier
    /// pattern: macOS tray code stays out of this module, and app-less
    /// test call sites construct the state raw).
    on_stored: Mutex<Option<super::ingest::StoredHook>>,
    /// Exchanges awaiting distillation, retained across failures for
    /// retry. Bounded at [`QUEUE_CAP`], drop-oldest.
    queue: Mutex<VecDeque<Exchange>>,
    /// The current session's already-processed exchanges, accumulated for
    /// the rolling session summary (S02). Distinct from `queue`: an
    /// exchange enters here exactly once, when its per-exchange distill
    /// settles (stored or trivial-NOTHING) — never on a retained failure,
    /// which would re-enter it on retry. Bounded at
    /// [`SESSION_SUMMARY_THRESHOLD`], drop-oldest.
    session: Mutex<VecDeque<Exchange>>,
    /// Serializes concurrent [`ingest_exchange`] spawns — two chat
    /// requests completing back-to-back must not double-distill the same
    /// queued exchange.
    process: tokio::sync::Mutex<()>,
}

impl Default for ChatIngestState {
    fn default() -> Self {
        Self {
            // Default ON (opt-out): chat recall is the milestone's core
            // promise; config's absent/garbage interpretation matches.
            enabled: AtomicBool::new(true),
            ingested_count: AtomicU64::new(0),
            last_error: Mutex::new(None),
            on_stored: Mutex::new(None),
            queue: Mutex::new(VecDeque::new()),
            session: Mutex::new(VecDeque::new()),
            process: tokio::sync::Mutex::new(()),
        }
    }
}

impl ChatIngestState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Snapshot for `memory_status` (T04) — camelCase JSON, additive to the
    /// memory IPC contract.
    pub fn status(&self) -> ChatIngestStatus {
        ChatIngestStatus {
            buffered: self.queue.lock().unwrap().len(),
            ingested_count: self.ingested_count.load(Ordering::SeqCst),
            last_error: self.last_error.lock().unwrap().clone(),
            enabled: self.enabled(),
        }
    }

    /// Whether chat ingest is enabled (the S03 toggle). SeqCst pairs with
    /// [`Self::set_enabled`]; the pre-capture gate reads this before any
    /// exchange is composed, queued, redacted, or distilled.
    pub fn enabled(&self) -> bool {
        self.enabled.load(Ordering::SeqCst)
    }

    /// Flip the chat-memory toggle. Single mutation site outside tests is
    /// the T02 applier (plus the startup restore in setup()).
    pub fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::SeqCst);
    }

    /// Install the stored-memories notifier (setup-time). Guard-notifier
    /// pattern shared with [`super::ingest::IngestState`].
    pub fn install_stored_notifier(&self, notify: impl Fn(u64) + Send + 'static) {
        *self.on_stored.lock().unwrap() = Some(Box::new(notify));
    }

    /// Queue an exchange, drop-oldest at [`QUEUE_CAP`]. Returns whether the
    /// oldest exchange was dropped to make room.
    fn push(&self, exchange: Exchange) -> bool {
        let mut queue = self.queue.lock().unwrap();
        let dropped = queue.len() >= QUEUE_CAP;
        if dropped {
            queue.pop_front();
        }
        queue.push_back(exchange);
        dropped
    }

    fn front(&self) -> Option<Exchange> {
        self.queue.lock().unwrap().front().cloned()
    }

    fn pop_front(&self) {
        self.queue.lock().unwrap().pop_front();
    }

    /// Record a settled exchange into the session buffer (S02), drop-oldest
    /// at [`SESSION_SUMMARY_THRESHOLD`]. Returns the buffer depth after the
    /// push. Depth is log-only in S02 — deliberately NOT on
    /// [`ChatIngestStatus`]; S03's visibility slice owns any UI counter.
    fn session_push(&self, exchange: Exchange) -> usize {
        let mut session = self.session.lock().unwrap();
        if session.len() >= SESSION_SUMMARY_THRESHOLD {
            session.pop_front();
        }
        session.push_back(exchange);
        session.len()
    }

    /// Whether the session buffer has reached the summary threshold.
    /// T03's dispatch (after the drain loop) keys off this; in S02-T01 it
    /// only feeds the depth log line.
    fn session_at_threshold(&self) -> bool {
        self.session.lock().unwrap().len() >= SESSION_SUMMARY_THRESHOLD
    }

    /// Snapshot the session buffer oldest-first for one summary attempt.
    /// The buffer is NOT drained here: a failed attempt must retain it
    /// (R006), so clearing happens only on a settled attempt via
    /// [`Self::session_clear`].
    fn session_snapshot(&self) -> Vec<Exchange> {
        self.session.lock().unwrap().iter().cloned().collect()
    }

    /// A summary attempt settled (stored, trivial-NOTHING, or store
    /// refusal): the covered exchanges leave the buffer so the next
    /// session window starts empty.
    fn session_clear(&self) {
        self.session.lock().unwrap().clear();
    }

    /// An attempt succeeded: count what was stored and clear any persisted
    /// failure. A success that stored nothing (trivial exchange) fires no
    /// notifier: there is no note to signal.
    fn record_success(&self, stored: u64) {
        self.ingested_count.fetch_add(stored, Ordering::SeqCst);
        *self.last_error.lock().unwrap() = None;
        if stored > 0 {
            if let Some(notify) = self.on_stored.lock().unwrap().as_ref() {
                notify(stored);
            }
        }
    }

    fn record_failure(&self, err: LlmError) {
        *self.last_error.lock().unwrap() = Some(err);
    }

    #[cfg(test)]
    fn queued_texts(&self) -> Vec<String> {
        self.queue
            .lock()
            .unwrap()
            .iter()
            .map(|e| e.text.clone())
            .collect()
    }

    #[cfg(test)]
    fn session_texts(&self) -> Vec<String> {
        self.session
            .lock()
            .unwrap()
            .iter()
            .map(|e| e.text.clone())
            .collect()
    }
}

/// The chat-ingest half of `memory_status` (T04). camelCase on the wire.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatIngestStatus {
    pub buffered: usize,
    pub ingested_count: u64,
    pub last_error: Option<LlmError>,
    /// The S03 chat-memory toggle, additive to the S01 shape.
    pub enabled: bool,
}

/// Queue `exchange` and drain the queue: one distillation per queued
/// exchange (at most one stored memory each), snapshotting the thin lane
/// per attempt so an S07 runtime re-pin applies without restart. On a typed
/// [`LlmError`] the remaining queue is retained and the error persists on
/// [`ChatIngestState`] until a success clears it (R006). Returns nothing
/// and never panics — the caller fire-and-forgets it after the reply is
/// already emitted, so no failure here can reach the reply path.
pub async fn ingest_exchange(
    state: Arc<ChatIngestState>,
    router: Arc<ModelRouter>,
    store: Arc<MemoryStore>,
    exchange: Exchange,
) {
    if state.push(exchange) {
        log::warn!("memory: chat ingest queue full ({QUEUE_CAP}); dropped oldest exchange");
    }
    // Serialize concurrent spawns; each holder drains everything queued.
    let _serial = state.process.lock().await;
    while let Some(next) = state.front() {
        let (model, client) = match router.lane_client(THIN_LANE) {
            Ok(lane) => lane,
            Err(e) => {
                log::error!(
                    "memory: chat ingest thin lane unavailable: {e}; retaining {} queued \
                     exchanges",
                    state.status().buffered
                );
                return;
            }
        };
        log::info!("memory: chat distillation start via lane={THIN_LANE} model={model}");
        match distill_one(&client, &next).await {
            Ok(Some(summary)) => {
                let (pinned, summary) = split_pin_marker(&summary);
                let stored = match store.insert(NewMemory {
                    summary,
                    apps: Vec::new(),
                    span_start_ms: next.captured_at_ms,
                    span_end_ms: next.captured_at_ms,
                    embedding: None,
                    source: MemorySource::Chat,
                    category: "communication".into(),
                    tags: Vec::new(),
                    pinned,
                    expires_at_ms: None,
                }) {
                    Ok(record) => {
                        log::info!("memory: chat memory stored (id={})", record.id);
                        1
                    }
                    Err(e) => {
                        // A store refusal is not retryable model weather —
                        // drop the exchange visibly instead of looping.
                        log::error!(
                            "memory: chat insert failed ({}): {e}; dropping exchange",
                            e.kind()
                        );
                        0
                    }
                };
                state.pop_front();
                state.record_success(stored);
                record_session_exchange(&state, next);
            }
            Ok(None) => {
                log::info!("memory: chat distillation dropped trivial exchange (NOTHING/empty)");
                state.pop_front();
                state.record_success(0);
                record_session_exchange(&state, next);
            }
            Err(err) => {
                log::error!(
                    "memory: chat distillation failed ({}): {err}; retaining {} queued \
                     exchanges",
                    err.kind(),
                    state.status().buffered
                );
                state.record_failure(err);
                return;
            }
        }
    }
    maybe_session_summary(&state, &router, &store).await;
}

/// The rolling session summary dispatch (S02 T03): fires after the drain
/// loop, still under the process mutex (no re-entrancy, no new spawn
/// sites), when the session buffer has reached
/// [`SESSION_SUMMARY_THRESHOLD`]. Success — stored or trivial-NOTHING —
/// clears the buffer; a typed [`LlmError`] retains it and persists on
/// `lastError` until a success clears it (R006), so the next completed
/// exchange retries the summary. The stored memory's span covers the
/// buffered session oldest..newest.
async fn maybe_session_summary(state: &ChatIngestState, router: &ModelRouter, store: &MemoryStore) {
    if !state.session_at_threshold() {
        return;
    }
    let exchanges = state.session_snapshot();
    // Snapshot the thin lane per attempt (S07 re-pin applies without
    // restart), mirroring the per-exchange drain loop.
    let (model, client) = match router.lane_client(THIN_LANE) {
        Ok(lane) => lane,
        Err(e) => {
            log::error!(
                "memory: chat session summary thin lane unavailable: {e}; session buffer \
                 retained ({} exchanges)",
                exchanges.len()
            );
            return;
        }
    };
    log::info!(
        "memory: chat session summary start ({} exchanges) via lane={THIN_LANE} model={model}",
        exchanges.len()
    );
    let request = ChatRequest::new(compose_session_summary_prompt(&exchanges));
    match client.stream_chat(&request, &|_| {}).await {
        Ok(outcome) => match parse_distilled(&outcome.text) {
            Some(summary) => {
                let (pinned, summary) = split_pin_marker(&summary);
                let span_start_ms = exchanges
                    .first()
                    .map(|e| e.captured_at_ms)
                    .unwrap_or_default();
                let span_end_ms = exchanges
                    .last()
                    .map(|e| e.captured_at_ms)
                    .unwrap_or(span_start_ms);
                let stored = match store.insert(NewMemory {
                    summary,
                    apps: Vec::new(),
                    span_start_ms,
                    span_end_ms,
                    embedding: None,
                    source: MemorySource::Chat,
                    category: "communication".into(),
                    tags: Vec::new(),
                    pinned,
                    expires_at_ms: None,
                }) {
                    Ok(record) => {
                        log::info!("memory: chat session summary stored (id={})", record.id);
                        1
                    }
                    Err(e) => {
                        // A store refusal is not retryable model weather —
                        // drop the session visibly instead of looping.
                        log::error!(
                            "memory: chat session summary insert failed ({}): {e}; dropping \
                             session buffer",
                            e.kind()
                        );
                        0
                    }
                };
                state.session_clear();
                state.record_success(stored);
            }
            None => {
                log::info!("memory: chat session summary trivial (NOTHING/empty); nothing stored");
                state.session_clear();
                state.record_success(0);
            }
        },
        Err(err) => {
            log::error!(
                "memory: chat session summary failed ({}): {err}; session buffer retained ({} \
                 exchanges)",
                err.kind(),
                exchanges.len()
            );
            state.record_failure(err);
        }
    }
}

/// A per-exchange distill settled (stored or trivial-NOTHING): the
/// exchange enters the session buffer exactly once, here — the retained
/// failure arm never reaches this, so a retried exchange cannot enter
/// twice. Depth is log-only in S02 (S03 owns any UI counter); T03's
/// summary dispatch reads [`ChatIngestState::session_at_threshold`] after
/// the drain loop.
fn record_session_exchange(state: &ChatIngestState, exchange: Exchange) {
    let depth = state.session_push(exchange);
    log::debug!(
        "memory: chat session buffer at {depth}/{SESSION_SUMMARY_THRESHOLD}{}",
        if state.session_at_threshold() {
            " (summary threshold reached)"
        } else {
            ""
        }
    );
}

/// One distillation attempt for one exchange: prompt → at most one summary.
async fn distill_one(
    client: &Arc<dyn LlmClient>,
    exchange: &Exchange,
) -> Result<Option<String>, LlmError> {
    let outcome = client
        .stream_chat(&ChatRequest::new(distill_messages(exchange)), &|_| {})
        .await?;
    Ok(parse_distilled(&outcome.text))
}

/// Milliseconds since the Unix epoch — `Exchange.captured_at_ms`.
fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pin_marker_splits_and_plain_lines_stay_unpinned() {
        assert_eq!(
            split_pin_marker("! The user's name is Alex"),
            (true, "The user's name is Alex".to_string())
        );
        assert_eq!(
            split_pin_marker("Searched eBay for Half-Life 2"),
            (false, "Searched eBay for Half-Life 2".to_string())
        );
    }
    use std::sync::atomic::AtomicUsize;

    use async_trait::async_trait;

    use crate::llm::router::{Lane, HEAVY_LANE};
    use crate::llm::toolloop::{ToolCallEvent, ToolResultEvent};
    use crate::llm::{LlmHealth, StreamOutcome, TokenSink, ToolCall};

    /// Scripted lane client (mirrors ingest.rs's double): fails its first
    /// `fail_first` calls with a typed offline error, then replies with
    /// `reply`. Records every call and the last prompt.
    struct ScriptedClient {
        reply: String,
        fail_first: usize,
        calls: AtomicUsize,
        last_messages: Mutex<Vec<ChatMessage>>,
    }

    impl ScriptedClient {
        fn ok(reply: &str) -> Arc<Self> {
            Self::failing_then(0, reply)
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
            Ok(StreamOutcome {
                text: self.reply.clone(),
                token_count: 1,
                tool_calls: Vec::new(),
                prompt_tokens: None,
                completion_tokens: None,
            })
        }

        async fn health(&self) -> LlmHealth {
            LlmHealth {
                online: true,
                endpoint: self.endpoint().into(),
            }
        }
    }

    fn thin_router(client: Arc<ScriptedClient>) -> Arc<ModelRouter> {
        Arc::new(ModelRouter::new(vec![Lane::new(
            THIN_LANE,
            Some("thin-test".into()),
            client,
        )]))
    }

    /// One scripted call outcome for [`SequenceClient`].
    enum Step {
        Ok(&'static str),
        Fail,
    }

    /// Lane client scripted call-by-call (unlike [`ScriptedClient`]'s
    /// fail-first-then-fixed-reply shape) — the session-summary pins need
    /// a distinct summary reply and a failure at an exact call index. Past
    /// the end of the script, the last step repeats.
    struct SequenceClient {
        script: Vec<Step>,
        calls: AtomicUsize,
    }

    impl SequenceClient {
        fn new(script: Vec<Step>) -> Arc<Self> {
            Arc::new(Self {
                script,
                calls: AtomicUsize::new(0),
            })
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl LlmClient for SequenceClient {
        fn endpoint(&self) -> &str {
            "http://mock.invalid"
        }

        async fn stream_chat(
            &self,
            _request: &ChatRequest,
            _on_token: TokenSink<'_>,
        ) -> Result<StreamOutcome, LlmError> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            let step = self
                .script
                .get(call)
                .or(self.script.last())
                .expect("non-empty script");
            match step {
                Step::Ok(reply) => Ok(StreamOutcome {
                    text: (*reply).into(),
                    token_count: 1,
                    tool_calls: Vec::new(),
                    prompt_tokens: None,
                    completion_tokens: None,
                }),
                Step::Fail => Err(LlmError::Offline {
                    endpoint: self.endpoint().into(),
                    detail: "connection refused".into(),
                }),
            }
        }

        async fn health(&self) -> LlmHealth {
            LlmHealth {
                online: true,
                endpoint: self.endpoint().into(),
            }
        }
    }

    fn thin_seq_router(client: Arc<SequenceClient>) -> Arc<ModelRouter> {
        Arc::new(ModelRouter::new(vec![Lane::new(
            THIN_LANE,
            Some("thin-test".into()),
            client,
        )]))
    }

    fn call_event(id: &str, name: &str, arguments: &str) -> ToolEvent {
        ToolEvent::Call(ToolCallEvent {
            request_id: 1,
            round: 0,
            call: ToolCall {
                id: id.into(),
                name: name.into(),
                arguments: arguments.into(),
            },
        })
    }

    fn result_event(id: &str, name: &str, ok: bool, failure: Option<&str>) -> ToolEvent {
        ToolEvent::Result(ToolResultEvent {
            request_id: 1,
            round: 0,
            call_id: id.into(),
            name: name.into(),
            ok,
            result_count: None,
            mode: None,
            failure: failure.map(Into::into),
            preview: None,
        })
    }

    fn plain_exchange(text: &str) -> Exchange {
        Exchange {
            text: text.into(),
            captured_at_ms: 1234,
        }
    }

    // --- capture ---

    #[test]
    fn capture_composes_ask_tool_outcomes_and_reply() {
        let events = vec![
            call_event("c1", "type_text", r#"{"text":"rust async traits"}"#),
            result_event("c1", "type_text", true, None),
            call_event("c2", "mouse_click", r#"{"x":1,"y":2}"#),
            result_event("c2", "mouse_click", false, Some("input-failed")),
            call_event("c3", "key_press", r#"{"key":"return"}"#),
            // c3 has no result: the run was cut short.
        ];
        let exchange = capture_exchange(
            "search for rust async traits in Chrome",
            &events,
            "Done, I searched.",
        )
        .expect("non-trivial exchange must capture");
        let text = &exchange.text;
        assert!(
            text.contains("search for rust async traits in Chrome"),
            "{text}"
        );
        assert!(text.contains("type_text"), "{text}");
        assert!(
            text.contains("rust async traits"),
            "arguments JSON must ride along: {text}"
        );
        assert!(text.contains("-> ok"), "{text}");
        assert!(
            text.contains("failed (input-failed)"),
            "verified failure outcome: {text}"
        );
        assert!(
            text.contains("-> no result"),
            "cut-short call must not panic: {text}"
        );
        assert!(text.contains("Done, I searched."), "{text}");
    }

    #[test]
    fn capture_of_empty_exchange_is_none() {
        assert!(capture_exchange("", &[], "").is_none());
        assert!(capture_exchange("  \n ", &[], "  ").is_none());
    }

    #[test]
    fn capture_redacts_secrets_before_an_exchange_exists() {
        // The D029 mount: a pasted card number must be gone from the
        // Exchange itself, not just from downstream prompts.
        let secret = "4111 1111 1111 1111";
        let exchange = capture_exchange(
            &format!("my card is {secret} please check it"),
            &[],
            "I will not store that.",
        )
        .expect("redaction success must capture");
        assert!(
            !exchange.text.contains(secret),
            "raw secret leaked: {}",
            exchange.text
        );
        assert!(
            exchange.text.contains("[REDACTED:card]"),
            "placeholder missing: {}",
            exchange.text
        );
    }

    #[test]
    fn capture_fails_closed_when_redaction_errs() {
        // Err from the engine drops the whole exchange — no partial or
        // unredacted Exchange can exist.
        let out = capture_with_redactor("secret ask", &[], "reply", |_| {
            Err(RedactionError::PatternCompile { detector: "test" })
        });
        assert!(out.is_none());
    }

    // --- parse ---

    #[test]
    fn parse_distilled_caps_at_one_summary() {
        assert_eq!(
            parse_distilled("- User searched for whales.\n- User also did other things.\n"),
            Some("User searched for whales.".to_string())
        );
    }

    #[test]
    fn parse_distilled_treats_nothing_and_empty_as_trivial() {
        assert_eq!(parse_distilled("NOTHING"), None);
        assert_eq!(parse_distilled("nothing"), None);
        assert_eq!(parse_distilled(" Nothing. "), None);
        assert_eq!(parse_distilled(""), None);
        assert_eq!(parse_distilled("  \n \n"), None);
    }

    // --- session summary prompt (S02 T02) ---

    #[test]
    fn session_prompt_labels_each_exchange_in_order_as_one_conversation() {
        let exchanges = vec![
            plain_exchange("User asked about whales."),
            plain_exchange("User asked about rust traits."),
            plain_exchange("User asked about tax forms."),
        ];
        let messages = compose_session_summary_prompt(&exchanges);
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, crate::llm::Role::System);
        let system = &messages[0].content;
        assert!(
            system.contains("ONE line"),
            "one-line contract missing: {system}"
        );
        assert!(
            system.contains("NOTHING"),
            "trivial-session escape missing: {system}"
        );
        assert!(
            system.contains("ONE conversation"),
            "session framing missing: {system}"
        );
        let body = &messages[1].content;
        let idx = |label: &str| {
            body.find(label)
                .unwrap_or_else(|| panic!("label {label} missing: {body}"))
        };
        assert!(idx("[exchange 1]") < idx("[exchange 2]"), "{body}");
        assert!(idx("[exchange 2]") < idx("[exchange 3]"), "{body}");
        assert!(
            idx("[exchange 1]") < idx("whales"),
            "text must follow its label: {body}"
        );
        assert!(body.contains("rust traits"), "{body}");
        assert!(body.contains("tax forms"), "{body}");
    }

    #[test]
    fn session_prompt_caps_each_exchange_bounding_the_whole_prompt() {
        // Q6 pin: SESSION_SUMMARY_THRESHOLD over-long exchanges must not
        // blow the thin model's context — the body stays bounded by
        // threshold * per-exchange cap (plus small per-exchange framing).
        let long = "x".repeat(SESSION_EXCHANGE_MAX_CHARS * 3);
        let exchanges: Vec<Exchange> = (0..SESSION_SUMMARY_THRESHOLD)
            .map(|_| plain_exchange(&long))
            .collect();
        let messages = compose_session_summary_prompt(&exchanges);
        let body = &messages[1].content;
        let bound = SESSION_SUMMARY_THRESHOLD * (SESSION_EXCHANGE_MAX_CHARS + 32);
        assert!(
            body.len() <= bound,
            "session prompt body must stay bounded (got {} chars, bound {bound})",
            body.len()
        );
    }

    #[test]
    fn session_prompt_of_empty_buffer_has_empty_body() {
        // Defensive: dispatch (T03) only fires at threshold, but an empty
        // buffer must not panic or fabricate content.
        let messages = compose_session_summary_prompt(&[]);
        assert_eq!(messages.len(), 2);
        assert!(messages[1].content.is_empty());
    }

    #[test]
    fn session_summary_reply_parses_through_the_one_line_or_nothing_contract() {
        // The session reply reuses parse_distilled: cap 1 even when the
        // model rambles, NOTHING (any case/period) = trivial session.
        assert_eq!(
            parse_distilled(
                "- User researched whales, rust traits, and tax forms.\n\
                 - The user also asked follow-ups.\n"
            ),
            Some("User researched whales, rust traits, and tax forms.".to_string())
        );
        assert_eq!(parse_distilled("NOTHING"), None);
        assert_eq!(parse_distilled(" nothing. "), None);
    }

    // --- status shape ---

    #[test]
    fn chat_ingest_status_serializes_camel_case() {
        // memory_status (T04) reads exactly these keys; a change here is a
        // breaking IPC change.
        let state = ChatIngestState::new();
        state.push(plain_exchange("x"));
        state.record_failure(LlmError::Offline {
            endpoint: "http://x:1".into(),
            detail: "down".into(),
        });
        let v = serde_json::to_value(state.status()).unwrap();
        assert_eq!(v["buffered"], 1);
        assert_eq!(v["ingestedCount"], 0);
        assert_eq!(v["lastError"]["kind"], "offline");
        assert_eq!(
            v["enabled"], true,
            "S03 toggle rides the wire as \"enabled\""
        );

        state.record_success(2);
        let v = serde_json::to_value(state.status()).unwrap();
        assert_eq!(v["ingestedCount"], 2);
        assert!(
            v["lastError"].is_null(),
            "success must clear the persisted error"
        );
    }

    // --- chat-memory toggle (S03 T01) ---

    #[test]
    fn chat_ingest_defaults_to_enabled() {
        // R032 opt-out contract: a fresh state is ON until the user says
        // otherwise (config restore or the T02 applier flips it).
        let state = ChatIngestState::new();
        assert!(state.enabled());
        assert!(state.status().enabled);
    }

    #[test]
    fn set_enabled_false_flips_status_and_leaves_counters_untouched() {
        // Q7: disabling is a gate, not a reset — counters and the queue are
        // untouched so re-enabling resumes exactly where the user left off.
        let state = ChatIngestState::new();
        state.push(plain_exchange("queued"));
        state.record_success(3);
        state.set_enabled(false);
        let status = state.status();
        assert!(!status.enabled);
        assert_eq!(status.buffered, 1, "queue untouched by the toggle");
        assert_eq!(status.ingested_count, 3, "counters untouched by the toggle");
        state.set_enabled(true);
        assert!(state.status().enabled, "flip-back restores the prior value");
    }

    #[test]
    fn stored_notifier_fires_only_on_real_stores() {
        let state = ChatIngestState::new();
        let seen = Arc::new(AtomicU64::new(0));
        let sink = seen.clone();
        state.install_stored_notifier(move |stored| {
            sink.fetch_add(stored, Ordering::SeqCst);
        });
        state.record_success(0);
        assert_eq!(
            seen.load(Ordering::SeqCst),
            0,
            "empty success must not notify"
        );
        state.record_success(1);
        assert_eq!(seen.load(Ordering::SeqCst), 1);
    }

    // --- queue bound ---

    #[test]
    fn queue_caps_at_16_dropping_oldest() {
        let state = ChatIngestState::new();
        for i in 0..QUEUE_CAP {
            assert!(!state.push(plain_exchange(&format!("exchange {i}"))));
        }
        assert!(
            state.push(plain_exchange("overflow")),
            "17th push must drop the oldest"
        );
        let texts = state.queued_texts();
        assert_eq!(texts.len(), QUEUE_CAP);
        assert_eq!(texts.first().unwrap(), "exchange 1", "oldest must be gone");
        assert_eq!(texts.last().unwrap(), "overflow");
    }

    // --- session buffer (S02 T01 plumbing) ---

    #[test]
    fn session_buffer_caps_at_threshold_dropping_oldest() {
        let state = ChatIngestState::new();
        for i in 0..SESSION_SUMMARY_THRESHOLD {
            assert_eq!(
                state.session_push(plain_exchange(&format!("exchange {i}"))),
                i + 1
            );
            assert_eq!(
                state.session_at_threshold(),
                i + 1 == SESSION_SUMMARY_THRESHOLD
            );
        }
        assert_eq!(
            state.session_push(plain_exchange("overflow")),
            SESSION_SUMMARY_THRESHOLD,
            "buffer must stay bounded at the threshold"
        );
        let texts = state.session_texts();
        assert_eq!(texts.len(), SESSION_SUMMARY_THRESHOLD);
        assert_eq!(texts.first().unwrap(), "exchange 1", "oldest must be gone");
        assert_eq!(texts.last().unwrap(), "overflow");
    }

    #[tokio::test]
    async fn settled_exchanges_enter_session_buffer_stored_and_trivial_alike() {
        let state = Arc::new(ChatIngestState::new());
        let store = Arc::new(MemoryStore::open_in_memory().unwrap());
        let client = ScriptedClient::ok("User searched for whales.");
        let router = thin_router(client);
        ingest_exchange(
            state.clone(),
            router.clone(),
            store.clone(),
            plain_exchange("stored"),
        )
        .await;
        // A trivial (NOTHING) settle counts toward the session too — the
        // session summary covers the whole conversation, not just the
        // exchanges the per-exchange distiller found memorable.
        let trivial = ScriptedClient::ok("NOTHING");
        ingest_exchange(
            state.clone(),
            thin_router(trivial),
            store,
            plain_exchange("trivial"),
        )
        .await;
        assert_eq!(
            state.session_texts(),
            vec!["stored".to_string(), "trivial".to_string()]
        );
    }

    #[tokio::test]
    async fn retained_failure_keeps_exchange_out_of_session_until_it_settles_once() {
        let state = Arc::new(ChatIngestState::new());
        let store = Arc::new(MemoryStore::open_in_memory().unwrap());
        let client = ScriptedClient::failing_then(1, "User searched for whales.");
        let router = thin_router(client);

        ingest_exchange(
            state.clone(),
            router.clone(),
            store.clone(),
            plain_exchange("first"),
        )
        .await;
        assert_eq!(
            state.status().buffered,
            1,
            "retry queue retains the failed exchange"
        );
        assert!(
            state.session_texts().is_empty(),
            "a retained failure must not enter the session"
        );

        ingest_exchange(state.clone(), router, store, plain_exchange("second")).await;
        assert_eq!(
            state.session_texts(),
            vec!["first".to_string(), "second".to_string()],
            "a retried exchange enters the session exactly once, at settle time"
        );
    }

    // --- session summary dispatch (S02 T03) ---

    fn stamped_exchange(text: &str, ms: i64) -> Exchange {
        Exchange {
            text: text.into(),
            captured_at_ms: ms,
        }
    }

    const EXCHANGE_NOTE: &str = "User did one thing.";
    const SESSION_NOTE: &str = "User researched whales, rust traits, and tax forms.";

    #[tokio::test]
    async fn session_summary_fires_once_at_threshold_and_resets() {
        // Script: 5 per-exchange distills, then the session summary with a
        // distinct reply, then the 6th exchange's distill.
        let mut script: Vec<Step> = (0..SESSION_SUMMARY_THRESHOLD)
            .map(|_| Step::Ok(EXCHANGE_NOTE))
            .collect();
        script.push(Step::Ok(SESSION_NOTE));
        script.push(Step::Ok(EXCHANGE_NOTE));
        let client = SequenceClient::new(script);
        let router = thin_seq_router(client.clone());
        let state = Arc::new(ChatIngestState::new());
        let store = Arc::new(MemoryStore::open_in_memory().unwrap());
        let notified = Arc::new(AtomicU64::new(0));
        let sink = notified.clone();
        state.install_stored_notifier(move |n| {
            sink.fetch_add(n, Ordering::SeqCst);
        });

        for i in 0..SESSION_SUMMARY_THRESHOLD {
            ingest_exchange(
                state.clone(),
                router.clone(),
                store.clone(),
                stamped_exchange(&format!("exchange {i}"), (i + 1) as i64),
            )
            .await;
        }
        assert_eq!(
            client.calls(),
            SESSION_SUMMARY_THRESHOLD + 1,
            "exactly one summary call at the Nth exchange"
        );
        assert!(
            state.session_texts().is_empty(),
            "stored summary must reset the session buffer"
        );
        assert_eq!(store.count().unwrap(), SESSION_SUMMARY_THRESHOLD + 1);
        let summary = store
            .list(50, 0)
            .unwrap()
            .into_iter()
            .find(|r| r.summary == SESSION_NOTE)
            .expect("session summary memory stored");
        assert_eq!(summary.source, MemorySource::Chat);
        assert_eq!(
            summary.span_start_ms, 1,
            "span covers the oldest buffered exchange"
        );
        assert_eq!(
            summary.span_end_ms, SESSION_SUMMARY_THRESHOLD as i64,
            "…to the newest"
        );
        let status = state.status();
        assert_eq!(
            status.ingested_count,
            (SESSION_SUMMARY_THRESHOLD + 1) as u64,
            "stored summary increments ingestedCount"
        );
        assert_eq!(
            notified.load(Ordering::SeqCst),
            (SESSION_SUMMARY_THRESHOLD + 1) as u64,
            "stored summary fires the tray notifier"
        );

        // The next exchange starts a fresh window: no second summary.
        ingest_exchange(state.clone(), router, store, stamped_exchange("after", 99)).await;
        assert_eq!(
            client.calls(),
            SESSION_SUMMARY_THRESHOLD + 2,
            "no summary below threshold after reset"
        );
        assert_eq!(state.session_texts(), vec!["after".to_string()]);
    }

    #[tokio::test]
    async fn all_nothing_session_stores_nothing_and_resets() {
        // Trivial settles still fill the session buffer; a NOTHING summary
        // is a success that stores nothing and clears the window.
        let client = SequenceClient::new(vec![Step::Ok("NOTHING")]);
        let router = thin_seq_router(client.clone());
        let state = Arc::new(ChatIngestState::new());
        let store = Arc::new(MemoryStore::open_in_memory().unwrap());
        for i in 0..SESSION_SUMMARY_THRESHOLD {
            ingest_exchange(
                state.clone(),
                router.clone(),
                store.clone(),
                plain_exchange(&format!("hi {i}")),
            )
            .await;
        }
        assert_eq!(
            client.calls(),
            SESSION_SUMMARY_THRESHOLD + 1,
            "summary was attempted"
        );
        assert_eq!(
            store.count().unwrap(),
            0,
            "all-NOTHING session stores nothing"
        );
        assert!(
            state.session_texts().is_empty(),
            "trivial summary still resets the window"
        );
        let status = state.status();
        assert_eq!(status.ingested_count, 0);
        assert!(
            status.last_error.is_none(),
            "NOTHING summary is success, not failure"
        );
    }

    #[tokio::test]
    async fn session_summary_failure_retains_buffer_and_next_exchange_retries() {
        // Script: 5 per-exchange ok, summary FAILS, 6th exchange ok, summary
        // retry succeeds.
        let mut script: Vec<Step> = (0..SESSION_SUMMARY_THRESHOLD)
            .map(|_| Step::Ok(EXCHANGE_NOTE))
            .collect();
        script.push(Step::Fail);
        script.push(Step::Ok(EXCHANGE_NOTE));
        script.push(Step::Ok(SESSION_NOTE));
        let client = SequenceClient::new(script);
        let router = thin_seq_router(client.clone());
        let state = Arc::new(ChatIngestState::new());
        let store = Arc::new(MemoryStore::open_in_memory().unwrap());

        for i in 0..SESSION_SUMMARY_THRESHOLD {
            ingest_exchange(
                state.clone(),
                router.clone(),
                store.clone(),
                plain_exchange(&format!("exchange {i}")),
            )
            .await;
        }
        assert_eq!(
            state.session_texts().len(),
            SESSION_SUMMARY_THRESHOLD,
            "failed summary must retain the session buffer (R006)"
        );
        let status = state.status();
        assert_eq!(
            status.last_error.as_ref().map(|e| e.kind()),
            Some("offline"),
            "summary failure persists on lastError like per-exchange ones"
        );
        assert_eq!(store.count().unwrap(), SESSION_SUMMARY_THRESHOLD);

        // Next exchange settles (drop-oldest keeps depth at threshold) and
        // the summary retries and succeeds.
        ingest_exchange(
            state.clone(),
            router,
            store.clone(),
            plain_exchange("retry"),
        )
        .await;
        assert_eq!(client.calls(), SESSION_SUMMARY_THRESHOLD + 3);
        assert!(
            state.session_texts().is_empty(),
            "successful retry clears the buffer"
        );
        assert!(
            store
                .list(50, 0)
                .unwrap()
                .iter()
                .any(|r| r.summary == SESSION_NOTE),
            "retried summary stored"
        );
        let status = state.status();
        assert!(
            status.last_error.is_none(),
            "summary success clears the persisted error"
        );
        assert_eq!(
            status.ingested_count,
            (SESSION_SUMMARY_THRESHOLD + 2) as u64
        );
    }

    // --- ingest end-to-end (mock lane, real store) ---

    #[tokio::test]
    async fn ingest_stores_at_most_one_memory_even_for_a_multiline_reply() {
        let state = Arc::new(ChatIngestState::new());
        let store = Arc::new(MemoryStore::open_in_memory().unwrap());
        let client = ScriptedClient::ok(
            "User searched for rust async traits in Chrome.\nUser also opened a tab.\nExtra.",
        );
        ingest_exchange(
            state.clone(),
            thin_router(client),
            store.clone(),
            plain_exchange("x"),
        )
        .await;
        assert_eq!(
            store.count().unwrap(),
            1,
            "one exchange → at most one memory"
        );
        assert_eq!(
            store.list(10, 0).unwrap()[0].summary,
            "User searched for rust async traits in Chrome."
        );
        let status = state.status();
        assert_eq!(status.buffered, 0);
        assert_eq!(status.ingested_count, 1);
        assert!(status.last_error.is_none());
    }

    #[tokio::test]
    async fn stored_chat_memory_round_trips_with_source_chat() {
        let state = Arc::new(ChatIngestState::new());
        let store = Arc::new(MemoryStore::open_in_memory().unwrap());
        let client = ScriptedClient::ok("User searched for whales.");
        let exchange = Exchange {
            text: "x".into(),
            captured_at_ms: 777,
        };
        ingest_exchange(state, thin_router(client), store.clone(), exchange).await;
        let record = store.list(10, 0).unwrap().remove(0);
        assert_eq!(record.source, MemorySource::Chat);
        assert_eq!(record.span_start_ms, 777, "span = capture timestamp");
        assert_eq!(record.span_end_ms, 777);
        let v = serde_json::to_value(&record).unwrap();
        assert_eq!(v["source"], "chat", "IPC wire form");
    }

    #[tokio::test]
    async fn nothing_reply_stores_nothing_and_counts_as_success() {
        let state = Arc::new(ChatIngestState::new());
        let store = Arc::new(MemoryStore::open_in_memory().unwrap());
        let notified = Arc::new(AtomicU64::new(0));
        let sink = notified.clone();
        state.install_stored_notifier(move |n| {
            sink.fetch_add(n, Ordering::SeqCst);
        });
        let client = ScriptedClient::ok("NOTHING");
        ingest_exchange(
            state.clone(),
            thin_router(client),
            store.clone(),
            plain_exchange("hi"),
        )
        .await;
        assert_eq!(store.count().unwrap(), 0);
        let status = state.status();
        assert_eq!(status.buffered, 0, "trivial exchange dropped, not retained");
        assert_eq!(status.ingested_count, 0);
        assert!(
            status.last_error.is_none(),
            "NOTHING is success, not failure"
        );
        assert_eq!(notified.load(Ordering::SeqCst), 0, "no note, no flash");
    }

    #[tokio::test]
    async fn typed_failure_retains_exchange_and_next_success_clears_both() {
        let state = Arc::new(ChatIngestState::new());
        let store = Arc::new(MemoryStore::open_in_memory().unwrap());
        let client = ScriptedClient::failing_then(1, "User searched for whales in Chrome.");
        let router = thin_router(client.clone());

        ingest_exchange(
            state.clone(),
            router.clone(),
            store.clone(),
            plain_exchange("first"),
        )
        .await;
        assert_eq!(store.count().unwrap(), 0, "nothing stored on failure");
        let status = state.status();
        assert_eq!(status.buffered, 1, "exchange retained for retry");
        assert_eq!(
            status.last_error.as_ref().map(|e| e.kind()),
            Some("offline"),
            "typed error must be queryable until a success clears it"
        );

        ingest_exchange(
            state.clone(),
            router,
            store.clone(),
            plain_exchange("second"),
        )
        .await;
        assert_eq!(
            client.calls(),
            3,
            "one failure, then both queued exchanges distilled"
        );
        assert_eq!(store.count().unwrap(), 2, "retained + new both stored");
        let status = state.status();
        assert_eq!(status.buffered, 0);
        assert_eq!(status.ingested_count, 2);
        assert!(
            status.last_error.is_none(),
            "success must clear the persisted error"
        );
    }

    #[tokio::test]
    async fn redaction_placeholder_not_secret_reaches_the_distiller() {
        // End-to-end privacy pin: capture → queue → prompt. The scripted
        // client sees the prompt; the secret must not be in it.
        let secret = "4111 1111 1111 1111";
        let exchange = capture_exchange(
            &format!("pay with {secret} on the checkout page"),
            &[],
            "I did not use the card.",
        )
        .unwrap();
        let state = Arc::new(ChatIngestState::new());
        let store = Arc::new(MemoryStore::open_in_memory().unwrap());
        let client = ScriptedClient::ok("User asked about a checkout page.");
        ingest_exchange(state, thin_router(client.clone()), store, exchange).await;
        let sent = client.last_messages.lock().unwrap().clone();
        assert_eq!(sent.len(), 2);
        assert!(
            !sent[1].content.contains(secret),
            "secret reached the distiller"
        );
        assert!(sent[1].content.contains("[REDACTED:card]"));
    }

    #[tokio::test]
    async fn distillation_stays_pinned_to_thin_while_active_lane_is_heavy() {
        let thin = ScriptedClient::ok("User worked in the thin lane.");
        let heavy = ScriptedClient::ok("wrong lane");
        let router = Arc::new(ModelRouter::new(vec![
            Lane::new(THIN_LANE, Some("thin-test".into()), thin.clone()),
            Lane::new(HEAVY_LANE, Some("heavy-test".into()), heavy.clone()),
        ]));
        router.set_active(HEAVY_LANE).unwrap();
        let state = Arc::new(ChatIngestState::new());
        let store = Arc::new(MemoryStore::open_in_memory().unwrap());
        ingest_exchange(state, router, store.clone(), plain_exchange("x")).await;
        assert_eq!(thin.calls(), 1, "chat distillation must ride the thin lane");
        assert_eq!(
            heavy.calls(),
            0,
            "the user's active lane must never see ingest traffic"
        );
        assert_eq!(store.count().unwrap(), 1);
    }
}
