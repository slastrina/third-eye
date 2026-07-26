//! MCP host executor (M007 S02): bridge the agent tool-loop's model side (the
//! [`ToolExecutor`] trait) to the MCP wire side (an already-serving rmcp client
//! peer). One [`McpExecutor`] wraps a `Peer<RoleClient>` handle plus the tool
//! catalogue fetched from the server; its [`ToolExecutor::definitions`] advertises
//! each remote tool namespaced under [`MCP_TOOL_PREFIX`] (`mcp__`) so an external
//! tool can never structurally collide with the built-in `memory_search` /
//! `input_action` / `screen_query` / `focus_app` (S02 must-have 5), and its
//! [`ToolExecutor::execute`] marshals a [`ToolCall`] into an rmcp `call_tool` and
//! maps the [`CallToolResult`] / transport error back to a typed [`ToolOutcome`].
//!
//! `execute()` never returns an `Err` (the trait contract, R006): every failure
//! rides back as a typed `ToolOutcome::failure` the model and the
//! `llm://tool-result` surface both see —
//! - a non-object / unparseable arguments string → [`INVALID_ARGUMENTS_KIND`];
//! - an rmcp transport / protocol error → [`MCP_TRANSPORT_ERROR_KIND`];
//! - a server-side `is_error: true` result → [`MCP_TOOL_ERROR_KIND`] (`ok: false`).
//!
//! This file is the pure-mapper core proven by [`tests`] on rmcp model values;
//! the live protocol round-trip through a real rmcp client is the S02 T03/T04
//! contract and tool-loop tests. Full settings-driven spawn/lifecycle of the
//! server child is deferred to S04 — this executor takes an injected peer.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;

use rmcp::model::{CallToolRequestParams, CallToolResult, JsonObject, Tool};
use rmcp::service::RunningServiceCancellationToken;
use rmcp::{Peer, RoleClient, ServiceError};

use super::toolloop::{ToolExecutor, ToolOutcome};
use super::{ToolCall, ToolDefinition};

/// The namespace every external MCP tool is advertised under. Prefixing the
/// server's tool name with `mcp__` makes a collision with a built-in tool
/// (`memory_search`, `input_action`, `screen_query`, `focus_app` — none of
/// which start with `mcp__`) structurally impossible rather than a runtime
/// check (S02 must-have 5): even a server that ships a tool literally named
/// `memory_search` is offered to the model as `mcp__memory_search`.
pub const MCP_TOOL_PREFIX: &str = "mcp__";

/// The typed failure kind when the rmcp `call_tool` itself fails at the
/// transport / protocol layer (connection dropped, timeout, malformed
/// JSON-RPC) — distinct from a tool that ran and reported its own error, so a
/// wire failure is attributable on the `llm://tool-result` surface (R006).
pub const MCP_TRANSPORT_ERROR_KIND: &str = "mcp-transport-error";

/// The typed failure kind when the server ran the tool but returned
/// `is_error: true` — the request reached the tool and the tool decided it
/// failed (a query returned nothing, an upstream API 500'd). The model sees the
/// tool's own error content and can recover; the UI sees this kind.
pub const MCP_TOOL_ERROR_KIND: &str = "mcp-tool-error";

/// The typed failure kind when the model's `arguments` string is not a JSON
/// object (MCP tool arguments must be an object matching the input schema). A
/// non-object or unparseable argument is refused before any wire call.
pub const INVALID_ARGUMENTS_KIND: &str = "invalid-arguments";

/// The typed failure kind when a call routed here does not carry the
/// [`MCP_TOOL_PREFIX`]. The [`super::toolloop::CompositeExecutor`] routes by
/// name so this is defensive — a bare (un-namespaced) name is not one of ours.
pub const UNKNOWN_TOOL_KIND: &str = "unknown-tool";

/// The typed failure kind when the privacy/action guard blocks a non-allowlisted
/// (or prompt-denied) MCP tool action before it reaches the server's `call_tool`
/// — the MCP twin of the HID approval-denied refusal (D038). Distinct from
/// [`MCP_TRANSPORT_ERROR_KIND`] (the wire failed), [`MCP_TOOL_ERROR_KIND`] (the
/// tool ran and reported failure), and [`INVALID_ARGUMENTS_KIND`] (bad args) so a
/// *guarded* action is attributable on the `llm://tool-result` surface and never
/// a silent no-op (R006/R007, S03 slice verification). The gate (T02) attaches
/// this kind; the choke point (`call_tool`) is never reached when it fires.
pub const MCP_ACTION_BLOCKED_KIND: &str = "mcp-action-blocked";

/// Namespace one rmcp [`Tool`] into the model-facing [`ToolDefinition`] the loop
/// advertises: the name is prefixed with [`MCP_TOOL_PREFIX`], the description
/// carries the server's (empty when the server sent none), and the parameters
/// are the tool's JSON-Schema `input_schema` verbatim so the model fills exactly
/// the shape the server expects. Pure — the unit-test seam (S02 T02).
pub fn namespace_tool(tool: &Tool) -> ToolDefinition {
    ToolDefinition {
        name: format!("{MCP_TOOL_PREFIX}{}", tool.name),
        description: tool.description.as_deref().unwrap_or_default().to_string(),
        // input_schema is an Arc<JsonObject>; clone the map out into a Value so
        // the definition owns its schema (the OpenAI `parameters` object).
        parameters: serde_json::Value::Object((*tool.input_schema).clone()),
    }
}

/// Recover the server-side tool name from a namespaced [`ToolCall`] name — the
/// inverse of [`namespace_tool`]. `None` when the name is not one of ours
/// (missing the [`MCP_TOOL_PREFIX`]). Pure — unit-tested.
fn strip_namespace(call_name: &str) -> Option<&str> {
    call_name.strip_prefix(MCP_TOOL_PREFIX)
}

/// Parse the model's raw `arguments` string into the [`JsonObject`] an MCP
/// `call_tool` needs. An empty (or whitespace-only) string is the no-argument
/// call — a tool whose schema takes no parameters — and maps to an empty
/// object. Anything that is not a JSON object (a bare array, number, string, or
/// syntactically invalid JSON) is an error the caller turns into a typed
/// [`INVALID_ARGUMENTS_KIND`] failure. Pure — unit-tested.
fn parse_arguments(raw: &str) -> Result<JsonObject, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(JsonObject::new());
    }
    match serde_json::from_str::<serde_json::Value>(trimmed) {
        Ok(serde_json::Value::Object(map)) => Ok(map),
        Ok(other) => Err(format!(
            "expected a JSON object of tool arguments, got {}",
            json_type_name(&other)
        )),
        Err(e) => Err(e.to_string()),
    }
}

/// A human-readable name for a non-object JSON value, for the invalid-arguments
/// detail the model reads.
fn json_type_name(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "a boolean",
        serde_json::Value::Number(_) => "a number",
        serde_json::Value::String(_) => "a string",
        serde_json::Value::Array(_) => "an array",
        serde_json::Value::Object(_) => "an object",
    }
}

/// Render a [`CallToolResult`]'s content into the string the model reads back as
/// the tool-role turn. Text blocks are the common case (joined by newlines);
/// when the tool returned no text (an image / resource / structured-only
/// result) the structured content — else the serialized content blocks — is
/// used so the outcome is never a silent empty string (R006). Pure —
/// unit-tested.
fn render_content(result: &CallToolResult) -> String {
    let texts: Vec<&str> = result
        .content
        .iter()
        .filter_map(|block| block.as_text())
        .map(|text| text.text.as_str())
        .collect();
    if !texts.is_empty() {
        return texts.join("\n");
    }
    if let Some(structured) = &result.structured_content {
        return structured.to_string();
    }
    serde_json::to_string(&result.content)
        .unwrap_or_else(|e| format!(r#"{{"error":"MCP result serialization failed: {e}"}}"#))
}

/// Map a successful rmcp `call_tool` outcome (`Ok(CallToolResult)`) into a typed
/// [`ToolOutcome`]. A `is_error: true` result is the server-side tool error path
/// — `ok: false` with the [`MCP_TOOL_ERROR_KIND`] kind, the tool's own error
/// content riding back so the model can recover — while any other result is a
/// success carrying the rendered content. Pure — unit-tested.
fn map_call_result(result: CallToolResult) -> ToolOutcome {
    let content = render_content(&result);
    if result.is_error == Some(true) {
        ToolOutcome {
            content,
            ok: false,
            result_count: None,
            mode: None,
            failure: Some(MCP_TOOL_ERROR_KIND.to_string()),
        }
    } else {
        ToolOutcome {
            content,
            ok: true,
            result_count: None,
            mode: None,
            failure: None,
        }
    }
}

/// A [`ToolExecutor`] that registers an external MCP server's tools into the
/// agent tool-loop. It holds a clonable rmcp client [`Peer`] (the injected,
/// already-serving handle — spawn/lifecycle is S04) and the namespaced tool
/// catalogue advertised to the model. Every `execute()` is one `call_tool`
/// round-trip whose result — or whose transport/argument failure — maps to a
/// typed [`ToolOutcome`], never an `Err` and never a silent no-op (R006). The
/// single `call_tool` choke point is where S03 will add its guard gate.
pub struct McpExecutor {
    peer: Peer<RoleClient>,
    definitions: Vec<ToolDefinition>,
}

impl McpExecutor {
    /// Build an executor from an injected peer and a pre-fetched tool catalogue,
    /// namespacing each tool for the model. Pure (no wire call) — the seam the
    /// contract test builds after `list_all_tools`, and the shape the T02 unit
    /// tests exercise the namespacing through.
    pub fn new(peer: Peer<RoleClient>, tools: Vec<Tool>) -> Self {
        let definitions = tools.iter().map(namespace_tool).collect::<Vec<_>>();
        log::info!(
            "llm: McpExecutor mounted with {} MCP tool(s): {:?}",
            definitions.len(),
            definitions
                .iter()
                .map(|d| d.name.as_str())
                .collect::<Vec<_>>()
        );
        Self { peer, definitions }
    }

    /// Connect to an already-serving MCP peer: fetch its tool catalogue over the
    /// wire (`tools/list`) and namespace it. Bubbles the rmcp [`ServiceError`] so
    /// a mount-time handshake/list failure is visible to the caller (the S02
    /// mount logs it, never silent); once built, `execute()` never errors.
    pub async fn connect(peer: Peer<RoleClient>) -> Result<Self, ServiceError> {
        let tools = peer.list_all_tools().await?;
        Ok(Self::new(peer, tools))
    }
}

#[async_trait]
impl ToolExecutor for McpExecutor {
    fn definitions(&self) -> Vec<ToolDefinition> {
        self.definitions.clone()
    }

    async fn execute(&self, call: &ToolCall) -> ToolOutcome {
        // Recover the server-side name from the namespaced call. The composite
        // routes by name so a non-namespaced call should never reach here —
        // treat it defensively as an unknown tool rather than call the server
        // with a name it does not have.
        let Some(mcp_name) = strip_namespace(&call.name) else {
            return ToolOutcome::failure(
                UNKNOWN_TOOL_KIND,
                format!(
                    "not an MCP tool (missing `{MCP_TOOL_PREFIX}` prefix): {}",
                    call.name
                ),
            );
        };
        // Arguments must be a JSON object matching the tool's input schema. A
        // non-object / unparseable value is refused before any wire call.
        let arguments = match parse_arguments(&call.arguments) {
            Ok(args) => args,
            Err(detail) => {
                log::warn!(
                    "llm: MCP tool {} refused — invalid arguments (kind={INVALID_ARGUMENTS_KIND}): {detail}",
                    call.name
                );
                return ToolOutcome::failure(
                    INVALID_ARGUMENTS_KIND,
                    format!("invalid arguments for {}: {detail}", call.name),
                );
            }
        };
        let params = CallToolRequestParams::new(mcp_name.to_string()).with_arguments(arguments);
        match self.peer.call_tool(params).await {
            Ok(result) => map_call_result(result),
            // A transport / protocol error is NOT the tool reporting failure —
            // it is the wire itself. Typed distinctly so a failed MCP call is
            // attributable, never a silent no-op (R006).
            Err(err) => {
                log::warn!(
                    "llm: MCP tool {} call failed (kind={MCP_TRANSPORT_ERROR_KIND}): {err}",
                    call.name
                );
                ToolOutcome::failure(
                    MCP_TRANSPORT_ERROR_KIND,
                    format!("MCP call to {} failed: {err}", call.name),
                )
            }
        }
    }
}

/// Managed holder for the optional, already-serving MCP client peer a chat run
/// mounts an [`McpExecutor`] from. Empty by default: a build that has not wired
/// an MCP server runs exactly as before, and the mount logs the absence rather
/// than failing (mirrors [`crate::memory::MemoryState::store`]'s `Some`/`None`).
/// The full settings-driven spawn/lifecycle that injects a peer is S04; S02 only
/// consumes an already-serving one, so the peer stays `None` until then and the
/// mount is a no-op no matter how many chat runs go by.
pub struct McpState {
    peer: Mutex<Option<Peer<RoleClient>>>,
    /// The three-way run mode the [`McpApprovalGate`] snapshots per chat run — the
    /// MCP twin of [`crate::input::commands::InputState`]'s HID mode. `Off` (the
    /// fail-closed default) until S04 wires the persisted setting; the mount reads
    /// it through [`Self::mode`] so S04 changes config, not the gate.
    mode: Mutex<McpRunMode>,
    /// The session-scoped by-name allowlist the gate grants into on "Always allow
    /// this tool" — the MCP twin of [`crate::llm::commands::ApprovalState`]'s
    /// whitelist. App-session lived (survives across runs within one app launch,
    /// drops when this state drops on app exit) so a grant never outlives the
    /// session (R023).
    allowlist: Arc<Mutex<McpAllowlist>>,
    /// Correlation-id → the waiting gate's verdict sender. An entry lives only
    /// between the `mcp://approval-request` emit and the reply/timeout, then is
    /// removed — the registry the S04 `respond_mcp_approval` IPC delivers into.
    pending: Mutex<HashMap<u64, oneshot::Sender<McpApprovalVerdict>>>,
    next_id: AtomicU64,
    /// The lifecycle health the [`McpHealthStatus`] value exposes — phase, last
    /// error, tool count, and the last-transition timestamp — mutated only
    /// through the `mark_*` seams (S04 T02) so every spawn/handshake/crash
    /// transition is one auditable write. Health-as-value: the `mcp_status`
    /// query and the `mcp://state` broadcast read it any time, never an error
    /// (R007). The `mode` half of the status is read from [`Self::mode`] so the
    /// gate and the health surface share one source of truth.
    health: Mutex<McpHealthCore>,
    /// The owning shutdown handle for the spawned MCP child (S04 T03) — a
    /// cancellation token cloned off the `RunningService` at handshake. Held here
    /// (not the clonable [`Peer`], which cannot cancel — RESEARCH constraint 1)
    /// because only the `RunningService` side can shut the child down: cancelling
    /// this token stops the service loop and terminates the child cleanly via the
    /// portable rmcp path (R020 — no unix/windows-only kill). `None` until a child
    /// is spawned; `take`n exactly once on app exit so the child is cancelled once.
    shutdown: Mutex<Option<RunningServiceCancellationToken>>,
}

impl Default for McpState {
    fn default() -> Self {
        Self::new()
    }
}

impl McpState {
    /// An empty holder — no MCP server peer injected yet, mode fail-closed `Off`,
    /// an empty allowlist, and no pending prompts.
    pub fn new() -> Self {
        Self {
            peer: Mutex::new(None),
            mode: Mutex::new(McpRunMode::default()),
            allowlist: Arc::new(Mutex::new(McpAllowlist::new())),
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            health: Mutex::new(McpHealthCore::default()),
            shutdown: Mutex::new(None),
        }
    }

    /// The injected peer for this run, if an MCP server is serving. `Peer` is
    /// clonable, so a chat run takes its own handle and never holds the lock
    /// across the `tools/list` await.
    pub fn peer(&self) -> Option<Peer<RoleClient>> {
        self.peer.lock().unwrap().clone()
    }

    /// Inject an already-serving peer (the S04 lifecycle, or a test harness).
    pub fn set_peer(&self, peer: Peer<RoleClient>) {
        *self.peer.lock().unwrap() = Some(peer);
    }

    /// Drop the injected peer so a subsequent chat run degrades to "tools
    /// unavailable" — the `None` mount branch at `commands.rs` — rather than
    /// dispatching to a dead child (S04 T03 crash→degrade). Paired with
    /// [`Self::mark_crashed`] on a mid-session drop / spawn failure; a no-op when
    /// no peer was injected, never a panic.
    pub fn clear_peer(&self) {
        *self.peer.lock().unwrap() = None;
    }

    /// Store the shutdown handle for the spawned child (S04 T03). Replaces any
    /// prior handle (a re-spawn); the old token is dropped, which cancels its
    /// already-dead service. The handle is the `RunningService`'s cancellation
    /// token — the ONLY thing that can shut the child down cleanly (the `Peer`
    /// cannot), held so the RunEvent exit hook can cancel it (R020).
    pub fn set_shutdown_handle(&self, token: RunningServiceCancellationToken) {
        *self.shutdown.lock().unwrap() = Some(token);
    }

    /// Take the shutdown handle for a clean app-exit cancel (S04 T03): the
    /// RunEvent exit hook calls `token.cancel()` on the returned handle to
    /// terminate the child. `None` when no child was spawned; taking it leaves
    /// `None` so the child is cancelled exactly once (never double-cancelled).
    pub fn take_shutdown_handle(&self) -> Option<RunningServiceCancellationToken> {
        self.shutdown.lock().unwrap().take()
    }

    /// The current MCP run mode the gate snapshots for a run. The seam S04 sets
    /// from persisted config — the gate never reads config directly.
    pub fn mode(&self) -> McpRunMode {
        *self.mode.lock().unwrap()
    }

    /// Set the MCP run mode (the S04 settings apply, or a test harness). A mode
    /// change is a state transition the health surface reflects, so it stamps
    /// [`McpHealthCore::updated_at`] — the `mcp://state` broadcast the applier
    /// fires after this carries a fresh timestamp. Locks the two mutexes in
    /// sequence (never nested) so it can never deadlock against [`Self::status`].
    pub fn set_mode(&self, mode: McpRunMode) {
        *self.mode.lock().unwrap() = mode;
        self.health.lock().unwrap().updated_at = now_millis();
    }

    /// Enter the `spawning` phase (S04 T03 calls this before launching a child):
    /// clears any prior error and tool count and stamps the transition time. The
    /// tools are not yet reachable, so `tool_count` resets to 0.
    pub fn mark_spawning(&self) {
        let mut health = self.health.lock().unwrap();
        health.phase = McpPhase::Spawning;
        health.last_error = None;
        health.tool_count = 0;
        health.updated_at = now_millis();
    }

    /// Enter the `ready` phase after a successful handshake (S04 T03), recording
    /// how many tools the server advertised. Clears any prior error — the child
    /// is serving.
    pub fn mark_ready(&self, tool_count: usize) {
        let mut health = self.health.lock().unwrap();
        health.phase = McpPhase::Ready;
        health.last_error = None;
        health.tool_count = tool_count;
        health.updated_at = now_millis();
    }

    /// Enter the `crashed` phase on a spawn/handshake failure or a mid-session
    /// child drop (S04 T03), recording the cause. Tools degrade to unavailable
    /// (`tool_count` → 0) — the app keeps running (never a panic); a subsequent
    /// chat run sees no MCP tools, and this value surfaces WHY on the health
    /// line rather than silently vanishing (R006/R007).
    pub fn mark_crashed(&self, error: impl Into<String>) {
        let mut health = self.health.lock().unwrap();
        health.phase = McpPhase::Crashed;
        health.last_error = Some(error.into());
        health.tool_count = 0;
        health.updated_at = now_millis();
    }

    /// The current health as a value — the `mcp_status` query and the
    /// `mcp://state` broadcast payload. Never an error (R007): safe to poll at
    /// any time. Reads the mode and the health core in sequence (never holding
    /// both locks) so it can never deadlock against [`Self::set_mode`].
    pub fn status(&self) -> McpHealthStatus {
        let mode = self.mode();
        let health = self.health.lock().unwrap();
        McpHealthStatus {
            phase: health.phase,
            last_error: health.last_error.clone(),
            updated_at: health.updated_at,
            mode,
            tool_count: health.tool_count,
        }
    }

    /// The shared session allowlist handle the gate mutates on "Always allow this
    /// tool", cloned into each run's gate so a grant survives to the next run.
    pub fn allowlist(&self) -> Arc<Mutex<McpAllowlist>> {
        self.allowlist.clone()
    }

    /// Allocate a correlation id and register the one-shot channel the overlay's
    /// reply will be delivered into (the production [`McpApprovalPrompt`] path).
    pub fn register(&self) -> (u64, oneshot::Receiver<McpApprovalVerdict>) {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        (id, rx)
    }

    /// Drop a pending waiter without a verdict — the timeout / emit-failure path.
    pub fn cancel(&self, id: u64) {
        self.pending.lock().unwrap().remove(&id);
    }

    /// Deliver a verdict to the gate waiting on `id` — the seam the S04
    /// `respond_mcp_approval` IPC calls. Returns whether a live waiter existed; a
    /// stale id (already timed out / replied) is a no-op, never a panic.
    pub fn respond(&self, id: u64, verdict: McpApprovalVerdict) -> bool {
        match self.pending.lock().unwrap().remove(&id) {
            Some(tx) => tx.send(verdict).is_ok(),
            None => false,
        }
    }
}

/// Milliseconds since the Unix epoch — the timestamp [`McpState::status`]
/// stamps each lifecycle transition with. A backwards clock (pre-epoch) or a
/// failed read maps to `0` rather than panicking; `0` also means "no transition
/// yet" (the fresh `Disconnected` default), so the health line reads `never`
/// until the first spawn. Pure Rust `std` (no chrono, no Tauri) so the whole
/// health surface compiles on every R020 target.
fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// The lifecycle phase of the external MCP child — the coarse state the health
/// line renders. `Disconnected` is the fail-safe default (no server configured,
/// or none spawned yet); `Spawning` while the startup launch task is bringing a
/// child up + handshaking; `Ready` once the handshake succeeded and the tools
/// are injected; `Crashed` after a spawn/handshake failure or a mid-session drop
/// (tools degrade to unavailable, the app keeps running). Serialized kebab-case
/// so `src/mcp-state.ts` (S04 T04) shares the exact wire strings, mirroring
/// [`McpRunMode`]. Pure value — no Tauri (R020).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum McpPhase {
    /// No MCP child is serving — nothing configured/enabled, or none spawned yet.
    #[default]
    Disconnected,
    /// The startup launch task is spawning the child and awaiting the handshake.
    Spawning,
    /// The handshake succeeded and the peer is injected — tools are reachable.
    Ready,
    /// A spawn/handshake failure or a mid-session drop — tools unavailable, the
    /// app still runs; [`McpHealthStatus::last_error`] names the cause.
    Crashed,
}

/// Queryable MCP host health — the value the `mcp_status` command returns and
/// the `mcp://state` broadcast carries at every lifecycle transition. The
/// health-as-value shape (a value any time, never an IPC error — R007), the MCP
/// twin of `CloudOptInStatus` / `WatcherStatus`:
/// `{ phase, lastError, updatedAt, mode, toolCount }`. `lastError` carries the
/// most recent lifecycle failure (spawn/handshake/mid-session) so a crashed
/// child stays diagnosable after the fact; `updatedAt` is epoch-millis of the
/// last transition (`0` = none yet); `mode` mirrors the gate's live
/// [`McpRunMode`]; `toolCount` is how many tools the server advertised (`0`
/// unless `Ready`). Serialized camelCase to match every other status payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpHealthStatus {
    pub phase: McpPhase,
    pub last_error: Option<String>,
    pub updated_at: u64,
    pub mode: McpRunMode,
    pub tool_count: usize,
}

/// The mutable lifecycle core [`McpState`] holds behind a `Mutex` — the fields
/// the `mark_*` seams write and [`McpState::status`] assembles into an
/// [`McpHealthStatus`] (joined with the live [`McpRunMode`]). Private: mutated
/// only through the state's seams so every transition is one auditable write.
#[derive(Debug, Clone)]
struct McpHealthCore {
    phase: McpPhase,
    last_error: Option<String>,
    tool_count: usize,
    updated_at: u64,
}

impl Default for McpHealthCore {
    /// Fail-safe resting state: `Disconnected`, no error, no tools, and a `0`
    /// timestamp ("no transition yet") — what a build with no MCP server shows.
    fn default() -> Self {
        Self {
            phase: McpPhase::Disconnected,
            last_error: None,
            tool_count: 0,
            updated_at: 0,
        }
    }
}

/// Run mode for external MCP tool actions — the MCP twin of
/// [`crate::input::commands::HidRunMode`]. Keyed the same three ways the HID
/// guard is, but the "unit of approval" is an arbitrary namespaced tool-name
/// STRING (`mcp__foo_bar`), not the fixed 5-variant `ActionKind` enum: an
/// external server advertises tools by name, so the allow/block/confirm decision
/// is keyed on the name, not a closed kind set. Serialized kebab-case
/// (`off` / `ask` / `auto-run`) — the exact strings S04's `config.rs` will
/// persist and `src/chat.ts` will match on, mirroring `HidRunMode`. `Off` is the
/// `Default` so a missing/garbage persisted value maps to the safe inert state
/// (fail-closed).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum McpRunMode {
    /// MCP tool actions off: every external tool call is refused before the wire
    /// (the MCP twin of the HID disabled refusal, D038).
    #[default]
    Off,
    /// Prompt before each MCP tool name not yet allowlisted this session.
    Ask,
    /// Run every MCP tool call without prompting.
    AutoRun,
}

/// Which transport reaches one configured MCP server — the S05 discriminator on
/// [`McpServerConfig`]. `Stdio` spawns a local child process (S04:
/// [`command`](McpServerConfig::command) + [`args`](McpServerConfig::args));
/// `Http` connects to a remote streamable-HTTP / SSE endpoint at
/// [`url`](McpServerConfig::url) with an optional keychain-backed bearer token
/// named by [`auth_ref`](McpServerConfig::auth_ref). Serialized kebab-case
/// (`stdio` / `http`); `Stdio` is the `Default` and the config field carries
/// `#[serde(default)]`, so an S04-persisted entry with no `transport` key still
/// deserializes as the local stdio server it has always been (back-compat).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum McpTransport {
    /// Local child process spawned from `command` + `args` (the S04 stdio path).
    #[default]
    Stdio,
    /// Remote streamable-HTTP / SSE endpoint at `url`, optionally authenticated
    /// with the keychain bearer token named by `auth_ref` (S05).
    Http,
}

/// One user-configured external MCP server (S04 T01) — the persisted record a
/// user adds in settings and the spawn lifecycle reads to launch/connect. A
/// stdio server uses [`command`](Self::command) plus [`args`](Self::args) as the
/// process to spawn; an http server (S05) uses [`url`](Self::url) plus an
/// optional [`auth_ref`](Self::auth_ref) keychain bearer token. [`id`](Self::id)
/// is a stable key / display name and [`enabled`](Self::enabled) whether the
/// startup launch task spawns/connects it. Serde camelCase — the exact JSON shape
/// persisted under `mcpServers` in settings.json and mirrored by
/// `src/mcp-state.ts`. [`transport`](Self::transport) is the S05 stdio|http
/// discriminator, defaulting to [`Stdio`](McpTransport::Stdio) so an S04 entry
/// with no `transport` key still deserializes. `enabled` defaults to `false`
/// (fail-closed: a persisted entry missing the flag is inert until the user turns
/// it on) and `args` defaults to empty so a partially-written entry still
/// deserializes rather than being dropped whole.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpServerConfig {
    /// Stable key / display name for this server (the settings row identity). A
    /// blank id is treated as a corrupt entry and dropped by the config
    /// interpreter — a server the user cannot name cannot be managed.
    pub id: String,
    /// The stdio process to spawn (the executable / launcher, e.g. `npx`). A
    /// blank command is a corrupt entry and dropped for a stdio server — there is
    /// nothing to spawn; unused (and typically empty) for an http server.
    /// Defaults to empty so an http entry that omits it still deserializes.
    #[serde(default)]
    pub command: String,
    /// The arguments passed to [`command`](Self::command). Defaults to empty when
    /// absent so a minimal `{id, command}` entry still round-trips.
    #[serde(default)]
    pub args: Vec<String>,
    /// Whether the startup launch task spawns/connects this server. Defaults to
    /// `false` (fail-closed) so a persisted-but-flagless entry stays inert until
    /// the user explicitly enables it.
    #[serde(default)]
    pub enabled: bool,
    /// Which transport reaches this server (S05). Defaults to
    /// [`Stdio`](McpTransport::Stdio) so an S04 entry with no `transport` key
    /// stays the local child-process server it was (back-compat).
    #[serde(default)]
    pub transport: McpTransport,
    /// The remote endpoint URL for an [`Http`](McpTransport::Http) server (`None`
    /// for stdio). A blank / unparseable url makes an http entry corrupt and it
    /// is dropped by config repair.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Non-secret keychain account key naming where an http server's bearer token
    /// lives (R018 — the secret never rides in settings.json; only this reference
    /// does). `None` for stdio or an unauthenticated http server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_ref: Option<String>,
}

/// The decision the pure MCP approval resolver returns for one pending tool call
/// — the MCP twin of [`crate::input::commands::ApprovalDecision`]. A value the
/// gate (T02) acts on, never a side effect. Defined locally rather than reused
/// from the `#[cfg(desktop)]`-gated `input` module so this pure decision core
/// stays Tauri-free and target-agnostic (R020: Windows/Linux/mobile all compile
/// the `llm` module).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpApprovalDecision {
    /// `Off`: refuse the call with a typed [`MCP_ACTION_BLOCKED_KIND`] outcome
    /// before the `call_tool` choke point is touched — the allowlist cannot
    /// un-inert a disabled MCP surface (D038).
    Refuse,
    /// Perform the call without prompting — `AutoRun`, or `Ask` with the tool
    /// name already allowlisted for this session.
    Perform,
    /// `Ask` and the tool name is not yet allowlisted: ask the injected prompt
    /// seam (Allow once / Always allow this tool / Deny).
    Prompt,
}

/// The session-scoped by-name approval allowlist — the MCP twin of
/// [`crate::input::commands::SessionWhitelist`], but keyed on namespaced
/// tool-name STRINGS (`mcp__foo_bar`) which a `HashSet<ActionKind>` cannot hold.
/// Grants are session-only: [`Self::clear`] empties it on run/session end so an
/// allow never outlives the run that granted it (R023 — nothing about a session
/// is persisted). Mutated ONLY by [`Self::allow`] (the "Always allow this tool"
/// verdict); an "Allow once" verdict performs without touching the set.
#[derive(Debug, Default)]
pub struct McpAllowlist {
    names: HashSet<String>,
}

impl McpAllowlist {
    /// An empty allowlist — the start-of-session posture: every tool name
    /// prompts under `Ask`.
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether `tool_name` has been granted for this session (an `Ask`-mode call
    /// to this tool performs without a prompt).
    pub fn contains(&self, tool_name: &str) -> bool {
        self.names.contains(tool_name)
    }

    /// Grant `tool_name` for the rest of this session — the "Always allow this
    /// tool" verdict. Idempotent; the only mutation that adds to the set.
    pub fn allow(&mut self, tool_name: impl Into<String>) {
        self.names.insert(tool_name.into());
    }

    /// Empty the allowlist — called on run/session end so a grant never outlives
    /// its session (R023). After this every tool name prompts again.
    pub fn clear(&mut self) {
        self.names.clear();
    }

    /// Whether no tool name is currently granted.
    pub fn is_empty(&self) -> bool {
        self.names.is_empty()
    }
}

/// Pure MCP approval resolver (S03 T01): given the current [`McpRunMode`], the
/// pending namespaced `tool_name`, and the session [`McpAllowlist`], decide
/// whether to [`McpApprovalDecision::Refuse`], [`McpApprovalDecision::Perform`],
/// or [`McpApprovalDecision::Prompt`]. Tauri-free and side-effect-free — the twin
/// of [`crate::input::commands::resolve_approval`] — so every mode ×
/// allowlisted/not transition is unit-testable without a Tauri app or an rmcp
/// peer. The gate layer (T02) owns the effects (ask the prompt seam, mutate the
/// allowlist on "Always allow", reach the `call_tool` choke point on `Perform`).
///
/// `Off` maps to `Refuse` unconditionally — the allowlist cannot un-inert a
/// disabled MCP surface (D038). `AutoRun` maps to `Perform` unconditionally.
/// `Ask` consults the allowlist: a granted name performs, an ungranted name
/// prompts.
pub fn resolve_mcp_approval(
    mode: McpRunMode,
    tool_name: &str,
    allowlist: &McpAllowlist,
) -> McpApprovalDecision {
    match mode {
        McpRunMode::Off => McpApprovalDecision::Refuse,
        McpRunMode::AutoRun => McpApprovalDecision::Perform,
        McpRunMode::Ask => {
            if allowlist.contains(tool_name) {
                McpApprovalDecision::Perform
            } else {
                McpApprovalDecision::Prompt
            }
        }
    }
}

/// One verdict the overlay returns for a pending MCP tool call (S03 T02) — the
/// user's answer to an [`McpApprovalDecision::Prompt`], the MCP twin of
/// [`crate::llm::toolloop::ApprovalVerdict`]. Keyed on the tool NAME rather than a
/// fixed kind: "Always allow this tool" grants the exact namespaced name for the
/// session. Serialized kebab-case so the S04 `respond_mcp_approval` IPC and
/// `src/chat.ts` will share the exact strings, mirroring `ApprovalVerdict`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum McpApprovalVerdict {
    /// Perform this one call; do not remember the tool (it prompts again next time
    /// it is requested).
    AllowOnce,
    /// Perform and grant this tool name for the session — no more prompts for it
    /// until the session ends ("Always allow this tool").
    AllowTool,
    /// Refuse this call — a visible, typed [`MCP_ACTION_BLOCKED_KIND`] result; the
    /// `call_tool` choke point is never reached.
    Deny,
}

/// The prompt seam (S03 T02): when [`resolve_mcp_approval`] returns
/// [`McpApprovalDecision::Prompt`], the [`McpApprovalGate`] calls this to surface
/// the pending MCP tool call and await the user's [`McpApprovalVerdict`]. Injected
/// so the gate stays Tauri-free — production emits a `mcp://approval-request`
/// event and awaits the reply with a bounded timeout (a timeout is
/// [`McpApprovalVerdict::Deny`], fail-closed), while tests script the verdict
/// directly. The MCP twin of [`crate::llm::toolloop::ApprovalPrompt`].
#[async_trait]
pub trait McpApprovalPrompt: Send + Sync {
    /// Surface `summary` (a human sentence describing the pending call to
    /// `tool_name`) and await the user's verdict. Never errors — a timeout or a
    /// closed channel resolves to [`McpApprovalVerdict::Deny`] (fail-closed).
    async fn request(&self, tool_name: String, summary: String) -> McpApprovalVerdict;
}

/// A human sentence describing the pending MCP tool call — what the overlay shows
/// so the user knows exactly what they are approving: the namespaced tool name
/// plus a short, bounded preview of the JSON arguments. The preview is transient
/// prompt context only; it is never persisted (R011/R023).
fn mcp_summary(call: &ToolCall) -> String {
    let args = call.arguments.trim();
    if args.is_empty() {
        return format!("Run the external MCP tool {}", call.name);
    }
    const MAX: usize = 120;
    let preview: String = args.chars().take(MAX).collect();
    let ellipsis = if args.chars().count() > MAX {
        "…"
    } else {
        ""
    };
    format!(
        "Run the external MCP tool {} with {preview}{ellipsis}",
        call.name
    )
}

/// Wraps [`McpExecutor`] with the S03 per-call approval gate: before any external
/// MCP tool call reaches the server's `call_tool` choke point it consults the pure
/// [`resolve_mcp_approval`] resolver (T01) against the current [`McpRunMode`] and
/// the session [`McpAllowlist`], and — only when the resolver says
/// [`McpApprovalDecision::Prompt`] — asks the user via the injected
/// [`McpApprovalPrompt`]. The MCP twin of the HID
/// [`crate::llm::toolloop::ApprovalGate`] and the runtime half of R016's extension
/// of the guard boundary to external tool actions: after this wrap no production
/// MCP tool-action path reaches a server unguarded (pinned structurally by
/// `scripts/check-mcp-guard.sh`, T03).
///
/// [`ToolExecutor::definitions`] forwards the inner catalogue UNCHANGED so MCP
/// tools stay dispatchable by name in every mode — a blocked call returns a typed,
/// visible [`MCP_ACTION_BLOCKED_KIND`] outcome at `execute()` time rather than the
/// tool silently vanishing from the model's view (R006/R007). `Off` blocks every
/// call before the wire (D038); `Perform` (AutoRun, or Ask with the tool already
/// allowlisted) delegates straight to the inner executor; a `Prompt` that is
/// denied (or times out) returns the typed blocked result and never reaches
/// `call_tool`; "Always allow this tool" grants the name in the session allowlist
/// so it performs unprompted for the rest of the session.
pub struct McpApprovalGate {
    inner: McpExecutor,
    mode: McpRunMode,
    allowlist: Arc<Mutex<McpAllowlist>>,
    approver: Arc<dyn McpApprovalPrompt>,
}

impl McpApprovalGate {
    /// Wrap an [`McpExecutor`] in the gate with the run's mode snapshot, the shared
    /// session allowlist, and the injected prompt seam.
    pub fn new(
        inner: McpExecutor,
        mode: McpRunMode,
        allowlist: Arc<Mutex<McpAllowlist>>,
        approver: Arc<dyn McpApprovalPrompt>,
    ) -> Self {
        Self {
            inner,
            mode,
            allowlist,
            approver,
        }
    }
}

#[async_trait]
impl ToolExecutor for McpApprovalGate {
    fn definitions(&self) -> Vec<ToolDefinition> {
        // Forward the inner catalogue UNCHANGED: MCP tools stay dispatchable by
        // name in every mode. The gate blocks at execute() with a typed, visible
        // outcome — it does not withhold the tool from the model (a withheld tool
        // would be an invisible no-op, the opposite of R006/R007's attributable
        // block).
        self.inner.definitions()
    }

    async fn execute(&self, call: &ToolCall) -> ToolOutcome {
        let tool_name = call.name.as_str();
        // Resolve under the lock, then drop it before any `.await` — the allowlist
        // guard must never be held across the approval prompt round-trip.
        let decision = {
            let allowlist = self.allowlist.lock().unwrap();
            resolve_mcp_approval(self.mode, tool_name, &allowlist)
        };
        match decision {
            McpApprovalDecision::Refuse => {
                // Off is structurally inert (D038): the call is blocked before the
                // `call_tool` choke point with the distinct, attributable kind.
                log::warn!(
                    "llm: MCP tool {tool_name} blocked — MCP actions off (kind={MCP_ACTION_BLOCKED_KIND})"
                );
                ToolOutcome::failure(
                    MCP_ACTION_BLOCKED_KIND,
                    format!("MCP tool actions are off; {tool_name} was blocked"),
                )
            }
            McpApprovalDecision::Perform => {
                log::info!(
                    "llm: MCP tool {tool_name} allowed without prompt mode={:?} (auto-run or allowlisted)",
                    self.mode
                );
                self.inner.execute(call).await
            }
            McpApprovalDecision::Prompt => {
                let summary = mcp_summary(call);
                let verdict = self.approver.request(tool_name.to_string(), summary).await;
                match verdict {
                    McpApprovalVerdict::Deny => {
                        log::warn!(
                            "llm: MCP tool {tool_name} denied by user (kind={MCP_ACTION_BLOCKED_KIND})"
                        );
                        ToolOutcome::failure(
                            MCP_ACTION_BLOCKED_KIND,
                            format!("the user denied this MCP tool action ({tool_name})"),
                        )
                    }
                    McpApprovalVerdict::AllowOnce => {
                        log::info!("llm: MCP tool {tool_name} allowed once by user");
                        self.inner.execute(call).await
                    }
                    McpApprovalVerdict::AllowTool => {
                        self.allowlist.lock().unwrap().allow(tool_name);
                        log::info!(
                            "llm: MCP tool {tool_name} allowed + tool allowlisted for session"
                        );
                        self.inner.execute(call).await
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::toolloop::{
        FOCUS_APP_TOOL, INPUT_ACTION_TOOL, MEMORY_SEARCH_TOOL, SCREEN_QUERY_TOOL,
    };
    use rmcp::model::ContentBlock;

    /// The four built-in tool names an MCP tool must never structurally collide
    /// with (S02 must-have 5).
    const BUILTINS: [&str; 4] = [
        MEMORY_SEARCH_TOOL,
        INPUT_ACTION_TOOL,
        SCREEN_QUERY_TOOL,
        FOCUS_APP_TOOL,
    ];

    fn schema() -> JsonObject {
        let serde_json::Value::Object(map) = serde_json::json!({
            "type": "object",
            "properties": { "message": { "type": "string" } },
            "required": ["message"]
        }) else {
            unreachable!("object literal")
        };
        map
    }

    #[test]
    fn namespace_prefixes_name_and_carries_schema_and_description() {
        let tool = Tool::new("echo", "Echo the message back", schema());
        let def = namespace_tool(&tool);
        assert_eq!(def.name, "mcp__echo");
        assert_eq!(def.description, "Echo the message back");
        // The advertised parameters ARE the server's input_schema verbatim.
        assert_eq!(def.parameters, serde_json::Value::Object(schema()));
    }

    #[test]
    fn namespace_of_a_tool_with_no_description_is_empty_not_a_panic() {
        let tool = Tool::new_with_raw("bare", None, schema());
        let def = namespace_tool(&tool);
        assert_eq!(def.name, "mcp__bare");
        assert_eq!(def.description, "");
    }

    #[test]
    fn namespaced_names_never_collide_with_builtins() {
        // Even a server that ships a tool named EXACTLY like a built-in is
        // namespaced away from it — the collision is structurally impossible.
        for builtin in BUILTINS {
            let tool = Tool::new(builtin, "impostor", schema());
            let def = namespace_tool(&tool);
            assert!(
                def.name.starts_with(MCP_TOOL_PREFIX),
                "namespaced name must carry the mcp__ prefix: {}",
                def.name
            );
            assert!(
                !BUILTINS.contains(&def.name.as_str()),
                "namespaced name {} collided with a built-in tool",
                def.name
            );
        }
    }

    #[test]
    fn strip_namespace_is_the_inverse_of_prefixing() {
        assert_eq!(strip_namespace("mcp__echo"), Some("echo"));
        // An MCP tool whose own name contains `mcp__` round-trips too.
        assert_eq!(strip_namespace("mcp__mcp__weird"), Some("mcp__weird"));
    }

    #[test]
    fn strip_namespace_rejects_a_non_namespaced_name() {
        assert_eq!(strip_namespace("memory_search"), None);
        assert_eq!(strip_namespace("echo"), None);
    }

    #[test]
    fn parse_arguments_reads_a_json_object() {
        let map = parse_arguments(r#"{"message":"hi","n":3}"#).expect("object parses");
        assert_eq!(map.get("message").unwrap(), "hi");
        assert_eq!(map.get("n").unwrap(), 3);
    }

    #[test]
    fn parse_arguments_treats_empty_string_as_the_no_arg_call() {
        assert!(parse_arguments("").expect("empty is ok").is_empty());
        assert!(parse_arguments("   ").expect("whitespace is ok").is_empty());
    }

    #[test]
    fn parse_arguments_rejects_a_non_object_json_value() {
        // A bare array is valid JSON but not a tool-arguments object.
        assert!(parse_arguments("[1,2,3]").is_err());
        assert!(parse_arguments("42").is_err());
        assert!(parse_arguments(r#""just a string""#).is_err());
    }

    #[test]
    fn parse_arguments_rejects_unparseable_json() {
        assert!(parse_arguments("{not valid").is_err());
    }

    #[test]
    fn map_success_result_is_ok_with_joined_text() {
        let result = CallToolResult::success(vec![
            ContentBlock::text("line one"),
            ContentBlock::text("line two"),
        ]);
        let outcome = map_call_result(result);
        assert!(outcome.ok);
        assert_eq!(outcome.failure, None);
        assert_eq!(outcome.content, "line one\nline two");
    }

    #[test]
    fn map_error_result_is_a_typed_tool_error_carrying_the_detail() {
        // A server-side is_error=true is NOT a transport error: the tool ran and
        // reported its own failure. It maps to ok:false with the distinct
        // mcp-tool-error kind, and the tool's error text rides back to the model.
        let result = CallToolResult::error(vec![ContentBlock::text("the query matched nothing")]);
        let outcome = map_call_result(result);
        assert!(!outcome.ok);
        assert_eq!(outcome.failure.as_deref(), Some(MCP_TOOL_ERROR_KIND));
        assert_eq!(outcome.content, "the query matched nothing");
    }

    #[test]
    fn map_result_falls_back_to_structured_content_when_no_text() {
        let result = CallToolResult::structured(serde_json::json!({ "temp": 22 }));
        let outcome = map_call_result(result);
        assert!(outcome.ok);
        // structured() also puts the value's string in a text block, so the
        // rendered content carries the value either way — never empty.
        assert!(
            outcome.content.contains("22"),
            "content was {:?}",
            outcome.content
        );
    }

    #[test]
    fn map_result_never_yields_an_empty_content_string() {
        // A result with no content and no structured payload still renders a
        // visible (non-empty) string, never a silent empty turn (R006).
        let result = CallToolResult::success(Vec::new());
        let outcome = map_call_result(result);
        assert!(outcome.ok);
        assert!(!outcome.content.is_empty(), "content must never be empty");
    }

    // --- Pure MCP approval resolver (S03 T01) -------------------------------
    // Every mode × allowlisted/not transition, mirroring the HID
    // resolve_approval unit tests (src/input/commands.rs). Side-effect-free.

    const TOOL: &str = "mcp__weather_lookup";
    const OTHER: &str = "mcp__file_write";

    #[test]
    fn off_mode_refuses_every_tool_regardless_of_allowlist() {
        // Off is structurally inert: even a tool the user explicitly allowlisted
        // is refused — the allowlist cannot un-inert a disabled surface (D038).
        let empty = McpAllowlist::new();
        assert_eq!(
            resolve_mcp_approval(McpRunMode::Off, TOOL, &empty),
            McpApprovalDecision::Refuse
        );
        let mut populated = McpAllowlist::new();
        populated.allow(TOOL);
        assert_eq!(
            resolve_mcp_approval(McpRunMode::Off, TOOL, &populated),
            McpApprovalDecision::Refuse,
            "an allowlisted tool must still be refused when the mode is Off"
        );
    }

    #[test]
    fn auto_run_mode_performs_every_tool_regardless_of_allowlist() {
        let empty = McpAllowlist::new();
        // Not allowlisted, yet AutoRun performs unconditionally.
        assert_eq!(
            resolve_mcp_approval(McpRunMode::AutoRun, TOOL, &empty),
            McpApprovalDecision::Perform
        );
        let mut populated = McpAllowlist::new();
        populated.allow(OTHER);
        assert_eq!(
            resolve_mcp_approval(McpRunMode::AutoRun, TOOL, &populated),
            McpApprovalDecision::Perform
        );
    }

    #[test]
    fn ask_mode_prompts_an_ungranted_tool_and_performs_a_granted_one() {
        let mut wl = McpAllowlist::new();
        // Ungranted → Prompt.
        assert_eq!(
            resolve_mcp_approval(McpRunMode::Ask, TOOL, &wl),
            McpApprovalDecision::Prompt
        );
        // "Always allow this tool" grants it for the session → now Perform.
        wl.allow(TOOL);
        assert_eq!(
            resolve_mcp_approval(McpRunMode::Ask, TOOL, &wl),
            McpApprovalDecision::Perform
        );
        // The grant is by exact name: a *different* tool still prompts.
        assert_eq!(
            resolve_mcp_approval(McpRunMode::Ask, OTHER, &wl),
            McpApprovalDecision::Prompt,
            "granting one tool name must not allow a different tool"
        );
    }

    #[test]
    fn allowlist_grant_is_cleared_at_session_end() {
        // A grant never outlives its session (R023): after clear() the same tool
        // prompts again under Ask.
        let mut wl = McpAllowlist::new();
        wl.allow(TOOL);
        assert!(wl.contains(TOOL));
        assert!(!wl.is_empty());
        wl.clear();
        assert!(wl.is_empty());
        assert!(!wl.contains(TOOL));
        assert_eq!(
            resolve_mcp_approval(McpRunMode::Ask, TOOL, &wl),
            McpApprovalDecision::Prompt
        );
    }

    #[test]
    fn allowlist_allow_is_idempotent() {
        let mut wl = McpAllowlist::new();
        wl.allow(TOOL);
        wl.allow(TOOL);
        assert!(wl.contains(TOOL));
    }

    #[test]
    fn run_mode_defaults_to_off_and_round_trips_kebab_case() {
        // Fail-closed: a missing/garbage persisted mode deserializes to Off.
        assert_eq!(McpRunMode::default(), McpRunMode::Off);
        assert_eq!(serde_json::to_value(McpRunMode::Off).unwrap(), "off");
        assert_eq!(serde_json::to_value(McpRunMode::Ask).unwrap(), "ask");
        assert_eq!(
            serde_json::to_value(McpRunMode::AutoRun).unwrap(),
            "auto-run"
        );
        for mode in [McpRunMode::Off, McpRunMode::Ask, McpRunMode::AutoRun] {
            let v = serde_json::to_value(mode).unwrap();
            let back: McpRunMode = serde_json::from_value(v).unwrap();
            assert_eq!(back, mode);
        }
    }

    #[test]
    fn server_config_round_trips_camel_case_json() {
        // The exact JSON shape persisted under `mcpServers` and mirrored by
        // src/mcp-state.ts: camelCase fields, args + enabled present, transport
        // emitted (kebab-case). A stdio server has no url/auth_ref, so those are
        // skipped in the wire (Option::is_none) — the S04 shape plus `transport`.
        let cfg = McpServerConfig {
            id: "everything".to_string(),
            command: "npx".to_string(),
            args: vec![
                "-y".to_string(),
                "@modelcontextprotocol/server-everything".to_string(),
            ],
            enabled: true,
            transport: McpTransport::Stdio,
            url: None,
            auth_ref: None,
        };
        let wire = serde_json::to_value(&cfg).unwrap();
        assert_eq!(
            wire,
            serde_json::json!({
                "id": "everything",
                "command": "npx",
                "args": ["-y", "@modelcontextprotocol/server-everything"],
                "enabled": true,
                "transport": "stdio"
            })
        );
        let back: McpServerConfig = serde_json::from_value(wire).unwrap();
        assert_eq!(back, cfg);
    }

    #[test]
    fn http_server_config_round_trips_camel_case_json() {
        // The S05 http shape: transport "http", url + camelCase authRef present.
        let cfg = McpServerConfig {
            id: "weather".to_string(),
            command: String::new(),
            args: vec![],
            enabled: true,
            transport: McpTransport::Http,
            url: Some("https://mcp.example.com/sse".to_string()),
            auth_ref: Some("mcp:weather".to_string()),
        };
        let wire = serde_json::to_value(&cfg).unwrap();
        assert_eq!(
            wire,
            serde_json::json!({
                "id": "weather",
                "command": "",
                "args": [],
                "enabled": true,
                "transport": "http",
                "url": "https://mcp.example.com/sse",
                "authRef": "mcp:weather"
            })
        );
        let back: McpServerConfig = serde_json::from_value(wire).unwrap();
        assert_eq!(back, cfg);
    }

    #[test]
    fn server_config_without_transport_key_deserializes_as_stdio() {
        // Back-compat: an S04-persisted entry has no `transport`/`url`/`authRef`
        // keys, so #[serde(default)] must resolve transport to Stdio (and the
        // Options to None) rather than failing deserialization.
        let s04 = serde_json::json!({
            "id": "everything",
            "command": "npx",
            "args": ["-y", "pkg"],
            "enabled": true
        });
        let back: McpServerConfig = serde_json::from_value(s04).unwrap();
        assert_eq!(back.transport, McpTransport::Stdio);
        assert_eq!(back.url, None);
        assert_eq!(back.auth_ref, None);
    }

    #[test]
    fn server_config_defaults_args_and_enabled_when_absent() {
        // A minimal {id, command} entry still deserializes: args → empty,
        // enabled → false (fail-closed), so a partially-written entry is not
        // dropped whole by the config interpreter.
        let cfg: McpServerConfig =
            serde_json::from_value(serde_json::json!({ "id": "x", "command": "run-me" })).unwrap();
        assert!(cfg.args.is_empty());
        assert!(
            !cfg.enabled,
            "enabled must fail-closed to false when absent"
        );
    }

    #[test]
    fn mcp_action_blocked_kind_is_distinct_from_the_other_failure_kinds() {
        // The guard's blocked kind must be attributable — never conflated with a
        // wire failure, a tool-reported failure, an argument failure, or the
        // routing-defensive unknown-tool kind (R006/R007, S03 verification).
        for other in [
            MCP_TRANSPORT_ERROR_KIND,
            MCP_TOOL_ERROR_KIND,
            INVALID_ARGUMENTS_KIND,
            UNKNOWN_TOOL_KIND,
        ] {
            assert_ne!(MCP_ACTION_BLOCKED_KIND, other);
        }
    }

    // --- Gate prompt seam (S03 T02) -----------------------------------------
    // The gate's runtime block/perform paths (which touch the inner executor)
    // are proven over the in-process fake transport in tests/mcp_guard.rs (T04);
    // here the PURE pieces the gate composes are unit-tested.

    fn call(name: &str, arguments: &str) -> ToolCall {
        ToolCall {
            id: "call_1".to_string(),
            name: name.to_string(),
            arguments: arguments.to_string(),
        }
    }

    #[test]
    fn verdict_round_trips_the_kebab_case_wire_strings() {
        // The exact strings the S04 respond_mcp_approval IPC and src/chat.ts share
        // (mirrors the ApprovalVerdict wire contract).
        assert_eq!(
            serde_json::from_value::<McpApprovalVerdict>(serde_json::json!("allow-once")).unwrap(),
            McpApprovalVerdict::AllowOnce
        );
        assert_eq!(
            serde_json::from_value::<McpApprovalVerdict>(serde_json::json!("allow-tool")).unwrap(),
            McpApprovalVerdict::AllowTool
        );
        assert_eq!(
            serde_json::from_value::<McpApprovalVerdict>(serde_json::json!("deny")).unwrap(),
            McpApprovalVerdict::Deny
        );
        // An unknown verdict string is refused (fail-closed at the deserialize
        // boundary — never silently coerced to an allow).
        assert!(serde_json::from_value::<McpApprovalVerdict>(serde_json::json!("yes")).is_err());
    }

    #[test]
    fn summary_names_the_tool_and_previews_arguments() {
        // No-arg call: names the tool, no dangling "with".
        let s = mcp_summary(&call("mcp__weather_lookup", "   "));
        assert_eq!(s, "Run the external MCP tool mcp__weather_lookup");
        // With args: the tool name plus a preview of the JSON.
        let s = mcp_summary(&call("mcp__weather_lookup", r#"{"city":"Oslo"}"#));
        assert!(s.contains("mcp__weather_lookup"), "summary was {s:?}");
        assert!(s.contains("Oslo"), "summary must preview the args: {s:?}");
    }

    #[test]
    fn summary_bounds_a_long_argument_preview() {
        // A pathologically long argument string is truncated with an ellipsis so
        // the prompt line stays bounded (never dumps an unbounded blob).
        let big = format!(r#"{{"blob":"{}"}}"#, "x".repeat(10_000));
        let s = mcp_summary(&call("mcp__file_write", &big));
        assert!(
            s.chars().count() < 200,
            "summary must be bounded: {} chars",
            s.chars().count()
        );
        assert!(
            s.ends_with('…'),
            "a truncated preview must end with an ellipsis: {s:?}"
        );
    }

    // --- Health-as-value + lifecycle transitions (S04 T02) ------------------
    // The mcp_status / mcp://state value the applier and the T03 spawn
    // lifecycle write through the mark_* seams. Pure — no Tauri, no rmcp peer.

    #[test]
    fn phase_serializes_kebab_case_and_defaults_disconnected() {
        // src/mcp-state.ts (T04) matches these exact wire strings.
        assert_eq!(McpPhase::default(), McpPhase::Disconnected);
        assert_eq!(
            serde_json::to_value(McpPhase::Disconnected).unwrap(),
            "disconnected"
        );
        assert_eq!(
            serde_json::to_value(McpPhase::Spawning).unwrap(),
            "spawning"
        );
        assert_eq!(serde_json::to_value(McpPhase::Ready).unwrap(), "ready");
        assert_eq!(serde_json::to_value(McpPhase::Crashed).unwrap(), "crashed");
    }

    #[test]
    fn fresh_state_health_is_disconnected_off_camelcase() {
        // The resting value a build with no MCP server shows: the exact
        // { phase, lastError, updatedAt, mode, toolCount } camelCase shape.
        let state = McpState::new();
        let status = state.status();
        assert_eq!(status.phase, McpPhase::Disconnected);
        assert_eq!(
            status.mode,
            McpRunMode::Off,
            "mode fail-closed off by default"
        );
        assert_eq!(status.last_error, None);
        assert_eq!(status.tool_count, 0);
        assert_eq!(status.updated_at, 0, "no lifecycle transition yet → 0");

        let v = serde_json::to_value(&status).unwrap();
        let obj = v.as_object().unwrap();
        assert_eq!(obj.len(), 5, "health status shape drifted: {obj:?}");
        assert_eq!(obj["phase"], "disconnected");
        assert!(obj["lastError"].is_null(), "absent error must be JSON null");
        assert_eq!(obj["updatedAt"], 0);
        assert_eq!(obj["mode"], "off");
        assert_eq!(obj["toolCount"], 0);
    }

    #[test]
    fn lifecycle_transitions_spawning_ready_then_crashed() {
        let state = McpState::new();

        state.mark_spawning();
        let s = state.status();
        assert_eq!(s.phase, McpPhase::Spawning);
        assert_eq!(s.tool_count, 0, "no tools reachable while spawning");
        assert_eq!(s.last_error, None);
        assert!(s.updated_at > 0, "a transition must stamp a real timestamp");

        state.mark_ready(7);
        let s = state.status();
        assert_eq!(s.phase, McpPhase::Ready);
        assert_eq!(s.tool_count, 7, "ready records the advertised tool count");
        assert_eq!(s.last_error, None, "a healthy child carries no error");

        // A mid-session drop degrades to crashed with the cause named, tools
        // gone — not a panic (R006/R007).
        state.mark_crashed("child exited: signal 9");
        let s = state.status();
        assert_eq!(s.phase, McpPhase::Crashed);
        assert_eq!(s.tool_count, 0, "crashed tools degrade to unavailable");
        assert_eq!(s.last_error.as_deref(), Some("child exited: signal 9"));
    }

    #[test]
    fn mark_ready_clears_a_prior_crash_error() {
        // A retry that succeeds must not leave the stale crash reason on the
        // health line.
        let state = McpState::new();
        state.mark_crashed("handshake timed out");
        assert!(state.status().last_error.is_some());
        state.mark_ready(3);
        assert_eq!(
            state.status().last_error,
            None,
            "ready clears the prior error"
        );
        assert_eq!(state.status().tool_count, 3);
    }

    #[test]
    fn clear_peer_degrades_to_no_peer_and_is_a_no_op_when_empty() {
        // The crash→degrade seam (S04 T03): after a mid-session drop the spawn
        // supervisor clears the dead peer so a subsequent run takes the `None`
        // mount branch ("tools unavailable") instead of dispatching to a corpse.
        let state = McpState::new();
        assert!(state.peer().is_none(), "fresh state has no peer");
        // Clearing an already-empty peer is a no-op, never a panic.
        state.clear_peer();
        assert!(state.peer().is_none());
    }

    #[test]
    fn crash_after_ready_degrades_health_and_drops_the_peer() {
        // The full mid-session drop the supervisor performs: a ready child with a
        // tool count crashes → health is Crashed with the cause named, tools gone,
        // and the peer cleared so the next run degrades rather than panics
        // (R006/R007). No live peer is needed — this is the value-level proof of
        // the crash→degrade contract the T05 live test exercises end-to-end.
        let state = McpState::new();
        state.mark_ready(5);
        assert_eq!(state.status().phase, McpPhase::Ready);
        assert_eq!(state.status().tool_count, 5);

        // What supervise() does on a non-cancelled quit:
        state.clear_peer();
        state.mark_crashed("MCP server 'everything' exited mid-session (Closed)");
        let s = state.status();
        assert_eq!(s.phase, McpPhase::Crashed);
        assert_eq!(s.tool_count, 0, "crashed tools degrade to unavailable");
        assert_eq!(
            s.last_error.as_deref(),
            Some("MCP server 'everything' exited mid-session (Closed)")
        );
        assert!(state.peer().is_none(), "the dead peer is cleared on crash");
    }

    #[test]
    fn shutdown_handle_is_absent_until_a_child_is_spawned() {
        // No child spawned → the RunEvent exit hook finds no handle and is a
        // no-op (never cancels a non-existent child). The handle is populated
        // only by the spawn lifecycle (set_shutdown_handle), which needs a real
        // RunningService — proven in the T05 live test, not constructible here.
        let state = McpState::new();
        assert!(state.take_shutdown_handle().is_none());
        // Taking again is still None (idempotent, exactly-once semantics).
        assert!(state.take_shutdown_handle().is_none());
    }

    #[test]
    fn set_mode_is_reflected_in_status_and_stamps_the_transition() {
        // The gate reads mode() and the health surface reads status().mode from
        // the same source; a mode change is a timestamped transition so the
        // broadcast carries a fresh updatedAt.
        let state = McpState::new();
        assert_eq!(state.status().mode, McpRunMode::Off);
        assert_eq!(state.status().updated_at, 0);
        state.set_mode(McpRunMode::Ask);
        let s = state.status();
        assert_eq!(s.mode, McpRunMode::Ask);
        assert_eq!(
            s.phase,
            McpPhase::Disconnected,
            "mode change does not fake a lifecycle phase"
        );
        assert!(s.updated_at > 0, "a mode change stamps the transition time");
    }
}
