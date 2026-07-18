//! Tauri IPC surface for chat (R002) and model routing (R003): the `chat`,
//! `llm_health`, `set_model`, and `model_info` commands plus the
//! `llm://token` / `llm://done` / `llm://error` event stream.
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

use super::openai::{OpenAiClient, DEFAULT_ENDPOINT};
use super::router::{ModelInfo, ModelRouter, HEAVY_LANE, THIN_LANE};
use super::{ChatMessage, LlmClient, LlmError, LlmHealth};

/// Event names — the string half of the IPC contract with `src/chat.ts`.
pub const TOKEN_EVENT: &str = "llm://token";
pub const DONE_EVENT: &str = "llm://done";
pub const ERROR_EVENT: &str = "llm://error";
/// Routing-state broadcast (S07): mutation responses only reach the calling
/// window, so every successful `set_model` / `set_lane_model` also emits the
/// updated [`ModelInfo`] app-wide — the overlay stays truthful when the
/// settings window changes routing.
pub const MODEL_INFO_EVENT: &str = "llm://model-info";

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

    /// State backed by the project-default LM Studio endpoint with the
    /// canonical thin/heavy lane pair. Lane model ids come from the
    /// `THIRD_EYE_THIN_MODEL` / `THIRD_EYE_HEAVY_MODEL` env vars — the single
    /// override site until S05 ships real configurability. An unset (or
    /// blank) var leaves that lane unpinned: requests omit the `model` key
    /// and a single-model LM Studio serves whatever it has loaded.
    pub fn with_default_endpoint() -> Self {
        Self::new(Arc::new(ModelRouter::thin_heavy(
            DEFAULT_ENDPOINT,
            env_model(std::env::var("THIRD_EYE_THIN_MODEL").ok()),
            env_model(std::env::var("THIRD_EYE_HEAVY_MODEL").ok()),
        )))
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

        let result = client.stream_chat(&messages, &on_token).await;
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

/// Treat unset, empty, and whitespace-only env values as "no pinned model"
/// so `THIRD_EYE_THIN_MODEL=""` behaves like an absent var instead of
/// pinning a nameless model.
fn env_model(value: Option<String>) -> Option<String> {
    value.map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
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
            _messages: &[ChatMessage],
            _on_token: super::super::TokenSink<'_>,
        ) -> Result<super::super::StreamOutcome, LlmError> {
            Ok(super::super::StreamOutcome { text: String::new(), token_count: 0 })
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
}
