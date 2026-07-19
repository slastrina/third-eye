//! Tauri IPC surface for chat (R002) and model routing (R003): the `chat`,
//! `llm_health`, `set_model`, and `model_info` commands plus the
//! `llm://token` / `llm://done` / `llm://error` event stream and the S03
//! tool-phase events `llm://tool-call` / `llm://tool-result`.
//!
//! The webview never talks HTTP to the model endpoint — this module is the
//! whole contract. A `chat` invoke returns a request id immediately; tokens
//! and the terminal outcome arrive as events carrying that id, so the UI can
//! drop stale events after a resubmit. Single-flight is enforced here: a new
//! request aborts the in-flight one (R006 — no zombie streams racing each
//! other into the same message list).
//!
//! Observability (R005, mirroring the S01 summon-latency pattern): info logs
//! `llm first token: N ms (request=N)` and total stream duration; error logs
//! name the endpoint and typed error kind; debug logs cover request start,
//! cancellation, token count, and done.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State};

use super::guard::{GuardState, GuardTelemetry};
use super::openai::{OpenAiClient, DEFAULT_ENDPOINT};
use super::router::{ModelInfo, ModelRouter, HEAVY_LANE, THIN_LANE};
use super::toolloop::{
    run_tool_loop, MemorySearchTool, ToolEvent, TOOL_CALL_EVENT, TOOL_RESULT_EVENT,
};
use super::{ChatMessage, ChatRequest, LlmClient, LlmError, LlmHealth};

/// Event names — the string half of the IPC contract with `src/chat.ts`.
pub const TOKEN_EVENT: &str = "llm://token";
pub const DONE_EVENT: &str = "llm://done";
pub const ERROR_EVENT: &str = "llm://error";
/// Routing-state broadcast (S07): mutation responses only reach the calling
/// window, so every successful `set_model` / `set_lane_model` also emits the
/// updated [`ModelInfo`] app-wide — the overlay stays truthful when the
/// settings window changes routing.
pub const MODEL_INFO_EVENT: &str = "llm://model-info";
/// Privacy-guard telemetry broadcast (M003 S03): every [`GuardState`]
/// mutation — guarded forward with detections, guard block, watcher
/// redaction — emits the fresh kinds-and-counts-only [`GuardTelemetry`]
/// snapshot app-wide, so the Settings guard surface stays truthful without
/// polling. The string half of the contract with `src/privacy-state.ts`.
pub const PRIVACY_STATE_EVENT: &str = "privacy://state";

/// One content delta. Fired per token in arrival order.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenEvent {
    pub request_id: u64,
    pub token: String,
}

/// Terminal success event: full accumulated text plus the latency figures
/// that were also logged at info level.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DoneEvent {
    pub request_id: u64,
    pub text: String,
    pub token_count: usize,
    /// `None` when the stream completed without producing any token.
    pub first_token_ms: Option<u64>,
    pub total_ms: u64,
}

/// Terminal failure event. `error` is the kind-tagged [`LlmError`] JSON the
/// UI matches on (`offline` / `no-model` / `interrupted`).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ErrorEvent {
    pub request_id: u64,
    pub error: LlmError,
}

/// Cancels an in-flight stream task. Boxed so the single-flight state machine
/// stays testable without a Tauri runtime.
type Aborter = Box<dyn Fn() + Send>;

struct ActiveRequest {
    id: u64,
    /// `None` between `begin` and `arm` — the window where the task is being
    /// spawned and there is nothing to abort yet.
    aborter: Option<Aborter>,
}

/// Managed chat state: the routing seam plus single-flight request tracking.
/// Holds the [`ModelRouter`] concretely so `set_model` / `model_info` can
/// reach its lane API, while `chat` and `llm_health` use it as a plain
/// `Arc<dyn LlmClient>` — the S02 streaming contract is unchanged.
pub struct LlmState {
    router: Arc<ModelRouter>,
    /// Unpinned client against the router's endpoint for `/v1/models`
    /// listing — the model list is endpoint-scoped, not lane-scoped.
    probe: OpenAiClient,
    next_request_id: AtomicU64,
    active: Mutex<Option<ActiveRequest>>,
}

impl LlmState {
    pub fn new(router: Arc<ModelRouter>) -> Self {
        let probe = OpenAiClient::new(router.endpoint());
        Self { router, probe, next_request_id: AtomicU64::new(1), active: Mutex::new(None) }
    }

    /// State backed by the configured LM Studio endpoint with the canonical
    /// thin/heavy lane pair. The endpoint comes from `THIRD_EYE_ENDPOINT`
    /// (unset or blank → [`DEFAULT_ENDPOINT`], see [`env_endpoint`]); lane
    /// model ids come from the `THIRD_EYE_THIN_MODEL` /
    /// `THIRD_EYE_HEAVY_MODEL` env vars — the single override site until S05
    /// ships real configurability. An unset (or blank) var leaves that lane
    /// unpinned: requests omit the `model` key and a single-model LM Studio
    /// serves whatever it has loaded.
    ///
    /// `guard` is the app-shared privacy-guard telemetry (M003 S02): every
    /// lane client the router builds — now and on runtime re-pins — is
    /// wrapped against it.
    pub fn with_default_endpoint(guard: Arc<GuardState>) -> Self {
        Self::from_env(
            std::env::var("THIRD_EYE_ENDPOINT").ok(),
            std::env::var("THIRD_EYE_THIN_MODEL").ok(),
            std::env::var("THIRD_EYE_HEAVY_MODEL").ok(),
            guard,
        )
    }

    /// The whole construction path of [`Self::with_default_endpoint`] minus
    /// the process-global env reads, so tests can drive the exact production
    /// delegation (router → guarded lane clients → probe) with pinned
    /// values instead of racing on `std::env`.
    fn from_env(
        endpoint: Option<String>,
        thin: Option<String>,
        heavy: Option<String>,
        guard: Arc<GuardState>,
    ) -> Self {
        Self::new(Arc::new(ModelRouter::thin_heavy(
            &env_endpoint(endpoint),
            env_model(thin),
            env_model(heavy),
            guard,
        )))
    }

    /// The routing seam itself — S02 ingestion holds this to snapshot the
    /// thin lane's client per batch via [`ModelRouter::lane_client`], so
    /// runtime re-pins apply to the next distillation without new plumbing.
    pub fn router(&self) -> Arc<ModelRouter> {
        self.router.clone()
    }

    /// Switch the active routing lane — the validated core of the
    /// `set_model` command. Unknown lane names are rejected with an error
    /// naming the lane and the known set; routing is left unchanged. The
    /// router logs every real switch at info level (old → new).
    pub fn set_model(&self, lane: &str) -> Result<ModelInfo, String> {
        self.router
            .set_active(lane)
            .inspect_err(|e| log::warn!("llm: set_model rejected: {e}"))
    }

    /// Re-pin a lane's model — the validated core of the `set_lane_model`
    /// command and the startup persistence application. The router logs
    /// every re-pin at info level (old → new) and rejects unknown lanes
    /// naming the lane and the known set.
    pub fn set_lane_model(&self, lane: &str, model: Option<String>) -> Result<ModelInfo, String> {
        self.router
            .set_lane_model(lane, model)
            .inspect_err(|e| log::warn!("llm: set_lane_model rejected: {e}"))
    }

    /// Fetch the model ids the endpoint serves — the core of the
    /// `list_models` command. Outcome is logged either way (Q5/R006: an
    /// unreachable endpoint is a typed `offline` error, never a hang).
    pub async fn list_models(&self) -> Result<Vec<String>, LlmError> {
        match self.probe.list_models().await {
            Ok(models) => {
                log::info!(
                    "llm: model list fetched: {} models from {}",
                    models.len(),
                    LlmClient::endpoint(&self.probe)
                );
                Ok(models)
            }
            Err(err) => {
                log::warn!("llm: model list fetch failed: kind={} {err}", err.kind());
                Err(err)
            }
        }
    }

    /// Routing state snapshot — the core of the `model_info` command
    /// (health-as-value pattern, like `llm_health`).
    pub fn model_info(&self) -> ModelInfo {
        let info = self.router.info();
        log::debug!(
            "llm: model info query active={} lanes={}",
            info.active_lane,
            info.lanes.len()
        );
        info
    }

    /// Allocate the next request id and abort any in-flight request.
    fn begin(&self) -> u64 {
        let id = self.next_request_id.fetch_add(1, Ordering::Relaxed);
        let mut active = self.active.lock().unwrap();
        if let Some(prev) = active.take() {
            if let Some(abort) = prev.aborter {
                abort();
            }
            log::debug!("llm: request {} cancelled (superseded by request {})", prev.id, id);
        }
        *active = Some(ActiveRequest { id, aborter: None });
        id
    }

    /// Attach the abort handle once the stream task exists. A no-op if the
    /// request already finished or was superseded in the meantime.
    fn arm(&self, id: u64, aborter: Aborter) {
        let mut active = self.active.lock().unwrap();
        if let Some(req) = active.as_mut() {
            if req.id == id {
                req.aborter = Some(aborter);
            }
        }
    }

    /// Clear the active slot when a stream reaches a terminal state. Only the
    /// request that owns the slot may clear it — a finished request that was
    /// already superseded must not cancel its successor's tracking.
    fn finish(&self, id: u64) {
        let mut active = self.active.lock().unwrap();
        if active.as_ref().map(|r| r.id) == Some(id) {
            *active = None;
        }
    }
}

/// Start a streaming chat completion. Returns the request id immediately;
/// tokens and the terminal outcome arrive as `llm://*` events tagged with it.
/// Any in-flight request is aborted first (single-flight).
#[tauri::command]
pub async fn chat(
    app: AppHandle,
    state: State<'_, LlmState>,
    messages: Vec<ChatMessage>,
) -> Result<u64, String> {
    let id = state.begin();
    let client: Arc<dyn LlmClient> = state.router.clone();
    log::debug!(
        "llm: request {id} start endpoint={} messages={}",
        client.endpoint(),
        messages.len()
    );

    let task_app = app.clone();
    let handle = tauri::async_runtime::spawn(async move {
        // Tray shows "watching" for the whole stream; the guard drops on
        // every exit path, including abort — single-flight supersede aborts
        // this task, which drops the future and runs this destructor.
        #[cfg(desktop)]
        let _activity =
            crate::tray::begin_activity(&task_app, crate::tray::ActivityKind::Stream);
        let started = Instant::now();
        let first_token_at: Mutex<Option<Instant>> = Mutex::new(None);

        let on_token = |token: &str| {
            {
                let mut first = first_token_at.lock().unwrap();
                if first.is_none() {
                    *first = Some(Instant::now());
                    log::info!(
                        "llm first token: {} ms (request={id})",
                        started.elapsed().as_millis()
                    );
                }
            }
            let payload = TokenEvent { request_id: id, token: token.into() };
            if let Err(e) = task_app.emit(TOKEN_EVENT, payload) {
                log::warn!("llm: request {id} token emit failed: {e}");
            }
        };

        // Tool phases surface as llm://tool-call / llm://tool-result — the
        // UI's memory-consulted indicator (T04). Emission failure is logged,
        // never fatal, same policy as tokens.
        let on_event = |event: &ToolEvent| {
            let emitted = match event {
                ToolEvent::Call(e) => task_app.emit(TOOL_CALL_EVENT, e.clone()),
                ToolEvent::Result(e) => task_app.emit(TOOL_RESULT_EVENT, e.clone()),
            };
            if let Err(e) = emitted {
                log::warn!("llm: request {id} tool event emit failed: {e}");
            }
        };

        // memory_search is available exactly when the S02 store opened this
        // run; without it the request runs tools-free (the pre-S03 path) and
        // the absence is logged, not silent.
        let memory = task_app.state::<crate::memory::MemoryState>();
        let result = match memory.store() {
            Some(store) => {
                let tool = MemorySearchTool::new(store, memory.embedder(client.endpoint()));
                run_tool_loop(client.as_ref(), &tool, messages, id, &on_token, &on_event).await
            }
            None => {
                log::info!(
                    "llm: request {id} runs without tools (memory store unavailable)"
                );
                client.stream_chat(&ChatRequest::new(messages), &on_token).await
            }
        };
        let total_ms = started.elapsed().as_millis() as u64;

        match result {
            Ok(outcome) => {
                let first_token_ms = first_token_at
                    .lock()
                    .unwrap()
                    .map(|t| t.duration_since(started).as_millis() as u64);
                log::info!(
                    "llm stream total: {total_ms} ms, {} tokens (request={id})",
                    outcome.token_count
                );
                log::debug!(
                    "llm: request {id} done tokens={} chars={}",
                    outcome.token_count,
                    outcome.text.chars().count()
                );
                let payload = DoneEvent {
                    request_id: id,
                    text: outcome.text,
                    token_count: outcome.token_count,
                    first_token_ms,
                    total_ms,
                };
                if let Err(e) = task_app.emit(DONE_EVENT, payload) {
                    log::warn!("llm: request {id} done emit failed: {e}");
                }
            }
            Err(err) => {
                log::error!(
                    "llm error: kind={} endpoint={} (request={id}): {err}",
                    err.kind(),
                    err.endpoint()
                );
                let payload = ErrorEvent { request_id: id, error: err };
                if let Err(e) = task_app.emit(ERROR_EVENT, payload) {
                    log::warn!("llm: request {id} error emit failed: {e}");
                }
            }
        }

        task_app.state::<LlmState>().finish(id);
    });

    state.arm(id, Box::new(move || handle.abort()));
    Ok(id)
}

/// Liveness probe behind the UI's exponential-backoff retry loop. Offline is
/// a value (`online: false`), never an error — probing must be safe to spam.
#[tauri::command]
pub async fn llm_health(state: State<'_, LlmState>) -> Result<LlmHealth, String> {
    let health = state.router.health().await;
    log::debug!("llm: health probe endpoint={} online={}", health.endpoint, health.online);
    Ok(health)
}

/// Switch the active routing lane (R003). Unknown lanes are rejected with an
/// error naming the lane and the known set, leaving routing unchanged.
/// Returns the updated [`ModelInfo`] so the UI can render the new state
/// without a second round-trip, and broadcasts it app-wide (S07).
#[tauri::command]
pub fn set_model(app: AppHandle, state: State<'_, LlmState>, lane: String) -> Result<ModelInfo, String> {
    let info = state.set_model(&lane)?;
    broadcast_model_info(&app, &info);
    Ok(info)
}

/// Re-pin a lane's model and persist it to settings.json (S07): the store
/// replaces the THIRD_EYE_* env vars as the configuration site. `None`
/// persists an explicit null — "unpinned" must survive restart too. If
/// persistence fails, the in-memory re-pin is rolled back (an unpersisted
/// pin must never silently revert on restart — hotkey precedent) and the
/// error naming the persist path is returned.
#[tauri::command]
pub fn set_lane_model(
    app: AppHandle,
    state: State<'_, LlmState>,
    lane: String,
    model: Option<String>,
) -> Result<ModelInfo, String> {
    let old = state
        .model_info()
        .lanes
        .into_iter()
        .find(|l| l.name == lane)
        .and_then(|l| l.model_id);
    let info = state.set_lane_model(&lane, model.clone())?;
    if let Err(e) = crate::config::save_lane_model(&app, &lane, model.as_deref()) {
        log::error!("llm: {e}");
        if let Err(rollback) = state.set_lane_model(&lane, old) {
            log::error!("llm: rollback after failed persist also failed: {rollback}");
        }
        return Err(e);
    }
    broadcast_model_info(&app, &info);
    Ok(info)
}

/// List the model ids the LM Studio endpoint actually serves (S07 settings
/// pickers). Transport/protocol failures surface as the kind-tagged
/// [`LlmError`] (`offline`), same contract as the `llm://error` event.
#[tauri::command]
pub async fn list_models(state: State<'_, LlmState>) -> Result<Vec<String>, LlmError> {
    state.list_models().await
}

/// Apply persisted lane pins at startup (called from `setup()`): a present
/// store key wins over the THIRD_EYE_* env fallback the router was built
/// with; an absent key leaves the env-derived pin untouched. Application
/// failure is logged, never fatal — the app still runs on the env pins.
pub fn apply_persisted_lane_models(app: &AppHandle) {
    let state = app.state::<LlmState>();
    for (lane, key) in
        [(THIN_LANE, crate::config::THIN_MODEL_KEY), (HEAVY_LANE, crate::config::HEAVY_MODEL_KEY)]
    {
        if let Some(pin) = crate::config::load_lane_model(app, key) {
            match state.set_lane_model(lane, pin.clone()) {
                Ok(_) => log::info!("llm: applied persisted {key} ({lane} lane → {:?})", pin),
                Err(e) => log::error!("llm: failed to apply persisted {key}: {e}"),
            }
        }
    }
}

/// Emit the routing state to every window. Emission failure is logged, not
/// fatal — the mutating command already returned the state to its caller.
fn broadcast_model_info(app: &AppHandle, info: &ModelInfo) {
    if let Err(e) = app.emit(MODEL_INFO_EVENT, info.clone()) {
        log::warn!("llm: model-info broadcast failed: {e}");
    }
}

/// Queryable routing state (health-as-value, like `llm_health`): the active
/// lane, the shared endpoint, and every configured lane with its model id.
#[tauri::command]
pub fn model_info(state: State<'_, LlmState>) -> ModelInfo {
    state.model_info()
}

/// Current privacy-guard telemetry (S03 mount-time query) — health-as-value
/// beside `watcher_status` and `model_info`: a kinds-and-counts-only
/// snapshot at any time, never an error.
#[tauri::command]
pub fn guard_status(state: State<'_, Arc<GuardState>>) -> GuardTelemetry {
    let snapshot = state.snapshot();
    log::debug!(
        "guard: status query redactionKinds={} blocked={} lastBlockReason={:?}",
        snapshot.redactions.len(),
        snapshot.blocked_count,
        snapshot.last_block_reason
    );
    snapshot
}

/// Install the `privacy://state` emitter on the shared [`GuardState`]
/// (called once from `setup()`): the one choke point that makes all three
/// mutation sites — guarded forward, guard block, watcher redaction —
/// user-visible. Broadcast failure is cosmetic (the truth stays queryable
/// via `guard_status`), so it is logged, never bubbled — the `watcher://state`
/// posture.
pub fn install_guard_notifier(app: &AppHandle) {
    let handle = app.clone();
    let state: Arc<GuardState> = app.state::<Arc<GuardState>>().inner().clone();
    state.set_notifier(Arc::new(move |snapshot: GuardTelemetry| {
        if let Err(e) = handle.emit(PRIVACY_STATE_EVENT, snapshot) {
            log::warn!("guard: privacy state broadcast failed: {e}");
        }
    }));
    log::debug!("guard: privacy://state notifier installed");
}

/// Treat unset, empty, and whitespace-only env values as "no pinned model"
/// so `THIRD_EYE_THIN_MODEL=""` behaves like an absent var instead of
/// pinning a nameless model.
fn env_model(value: Option<String>) -> Option<String> {
    value.map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

/// `THIRD_EYE_ENDPOINT` semantics — trim whitespace, drop trailing slashes,
/// and treat unset/blank as [`DEFAULT_ENDPOINT`]. Public so the live test
/// harnesses (`tests/nudge_live.rs`, `tests/privacy_live.rs`) resolve their
/// endpoint through the same rules as production instead of re-implementing
/// them.
pub fn env_endpoint(value: Option<String>) -> String {
    value
        .map(|v| v.trim().trim_end_matches('/').to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| DEFAULT_ENDPOINT.to_string())
}

#[cfg(test)]
mod tests {
    use super::super::router::{Lane, HEAVY_LANE, THIN_LANE};
    use super::*;
    use std::sync::atomic::AtomicBool;

    struct NoopClient;

    #[async_trait::async_trait]
    impl LlmClient for NoopClient {
        fn endpoint(&self) -> &str {
            "http://noop.invalid"
        }

        async fn stream_chat(
            &self,
            _request: &ChatRequest,
            _on_token: super::super::TokenSink<'_>,
        ) -> Result<super::super::StreamOutcome, LlmError> {
            Ok(super::super::StreamOutcome {
                text: String::new(),
                token_count: 0,
                tool_calls: Vec::new(),
            })
        }

        async fn health(&self) -> LlmHealth {
            LlmHealth { online: true, endpoint: self.endpoint().into() }
        }
    }

    fn state() -> LlmState {
        LlmState::new(Arc::new(ModelRouter::new(vec![Lane::new(
            THIN_LANE,
            None,
            Arc::new(NoopClient),
        )])))
    }

    /// State over the canonical pinned thin/heavy pair — what
    /// `with_default_endpoint` builds when both env vars are set.
    fn routed_state() -> LlmState {
        LlmState::new(Arc::new(ModelRouter::new(vec![
            Lane::new(THIN_LANE, Some("thin-1b".into()), Arc::new(NoopClient)),
            Lane::new(HEAVY_LANE, Some("heavy-7b".into()), Arc::new(NoopClient)),
        ])))
    }

    fn flag_aborter(flag: &Arc<AtomicBool>) -> Aborter {
        let flag = flag.clone();
        Box::new(move || flag.store(true, Ordering::SeqCst))
    }

    #[test]
    fn request_ids_are_monotonic_and_start_at_one() {
        let s = state();
        assert_eq!(s.begin(), 1);
        assert_eq!(s.begin(), 2);
        assert_eq!(s.begin(), 3);
    }

    #[test]
    fn new_request_aborts_the_prior_in_flight_request() {
        let s = state();
        let aborted = Arc::new(AtomicBool::new(false));
        let id = s.begin();
        s.arm(id, flag_aborter(&aborted));
        assert!(!aborted.load(Ordering::SeqCst));

        s.begin();
        assert!(aborted.load(Ordering::SeqCst), "prior request must be aborted on resubmit");
    }

    #[test]
    fn finished_request_is_not_aborted_by_the_next_one() {
        let s = state();
        let aborted = Arc::new(AtomicBool::new(false));
        let id = s.begin();
        s.arm(id, flag_aborter(&aborted));
        s.finish(id);

        s.begin();
        assert!(!aborted.load(Ordering::SeqCst), "finished request must not be aborted");
    }

    #[test]
    fn stale_finish_does_not_clear_the_successor() {
        let s = state();
        let aborted = Arc::new(AtomicBool::new(false));
        let old = s.begin();
        let new = s.begin();
        s.arm(new, flag_aborter(&aborted));

        // The superseded task reaches its terminal state late: must be a no-op.
        s.finish(old);
        s.begin();
        assert!(aborted.load(Ordering::SeqCst), "successor tracking was lost to a stale finish");
    }

    #[test]
    fn arm_after_supersede_is_a_no_op() {
        let s = state();
        let stale = Arc::new(AtomicBool::new(false));
        let old = s.begin();
        let new = s.begin();
        s.arm(old, flag_aborter(&stale)); // stale arm: request already superseded

        s.finish(new);
        s.begin();
        assert!(!stale.load(Ordering::SeqCst), "stale aborter must never be installed");
    }

    #[test]
    fn event_names_are_the_ipc_contract() {
        // src/chat.ts (and the S07 settings window) listen on these exact
        // strings.
        assert_eq!(TOKEN_EVENT, "llm://token");
        assert_eq!(DONE_EVENT, "llm://done");
        assert_eq!(ERROR_EVENT, "llm://error");
        assert_eq!(MODEL_INFO_EVENT, "llm://model-info");
    }

    #[test]
    fn privacy_state_event_name_is_contract_locked() {
        // src/privacy-state.ts pins the same string on the TS side — the
        // two const tests are the contract-lock pair (S03).
        assert_eq!(PRIVACY_STATE_EVENT, "privacy://state");
    }

    #[test]
    fn token_event_serializes_camel_case() {
        let v = serde_json::to_value(TokenEvent { request_id: 7, token: "hi".into() }).unwrap();
        assert_eq!(v["requestId"], 7);
        assert_eq!(v["token"], "hi");
    }

    #[test]
    fn done_event_serializes_camel_case_with_latency() {
        let v = serde_json::to_value(DoneEvent {
            request_id: 7,
            text: "full answer".into(),
            token_count: 3,
            first_token_ms: Some(120),
            total_ms: 900,
        })
        .unwrap();
        assert_eq!(v["requestId"], 7);
        assert_eq!(v["text"], "full answer");
        assert_eq!(v["tokenCount"], 3);
        assert_eq!(v["firstTokenMs"], 120);
        assert_eq!(v["totalMs"], 900);
    }

    #[test]
    fn done_event_without_tokens_has_null_first_token_ms() {
        let v = serde_json::to_value(DoneEvent {
            request_id: 1,
            text: String::new(),
            token_count: 0,
            first_token_ms: None,
            total_ms: 5,
        })
        .unwrap();
        assert!(v["firstTokenMs"].is_null());
    }

    #[test]
    fn error_event_carries_the_typed_kind_tagged_error() {
        let v = serde_json::to_value(ErrorEvent {
            request_id: 9,
            error: LlmError::Interrupted {
                endpoint: "http://192.168.182.224:1234".into(),
                partial_text: "half".into(),
                detail: "connection reset".into(),
            },
        })
        .unwrap();
        assert_eq!(v["requestId"], 9);
        assert_eq!(v["error"]["kind"], "interrupted");
        assert_eq!(v["error"]["endpoint"], "http://192.168.182.224:1234");
        assert_eq!(v["error"]["partialText"], "half");
    }

    #[test]
    fn set_model_switches_the_active_lane_and_returns_updated_info() {
        let s = routed_state();
        assert_eq!(s.model_info().active_lane, THIN_LANE);

        let info = s.set_model(HEAVY_LANE).unwrap();
        assert_eq!(info.active_lane, HEAVY_LANE, "returned info must reflect the switch");
        assert_eq!(s.model_info().active_lane, HEAVY_LANE, "the switch must persist");

        // And back: overriding is not one-way.
        assert_eq!(s.set_model(THIN_LANE).unwrap().active_lane, THIN_LANE);
    }

    #[test]
    fn set_model_rejects_unknown_lane_and_leaves_routing_unchanged() {
        let s = routed_state();
        let err = s.set_model("turbo").unwrap_err();
        assert!(err.contains("turbo"), "error must name the rejected lane: {err}");
        assert!(
            err.contains(THIN_LANE) && err.contains(HEAVY_LANE),
            "error must list known lanes: {err}"
        );
        assert_eq!(s.model_info().active_lane, THIN_LANE, "rejection must not change routing");
    }

    #[test]
    fn model_info_lists_every_lane_with_model_ids() {
        let info = routed_state().model_info();
        assert_eq!(info.active_lane, THIN_LANE);
        assert_eq!(info.lanes.len(), 2);
        assert_eq!(info.lanes[0].model_id.as_deref(), Some("thin-1b"));
        assert_eq!(info.lanes[1].model_id.as_deref(), Some("heavy-7b"));
    }

    #[test]
    fn state_set_lane_model_repins_and_reflects_in_info() {
        let s = routed_state();
        let info = s.set_lane_model(THIN_LANE, Some("qwen2.5-14b".into())).unwrap();
        assert_eq!(info.lanes[0].model_id.as_deref(), Some("qwen2.5-14b"));
        assert_eq!(s.model_info().lanes[0].model_id.as_deref(), Some("qwen2.5-14b"));

        // Explicit unpin round-trips too.
        let info = s.set_lane_model(THIN_LANE, None).unwrap();
        assert_eq!(info.lanes[0].model_id, None);
    }

    #[test]
    fn state_set_lane_model_rejects_unknown_lane_unchanged() {
        let s = routed_state();
        let err = s.set_lane_model("turbo", Some("x".into())).unwrap_err();
        assert!(err.contains("turbo"), "error must name the rejected lane: {err}");
        assert_eq!(s.model_info().lanes[0].model_id.as_deref(), Some("thin-1b"));
        assert_eq!(s.model_info().lanes[1].model_id.as_deref(), Some("heavy-7b"));
    }

    #[test]
    fn env_model_filters_unset_blank_and_whitespace_values() {
        assert_eq!(env_model(None), None);
        assert_eq!(env_model(Some(String::new())), None);
        assert_eq!(env_model(Some("   ".into())), None);
        assert_eq!(env_model(Some(" qwen2.5-7b ".into())), Some("qwen2.5-7b".into()));
    }

    #[test]
    fn env_endpoint_trims_strips_trailing_slash_and_defaults_when_blank() {
        // Aligned with the historical test-side read in tests/nudge_live.rs:
        // trim, drop trailing slashes, unset/blank → project default.
        assert_eq!(env_endpoint(None), DEFAULT_ENDPOINT);
        assert_eq!(env_endpoint(Some(String::new())), DEFAULT_ENDPOINT);
        assert_eq!(env_endpoint(Some("   ".into())), DEFAULT_ENDPOINT);
        assert_eq!(env_endpoint(Some(" http://127.0.0.1:9999/ ".into())), "http://127.0.0.1:9999");
        assert_eq!(env_endpoint(Some("http://192.0.2.1:9".into())), "http://192.0.2.1:9");
    }

    /// TEST-NET-1 (RFC 5737): reserved documentation address — nothing can
    /// listen there, so a connect attempt would surface as `offline`, and
    /// `guard-blocked` proves the guard fired before any connect.
    const TEST_NET_1: &str = "http://192.0.2.1:9";

    #[test]
    fn from_env_unset_endpoint_routes_to_the_project_default() {
        let s = LlmState::from_env(None, None, None, Arc::new(GuardState::new()));
        let client: Arc<dyn LlmClient> = s.router();
        assert_eq!(client.endpoint(), DEFAULT_ENDPOINT);
    }

    /// S04 T01 must-have: the *production constructor path* — not a
    /// hand-built router — pointed at an external endpoint returns typed
    /// guard-blocked (never offline) on a block-triggering chat, and the
    /// shared GuardState records the block for the Settings surface.
    #[tokio::test]
    async fn production_constructor_blocks_external_endpoint_fail_closed() {
        let guard = Arc::new(GuardState::new());
        let s = LlmState::from_env(
            Some(TEST_NET_1.into()),
            Some("thin-test".into()),
            None,
            guard.clone(),
        );
        let client: Arc<dyn LlmClient> = s.router();
        assert_eq!(client.endpoint(), TEST_NET_1);

        // The pinned Low-confidence redaction condition (Luhn-failing digits
        // beside card context) — a request the guard must refuse externally.
        let request =
            ChatRequest::new(vec![ChatMessage::user("credit card: 4111 1111 1111 1112")]);
        let err = client.stream_chat(&request, &|_| {}).await.unwrap_err();
        assert_eq!(
            err.kind(),
            "guard-blocked",
            "production-constructed state must block before connect; \
             offline would mean a connect was attempted"
        );
        assert_eq!(err.endpoint(), TEST_NET_1);

        let snapshot = guard.snapshot();
        assert_eq!(snapshot.blocked_count, 1);
        assert_eq!(
            snapshot.last_block_reason,
            Some(super::super::guard::GuardBlockReason::LowConfidence)
        );
    }
}
