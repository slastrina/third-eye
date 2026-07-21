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

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State};
use tokio::sync::oneshot;

use crate::input::commands::SessionWhitelist;
use crate::input::ActionKind;

use super::guard::{GuardState, GuardTelemetry};
use super::mcp::{
    McpApprovalGate, McpApprovalPrompt, McpApprovalVerdict, McpExecutor, McpHealthStatus,
    McpRunMode, McpServerConfig, McpState,
};
use super::mcp_keystore::{McpAuthError, McpAuthStore};
use super::openai::{OpenAiClient, DEFAULT_ENDPOINT};
use super::router::{ModelInfo, ModelRouter, HEAVY_LANE, THIN_LANE};
use super::toolloop::{
    run_tool_loop_with_stop, ApprovalGate, ApprovalPrompt, ApprovalVerdict, CompositeExecutor,
    FocusAppTool, FocusedApp, InputTool, MemorySearchTool, ScreenQueryTool, ScreenSeen, ToolEvent,
    ToolExecutor, HID_SYSTEM_PROMPT, TOOL_CALL_EVENT, TOOL_RESULT_EVENT,
};
use super::{skills, ChatMessage, LlmClient, LlmError, LlmHealth, Role};

/// Event names — the string half of the IPC contract with `src/chat.ts`.
pub const TOKEN_EVENT: &str = "llm://token";
pub const DONE_EVENT: &str = "llm://done";
pub const ERROR_EVENT: &str = "llm://error";
/// Reasoning-delta stream (thinking models): a model's chain-of-thought
/// (`delta.reasoning_content` / `delta.reasoning`) arrives as its own event
/// stream so the overlay can render a dimmed "Thinking…" region distinct from
/// the answer — and so reasoning never lands in the answer body (which used to
/// fill with blank newlines while the heavy model thought). Transient by
/// construction: never persisted, cleared per turn UI-side. The string half of
/// the contract with `src/chat.ts` (pinned by a Rust test and its TS twin).
pub const REASONING_EVENT: &str = "llm://reasoning";
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
/// HID approval-request broadcast (S04 T03): when the approval gate hits an
/// `Ask`-mode action whose kind is not yet whitelisted, it emits this event
/// carrying the pending action's summary and awaits the overlay's verdict via
/// the `respond_hid_approval` IPC. The string half of the contract with
/// `src/chat.ts` (pinned by a Rust test and its TS twin).
pub const HID_APPROVAL_EVENT: &str = "hid://approval-request";
/// MCP tool approval-request broadcast (S03 T02): when the [`McpApprovalGate`]
/// hits an `Ask`-mode call whose tool name is not yet allowlisted, it emits this
/// event carrying the pending call's summary and awaits the overlay's verdict via
/// the S04 `respond_mcp_approval` IPC (the MCP twin of [`HID_APPROVAL_EVENT`]).
/// The string half of the contract with `src/chat.ts` — its reply command lands
/// in S04; until then an `Ask` prompt fails closed (times out → Deny).
pub const MCP_APPROVAL_EVENT: &str = "mcp://approval-request";
/// MCP host health broadcast (S04 T02): every lifecycle transition — spawn
/// start, handshake ready, a spawn/handshake/mid-session crash — and every
/// run-mode change emits the resulting [`McpHealthStatus`] app-wide, so the
/// Settings MCP surface stays truthful without polling (the `cloud://optin` /
/// `watcher://state` health-as-value precedent). The string half of the
/// contract with `src/mcp-state.ts` (pinned by a Rust test and its TS twin).
pub const MCP_STATE_EVENT: &str = "mcp://state";
/// Chat run-state broadcast (S04 T04): every transition of the in-flight run —
/// `running` when a chat starts, `stopped` when the user's Stop cuts it short,
/// `idle` on a natural finish or error — emits this app-wide so the overlay's
/// Stop control appears exactly while a run is active and clears when it ends.
/// The string half of the contract with `src/chat.ts` (pinned by a Rust test
/// and its TS twin).
pub const RUN_STATE_EVENT: &str = "llm://run-state";

/// The chat loop's coarse run-state (S04 T04). `Running` while a chat task drives
/// its tool rounds; `Stopped` when the user hit Stop; `Idle` otherwise. Serialized
/// kebab-case so `src/chat.ts` shares the exact wire strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RunPhase {
    Idle,
    Running,
    Stopped,
}

/// The `llm://run-state` / `run_state` payload — the current [`RunPhase`]. A
/// struct (not the bare enum) so the surface can grow without a breaking IPC
/// change, and camelCase to match every other event payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RunStatePayload {
    pub phase: RunPhase,
}

/// How long the gate waits for the overlay's approval verdict before failing
/// closed. A slow (or absent) user is a [`ApprovalVerdict::Deny`], never a hung
/// tool loop (R006): a HID action is refused unless the user actively allows it.
const APPROVAL_TIMEOUT: Duration = Duration::from_secs(120);

/// The `hid://approval-request` payload — the pending action the overlay must
/// approve or deny. `approvalId` correlates the emitted request with the
/// `respond_hid_approval` reply; `summary` is the human sentence the overlay
/// shows. Pixel-free by construction: a kind tag and a summary string, never a
/// screenshot or persisted coordinate (R011/R023).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalRequestPayload {
    pub approval_id: u64,
    pub kind: ActionKind,
    pub summary: String,
}

/// App-shared HID approval state (S04 T03): the session-scoped by-kind whitelist
/// the gate consults and the pending-verdict registry the `respond_hid_approval`
/// command delivers into. Managed once, cloned into every chat run's gate — so
/// an "Always allow this kind" grant made in one run survives to the next within
/// the app session, and clears on app exit (this state dropping) so no grant
/// outlives the session (R023).
pub struct ApprovalState {
    whitelist: Arc<Mutex<SessionWhitelist>>,
    /// Correlation-id → the waiting gate's verdict sender. An entry lives only
    /// between the request emit and the reply/timeout, then is removed.
    pending: Mutex<HashMap<u64, oneshot::Sender<ApprovalVerdict>>>,
    next_id: AtomicU64,
}

impl Default for ApprovalState {
    fn default() -> Self {
        Self::new()
    }
}

impl ApprovalState {
    pub fn new() -> Self {
        Self {
            whitelist: Arc::new(Mutex::new(SessionWhitelist::new())),
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
        }
    }

    /// The shared session whitelist handle the gate mutates on "Always allow".
    pub fn whitelist(&self) -> Arc<Mutex<SessionWhitelist>> {
        self.whitelist.clone()
    }

    /// Allocate a correlation id and register a one-shot channel the overlay's
    /// reply will be delivered into.
    fn register(&self) -> (u64, oneshot::Receiver<ApprovalVerdict>) {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        (id, rx)
    }

    /// Drop a pending waiter without a verdict — the timeout / emit-failure path.
    fn cancel(&self, id: u64) {
        self.pending.lock().unwrap().remove(&id);
    }

    /// Deliver a verdict to the gate waiting on `id`. Returns whether a live
    /// waiter existed — a stale id (already timed out / replied) is a no-op the
    /// command logs, never a panic.
    fn respond(&self, id: u64, verdict: ApprovalVerdict) -> bool {
        match self.pending.lock().unwrap().remove(&id) {
            Some(tx) => tx.send(verdict).is_ok(),
            None => false,
        }
    }
}

/// The production [`ApprovalPrompt`]: emits `hid://approval-request` to the
/// overlay and awaits the `respond_hid_approval` reply through the shared
/// [`ApprovalState`] registry, with a bounded [`APPROVAL_TIMEOUT`]. Every
/// non-verdict outcome — a failed emit (no overlay), a closed channel, or a
/// timeout — resolves to [`ApprovalVerdict::Deny`] so a HID action is never
/// performed without an explicit allow (fail-closed, R006/R016 posture).
struct OverlayApprovalPrompt {
    app: AppHandle,
    state: Arc<ApprovalState>,
    request_id: u64,
}

impl OverlayApprovalPrompt {
    fn new(app: AppHandle, state: Arc<ApprovalState>, request_id: u64) -> Self {
        Self { app, state, request_id }
    }
}

#[async_trait::async_trait]
impl ApprovalPrompt for OverlayApprovalPrompt {
    async fn request(&self, kind: ActionKind, summary: String) -> ApprovalVerdict {
        let (approval_id, rx) = self.state.register();
        log::info!(
            "llm: HID approval requested id={approval_id} kind={kind:?} (request={})",
            self.request_id
        );
        let payload = ApprovalRequestPayload { approval_id, kind, summary };
        if let Err(e) = self.app.emit(HID_APPROVAL_EVENT, payload) {
            log::warn!("llm: HID approval-request emit failed id={approval_id}: {e}; denying");
            self.state.cancel(approval_id);
            return ApprovalVerdict::Deny;
        }
        match tokio::time::timeout(APPROVAL_TIMEOUT, rx).await {
            Ok(Ok(verdict)) => {
                log::info!("llm: HID approval id={approval_id} verdict={verdict:?}");
                verdict
            }
            Ok(Err(_closed)) => {
                log::warn!("llm: HID approval id={approval_id} channel closed; denying");
                ApprovalVerdict::Deny
            }
            Err(_elapsed) => {
                self.state.cancel(approval_id);
                log::warn!(
                    "llm: HID approval id={approval_id} timed out after {}s; denying",
                    APPROVAL_TIMEOUT.as_secs()
                );
                ApprovalVerdict::Deny
            }
        }
    }
}

/// The `mcp://approval-request` payload — the pending external MCP tool call the
/// overlay must approve or deny (S03 T02). `approvalId` correlates the emitted
/// request with the S04 `respond_mcp_approval` reply; `toolName` is the namespaced
/// tool name; `summary` is the human sentence the overlay shows. Pixel-free and
/// argument-bounded by construction (R011/R023) — the MCP twin of
/// [`ApprovalRequestPayload`].
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpApprovalRequestPayload {
    pub approval_id: u64,
    pub tool_name: String,
    pub summary: String,
}

/// The production [`McpApprovalPrompt`]: emits `mcp://approval-request` to the
/// overlay and awaits the reply through the shared [`McpState`] registry, with a
/// bounded [`APPROVAL_TIMEOUT`] (shared with the HID gate). Every non-verdict
/// outcome — a failed emit (no overlay), a closed channel, or a timeout —
/// resolves to [`McpApprovalVerdict::Deny`] so an MCP tool action is never
/// performed without an explicit allow (fail-closed, R006/R016 posture). Reaches
/// the managed [`McpState`] through the [`AppHandle`] at call time so it needs no
/// owned `Arc` handle (the state is managed by value).
struct OverlayMcpApprovalPrompt {
    app: AppHandle,
    request_id: u64,
}

impl OverlayMcpApprovalPrompt {
    fn new(app: AppHandle, request_id: u64) -> Self {
        Self { app, request_id }
    }
}

#[async_trait::async_trait]
impl McpApprovalPrompt for OverlayMcpApprovalPrompt {
    async fn request(&self, tool_name: String, summary: String) -> McpApprovalVerdict {
        let mcp = self.app.state::<McpState>();
        let (approval_id, rx) = mcp.register();
        log::info!(
            "llm: MCP approval requested id={approval_id} tool={tool_name} (request={})",
            self.request_id
        );
        let payload = McpApprovalRequestPayload {
            approval_id,
            tool_name: tool_name.clone(),
            summary,
        };
        if let Err(e) = self.app.emit(MCP_APPROVAL_EVENT, payload) {
            log::warn!("llm: MCP approval-request emit failed id={approval_id}: {e}; denying");
            mcp.cancel(approval_id);
            return McpApprovalVerdict::Deny;
        }
        match tokio::time::timeout(APPROVAL_TIMEOUT, rx).await {
            Ok(Ok(verdict)) => {
                log::info!("llm: MCP approval id={approval_id} verdict={verdict:?}");
                verdict
            }
            Ok(Err(_closed)) => {
                log::warn!("llm: MCP approval id={approval_id} channel closed; denying");
                McpApprovalVerdict::Deny
            }
            Err(_elapsed) => {
                mcp.cancel(approval_id);
                log::warn!(
                    "llm: MCP approval id={approval_id} timed out after {}s; denying",
                    APPROVAL_TIMEOUT.as_secs()
                );
                McpApprovalVerdict::Deny
            }
        }
    }
}

/// One content delta. Fired per token in arrival order.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenEvent {
    pub request_id: u64,
    pub token: String,
}

/// One reasoning delta (thinking models). Same shape as [`TokenEvent`] but on
/// the [`REASONING_EVENT`] channel — the UI appends these to a transient
/// Thinking… region, never to the answer text.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReasoningEvent {
    pub request_id: u64,
    pub delta: String,
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
    /// Cooperative Stop flag (S04 T04): the `stop_chat` command flips this and
    /// the tool loop observes it between rounds/actions, terminating with a
    /// typed `stopped` outcome rather than a hard-abort that would emit nothing.
    /// Shared into the spawned task's `should_stop` closure.
    stop: Arc<AtomicBool>,
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
    /// Coarse chat run-state (S04 T04): the health-as-value the `run_state`
    /// command reads and the `llm://run-state` broadcast carries. Mutated only
    /// through [`Self::mark_running`] / [`Self::request_stop`] /
    /// [`Self::finish_with_phase`] so every transition is one auditable seam.
    run_phase: Mutex<RunPhase>,
}

impl LlmState {
    pub fn new(router: Arc<ModelRouter>) -> Self {
        let probe = OpenAiClient::new(router.endpoint());
        Self {
            router,
            probe,
            next_request_id: AtomicU64::new(1),
            active: Mutex::new(None),
            run_phase: Mutex::new(RunPhase::Idle),
        }
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
        *active = Some(ActiveRequest { id, aborter: None, stop: Arc::new(AtomicBool::new(false)) });
        id
    }

    /// The cooperative Stop flag of the active request, when it is `id` — the
    /// chat task clones this into its `should_stop` closure (S04 T04). A
    /// superseded or absent id yields a fresh never-set flag (a dead handle:
    /// that run is already gone), so the closure is always safe to build.
    fn stop_flag(&self, id: u64) -> Arc<AtomicBool> {
        let active = self.active.lock().unwrap();
        match active.as_ref() {
            Some(req) if req.id == id => req.stop.clone(),
            _ => Arc::new(AtomicBool::new(false)),
        }
    }

    /// Current coarse run-state (health-as-value) — the `run_state` command and
    /// the terminal broadcast read this.
    fn run_phase(&self) -> RunPhase {
        *self.run_phase.lock().unwrap()
    }

    /// Mark the just-begun run Running (its request owns the slot). Returns the
    /// phase so the `chat` command broadcasts it.
    fn mark_running(&self) -> RunPhase {
        *self.run_phase.lock().unwrap() = RunPhase::Running;
        RunPhase::Running
    }

    /// Signal the in-flight run to stop cooperatively (S04 T04): flips the active
    /// request's Stop flag — the tool loop observes it at the next round/action
    /// boundary and terminates with a typed stopped outcome (not a hard-abort
    /// that would emit nothing) — and moves the phase to Stopped. A stop with
    /// nothing in flight is a no-op returning the current phase; never an error.
    fn request_stop(&self) -> RunPhase {
        let active = self.active.lock().unwrap();
        match active.as_ref() {
            Some(req) => {
                req.stop.store(true, Ordering::SeqCst);
                let id = req.id;
                drop(active);
                *self.run_phase.lock().unwrap() = RunPhase::Stopped;
                log::info!("llm: stop requested for in-flight request {id}");
                RunPhase::Stopped
            }
            None => {
                drop(active);
                self.run_phase()
            }
        }
    }

    /// Clear the active slot at a terminal state and record the resulting run
    /// phase — but only if `id` still owns the slot. A superseded request
    /// returns `None` (its successor is already Running; a stale terminal must
    /// not clobber that), so the caller broadcasts only a real transition.
    fn finish_with_phase(&self, id: u64, phase: RunPhase) -> Option<RunPhase> {
        let mut active = self.active.lock().unwrap();
        if active.as_ref().map(|r| r.id) == Some(id) {
            *active = None;
            drop(active);
            *self.run_phase.lock().unwrap() = phase;
            Some(phase)
        } else {
            None
        }
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

}

/// Compose the discovered markdown skill packs (M007 S06) into one system-turn
/// instruction block. Each skill's `description` (its documented triggering
/// signal, mirroring `.agents/skills/*/SKILL.md`) leads its Markdown `body` (the
/// instructions), so the model can decide which skill matches the task and follow
/// its steps. Returns `None` when nothing was discovered — an empty discovery dir
/// must never add a hollow system turn. `discover_skills` already fail-soft-skips
/// and logs malformed/missing packs, so this only ever sees good skills.
fn compose_skills_prompt(discovered: &[skills::Skill]) -> Option<String> {
    if discovered.is_empty() {
        return None;
    }
    let mut prompt = String::from(
        "You have access to the following skills. Each skill's description says when to use it; \
         when a skill matches the task, follow its instructions.\n",
    );
    for skill in discovered {
        prompt.push_str(&format!(
            "\n## Skill: {}\n{}\n\n{}\n",
            skill.name, skill.description, skill.body
        ));
    }
    Some(prompt)
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

    // Run-state: this request now owns the slot, so the run is Running. Broadcast
    // it before the task spawns so the overlay's Stop control appears immediately
    // (S04 T04). The cooperative Stop flag is cloned into the loop's should_stop.
    let stop = state.stop_flag(id);
    broadcast_run_state(&app, state.mark_running());

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

        // Reasoning deltas (thinking models) ride their own event so the overlay
        // shows a dimmed Thinking… region — kept out of the answer text, which is
        // what used to fill with blank newlines. Emission failure is logged, never
        // fatal, same policy as tokens.
        let on_reasoning = |delta: &str| {
            let payload = ReasoningEvent { request_id: id, delta: delta.into() };
            if let Err(e) = task_app.emit(REASONING_EVENT, payload) {
                log::warn!("llm: request {id} reasoning emit failed: {e}");
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

        // The model reaches HID input and screen_query unconditionally
        // (S01/S02, M005) and memory_search exactly when the S02 store opened
        // this run — the CompositeExecutor advertises whichever tools exist and
        // dispatches by name (D037). memory_search's absence is logged, not
        // silent; the input and screen-query tools are always mounted, so there
        // is no tools-free path. screen_query's coordinates flow only to the
        // model (to aim an input_action) and never reach the store (R011/R023).
        let memory = task_app.state::<crate::memory::MemoryState>();
        let input_state = task_app.state::<crate::input::commands::InputState>();
        let screen_state = task_app.state::<crate::screenquery::commands::ScreenQueryState>();
        let appfocus_state = task_app.state::<crate::appfocus::commands::AppFocusState>();
        let mut executors: Vec<Box<dyn ToolExecutor>> = Vec::new();
        match memory.store() {
            Some(store) => {
                executors.push(Box::new(MemorySearchTool::new(
                    store,
                    memory.embedder(client.endpoint()),
                )));
            }
            None => {
                log::info!("llm: request {id} runs without memory_search (store unavailable)");
            }
        }
        // The InputTool draws BOTH its backend and its armed-state handle from
        // the managed InputState (D038/S03): the composite advertises
        // input_action only while HID is armed, and a disarmed action reaching
        // execute() is refused before the backend is touched — one shared holder,
        // no separate mount. In S04 it is wrapped by the ApprovalGate so every
        // HID action is gated through the per-action approval resolver before it
        // reaches the backend.
        //
        // Run mode (S04/T05): the gate snapshots the persisted three-way run mode
        // the user picked in Settings (Off/Ask/Auto-run), applied AX-gated into
        // the shared InputState — Off stays inert (D038), Ask prompts inline per
        // not-yet-whitelisted kind, Auto-run performs without prompting. The gate
        // wraps BOTH HID-class surfaces (M005): input_action and focus_app, so a
        // best-effort app activation is gated on the exact same Off/Ask/Auto-run
        // path (ActionKind::FocusApp) — Off refuses before activating.
        let approval = task_app.state::<Arc<ApprovalState>>();
        let mode = input_state.mode();
        let approver = Arc::new(OverlayApprovalPrompt::new(
            task_app.clone(),
            approval.inner().clone(),
            id,
        ));
        // Per-run targeting gates (M005): both shared between the ScreenQueryTool
        // and the ApprovalGate. `screen_seen` refuses a mouse-move until the model
        // has grounded coordinates via screen_query; `focused_app` filters
        // screen_query results to the app the model focused so it can only click
        // inside that app, never the desktop. Fresh per request — neither carries
        // across conversations.
        let screen_seen = Arc::new(ScreenSeen::new());
        let focused_app = Arc::new(FocusedApp::new());
        executors.push(Box::new(ApprovalGate::new(
            InputTool::new(input_state.backend(), input_state.arm_state()),
            FocusAppTool::new(appfocus_state.backend()),
            mode,
            approval.whitelist(),
            approver,
            screen_seen.clone(),
            focused_app.clone(),
        )));
        executors.push(Box::new(ScreenQueryTool::new(
            screen_state.backend(),
            screen_seen,
            focused_app,
        )));
        // External MCP tools (M007 S02): when an already-serving MCP client peer
        // has been injected for this run, mount an McpExecutor so the agent loop
        // SEES the server's tools — each namespaced under `mcp__` so a collision
        // with the four built-ins (memory_search / input_action / screen_query /
        // focus_app) is structurally impossible. Absence is logged, never silent,
        // never fatal (mirrors the memory_search Some/None at :589-599); the full
        // settings-driven spawn/lifecycle that injects the peer is S04.
        //
        // S03: the McpExecutor is WRAPPED in the McpApprovalGate BEFORE being
        // pushed, so no production MCP tool-action path reaches the server's
        // call_tool choke point unguarded — the runtime half of R016's extension
        // of the guard boundary to external tool actions, mirroring the HID
        // ApprovalGate wrap above and pinned structurally by
        // scripts/check-mcp-guard.sh (T03). The gate reads its run mode and
        // session allowlist through McpState seams so S04 wires persisted config
        // with no gate change; the prompt seam emits mcp://approval-request and
        // fails closed (Deny) on timeout/emit-failure.
        let mcp = task_app.state::<McpState>();
        match mcp.peer() {
            Some(peer) => match McpExecutor::connect(peer).await {
                Ok(mcp_executor) => {
                    let mcp_mode = mcp.mode();
                    let mcp_approver = Arc::new(OverlayMcpApprovalPrompt::new(task_app.clone(), id));
                    executors.push(Box::new(McpApprovalGate::new(
                        mcp_executor,
                        mcp_mode,
                        mcp.allowlist(),
                        mcp_approver,
                    )));
                }
                Err(e) => log::warn!(
                    "llm: request {id} MCP tools unavailable — tools/list handshake failed: {e}"
                ),
            },
            None => {
                log::info!("llm: request {id} runs without MCP tools (no server peer injected)");
            }
        }
        let executor = CompositeExecutor::new(executors);
        // Ground the model in the focus→query→click discipline (M005 targeting
        // fix). Only when the caller didn't already send a system turn — the
        // summon-from-nudge path prepends its own screen-context system message
        // and must not be clobbered. Prepended so it leads the conversation.
        let mut messages = messages;
        if !messages.iter().any(|m| m.role == Role::System) {
            messages.insert(0, ChatMessage::system(HID_SYSTEM_PROMPT));
            log::debug!("llm: request {id} grounded with HID orchestration system prompt");
        }
        // Load discovered markdown skill packs (M007 S06) into the system turn so
        // a dropped-in SKILL.md visibly shapes behavior for a matching task. The
        // discovery dir resolves through the fail-soft config triad (unset/garbage
        // → app_data_dir/skills); discover_skills already logs+skips malformed or
        // missing packs and returns an empty vec for a missing dir, so this path is
        // never fatal. Prepended as its own LEADING system message — after the HID
        // check above (so HID grounding still fires when the caller sent no system
        // turn) and ahead of everything else, never clobbering the caller-sent
        // system turn (summon-from-nudge). An empty discovery injects nothing.
        let skills_dir = crate::config::resolve_skills_dir(&task_app);
        let discovered = skills::discover_skills(&skills_dir);
        if let Some(skill_prompt) = compose_skills_prompt(&discovered) {
            messages.insert(0, ChatMessage::system(skill_prompt));
            log::info!(
                "llm: request {id} loaded {} skill(s) into the system turn from {}",
                discovered.len(),
                skills_dir.display()
            );
        }
        // The loop observes the cooperative Stop flag between rounds/actions and
        // terminates with a typed stopped outcome (S04 T04).
        let should_stop = move || stop.load(Ordering::SeqCst);
        let result = run_tool_loop_with_stop(
            client.as_ref(),
            &executor,
            messages,
            id,
            &on_token,
            &on_reasoning,
            &on_event,
            &should_stop,
        )
        .await;
        let total_ms = started.elapsed().as_millis() as u64;

        // A user-stopped run ends Stopped; a natural finish or error ends Idle.
        // The terminal phase is broadcast below only if this request still owns
        // the slot (a superseded task must not clobber its successor's Running).
        let terminal_phase =
            if matches!(&result, Ok(outcome) if outcome.stopped) { RunPhase::Stopped } else { RunPhase::Idle };

        match result {
            Ok(loop_outcome) => {
                let outcome = loop_outcome.outcome;
                let first_token_ms = first_token_at
                    .lock()
                    .unwrap()
                    .map(|t| t.duration_since(started).as_millis() as u64);
                log::info!(
                    "llm stream total: {total_ms} ms, {} tokens stopped={} (request={id})",
                    outcome.token_count,
                    loop_outcome.stopped
                );
                log::debug!(
                    "llm: request {id} done tokens={} chars={} stopped={}",
                    outcome.token_count,
                    outcome.text.chars().count(),
                    loop_outcome.stopped
                );
                // A stopped run still emits DONE with whatever text streamed, so
                // the assistant message settles visibly (never silent, R006); the
                // Stopped run-state broadcast below is what tells the UI it was cut
                // short.
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

        if let Some(phase) = task_app.state::<LlmState>().finish_with_phase(id, terminal_phase) {
            broadcast_run_state(&task_app, phase);
        }
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

/// Emit the chat run-state to every window (S04 T04): the overlay's Stop control
/// keys on this. Emission failure is logged, not fatal — the state stays
/// queryable via `run_state`, same posture as the model-info/privacy broadcasts.
fn broadcast_run_state(app: &AppHandle, phase: RunPhase) {
    if let Err(e) = app.emit(RUN_STATE_EVENT, RunStatePayload { phase }) {
        log::warn!("llm: run-state broadcast failed: {e}");
    }
}

/// Stop the in-flight chat run (S04 T04): flips the cooperative Stop flag so the
/// tool loop terminates at the next round/action boundary with a typed stopped
/// outcome, and broadcasts the resulting run-state. Health-as-value: never
/// rejects — a Stop with nothing in flight returns the current phase (`idle`),
/// so the overlay can fire it without racing the run's own completion.
#[tauri::command]
pub fn stop_chat(app: AppHandle, state: State<'_, LlmState>) -> RunStatePayload {
    let phase = state.request_stop();
    broadcast_run_state(&app, phase);
    log::debug!("llm: stop_chat -> {phase:?}");
    RunStatePayload { phase }
}

/// Current chat run-state (health-as-value, like `llm_health`): `idle` /
/// `running` / `stopped`. Never rejects — the overlay queries it at mount to
/// render the Stop control truthfully before any broadcast arrives.
#[tauri::command]
pub fn run_state(state: State<'_, LlmState>) -> RunStatePayload {
    RunStatePayload { phase: state.run_phase() }
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

/// Deliver the overlay's verdict for a pending HID approval (S04 T03). The gate
/// is blocked awaiting this reply (or its timeout); a verdict resolves it. Never
/// rejects — an unknown or already-expired `approvalId` (the gate timed out
/// first, or a double reply) is a logged no-op returning `false`, so the overlay
/// can fire-and-forget without racing the timeout into an error.
#[tauri::command]
pub fn respond_hid_approval(
    state: State<'_, Arc<ApprovalState>>,
    approval_id: u64,
    verdict: ApprovalVerdict,
) -> bool {
    let delivered = state.respond(approval_id, verdict);
    if delivered {
        log::debug!("llm: HID approval reply id={approval_id} verdict={verdict:?} delivered");
    } else {
        log::warn!(
            "llm: HID approval reply id={approval_id} verdict={verdict:?} for unknown/expired request (no-op)"
        );
    }
    delivered
}

/// The one shared MCP run-mode applier (S04 T02) — the MCP twin of
/// [`crate::input::commands::apply_hid_run_mode`]. Every mode mutation (the
/// `set_mcp_run_mode` IPC, and any future path) funnels through here so they
/// cannot drift: it sets the in-memory mode the already-mounted gate
/// (`commands.rs` chat-task) snapshots per run, persists it to settings.json,
/// and on a persist failure ROLLS BACK the in-memory mode so an unpersisted
/// choice can never silently take effect for the session while the store still
/// says otherwise (fail-closed, the hotkey/opt-in precedent — a persisted
/// `off` must win on the next restart). Always emits and returns the resulting
/// [`McpHealthStatus`]; a persist failure is logged, never thrown (R007). No
/// gate change: the gate reads the mode through the [`McpState`] seam.
pub fn apply_mcp_run_mode(app: &AppHandle, desired: McpRunMode, via: &str) -> McpHealthStatus {
    let state = app.state::<McpState>();
    let previous = state.mode();
    state.set_mode(desired);
    match crate::config::save_mcp_run_mode(app, desired) {
        Ok(()) => log::info!("llm: MCP run mode = {desired:?} (via {via})"),
        Err(e) => {
            // Roll the in-memory mode back to the persisted value: an unpersisted
            // Ask/AutoRun must not run external tools this session when the store
            // still says off (R016 fail-closed). The error names the persist path.
            state.set_mode(previous);
            log::error!("llm: MCP run mode persist failed, rolled back to {previous:?}: {e}");
        }
    }
    let status = state.status();
    // Broadcast failure is cosmetic — the truth stays queryable via `mcp_status`
    // — so it is logged, never bubbled (the `cloud://optin` posture).
    if let Err(e) = app.emit(MCP_STATE_EVENT, status.clone()) {
        log::warn!("llm: MCP state broadcast failed: {e}");
    }
    status
}

/// Apply the persisted MCP run mode at startup (called from `setup()`).
/// In-memory only: no re-save, no broadcast — nothing is listening yet. An
/// absent/garbage key keeps the fail-closed default (`Off`, inert — no external
/// tool runs without an explicit choice, R016); load/interpret failures are
/// logged inside `config`, never fatal.
pub fn apply_persisted_mcp_run_mode(app: &AppHandle) {
    if let Some(mode) = crate::config::load_mcp_run_mode(app) {
        app.state::<McpState>().set_mode(mode);
        log::info!("llm: applied persisted MCP run mode ({mode:?})");
    }
}

/// Select the MCP run mode from the UI (S04 T02) — the MCP twin of
/// `set_hid_run_mode`. Returns the resulting [`McpHealthStatus`] instead of
/// erroring: a persist failure is data the caller renders (rolled back and
/// logged), the same health-as-value contract as `set_cloud_optin` (R007).
#[tauri::command]
pub fn set_mcp_run_mode(app: AppHandle, mode: McpRunMode) -> McpHealthStatus {
    apply_mcp_run_mode(&app, mode, "ipc")
}

/// Current MCP host health — health-as-value beside `cloud_optin_status` and
/// `run_state` (R007): the `{ phase, lastError, updatedAt, mode, toolCount }`
/// value at any time, never an error. The Settings MCP surface queries it at
/// mount to render the health line before any `mcp://state` broadcast arrives.
#[tauri::command]
pub fn mcp_status(state: State<'_, McpState>) -> McpHealthStatus {
    state.status()
}

/// Deliver the overlay's verdict for a pending MCP tool-call approval (S04 T04)
/// — the MCP twin of [`respond_hid_approval`]. The [`McpApprovalGate`]'s prompt
/// seam ([`OverlayMcpApprovalPrompt`]) is blocked awaiting this reply (or its
/// timeout); a verdict resolves it. Never rejects — an unknown or already-expired
/// `approvalId` (the gate timed out first, or a double reply) is a logged no-op
/// returning `false`, so the overlay can fire-and-forget without racing the
/// timeout into an error. Reaches the pending-verdict registry through the
/// managed [`McpState`] (managed by value, beside the HID `ApprovalState`).
#[tauri::command]
pub fn respond_mcp_approval(
    state: State<'_, McpState>,
    approval_id: u64,
    verdict: McpApprovalVerdict,
) -> bool {
    let delivered = state.respond(approval_id, verdict);
    if delivered {
        log::debug!("llm: MCP approval reply id={approval_id} verdict={verdict:?} delivered");
    } else {
        log::warn!(
            "llm: MCP approval reply id={approval_id} verdict={verdict:?} for unknown/expired request (no-op)"
        );
    }
    delivered
}

/// The queryable MCP server-list state (S04 T04) — health-as-value beside
/// [`McpHealthStatus`] and `CloudHeavyProviderStatus` (R007): the persisted
/// `mcpServers` list plus the most recent `persistError`, a value at any time,
/// never an IPC rejection. `servers` is always the authoritative persisted list
/// (on a save failure it stays the last-persisted list, so the UI never shows an
/// unpersisted change as if it took); `persistError` names a failed `save_mcp_servers`
/// so a change that could not be written stays visible.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpServersStatus {
    pub servers: Vec<McpServerConfig>,
    pub persist_error: Option<String>,
}

/// Current persisted MCP server list — health-as-value, never an error. The
/// Settings MCP surface queries it at mount to render the server rows. An absent
/// key / unopenable store collapses to the empty list (logged in `config`).
#[tauri::command]
pub fn mcp_servers(app: AppHandle) -> McpServersStatus {
    McpServersStatus {
        servers: crate::config::load_mcp_servers(&app).unwrap_or_default(),
        persist_error: None,
    }
}

/// Persist the MCP server list from the Settings add/remove surface (S04 T04).
/// Never rejects backend-side (the `set_cloud_heavy_provider` contract, R007): on
/// success returns the saved list with a null `persistError`; on a persist failure
/// it returns the still-authoritative previously-persisted list with `persistError`
/// set, so an unpersisted change never appears to have taken — the change takes
/// effect at the next startup launch task, so a silent revert on restart would be
/// invisible otherwise. The list edit is spawn-inert until restart by design (the
/// launch task reads `mcpServers` at startup), so there is no live in-memory list
/// to roll back — the persisted store is the single source of truth.
#[tauri::command]
pub fn set_mcp_servers(app: AppHandle, servers: Vec<McpServerConfig>) -> McpServersStatus {
    match crate::config::save_mcp_servers(&app, &servers) {
        Ok(()) => {
            log::info!("llm: persisted {} MCP server(s) via settings", servers.len());
            McpServersStatus { servers, persist_error: None }
        }
        Err(e) => {
            // The store still holds the previous list; surface THAT as the truth so
            // the UI cannot show an unpersisted edit as saved (fail-closed).
            log::error!("llm: MCP server list persist failed: {e}");
            McpServersStatus {
                servers: crate::config::load_mcp_servers(&app).unwrap_or_default(),
                persist_error: Some(e),
            }
        }
    }
}

/// Managed state: one production [`McpAuthStore`] for the app's lifetime — the
/// keychain home for remote MCP server bearer tokens (S05 T03, R018), beside
/// [`crate::cloud::commands::CloudKeysState`]. The store handle is crate-visible
/// so the http connect path (T04) can read a token for the `Authorization`
/// header through the crate-internal `get_token`; the IPC surface below only
/// ever exposes presence.
pub struct McpAuthState {
    store: McpAuthStore,
}

impl McpAuthState {
    pub fn new() -> Self {
        Self { store: McpAuthStore::new() }
    }

    /// The keystore handle — the http connect path (T04) reads tokens through
    /// this (and through the crate-internal `get_token` only).
    pub fn store(&self) -> &McpAuthStore {
        &self.store
    }
}

impl Default for McpAuthState {
    fn default() -> Self {
        Self::new()
    }
}

/// Presence-only snapshot for one MCP auth account — the entire outbound IPC
/// vocabulary of the MCP keystore, the twin of `CloudKeyStatus`. A single
/// camelCase boolean; adding any string field here (above all one that could
/// carry the token) should trip the `mcp_auth_status_carries_presence_boolean_only`
/// contract test.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpAuthStatus {
    pub present: bool,
}

/// IPC: store a bearer token for a remote MCP server's `authRef` account. The
/// token crosses IPC inbound here — the one legitimate crossing — and is handed
/// straight to the OS store, never held or echoed. Returns the fresh presence so
/// the Settings row renders truth without a second query (the `set_cloud_api_key`
/// contract).
#[tauri::command]
pub fn set_mcp_auth(
    state: State<'_, McpAuthState>,
    auth_ref: String,
    token: String,
) -> Result<McpAuthStatus, McpAuthError> {
    state.store.set_token(&auth_ref, &token).map_err(|e| {
        // The account key is non-secret (it is the settings.json authRef); the
        // token bytes are never logged.
        log::error!("llm: set MCP auth failed for {} ({})", auth_ref.trim(), e.kind());
        e
    })?;
    Ok(McpAuthStatus { present: state.store.token_present(&auth_ref)? })
}

/// IPC: delete a remote MCP server's stored bearer token. Deleting an absent
/// token succeeds (the `delete_cloud_api_key` contract). Returns the fresh
/// presence (`false` on success).
#[tauri::command]
pub fn delete_mcp_auth(
    state: State<'_, McpAuthState>,
    auth_ref: String,
) -> Result<McpAuthStatus, McpAuthError> {
    state.store.delete_token(&auth_ref).map_err(|e| {
        log::error!("llm: delete MCP auth failed for {} ({})", auth_ref.trim(), e.kind());
        e
    })?;
    Ok(McpAuthStatus { present: state.store.token_present(&auth_ref)? })
}

/// IPC: presence snapshot for one `authRef` account — the Settings MCP surface
/// renders the write-only token field's "stored / not stored" state from this.
/// Presence only, ever: the token itself has no outbound command (R018).
#[tauri::command]
pub fn mcp_auth_status(
    state: State<'_, McpAuthState>,
    auth_ref: String,
) -> Result<McpAuthStatus, McpAuthError> {
    state.store.token_present(&auth_ref).map(|present| McpAuthStatus { present }).map_err(|e| {
        log::error!("llm: MCP auth status query failed for {} ({})", auth_ref.trim(), e.kind());
        e
    })
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
    use super::super::ChatRequest;
    use super::*;
    use std::sync::atomic::AtomicBool;

    /// The outbound MCP-auth IPC contract: exactly one camelCase presence
    /// boolean. Any new field — above all a string that could carry the bearer
    /// token — fails this test and forces a deliberate contract change (the
    /// `CloudKeyStatus` presence-only pin, R018).
    #[test]
    fn mcp_auth_status_carries_presence_boolean_only() {
        for present in [true, false] {
            let v = serde_json::to_value(McpAuthStatus { present }).unwrap();
            let obj = v.as_object().unwrap();
            assert_eq!(obj.len(), 1, "status must stay presence-only: {obj:?}");
            assert_eq!(obj["present"], present);
            assert!(obj.values().all(|value| value.is_boolean()));
        }
    }

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
        s.finish_with_phase(id, RunPhase::Idle);

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
        s.finish_with_phase(old, RunPhase::Idle);
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

        s.finish_with_phase(new, RunPhase::Idle);
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
    fn run_state_event_name_and_wire_strings_are_the_ipc_contract() {
        // src/chat.ts pins the same event string and phase strings — the const
        // pair is the contract lock (S04 T04).
        assert_eq!(RUN_STATE_EVENT, "llm://run-state");
        assert_eq!(serde_json::to_value(RunPhase::Idle).unwrap(), "idle");
        assert_eq!(serde_json::to_value(RunPhase::Running).unwrap(), "running");
        assert_eq!(serde_json::to_value(RunPhase::Stopped).unwrap(), "stopped");
        let v = serde_json::to_value(RunStatePayload { phase: RunPhase::Running }).unwrap();
        assert_eq!(v["phase"], "running");
    }

    #[test]
    fn run_phase_begins_idle_and_marks_running_on_begin() {
        let s = state();
        assert_eq!(s.run_phase(), RunPhase::Idle, "a fresh state is idle");
        s.begin();
        assert_eq!(s.mark_running(), RunPhase::Running);
        assert_eq!(s.run_phase(), RunPhase::Running);
    }

    #[test]
    fn request_stop_flips_the_flag_and_moves_to_stopped() {
        let s = state();
        let id = s.begin();
        s.mark_running();
        let flag = s.stop_flag(id);
        assert!(!flag.load(Ordering::SeqCst), "the flag starts unset");

        assert_eq!(s.request_stop(), RunPhase::Stopped);
        assert!(flag.load(Ordering::SeqCst), "stop must flip the loop's cooperative flag");
        assert_eq!(s.run_phase(), RunPhase::Stopped);
    }

    #[test]
    fn request_stop_with_nothing_in_flight_is_an_idle_no_op() {
        let s = state();
        // Never began a request: stop is a health-as-value no-op returning idle.
        assert_eq!(s.request_stop(), RunPhase::Idle);
        assert_eq!(s.run_phase(), RunPhase::Idle);
    }

    #[test]
    fn finish_with_phase_only_the_owner_transitions_the_run_state() {
        let s = state();
        let old = s.begin();
        s.mark_running();
        // A newer request supersedes `old` and owns the slot.
        s.begin();
        // The superseded task reaching its terminal state must not clobber the
        // successor's Running phase.
        assert_eq!(s.finish_with_phase(old, RunPhase::Idle), None);
        assert_eq!(s.run_phase(), RunPhase::Running, "a stale finish left the phase alone");
    }

    #[test]
    fn stop_flag_for_a_superseded_id_is_a_dead_handle() {
        let s = state();
        let old = s.begin();
        s.begin(); // supersede
        // The old id no longer owns the slot: it gets a fresh never-set flag, so
        // a late should_stop closure over it can never falsely stop the new run.
        assert!(!s.stop_flag(old).load(Ordering::SeqCst));
    }

    #[test]
    fn hid_approval_event_name_is_the_ipc_contract() {
        // src/chat.ts pins the same string on the TS side — the two const tests
        // are the contract-lock pair (S04 T03).
        assert_eq!(HID_APPROVAL_EVENT, "hid://approval-request");
    }

    #[test]
    fn mcp_state_event_name_is_the_ipc_contract() {
        // src/mcp-state.ts (T04) listens on this exact string — the const test
        // pair is the contract lock, beside cloud://optin / watcher://state.
        assert_eq!(MCP_STATE_EVENT, "mcp://state");
    }

    #[test]
    fn mcp_approval_event_name_is_the_ipc_contract() {
        // src/chat.ts pins the same string on the TS side — the two const tests
        // are the contract-lock pair (S04 T04), the MCP twin of the HID pair.
        assert_eq!(MCP_APPROVAL_EVENT, "mcp://approval-request");
    }

    #[test]
    fn mcp_approval_request_payload_serializes_camel_case() {
        // src/chat.ts's McpApprovalRequest reads approvalId + toolName + summary;
        // a change here is a breaking IPC change the frontend must match.
        let v = serde_json::to_value(McpApprovalRequestPayload {
            approval_id: 7,
            tool_name: "mcp__weather_forecast".into(),
            summary: "Call mcp__weather_forecast({\"city\":\"Paris\"})".into(),
        })
        .unwrap();
        assert_eq!(v["approvalId"], 7);
        assert_eq!(v["toolName"], "mcp__weather_forecast");
        assert_eq!(v["summary"], "Call mcp__weather_forecast({\"city\":\"Paris\"})");
    }

    #[test]
    fn mcp_approval_verdict_deserializes_the_kebab_case_wire_strings() {
        // The exact strings src/chat.ts sends over respond_mcp_approval.
        assert_eq!(
            serde_json::from_str::<McpApprovalVerdict>("\"allow-once\"").unwrap(),
            McpApprovalVerdict::AllowOnce
        );
        assert_eq!(
            serde_json::from_str::<McpApprovalVerdict>("\"allow-tool\"").unwrap(),
            McpApprovalVerdict::AllowTool
        );
        assert_eq!(
            serde_json::from_str::<McpApprovalVerdict>("\"deny\"").unwrap(),
            McpApprovalVerdict::Deny
        );
        // A garbage verdict is rejected, not silently coerced to a permissive one.
        assert!(serde_json::from_str::<McpApprovalVerdict>("\"allow-everything\"").is_err());
    }

    #[test]
    fn mcp_servers_status_serializes_camel_case() {
        // src/mcp-state.ts's McpServersStatus reads servers + persistError; the
        // server entries carry the camelCase McpServerConfig shape.
        let v = serde_json::to_value(McpServersStatus {
            servers: vec![McpServerConfig {
                id: "weather".into(),
                command: "npx".into(),
                args: vec!["-y".into(), "@ref/weather".into()],
                enabled: true,
                transport: crate::llm::mcp::McpTransport::Stdio,
                url: None,
                auth_ref: None,
            }],
            persist_error: Some("failed to persist mcpServers to settings.json".into()),
        })
        .unwrap();
        assert_eq!(v["servers"][0]["id"], "weather");
        assert_eq!(v["servers"][0]["command"], "npx");
        assert_eq!(v["servers"][0]["args"][1], "@ref/weather");
        assert_eq!(v["servers"][0]["enabled"], true);
        assert_eq!(v["persistError"], "failed to persist mcpServers to settings.json");
    }

    #[test]
    fn approval_request_payload_serializes_camel_case() {
        // The overlay reads approvalId + kind + summary; a change here is a
        // breaking IPC change the frontend must match.
        let v = serde_json::to_value(ApprovalRequestPayload {
            approval_id: 3,
            kind: ActionKind::MouseClick,
            summary: "Click the left mouse button".into(),
        })
        .unwrap();
        assert_eq!(v["approvalId"], 3);
        assert_eq!(v["kind"], "mouse-click");
        assert_eq!(v["summary"], "Click the left mouse button");

        // The FocusApp kind (M005) flows through the same payload — the overlay
        // reads `focus-app` and its human summary for a focus_app approval.
        let v = serde_json::to_value(ApprovalRequestPayload {
            approval_id: 4,
            kind: ActionKind::FocusApp,
            summary: "Bring \"Google Chrome\" to the front".into(),
        })
        .unwrap();
        assert_eq!(v["kind"], "focus-app");
        assert_eq!(v["summary"], "Bring \"Google Chrome\" to the front");
    }

    #[test]
    fn approval_verdict_deserializes_the_kebab_case_wire_strings() {
        // The exact strings src/chat.ts sends over respond_hid_approval.
        assert_eq!(
            serde_json::from_str::<ApprovalVerdict>("\"allow-once\"").unwrap(),
            ApprovalVerdict::AllowOnce
        );
        assert_eq!(
            serde_json::from_str::<ApprovalVerdict>("\"allow-kind\"").unwrap(),
            ApprovalVerdict::AllowKind
        );
        assert_eq!(
            serde_json::from_str::<ApprovalVerdict>("\"deny\"").unwrap(),
            ApprovalVerdict::Deny
        );
        // A garbage verdict is rejected, not silently coerced.
        assert!(serde_json::from_str::<ApprovalVerdict>("\"allow-everything\"").is_err());
    }

    #[tokio::test]
    async fn approval_state_delivers_the_verdict_to_the_waiting_gate() {
        let s = Arc::new(ApprovalState::new());
        let (id, rx) = s.register();
        assert!(s.respond(id, ApprovalVerdict::AllowKind), "a live waiter must accept the verdict");
        assert_eq!(rx.await.unwrap(), ApprovalVerdict::AllowKind);
    }

    #[test]
    fn approval_state_respond_to_unknown_or_expired_id_is_a_false_no_op() {
        let s = ApprovalState::new();
        // Never registered.
        assert!(!s.respond(999, ApprovalVerdict::Deny));
        // Registered then cancelled (the timeout path): a late reply is a no-op.
        let (id, _rx) = s.register();
        s.cancel(id);
        assert!(!s.respond(id, ApprovalVerdict::AllowOnce), "a cancelled waiter must not accept");
    }

    #[test]
    fn approval_state_whitelist_handle_is_shared() {
        // The gate mutates the whitelist through the same Arc the state holds, so
        // an "Always allow" grant is visible on the next run's gate.
        let s = ApprovalState::new();
        let wl = s.whitelist();
        wl.lock().unwrap().allow(ActionKind::TypeText);
        assert!(s.whitelist().lock().unwrap().contains(ActionKind::TypeText));
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
    fn reasoning_event_name_and_payload_are_the_ipc_contract() {
        // src/chat.ts pins the same event string and reads `delta` — the const
        // and payload shape are the contract lock for the Thinking… stream.
        assert_eq!(REASONING_EVENT, "llm://reasoning");
        let v =
            serde_json::to_value(ReasoningEvent { request_id: 7, delta: "let me think".into() })
                .unwrap();
        assert_eq!(v["requestId"], 7);
        assert_eq!(v["delta"], "let me think");
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
