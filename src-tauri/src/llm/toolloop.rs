//! Bounded tool dispatch loop (S03 T03): when the model requests tools, run
//! them against the real backend and keep streaming per the OpenAI protocol.
//!
//! [`run_tool_loop`] is deliberately Tauri-runtime-independent: the client,
//! the tool executor, and both event sinks are injected, so unit tests and
//! the integration/live tests drive the exact production loop without an
//! `AppHandle`. The `chat` command wires it to the app: tokens go out as
//! `llm://token`, tool phases as [`TOOL_CALL_EVENT`] / [`TOOL_RESULT_EVENT`]
//! (the UI-facing memory-consulted surface, T04).
//!
//! Termination is structural (Q6): at most [`MAX_TOOL_ROUNDS`] tool rounds
//! carry tool definitions; the follow-up request after the last round strips
//! them, so the model must answer in text and the loop cannot spin. Tool
//! failures (unknown tool, malformed arguments, store errors) never abort
//! the stream — they ride back to the model as a JSON error payload and to
//! the UI as an `ok: false` result event (R006: typed, visible, never
//! silent).

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::appfocus::{AppFocus, AppFocusError};
use crate::input::commands::{
    resolve_approval, ApprovalDecision, HidArmState, HidRunMode, SessionWhitelist,
};
use crate::input::{ActionKind, InputAction, InputControl, InputError, MouseButton};
use crate::memory::commands::{DEFAULT_SEARCH_LIMIT, MAX_SEARCH_LIMIT};
use crate::memory::{search, Embedder, MemoryStore, SearchMode};
use crate::screenquery::{ScreenElement, ScreenQuery};

use super::{
    ChatMessage, ChatRequest, LlmClient, LlmError, ReasoningSink, StreamOutcome, TokenSink,
    ToolCall, ToolDefinition,
};

/// Event names — the tool-phase half of the IPC contract with `src/chat.ts`.
pub const TOOL_CALL_EVENT: &str = "llm://tool-call";
pub const TOOL_RESULT_EVENT: &str = "llm://tool-result";

/// Maximum rounds in which the request carries tool definitions. The loop
/// runs until the model stops calling tools (the normal agentic exit); this
/// is only the high safety ceiling that bounds a runaway model. The round
/// after the last one strips tools, forcing a text answer — the structural
/// bound that makes the loop terminate whatever the model does. Raised from
/// the S03 fixed 3-round assist cap to an agentic run-until-done ceiling
/// (S04 T01); real multi-step tasks (screen_query → input_action → …) need
/// many rounds, so this must be well above any legitimate task's tool count
/// while still guaranteeing termination.
pub const MAX_TOOL_ROUNDS: usize = 40;

/// The one tool S03 ships. The name is part of the model-facing contract
/// and the UI's memory-consulted check (T04).
pub const MEMORY_SEARCH_TOOL: &str = "memory_search";

/// The HID tool S01 ships (M005). One tool with a tagged `action` argument
/// (mirroring [`InputAction`]'s serde tag) keeps the composite's
/// dispatch-by-name simple and the model's tool list short.
pub const INPUT_ACTION_TOOL: &str = "input_action";

/// The screen-query tool S02 ships (M005): returns the on-screen text elements
/// with absolute screen-pixel coordinates the model then aims an
/// [`INPUT_ACTION_TOOL`] click at. Coordinates are transient — produced per
/// query, never persisted (R011/R023).
pub const SCREEN_QUERY_TOOL: &str = "screen_query";

/// The app-focus tool S05 ships (M005): brings a running app to the front by
/// best-effort name match, so the model can operate the app it means (e.g.
/// Chrome) rather than whatever happened to be frontmost. HID-class: gated
/// through the same [`ApprovalGate`] as [`INPUT_ACTION_TOOL`]
/// ([`ActionKind::FocusApp`]).
pub const FOCUS_APP_TOOL: &str = "focus_app";

/// The HID-orchestration system prompt prepended to every chat request that the
/// caller did not already ground with a `system` turn (M005 targeting fix). Small
/// local models do not infer the focus→query→click discipline from a flat tool
/// list, so it is spelled out. It is guidance; the structural [`ScreenSeen`] gate
/// enforces the coordinate rule even when the model ignores the prose.
pub const HID_SYSTEM_PROMPT: &str = "You can control this computer for the user with tools: \
focus_app (open an app or bring it to the front — it launches the app if it is not running), \
screen_query (read the text on screen with each item's \
exact pixel coordinates), and input_action (move/click the mouse, type text, press a key).\n\n\
To operate any app, follow this order every time:\n\
1. Call focus_app with the app name (e.g. \"Google Chrome\") to open it / bring it to the front.\n\
2. Call screen_query to see what is on screen and get the real pixel coordinates of the element \
you want. Each element gives x,y for its top-left corner plus width and height; aim at its centre \
(x + width/2, y + height/2).\n\
3. To click a target, call input_action with action \"mouse-click\" and pass the x,y you computed \
from that screen_query result — the click moves to that point and clicks it in one step.\n\
4. Use action \"type-text\" or \"key-press\" to enter text. Typing goes into whatever you last \
clicked, so ALWAYS mouse-click the exact field you want to fill before you type. To search the \
web: focus_app the browser, screen_query, mouse-click its address bar, type-text the search \
words, then key-press \"return\".\n\n\
Every x,y you pass MUST come from the most recent screen_query — never guess coordinates. A click \
or move to a coordinate you did not read from screen_query will be refused. After you focus_app the \
screen changes, so call screen_query again before you click.\n\n\
Always focus_app FIRST, before screen_query. Once you have focused an app, screen_query returns only \
that app's on-screen elements — so only ever aim at an element screen_query returned. Never click a \
coordinate that is not one of those elements: an empty spot is the desktop wallpaper, and clicking it \
hides the user's windows instead of doing what they asked.\n\n\
Report tool results honestly. If a tool call returns an error, tell the user it failed and why — \
never claim an action succeeded when the tool reported a failure.";

/// The typed failure kind the [`ApprovalGate`] returns when the model tries to
/// aim the mouse at coordinates it never obtained from [`SCREEN_QUERY_TOOL`].
/// A small model that guesses a coordinate lands on the wrong target (the
/// menubar / tray icon is the signature of a top-of-screen guess); refusing the
/// blind click structurally forces the `focus_app → screen_query → mouse` order
/// instead of leaving it to model discipline (M005 targeting fix).
pub const NO_SCREEN_QUERY_KIND: &str = "no-screen-query";

/// The typed failure a coordinate-bearing click/move gets when its (x, y) does
/// not land inside any element the last [`SCREEN_QUERY_TOOL`] returned — the
/// model aimed at bare desktop / between windows. Distinct from
/// [`NO_SCREEN_QUERY_KIND`] (never looked): here the model *did* look but is
/// clicking a coordinate that is not one of the real elements. Refusing it is
/// what actually stops the "click wallpaper → reveal desktop → windows hide"
/// failure (M005), rather than merely telling the model not to.
pub const OFF_TARGET_KIND: &str = "off-target";

/// A per-run flag recording whether the model has *seen the screen* — i.e.
/// called [`SCREEN_QUERY_TOOL`] and gotten real pixel coordinates — since the
/// last thing that changed what is on screen. It is the structural half of the
/// targeting fix: any mouse action that names an absolute coordinate (a
/// `mouse-move`, or a coordinate-bearing `mouse-click`) is refused until it is
/// set, so the model cannot aim at a coordinate it guessed.
///
/// Lifecycle, one instance per `chat()` request (shared by `Arc`):
/// - starts `false` (the model has not looked yet);
/// - a successful `screen_query` sets it `true` (coordinates are now grounded);
/// - a `focus_app` clears it back to `false` — activation changes what is
///   frontmost, so any prior coordinates are stale and the model must re-query.
///
/// A bare `mouse-click` (no x/y), `type-text`, and `key-press` carry no
/// coordinate, so they are never gated on this flag; only actions that name an
/// absolute pixel — `mouse-move` and a coordinate-bearing `mouse-click` — are.
/// One on-screen element's bounding box in absolute screen pixels, captured
/// from a `screen_query` result. The gate keeps the boxes the model was last
/// shown so it can verify a click's coordinate lands *inside a real element*,
/// not on bare desktop between windows (the M005 miss). `x,y` is the top-left
/// corner; the box spans `[x, x+width) × [y, y+height)` (right/bottom exclusive,
/// matching `attribute_app`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SeenBox {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

impl SeenBox {
    /// Is `(px, py)` inside this box? Right/bottom edges are exclusive.
    fn contains(&self, px: i32, py: i32) -> bool {
        px >= self.x
            && px < self.x + self.width
            && py >= self.y
            && py < self.y + self.height
    }
}

/// A per-run record of whether the model has *seen the screen* — i.e. called
/// [`SCREEN_QUERY_TOOL`] and gotten real pixel coordinates — since the last
/// thing that changed what is on screen, *and the exact boxes it was shown*.
/// It is the structural half of the targeting fix: a mouse action naming an
/// absolute coordinate is refused unless (1) the model has looked since the
/// last focus change AND (2) the coordinate lands inside one of the boxes the
/// last `screen_query` returned. Telling the model "only click real elements"
/// is not enough for a 9B model — this enforces it: a click on bare desktop
/// (which on Sonoma+ reveals the desktop and hides the user's windows) is
/// refused before it reaches the backend.
///
/// The seen-flag and the boxes are one field so they can never drift: seeing
/// the screen *is* holding a (possibly empty) box set; invalidating clears both.
///
/// Lifecycle, one instance per `chat()` request (shared by `Arc`):
/// - starts empty (`None` — the model has not looked yet; any aimed action is
///   refused with `no-screen-query`);
/// - a successful `screen_query` stores the filtered element boxes;
/// - a `focus_app` clears them — activation changes what is frontmost, so any
///   prior coordinates are stale and the model must re-query.
#[derive(Debug, Default)]
pub struct ScreenSeen(std::sync::Mutex<Option<Vec<SeenBox>>>);

impl ScreenSeen {
    /// A fresh gate for one chat request — the model has not looked at the
    /// screen yet.
    pub fn new() -> Self {
        Self(std::sync::Mutex::new(None))
    }

    /// The model just got real on-screen coordinates from `screen_query`: record
    /// the boxes it was shown (already filtered to the focused app) so a later
    /// click can be checked against them.
    pub fn mark_seen(&self, boxes: Vec<SeenBox>) {
        *self.0.lock().unwrap() = Some(boxes);
    }

    /// The frontmost app changed (a `focus_app` activation): prior coordinates
    /// are stale, so the model must query the screen again before aiming.
    pub fn invalidate(&self) {
        *self.0.lock().unwrap() = None;
    }

    /// Has the model seen the current screen (called `screen_query` since the
    /// last focus change)?
    pub fn seen(&self) -> bool {
        self.0.lock().unwrap().is_some()
    }

    /// Does `(x, y)` land inside one of the boxes the last `screen_query`
    /// returned? `false` when the model has not looked yet (no boxes) or when the
    /// coordinate is off every element — the desktop-click case. An empty box set
    /// (the model queried but the focused app had no recognized elements) also
    /// returns `false`: there is nothing legitimate to click.
    pub fn on_target(&self, x: i32, y: i32) -> bool {
        self.0
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|boxes| boxes.iter().any(|b| b.contains(x, y)))
    }
}

/// The app the model last brought to the front with `focus_app`, shared per
/// chat request (`Arc`) between the [`ApprovalGate`] that sets it and the
/// [`ScreenQueryTool`] that reads it. It is the second half of the targeting
/// fix: once an app is focused, `screen_query` returns ONLY that app's
/// elements, so the model structurally cannot aim a click at the desktop or
/// another app — the exact failure mode where a click on exposed wallpaper
/// triggered macOS "reveal desktop" and hid the user's windows (M005).
///
/// Lifecycle, one instance per `chat()` request:
/// - starts `None` (no app focused — `screen_query` returns everything, the
///   pre-focus survey the model uses to decide what to focus);
/// - a successful `focus_app` stores the app's resolved localized name;
/// - a later `focus_app` overwrites it with the new target.
///
/// Matching is case-insensitive and exact on the localized app name, which is
/// what both `focus_app` returns (`FocusedApp.app`) and `attribute_app` writes
/// onto each element (`ScreenElement.app`).
#[derive(Debug, Default)]
pub struct FocusedApp(std::sync::Mutex<Option<String>>);

impl FocusedApp {
    /// A fresh holder for one chat request — nothing focused yet.
    pub fn new() -> Self {
        Self(std::sync::Mutex::new(None))
    }

    /// Record the app `focus_app` just brought to the front (its resolved
    /// localized name), so subsequent `screen_query` results are filtered to it.
    pub fn set(&self, app: impl Into<String>) {
        *self.0.lock().unwrap() = Some(app.into());
    }

    /// The currently-focused app name, or `None` before any `focus_app`.
    pub fn current(&self) -> Option<String> {
        self.0.lock().unwrap().clone()
    }

    /// Keep only the elements owned by the focused app (case-insensitive exact
    /// match on the localized name). Before any focus (`None`) every element is
    /// returned — the pre-focus survey. An element with no `app` attribution is
    /// dropped once an app is focused: unattributed regions are the desktop /
    /// menu-bar chrome the model must not click.
    pub fn filter(&self, elements: Vec<ScreenElement>) -> Vec<ScreenElement> {
        match self.current() {
            None => elements,
            Some(focused) => elements
                .into_iter()
                .filter(|el| {
                    el.app
                        .as_deref()
                        .is_some_and(|a| a.eq_ignore_ascii_case(&focused))
                })
                .collect(),
        }
    }
}

/// A model-requested tool call, about to execute. Carries the round so the
/// UI (and logs) can reconstruct multi-round traces.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCallEvent {
    pub request_id: u64,
    pub round: usize,
    pub call: ToolCall,
}

/// One executed tool call's outcome. `ok: false` carries the typed failure
/// kind (`unknown-tool` / `invalid-arguments` / a [`crate::memory::MemoryError`]
/// kind); a successful memory search carries its result count and ranking
/// mode — the payload driving the memory-consulted indicator.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolResultEvent {
    pub request_id: u64,
    pub round: usize,
    pub call_id: String,
    pub name: String,
    pub ok: bool,
    pub result_count: Option<usize>,
    pub mode: Option<SearchMode>,
    pub failure: Option<String>,
}

/// A tool-phase event leaving the loop. The `chat` command maps each variant
/// to its `llm://` event name; tests capture them directly.
#[derive(Debug, Clone, PartialEq)]
pub enum ToolEvent {
    Call(ToolCallEvent),
    Result(ToolResultEvent),
}

/// Tool-phase callback, mirroring [`TokenSink`]: `Fn` so a `&dyn` reference
/// shares with the loop; collect state behind a `Mutex` or channel.
pub type ToolEventSink<'a> = &'a (dyn Fn(&ToolEvent) + Send + Sync);

/// What one executed tool call feeds back: `content` rides to the model as
/// the tool-role turn; the remaining fields become the [`ToolResultEvent`].
#[derive(Debug, Clone, PartialEq)]
pub struct ToolOutcome {
    pub content: String,
    pub ok: bool,
    pub result_count: Option<usize>,
    pub mode: Option<SearchMode>,
    /// Typed failure kind when `ok` is false.
    pub failure: Option<String>,
}

impl ToolOutcome {
    /// A typed failure: the model sees `{"error": detail}` (so it can
    /// recover or answer without the tool), the UI sees the kind.
    pub(crate) fn failure(kind: &str, detail: impl Into<String>) -> Self {
        let detail = detail.into();
        Self {
            content: serde_json::json!({ "error": detail }).to_string(),
            ok: false,
            result_count: None,
            mode: None,
            failure: Some(kind.to_string()),
        }
    }
}

/// The executor seam: what tools exist and how one call runs. Injected into
/// [`run_tool_loop`] so tests script it and S05+ can add tools without
/// touching the loop.
#[async_trait]
pub trait ToolExecutor: Send + Sync {
    /// The definitions advertised on tools-carrying rounds.
    fn definitions(&self) -> Vec<ToolDefinition>;

    /// Execute one call. Never errors — every failure is a typed
    /// [`ToolOutcome`] the model and UI both see (R006).
    async fn execute(&self, call: &ToolCall) -> ToolOutcome;
}

/// `memory_search` over the real S02 store — no new store logic, exactly the
/// `search` the `memory_search` IPC command uses, with the same clamps.
pub struct MemorySearchTool {
    store: Arc<MemoryStore>,
    embedder: Arc<dyn Embedder>,
}

impl MemorySearchTool {
    pub fn new(store: Arc<MemoryStore>, embedder: Arc<dyn Embedder>) -> Self {
        Self { store, embedder }
    }

    /// The model-facing definition. The schema keeps `limit` optional so a
    /// small model can call with just a query string.
    pub fn definition() -> ToolDefinition {
        ToolDefinition {
            name: MEMORY_SEARCH_TOOL.into(),
            description: "Search the user's stored activity memories (summaries of what they \
                          were doing on this computer, with app names and time spans). Call \
                          this when the user asks about their earlier work or activity."
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Free-text search query, e.g. \"rust debugging this morning\""
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum results to return (optional)",
                        "minimum": 1,
                        "maximum": MAX_SEARCH_LIMIT
                    }
                },
                "required": ["query"]
            }),
        }
    }
}

#[derive(Debug, serde::Deserialize)]
struct MemorySearchArgs {
    query: String,
    #[serde(default)]
    limit: Option<usize>,
}

#[async_trait]
impl ToolExecutor for MemorySearchTool {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![Self::definition()]
    }

    async fn execute(&self, call: &ToolCall) -> ToolOutcome {
        if call.name != MEMORY_SEARCH_TOOL {
            return ToolOutcome::failure(
                "unknown-tool",
                format!("unknown tool: {} (available: {MEMORY_SEARCH_TOOL})", call.name),
            );
        }
        let args: MemorySearchArgs = match serde_json::from_str(&call.arguments) {
            Ok(args) => args,
            Err(e) => {
                return ToolOutcome::failure(
                    "invalid-arguments",
                    format!("invalid {MEMORY_SEARCH_TOOL} arguments: {e}"),
                )
            }
        };
        // Same clamp as the memory_search IPC command (Q6): the model cannot
        // request an unbounded page out of SQLite.
        let limit = args.limit.unwrap_or(DEFAULT_SEARCH_LIMIT).clamp(1, MAX_SEARCH_LIMIT);
        match search(&self.store, self.embedder.as_ref(), &args.query, limit).await {
            Ok(outcome) => {
                let content = serde_json::to_string(&outcome)
                    .unwrap_or_else(|e| format!(r#"{{"error":"result serialization failed: {e}"}}"#));
                ToolOutcome {
                    content,
                    ok: true,
                    result_count: Some(outcome.results.len()),
                    mode: Some(outcome.mode),
                    failure: None,
                }
            }
            // Store failure: typed to model and UI, stream keeps going —
            // the model can still answer from context.
            Err(err) => ToolOutcome::failure(err.kind(), err.to_string()),
        }
    }
}

/// The HID input tool over the S01 [`InputControl`] backend (M005), gated on the
/// shared [`HidArmState`] (S03). Advertises one `input_action` tool whose
/// argument is an [`InputAction`] (tagged on `action`), parses the model's
/// arguments into it, and dispatches the real click/keystroke through the
/// backend. Every failure — bad arguments, a typed [`crate::input::InputError`]
/// (permission-denied / unsupported / input-failed) — rides back as a typed
/// [`ToolOutcome`], never a silent no-op (R007).
///
/// Structural gate (D038, non-negotiable): when the arm-state is disarmed the
/// tool contributes **zero** definitions (the model is never offered
/// `input_action` at all) and any `execute()` that still reaches it is refused
/// with a typed `disabled` [`InputError`] BEFORE the backend is touched. This is
/// structural inertness, not a UI hint — the gate is the tool's own state.
pub struct InputTool {
    backend: Arc<dyn InputControl>,
    arm: Arc<HidArmState>,
}

impl InputTool {
    pub fn new(backend: Arc<dyn InputControl>, arm: Arc<HidArmState>) -> Self {
        Self { backend, arm }
    }

    /// The model-facing definition. `action` is required and discriminates the
    /// remaining fields; the per-field descriptions name which action each
    /// belongs to so a small model can fill just the ones it needs.
    pub fn definition() -> ToolDefinition {
        ToolDefinition {
            name: INPUT_ACTION_TOOL.into(),
            description: "Drive this computer's mouse and keyboard: click the mouse (optionally \
                          moving to a target first), move the mouse, type text, or press a single \
                          key. Coordinates are absolute screen pixels and MUST come from a \
                          screen_query result — never guess an x/y. To click something: call \
                          focus_app to bring the app to the front, then screen_query to read its \
                          on-screen elements and their exact coordinates, then mouse-click with the \
                          x,y of the target (the click moves there and clicks in one step). A click \
                          or move to a guessed coordinate is refused."
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["mouse-move", "mouse-click", "type-text", "key-press"],
                        "description": "Which HID action to perform."
                    },
                    "x": {
                        "type": "integer",
                        "description": "mouse-move / mouse-click: absolute screen X coordinate in \
                                        pixels, from a screen_query element. For mouse-click, pass \
                                        x and y together to move to the target then click it."
                    },
                    "y": {
                        "type": "integer",
                        "description": "mouse-move / mouse-click: absolute screen Y coordinate in \
                                        pixels, from a screen_query element. For mouse-click, pass \
                                        x and y together to move to the target then click it."
                    },
                    "button": {
                        "type": "string",
                        "enum": ["left", "right", "middle"],
                        "description": "mouse-click: which mouse button to click (default left)."
                    },
                    "text": {
                        "type": "string",
                        "description": "type-text: the Unicode text to type as keystrokes."
                    },
                    "key": {
                        "type": "string",
                        "description": "key-press: a named key (return, tab, escape, space, \
                                        backspace, delete, up, down, left, right) or a single \
                                        character."
                    }
                },
                "required": ["action"]
            }),
        }
    }
}

#[async_trait]
impl ToolExecutor for InputTool {
    fn definitions(&self) -> Vec<ToolDefinition> {
        // Structural gate (D038): a disarmed tool advertises nothing, so the
        // CompositeExecutor never offers `input_action` to the model at all.
        if self.arm.armed() {
            vec![Self::definition()]
        } else {
            Vec::new()
        }
    }

    async fn execute(&self, call: &ToolCall) -> ToolOutcome {
        // Structural refusal FIRST (D038): a disarmed input action is rejected
        // with the typed `disabled` error before the InputControl backend is
        // ever touched — a visible tool result, never a silent no-op (R007).
        if !self.arm.armed() {
            let err = InputError::disabled();
            log::warn!("llm: input_action refused — HID disarmed (kind={})", err.kind());
            return ToolOutcome::failure(err.kind(), err.to_string());
        }
        if call.name != INPUT_ACTION_TOOL {
            return ToolOutcome::failure(
                "unknown-tool",
                format!("unknown tool: {} (available: {INPUT_ACTION_TOOL})", call.name),
            );
        }
        // The arguments ARE an InputAction (tagged on `action`) — one parse both
        // validates the shape and selects the action.
        let action: InputAction = match serde_json::from_str(&call.arguments) {
            Ok(action) => action,
            Err(e) => {
                return ToolOutcome::failure(
                    "invalid-arguments",
                    format!("invalid {INPUT_ACTION_TOOL} arguments: {e}"),
                )
            }
        };
        // Snapshot the action for the success payload before it moves into the
        // backend — the model sees exactly what was synthesized.
        let performed = serde_json::to_value(&action).unwrap_or(serde_json::Value::Null);
        match self.backend.perform(action).await {
            Ok(()) => ToolOutcome {
                content: serde_json::json!({ "ok": true, "performed": performed }).to_string(),
                ok: true,
                result_count: None,
                mode: None,
                failure: None,
            },
            // Typed InputError → same kind tag the UI matches on; the model sees
            // the detail and can recover (e.g. ask the user to grant access).
            Err(err) => ToolOutcome::failure(err.kind(), err.to_string()),
        }
    }
}

/// The screen-query tool over the S02 [`ScreenQuery`] backend (M005). Advertises
/// one `screen_query` tool with no arguments; each call captures the screen,
/// recognizes its on-screen text with bounding boxes on-device, and returns the
/// [`crate::screenquery::ScreenElement`]s (text + absolute screen-pixel box) as
/// a JSON array the model reads to aim an [`InputTool`] click. Every failure — a
/// typed [`crate::screenquery::ScreenQueryError`] (permission-denied /
/// recognition-failed / unsupported) — rides back as a typed [`ToolOutcome`],
/// never a silent empty result (R007). Coordinates are transient: they exist
/// only in this outcome's `content` and never reach the memory store (R011).
pub struct ScreenQueryTool {
    backend: Arc<dyn ScreenQuery>,
    /// Set on a successful query so the [`ApprovalGate`] knows the model has
    /// grounded coordinates and may aim the mouse (the targeting fix).
    screen_seen: Arc<ScreenSeen>,
    /// The app the model last focused. Once set, results are filtered to it so
    /// the model can only see (and thus click) elements inside the focused app,
    /// never the desktop or another app (the targeting fix, second half).
    focused_app: Arc<FocusedApp>,
}

impl ScreenQueryTool {
    pub fn new(
        backend: Arc<dyn ScreenQuery>,
        screen_seen: Arc<ScreenSeen>,
        focused_app: Arc<FocusedApp>,
    ) -> Self {
        Self { backend, screen_seen, focused_app }
    }

    /// The model-facing definition. No arguments: a screen query is a snapshot
    /// of whatever is on screen right now, so a small model can call it with an
    /// empty object.
    pub fn definition() -> ToolDefinition {
        ToolDefinition {
            name: SCREEN_QUERY_TOOL.into(),
            description: "Return the text currently visible on this computer's screen, each \
                          element with its absolute screen-pixel coordinates (x, y for the \
                          top-left corner, plus width and height). Call this to see what is on \
                          screen and to get the coordinates to pass to input_action to move or \
                          click the mouse on a target."
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {},
                "required": []
            }),
        }
    }
}

#[async_trait]
impl ToolExecutor for ScreenQueryTool {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![Self::definition()]
    }

    async fn execute(&self, call: &ToolCall) -> ToolOutcome {
        if call.name != SCREEN_QUERY_TOOL {
            return ToolOutcome::failure(
                "unknown-tool",
                format!("unknown tool: {} (available: {SCREEN_QUERY_TOOL})", call.name),
            );
        }
        // No arguments to parse — a screen query is a snapshot of the current
        // screen. The whole capture/recognize pipeline lives behind the backend.
        match self.backend.query().await {
            Ok(elements) => {
                // Filter to the focused app (if one was focused): the model then
                // only ever sees — and can only aim a click at — elements inside
                // that app, never the desktop wallpaper (which on Sonoma+ hides
                // all windows) or another app's chrome (M005 targeting fix).
                let focused = self.focused_app.current();
                let total = elements.len();
                if let Some(app) = &focused {
                    // Diagnostic (M005): the distinct app-attribution strings in
                    // the pre-filter set. If the focused name is absent here, the
                    // filter's name-authority (SC applicationName) disagrees with
                    // focus_app's (NSRunningApplication localizedName).
                    let mut attributed: Vec<&str> = elements
                        .iter()
                        .map(|el| el.app.as_deref().unwrap_or("<none>"))
                        .collect();
                    attributed.sort_unstable();
                    attributed.dedup();
                    log::debug!(
                        "screen_query: focused={app:?} distinct attributed apps={attributed:?}"
                    );
                }
                let elements = self.focused_app.filter(elements);
                if let Some(app) = &focused {
                    log::debug!(
                        "screen_query: filtered {} → {} element(s) for focused app {app:?}",
                        total,
                        elements.len()
                    );
                }
                // The model now holds real on-screen coordinates — unblock the
                // mouse-positioning gate AND record the exact boxes it was shown,
                // so a subsequent coordinate-bearing click is checked against them
                // (a click off every box is a desktop click and is refused). The
                // boxes are the *filtered* set, so only elements inside the focused
                // app are clickable (M005 targeting enforcement).
                self.screen_seen.mark_seen(
                    elements
                        .iter()
                        .map(|el| SeenBox {
                            x: el.x,
                            y: el.y,
                            width: el.width,
                            height: el.height,
                        })
                        .collect(),
                );
                let content = serde_json::to_string(&elements).unwrap_or_else(|e| {
                    format!(r#"{{"error":"result serialization failed: {e}"}}"#)
                });
                ToolOutcome {
                    content,
                    ok: true,
                    result_count: Some(elements.len()),
                    mode: None,
                    failure: None,
                }
            }
            // Typed ScreenQueryError → same kind tag the UI matches on; the model
            // sees the detail and can recover (e.g. ask the user to grant Screen
            // Recording) rather than aim a click at coordinates it never got.
            Err(err) => ToolOutcome::failure(err.kind(), err.to_string()),
        }
    }
}

/// The app-focus tool over the [`AppFocus`] backend (M005). Advertises one
/// `focus_app` tool with a single required `app` string; each call best-effort
/// matches an app by name and brings it to the front — launching it first
/// when it is not running — returning the localized name it verifiably
/// fronted and whether a launch was needed. Every failure — a typed
/// [`crate::appfocus::AppFocusError`] (not-found / activation-failed /
/// unsupported) — rides back as a typed [`ToolOutcome`], never a silent no-op
/// (R007). A `not-found` payload lists the running-app candidates so the model
/// can retry against a real name rather than guess again.
///
/// This tool is HID-class: the [`ApprovalGate`] wraps it so every activation is
/// gated through the per-action approval resolver ([`ActionKind::FocusApp`])
/// before it reaches the backend — the tool itself performs no gating.
pub struct FocusAppTool {
    backend: Arc<dyn AppFocus>,
}

impl FocusAppTool {
    pub fn new(backend: Arc<dyn AppFocus>) -> Self {
        Self { backend }
    }

    /// The model-facing definition. `app` is the required target name — a
    /// best-effort match, so a fuzzy value ("chrome") resolves to the running
    /// app ("Google Chrome") and the result reports which was fronted.
    pub fn definition() -> ToolDefinition {
        ToolDefinition {
            name: FOCUS_APP_TOOL.into(),
            description: "Open an application by name (e.g. \"Chrome\", \"Safari\", \
                          \"Finder\"): brings it to the front, launching it first if it is not \
                          running. Call this before operating an app with input_action so your \
                          clicks and keystrokes land in the app you mean, not whatever was \
                          frontmost. The match is best-effort; the result names the app actually \
                          brought to the front and whether it had to be launched."
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "app": {
                        "type": "string",
                        "description": "The application name to bring to the front, e.g. \"Google Chrome\"."
                    }
                },
                "required": ["app"]
            }),
        }
    }
}

#[derive(Debug, serde::Deserialize)]
struct FocusAppArgs {
    app: String,
}

#[async_trait]
impl ToolExecutor for FocusAppTool {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![Self::definition()]
    }

    async fn execute(&self, call: &ToolCall) -> ToolOutcome {
        if call.name != FOCUS_APP_TOOL {
            return ToolOutcome::failure(
                "unknown-tool",
                format!("unknown tool: {} (available: {FOCUS_APP_TOOL})", call.name),
            );
        }
        let args: FocusAppArgs = match serde_json::from_str(&call.arguments) {
            Ok(args) => args,
            Err(e) => {
                return ToolOutcome::failure(
                    "invalid-arguments",
                    format!("invalid {FOCUS_APP_TOOL} arguments: {e}"),
                )
            }
        };
        match self.backend.focus(&args.app).await {
            Ok(focused) => ToolOutcome {
                // `launched` lets the model say "opened" vs "switched to"
                // truthfully; the gate reads only `focused`, so it is additive.
                content: serde_json::json!({
                    "ok": true,
                    "focused": focused.app,
                    "launched": focused.launched,
                })
                .to_string(),
                ok: true,
                result_count: None,
                mode: None,
                failure: None,
            },
            // A not-found carries the running-app candidates back to the model so
            // it can retry against a real name; other typed errors ride their
            // kind back unchanged (R007).
            Err(AppFocusError::NotFound { requested, candidates }) => ToolOutcome {
                content: serde_json::json!({
                    "error": format!("no running or installed app matched {requested:?}"),
                    "candidates": candidates,
                })
                .to_string(),
                ok: false,
                result_count: None,
                mode: None,
                failure: Some("not-found".to_string()),
            },
            Err(err) => ToolOutcome::failure(err.kind(), err.to_string()),
        }
    }
}

/// Fans one [`run_tool_loop`] over several sub-executors (D037/MEM133):
/// concatenates their `definitions()` so the model sees every tool at once, and
/// dispatches `execute()` to whichever sub-executor advertises `call.name`. A
/// call no sub-executor owns returns the same typed `unknown-tool` failure a
/// lone tool would — the loop's signature is untouched, so every existing
/// tool-loop test stays green.
pub struct CompositeExecutor {
    executors: Vec<Box<dyn ToolExecutor>>,
}

impl CompositeExecutor {
    pub fn new(executors: Vec<Box<dyn ToolExecutor>>) -> Self {
        Self { executors }
    }
}

#[async_trait]
impl ToolExecutor for CompositeExecutor {
    fn definitions(&self) -> Vec<ToolDefinition> {
        self.executors.iter().flat_map(|e| e.definitions()).collect()
    }

    async fn execute(&self, call: &ToolCall) -> ToolOutcome {
        for executor in &self.executors {
            if executor.definitions().iter().any(|d| d.name == call.name) {
                return executor.execute(call).await;
            }
        }
        let available = self
            .definitions()
            .into_iter()
            .map(|d| d.name)
            .collect::<Vec<_>>()
            .join(", ");
        ToolOutcome::failure(
            "unknown-tool",
            format!("unknown tool: {} (available: {available})", call.name),
        )
    }
}

/// One verdict the overlay returns for a pending HID action (S04 T03) — the
/// user's answer to an [`ApprovalDecision::Prompt`]. Serialized kebab-case so
/// the `respond_hid_approval` IPC and `src/chat.ts` share the exact strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ApprovalVerdict {
    /// Perform this one action; do not remember the kind (prompts again next
    /// time this kind is requested).
    AllowOnce,
    /// Perform this action and grant its kind for the session — no more prompts
    /// for this kind until the session ends ("Always allow this kind").
    AllowKind,
    /// Refuse this action — a visible, typed `approval-denied` tool result; the
    /// backend is never touched.
    Deny,
}

/// The typed failure kind a denied (or timed-out) HID approval rides back as —
/// an `ok: false` [`ToolOutcome`] the model and UI both see (R006/R007), never
/// a silent no-op. Distinct from `disabled` (Off) so the surface can tell "you
/// said no to this action" from "HID is off".
pub const APPROVAL_DENIED_KIND: &str = "approval-denied";

/// The overlay-prompt seam (S04 T03): when the resolver says
/// [`ApprovalDecision::Prompt`], the gate calls this to surface the pending
/// action to the user and await their [`ApprovalVerdict`]. Injected into
/// [`ApprovalGate`] so the loop stays Tauri-free — production emits an
/// `hid://approval-request` event and awaits the `respond_hid_approval` IPC with
/// a bounded timeout (a timeout is [`ApprovalVerdict::Deny`], fail-closed), while
/// tests script the verdict directly.
#[async_trait]
pub trait ApprovalPrompt: Send + Sync {
    /// Surface `summary` (a human sentence describing the pending `kind` action)
    /// to the overlay and await the user's verdict. Never errors — a timeout or a
    /// closed channel resolves to [`ApprovalVerdict::Deny`] (fail-closed).
    async fn request(&self, kind: ActionKind, summary: String) -> ApprovalVerdict;
}

/// Wraps the [`InputTool`] with the S04 per-action approval gate: before any HID
/// action reaches the backend it consults the pure [`resolve_approval`] resolver
/// (T02) against the current [`HidRunMode`] and the session whitelist, and — only
/// when the resolver says [`ApprovalDecision::Prompt`] — asks the user via the
/// injected [`ApprovalPrompt`]. `Off` refuses with the S03 `disabled` error
/// before the action is even parsed (D038); `Perform` (AutoRun, or Ask with the
/// kind already whitelisted) delegates straight to the inner tool; a `Prompt`
/// that is denied (or times out) returns a typed `approval-denied` result and
/// never touches the backend; "Always allow this kind" mutates the session
/// whitelist so the same kind performs unprompted for the rest of the session.
///
/// The gate wraps the input tool AND the app-focus tool (both HID-class), so
/// memory_search / screen_query — sibling executors in the [`CompositeExecutor`]
/// — are never gated. `focus_app` is gated with [`ActionKind::FocusApp`] on the
/// exact same Off/Ask/AutoRun path as `input_action`; the two HID surfaces share
/// one gate so the mode snapshot, session whitelist, and approver are identical
/// for both.
pub struct ApprovalGate {
    inner: InputTool,
    focus: FocusAppTool,
    mode: HidRunMode,
    whitelist: Arc<std::sync::Mutex<SessionWhitelist>>,
    approver: Arc<dyn ApprovalPrompt>,
    /// The per-run "has the model looked at the screen" flag (the targeting
    /// fix): a `mouse-move` is refused until `screen_query` has grounded
    /// coordinates, and a successful `focus_app` invalidates it (the screen
    /// changed). Shared with the [`ScreenQueryTool`] that sets it.
    screen_seen: Arc<ScreenSeen>,
    /// The app the model has focused, recorded here on a successful `focus_app`
    /// and read by the [`ScreenQueryTool`] to filter results to that app (the
    /// targeting fix, second half). Shared per chat request.
    focused_app: Arc<FocusedApp>,
}

impl ApprovalGate {
    pub fn new(
        inner: InputTool,
        focus: FocusAppTool,
        mode: HidRunMode,
        whitelist: Arc<std::sync::Mutex<SessionWhitelist>>,
        approver: Arc<dyn ApprovalPrompt>,
        screen_seen: Arc<ScreenSeen>,
        focused_app: Arc<FocusedApp>,
    ) -> Self {
        Self { inner, focus, mode, whitelist, approver, screen_seen, focused_app }
    }

    /// A human sentence describing the pending action — what the overlay shows so
    /// the user knows exactly what they are approving. Pixel coordinates and typed
    /// text are transient prompt context only; they never persist (R011/R023).
    fn summary(action: &InputAction) -> String {
        match action {
            InputAction::MouseMove { x, y } => format!("Move the mouse to ({x}, {y})"),
            InputAction::MouseClick { button, x: Some(x), y: Some(y) } => {
                format!("Click the {} mouse button at ({x}, {y})", button_name(*button))
            }
            InputAction::MouseClick { button, .. } => {
                format!("Click the {} mouse button", button_name(*button))
            }
            InputAction::TypeText { text } => format!("Type {}", quote_preview(text)),
            InputAction::KeyPress { key } => format!("Press the {key} key"),
        }
    }

    /// The shared per-action gate for one HID-class call, factored out so
    /// `input_action` and `focus_app` follow the byte-identical Off/Ask/AutoRun
    /// path: Off refuses `disabled` (already handled by the caller), the resolver
    /// gates by `kind`, a `Prompt` asks the injected approver, and Perform /
    /// Allow-once / Allow-kind delegate to `run` — the closure that dispatches to
    /// the owning inner tool. A denied (or timed-out) prompt returns the typed
    /// `approval-denied` result and never runs. `tool` names the surface for logs.
    async fn gate_and_run<F, Fut>(
        &self,
        tool: &str,
        kind: ActionKind,
        summary: String,
        run: F,
    ) -> ToolOutcome
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = ToolOutcome>,
    {
        // Resolve under the lock, then drop it before any `.await` (the whitelist
        // guard must never be held across the approval round-trip).
        let decision = {
            let whitelist = self.whitelist.lock().unwrap();
            resolve_approval(self.mode, kind, &whitelist)
        };
        match decision {
            // Only Off resolves to Refuse and that is handled by the caller; defensive.
            ApprovalDecision::Refuse => {
                let err = InputError::disabled();
                ToolOutcome::failure(err.kind(), err.to_string())
            }
            ApprovalDecision::Perform => {
                log::info!(
                    "llm: {tool} approved without prompt kind={kind:?} mode={:?} (auto-run or whitelisted)",
                    self.mode
                );
                run().await
            }
            ApprovalDecision::Prompt => {
                let verdict = self.approver.request(kind, summary).await;
                match verdict {
                    ApprovalVerdict::Deny => {
                        log::warn!("llm: {tool} denied by user kind={kind:?}");
                        ToolOutcome::failure(
                            APPROVAL_DENIED_KIND,
                            format!("the user denied this HID action ({kind:?})"),
                        )
                    }
                    ApprovalVerdict::AllowOnce => {
                        log::info!("llm: {tool} allowed once kind={kind:?}");
                        run().await
                    }
                    ApprovalVerdict::AllowKind => {
                        self.whitelist.lock().unwrap().allow(kind);
                        log::info!(
                            "llm: {tool} allowed + kind whitelisted for session kind={kind:?}"
                        );
                        run().await
                    }
                }
            }
        }
    }
}

#[async_trait]
impl ToolExecutor for ApprovalGate {
    fn definitions(&self) -> Vec<ToolDefinition> {
        // Off is structurally inert (D038): advertise NEITHER HID surface, so the
        // composite never offers input_action or focus_app to the model at all —
        // the run-mode gate withholds them regardless of the inner arm state.
        if self.mode == HidRunMode::Off {
            return Vec::new();
        }
        // Armed mode: the inner tool's own S03 structural gate still applies (a
        // disarmed InputTool advertises nothing), plus the HID-class focus_app.
        let mut defs = self.inner.definitions();
        defs.extend(self.focus.definitions());
        defs
    }

    async fn execute(&self, call: &ToolCall) -> ToolOutcome {
        // Route by name. focus_app is HID-class and gated with ActionKind::FocusApp;
        // input_action keeps its byte-identical path. Anything else is not ours —
        // hand straight to the inner tool (defensive; the composite routes by name).
        if call.name == FOCUS_APP_TOOL {
            // Off is structurally inert (D038): refuse with the `disabled` error
            // BEFORE parsing or activating — the whitelist can never un-inert a
            // disarmed machine.
            if self.mode == HidRunMode::Off {
                let err = InputError::disabled();
                log::warn!("llm: focus_app refused — HID off (kind={})", err.kind());
                return ToolOutcome::failure(err.kind(), err.to_string());
            }
            // Parse the target app so the prompt names it. A malformed call is a
            // typed invalid-arguments failure — never a prompt, never an activate.
            let app = match serde_json::from_str::<FocusAppArgs>(&call.arguments) {
                Ok(args) => args.app,
                Err(e) => {
                    return ToolOutcome::failure(
                        "invalid-arguments",
                        format!("invalid {FOCUS_APP_TOOL} arguments: {e}"),
                    )
                }
            };
            let summary = format!("Open {} (bring to front, launching if needed)", quote_preview(&app));
            let outcome = self
                .gate_and_run(FOCUS_APP_TOOL, ActionKind::FocusApp, summary, || {
                    self.focus.execute(call)
                })
                .await;
            // A successful activation changes what is frontmost, so any pixel
            // coordinates the model already holds are now stale — force a fresh
            // screen_query before it may aim the mouse again (the targeting fix).
            if outcome.ok {
                self.screen_seen.invalidate();
                // Record the RESOLVED app name (best-effort match may differ from
                // the requested string, e.g. "chrome" → "Google Chrome") so the
                // ScreenQueryTool filters to exactly what attribute_app labels
                // elements with. Fall back to the requested name if the content
                // is somehow unparseable — never leave the filter unset after a
                // successful focus.
                let resolved = serde_json::from_str::<serde_json::Value>(&outcome.content)
                    .ok()
                    .and_then(|v| v.get("focused").and_then(|f| f.as_str()).map(str::to_owned))
                    .unwrap_or(app);
                self.focused_app.set(resolved.clone());
                log::info!(
                    "llm: focus_app succeeded ({resolved:?}) — coordinates invalidated; screen_query now filtered to this app"
                );
            }
            return outcome;
        }
        if call.name != INPUT_ACTION_TOOL {
            return self.inner.execute(call).await;
        }
        // Off is structurally inert (D038): refuse with the S03 `disabled` error
        // BEFORE parsing or touching anything — the whitelist can never un-inert a
        // disarmed machine.
        if self.mode == HidRunMode::Off {
            let err = InputError::disabled();
            log::warn!("llm: input_action refused — HID off (kind={})", err.kind());
            return ToolOutcome::failure(err.kind(), err.to_string());
        }
        // Parse to get the action kind the resolver gates on. A malformed action
        // is a typed invalid-arguments failure — never a prompt for a nonsense
        // action, never a backend touch.
        let action: InputAction = match serde_json::from_str(&call.arguments) {
            Ok(action) => action,
            Err(e) => {
                return ToolOutcome::failure(
                    "invalid-arguments",
                    format!("invalid {INPUT_ACTION_TOOL} arguments: {e}"),
                )
            }
        };
        // A coordinate-bearing click must carry both x and y (or neither). A
        // half-specified aim would silently degrade to a click-at-cursor — the
        // exact failure mode we are closing — so reject it as invalid-arguments
        // before it can reach the backend.
        if let Err(detail) = action.validate() {
            return ToolOutcome::failure(
                "invalid-arguments",
                format!("invalid {INPUT_ACTION_TOOL} arguments: {detail}"),
            );
        }
        // Structural targeting guard (M005): any action that names an absolute
        // pixel — a `mouse-move` OR a coordinate-bearing `mouse-click` (the shape
        // small models actually emit: they put the target on the click, not a
        // separate move) — must come from a real screen_query, not a guess.
        // Refuse it, with a typed actionable error the model can recover from,
        // until the model has looked at the screen since the last focus change.
        // This is what stops a small model from clicking the tray icon on a
        // guessed coordinate. A bare click / type / key carries no coordinate and
        // is never gated here.
        if let Some((ax, ay)) = action.aim_target() {
            // Two-stage structural targeting guard (M005). Stage 1: the model
            // must have looked at the screen since the last focus change — a blind
            // aim is refused with no-screen-query. Stage 2: even after looking, the
            // coordinate must land inside one of the elements screen_query actually
            // returned. A coordinate off every box is a click on bare desktop
            // between windows — the exact miss that reveals the desktop and hides
            // the user's windows. Telling the model "only click real elements" was
            // never enough for a 9B model; this refuses the off-target aim before
            // it reaches the backend and hands back the actionable reason so the
            // model re-queries and picks a real element.
            if !self.screen_seen.seen() {
                log::warn!(
                    "llm: input_action {} refused — no screen_query yet (kind={NO_SCREEN_QUERY_KIND})",
                    action.kind_str()
                );
                return ToolOutcome::failure(
                    NO_SCREEN_QUERY_KIND,
                    "call screen_query first to get real on-screen pixel coordinates before \
                     aiming the mouse — do not guess coordinates. Pass the x,y from screen_query \
                     to a mouse-click. If you need a specific app, call focus_app to bring it to \
                     the front, then screen_query, then click.",
                );
            }
            if !self.screen_seen.on_target(ax, ay) {
                log::warn!(
                    "llm: input_action {} refused — ({ax}, {ay}) is not inside any \
                     screen_query element (kind={OFF_TARGET_KIND})",
                    action.kind_str()
                );
                return ToolOutcome::failure(
                    OFF_TARGET_KIND,
                    format!(
                        "({ax}, {ay}) is not inside any element screen_query returned — that spot \
                         is empty desktop, and clicking it hides the user's windows instead of \
                         doing what they asked. Pick one of the elements from the most recent \
                         screen_query and aim at its centre (x + width/2, y + height/2). If the \
                         target is not among them, call screen_query again (or focus_app then \
                         screen_query) — never invent a coordinate."
                    ),
                );
            }
        }
        let kind = action.kind();
        let summary = Self::summary(&action);
        self.gate_and_run(INPUT_ACTION_TOOL, kind, summary, || self.inner.execute(call))
            .await
    }
}

fn button_name(button: MouseButton) -> &'static str {
    match button {
        MouseButton::Left => "left",
        MouseButton::Right => "right",
        MouseButton::Middle => "middle",
    }
}

/// A bounded, quoted preview of model-produced text for the approval prompt — the
/// user sees what will be typed without an unbounded string on screen.
fn quote_preview(text: &str) -> String {
    const MAX: usize = 60;
    if text.chars().count() <= MAX {
        format!("\"{text}\"")
    } else {
        let cut: String = text.chars().take(MAX).collect();
        format!("\"{cut}…\"")
    }
}

/// The result of a full tool loop (S04 T04): the model's final [`StreamOutcome`]
/// plus whether the user's Stop signal cut the run short mid-loop. A stopped run
/// is not an error — it carries whatever text streamed before the stop so the UI
/// settles a visible partial answer, never silence (R006). `stopped` is the
/// typed, visible outcome the Stop control needs, distinct from a natural finish.
#[derive(Debug, Clone, PartialEq)]
pub struct LoopOutcome {
    pub outcome: StreamOutcome,
    pub stopped: bool,
}

impl LoopOutcome {
    /// A natural finish — the model stopped calling tools, the ceiling forced a
    /// text answer, or a zombie call terminated the loop.
    fn done(outcome: StreamOutcome) -> Self {
        Self { outcome, stopped: false }
    }

    /// A user-stopped run: whatever streamed before the stop, no tool calls
    /// leaking out, flagged so the caller surfaces the `stopped` run-state.
    fn stopped(text: String, token_count: usize) -> Self {
        Self { outcome: StreamOutcome { text, token_count, tool_calls: Vec::new() }, stopped: true }
    }
}

/// Stop-signal seam: a cheap predicate the loop polls between rounds and before
/// each tool dispatch. `Fn` (not `FnMut`) so a `&dyn` reference shares with the
/// loop the way [`TokenSink`] does — production backs it by the request's
/// `AtomicBool` stop flag, tests script it directly. Never blocks.
pub type StopSignal<'a> = &'a (dyn Fn() -> bool + Send + Sync);

/// Drive one chat request through its tool rounds to a final text answer.
///
/// The never-stop wrapper over [`run_tool_loop_with_stop`]: every S01-S03 caller
/// (and the CI integration tests) keeps the pre-S04 signature and
/// [`StreamOutcome`] return. Runs inside the spawned chat task, so single-flight
/// supersede-abort still covers every round.
pub async fn run_tool_loop(
    client: &dyn LlmClient,
    executor: &dyn ToolExecutor,
    messages: Vec<ChatMessage>,
    request_id: u64,
    on_token: TokenSink<'_>,
    on_event: ToolEventSink<'_>,
) -> Result<StreamOutcome, LlmError> {
    run_tool_loop_with_stop(
        client,
        executor,
        messages,
        request_id,
        on_token,
        &|_| {},
        on_event,
        &|| false,
    )
    .await
    .map(|loop_outcome| loop_outcome.outcome)
}

/// Drive one chat request through its tool rounds, observing a Stop signal
/// between rounds and before each tool dispatch (S04 T04).
///
/// Each round streams via `on_token`; when the model stops to call tools,
/// every call is announced (`ToolEvent::Call`), executed, answered
/// (`ToolEvent::Result`), and appended as the OpenAI assistant-echo +
/// tool-role turns before the follow-up request. Client errors (offline,
/// tools-unsupported, interrupted) propagate unchanged — the caller's error
/// surface already speaks [`LlmError`].
///
/// The loop polls `should_stop` at the top of every round and before dispatching
/// each tool call, so a Stop takes effect at the next round/action boundary and
/// terminates with a typed [`LoopOutcome::stopped`] — the partial text already
/// streamed, no tool calls leaking out, no further dispatch (visible, never
/// silent). Structural termination is unchanged: with a never-stopping signal
/// the loop is exactly the S01-S03 bounded loop.
pub async fn run_tool_loop_with_stop(
    client: &dyn LlmClient,
    executor: &dyn ToolExecutor,
    mut messages: Vec<ChatMessage>,
    request_id: u64,
    on_token: TokenSink<'_>,
    on_reasoning: ReasoningSink<'_>,
    on_event: ToolEventSink<'_>,
    should_stop: StopSignal<'_>,
) -> Result<LoopOutcome, LlmError> {
    // Text streamed by the most recent round — what a stop between rounds
    // surfaces so a user-stopped answer keeps whatever the model already said.
    let mut streamed_text = String::new();
    let mut streamed_tokens = 0usize;
    for round in 0..=MAX_TOOL_ROUNDS {
        // Stop observed between rounds: terminate before issuing the next
        // request, with the text streamed so far (R006 — never silent).
        if should_stop() {
            log::info!(
                "llm: tool loop stopped by user before round {round} (request={request_id})"
            );
            return Ok(LoopOutcome::stopped(streamed_text, streamed_tokens));
        }
        let tools = if round < MAX_TOOL_ROUNDS { executor.definitions() } else { Vec::new() };
        let final_round = tools.is_empty();
        let request = ChatRequest { messages: std::mem::take(&mut messages), tools };
        let outcome = client.stream_chat_reasoning(&request, on_token, on_reasoning).await?;
        messages = request.messages;
        streamed_text = outcome.text.clone();
        streamed_tokens = outcome.token_count;

        if outcome.tool_calls.is_empty() {
            if round > 0 {
                log::info!(
                    "llm: tool loop done after {round} tool round(s) (request={request_id})"
                );
            }
            return Ok(LoopOutcome::done(outcome));
        }
        if final_round {
            // The tools-stripped round still "called" a tool the request
            // never offered — terminate with the text we have rather than
            // loop; never silence (R006).
            log::warn!(
                "llm: tool call on the tools-stripped final round ignored (request={request_id})"
            );
            return Ok(LoopOutcome::done(StreamOutcome { tool_calls: Vec::new(), ..outcome }));
        }

        // First half of the OpenAI round-trip: echo the requested calls.
        messages
            .push(ChatMessage::assistant_tool_calls(outcome.text.clone(), outcome.tool_calls.clone()));

        for call in &outcome.tool_calls {
            // Stop observed mid-round: refuse to dispatch this (or any later)
            // call and terminate — a Stop must never let one more HID action
            // through (visible, never silent).
            if should_stop() {
                log::info!(
                    "llm: tool loop stopped by user mid-round {round} before dispatching {} \
                     (request={request_id})",
                    call.name
                );
                return Ok(LoopOutcome::stopped(outcome.text.clone(), outcome.token_count));
            }
            log::info!(
                "llm: tool call round={round} name={} id={} args={} (request={request_id})",
                call.name,
                call.id,
                args_summary(&call.arguments)
            );
            on_event(&ToolEvent::Call(ToolCallEvent {
                request_id,
                round,
                call: call.clone(),
            }));

            let result = executor.execute(call).await;
            match &result.failure {
                None => log::info!(
                    "llm: tool result round={round} id={} count={} mode={} (request={request_id})",
                    call.id,
                    result.result_count.unwrap_or(0),
                    result.mode.map(mode_name).unwrap_or("-"),
                ),
                Some(kind) => log::error!(
                    "llm: tool result round={round} id={} failed kind={kind}: {} (request={request_id})",
                    call.id,
                    result.content,
                ),
            }
            on_event(&ToolEvent::Result(ToolResultEvent {
                request_id,
                round,
                call_id: call.id.clone(),
                name: call.name.clone(),
                ok: result.ok,
                result_count: result.result_count,
                mode: result.mode,
                failure: result.failure,
            }));

            // Second half of the round-trip: the tool-role answer.
            messages.push(ChatMessage::tool_result(&call.id, result.content));
        }
    }
    unreachable!("the tools-stripped final round always returns")
}

/// Bounded argument excerpt for logs — arguments are model-produced and
/// unbounded; logs are not.
fn args_summary(args: &str) -> String {
    const MAX: usize = 120;
    if args.chars().count() <= MAX {
        args.to_string()
    } else {
        let cut: String = args.chars().take(MAX).collect();
        format!("{cut}…")
    }
}

fn mode_name(mode: SearchMode) -> &'static str {
    match mode {
        SearchMode::Semantic => "semantic",
        SearchMode::Keyword => "keyword",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::store::NewMemory;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use super::super::LlmHealth;

    /// Scripted client: pops one canned outcome per stream_chat call and
    /// captures every request — the runtime-free stand-in for LM Studio.
    struct ScriptedClient {
        responses: Mutex<VecDeque<Result<StreamOutcome, LlmError>>>,
        requests: Mutex<Vec<ChatRequest>>,
    }

    impl ScriptedClient {
        fn new(responses: Vec<Result<StreamOutcome, LlmError>>) -> Self {
            Self {
                responses: Mutex::new(responses.into()),
                requests: Mutex::new(Vec::new()),
            }
        }

        fn requests(&self) -> Vec<ChatRequest> {
            self.requests.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl LlmClient for ScriptedClient {
        fn endpoint(&self) -> &str {
            "http://scripted.invalid"
        }

        async fn stream_chat(
            &self,
            request: &ChatRequest,
            on_token: TokenSink<'_>,
        ) -> Result<StreamOutcome, LlmError> {
            self.requests.lock().unwrap().push(request.clone());
            let next = self
                .responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("script exhausted: loop made more requests than expected");
            if let Ok(outcome) = &next {
                if !outcome.text.is_empty() {
                    on_token(&outcome.text);
                }
            }
            next
        }

        async fn health(&self) -> LlmHealth {
            LlmHealth { online: true, endpoint: self.endpoint().into() }
        }
    }

    fn text_outcome(text: &str) -> Result<StreamOutcome, LlmError> {
        Ok(StreamOutcome { text: text.into(), token_count: 1, tool_calls: Vec::new() })
    }

    fn tool_call_outcome(calls: Vec<ToolCall>) -> Result<StreamOutcome, LlmError> {
        Ok(StreamOutcome { text: String::new(), token_count: 0, tool_calls: calls })
    }

    fn search_call(id: &str, args: &str) -> ToolCall {
        ToolCall { id: id.into(), name: MEMORY_SEARCH_TOOL.into(), arguments: args.into() }
    }

    /// Embedder that always fails offline — forces the keyword degrade so
    /// tests need no embeddings endpoint.
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

    fn seeded_tool() -> MemorySearchTool {
        let store = MemoryStore::open_in_memory().unwrap();
        store
            .insert(NewMemory {
                summary: "Debugged the tokio broadcast lag in the watcher loop".into(),
                apps: vec!["Zed".into()],
                span_start_ms: 1_000,
                span_end_ms: 2_000,
                embedding: None,
            })
            .unwrap();
        MemorySearchTool::new(Arc::new(store), Arc::new(DownEmbedder))
    }

    struct Capture {
        events: Mutex<Vec<ToolEvent>>,
        tokens: Mutex<String>,
    }

    impl Capture {
        fn new() -> Self {
            Self { events: Mutex::new(Vec::new()), tokens: Mutex::new(String::new()) }
        }

        fn events(&self) -> Vec<ToolEvent> {
            self.events.lock().unwrap().clone()
        }
    }

    async fn run(
        client: &ScriptedClient,
        executor: &dyn ToolExecutor,
        capture: &Capture,
    ) -> Result<StreamOutcome, LlmError> {
        run_tool_loop(
            client,
            executor,
            vec![ChatMessage::user("what was I working on this morning?")],
            7,
            &|t| capture.tokens.lock().unwrap().push_str(t),
            &|e| capture.events.lock().unwrap().push(e.clone()),
        )
        .await
    }

    #[tokio::test]
    async fn no_tool_calls_resolves_in_one_round_with_no_events() {
        let client = ScriptedClient::new(vec![text_outcome("plain answer")]);
        let capture = Capture::new();
        let outcome = run(&client, &seeded_tool(), &capture).await.unwrap();
        assert_eq!(outcome.text, "plain answer");
        assert!(capture.events().is_empty());
        let requests = client.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].tools.len(), 1, "first round must advertise memory_search");
        assert_eq!(*capture.tokens.lock().unwrap(), "plain answer");
    }

    #[tokio::test]
    async fn one_tool_round_executes_search_and_feeds_result_back() {
        let client = ScriptedClient::new(vec![
            tool_call_outcome(vec![search_call("call_1", r#"{"query":"broadcast lag"}"#)]),
            text_outcome("you were debugging the watcher loop"),
        ]);
        let capture = Capture::new();
        let outcome = run(&client, &seeded_tool(), &capture).await.unwrap();
        assert_eq!(outcome.text, "you were debugging the watcher loop");

        // Events: one call, one ok result carrying count + degrade mode.
        let events = capture.events();
        assert_eq!(events.len(), 2);
        let ToolEvent::Call(call) = &events[0] else { panic!("first event must be Call") };
        assert_eq!(call.request_id, 7);
        assert_eq!(call.round, 0);
        assert_eq!(call.call.name, MEMORY_SEARCH_TOOL);
        let ToolEvent::Result(result) = &events[1] else { panic!("second event must be Result") };
        assert!(result.ok);
        assert_eq!(result.call_id, "call_1");
        assert_eq!(result.result_count, Some(1));
        assert_eq!(result.mode, Some(SearchMode::Keyword));
        assert_eq!(result.failure, None);

        // The follow-up request carries the OpenAI round-trip turns and the
        // actual stored memory rides in the tool-role content.
        let requests = client.requests();
        assert_eq!(requests.len(), 2);
        let followup = &requests[1].messages;
        assert_eq!(followup.len(), 3, "user + assistant echo + tool result");
        assert_eq!(followup[1].tool_calls.len(), 1);
        assert_eq!(followup[2].tool_call_id.as_deref(), Some("call_1"));
        assert!(
            followup[2].content.contains("watcher loop"),
            "tool result must carry the stored memory: {}",
            followup[2].content
        );
        assert_eq!(requests[1].tools.len(), 1, "round 1 still advertises tools");
    }

    #[tokio::test]
    async fn loop_is_bounded_and_final_round_strips_tools() {
        // The model calls a tool every single round: the loop must terminate
        // with MAX_TOOL_ROUNDS tool rounds plus one stripped final request.
        let mut responses: Vec<Result<StreamOutcome, LlmError>> = (0..MAX_TOOL_ROUNDS)
            .map(|i| {
                tool_call_outcome(vec![search_call(
                    &format!("call_{i}"),
                    r#"{"query":"again"}"#,
                )])
            })
            .collect();
        responses.push(text_outcome("forced final answer"));
        let client = ScriptedClient::new(responses);
        let capture = Capture::new();
        let outcome = run(&client, &seeded_tool(), &capture).await.unwrap();
        assert_eq!(outcome.text, "forced final answer");

        let requests = client.requests();
        assert_eq!(requests.len(), MAX_TOOL_ROUNDS + 1);
        for req in &requests[..MAX_TOOL_ROUNDS] {
            assert_eq!(req.tools.len(), 1);
        }
        assert!(
            requests[MAX_TOOL_ROUNDS].tools.is_empty(),
            "final round must strip tools to force a text answer"
        );
        assert_eq!(capture.events().len(), MAX_TOOL_ROUNDS * 2);
    }

    #[tokio::test]
    async fn loop_runs_past_the_old_three_round_cap_until_the_model_stops() {
        // S04 T01: the agentic loop keeps issuing tools-carrying rounds while the
        // model calls tools and terminates the moment it stops — not at a fixed 3.
        // Six tool rounds (double the old S03 cap) then a text answer: the loop
        // must run all six and resolve on the model's own stop, well under the
        // safety ceiling.
        const ROUNDS: usize = 6;
        assert!(ROUNDS > 3, "must exceed the retired S03 3-round cap");
        assert!(ROUNDS < MAX_TOOL_ROUNDS, "must resolve on the model's stop, not the ceiling");
        let mut responses: Vec<Result<StreamOutcome, LlmError>> = (0..ROUNDS)
            .map(|i| tool_call_outcome(vec![search_call(&format!("call_{i}"), r#"{"query":"again"}"#)]))
            .collect();
        responses.push(text_outcome("done after six rounds"));
        let client = ScriptedClient::new(responses);
        let capture = Capture::new();
        let outcome = run(&client, &seeded_tool(), &capture).await.unwrap();
        assert_eq!(outcome.text, "done after six rounds");

        let requests = client.requests();
        assert_eq!(requests.len(), ROUNDS + 1, "six tool rounds plus the model's text answer");
        // Every issued round still carried tools — the ceiling was never reached.
        for req in &requests {
            assert_eq!(req.tools.len(), 1, "the loop stopped on the model, not the tools-strip");
        }
        assert_eq!(capture.events().len(), ROUNDS * 2, "one call + one result per round");
    }

    #[tokio::test]
    async fn tool_call_on_stripped_final_round_terminates_without_dispatch() {
        // Defensive bound: even if the model "calls" a tool when none were
        // offered, the loop ends — no dispatch, no extra request.
        let mut responses: Vec<Result<StreamOutcome, LlmError>> = (0..MAX_TOOL_ROUNDS)
            .map(|i| {
                tool_call_outcome(vec![search_call(&format!("call_{i}"), r#"{"query":"q"}"#)])
            })
            .collect();
        responses.push(tool_call_outcome(vec![search_call("call_zombie", r#"{"query":"q"}"#)]));
        let client = ScriptedClient::new(responses);
        let capture = Capture::new();
        let outcome = run(&client, &seeded_tool(), &capture).await.unwrap();
        assert!(outcome.tool_calls.is_empty(), "zombie calls must not leak out of the loop");
        assert_eq!(client.requests().len(), MAX_TOOL_ROUNDS + 1);
        assert_eq!(
            capture.events().len(),
            MAX_TOOL_ROUNDS * 2,
            "the undispatched zombie call must produce no events"
        );
    }

    #[tokio::test]
    async fn stop_signal_mid_loop_terminates_before_next_round_without_further_dispatch() {
        use std::sync::atomic::{AtomicBool, Ordering};

        // An executor that runs the memory search once, then trips the stop flag
        // — modelling the user hitting Stop after the first tool round lands.
        struct StopAfterExecute {
            inner: MemorySearchTool,
            stop: Arc<AtomicBool>,
        }
        #[async_trait]
        impl ToolExecutor for StopAfterExecute {
            fn definitions(&self) -> Vec<ToolDefinition> {
                self.inner.definitions()
            }
            async fn execute(&self, call: &ToolCall) -> ToolOutcome {
                let outcome = self.inner.execute(call).await;
                self.stop.store(true, Ordering::SeqCst);
                outcome
            }
        }

        let stop = Arc::new(AtomicBool::new(false));
        let executor = StopAfterExecute { inner: seeded_tool(), stop: stop.clone() };

        // The model would call a tool every round; the loop must stop after the
        // first dispatch trips the flag, never issuing the round-1 request.
        let client = ScriptedClient::new(vec![
            tool_call_outcome(vec![search_call("call_0", r#"{"query":"again"}"#)]),
            tool_call_outcome(vec![search_call("call_1", r#"{"query":"again"}"#)]),
            text_outcome("must never be reached"),
        ]);
        let capture = Capture::new();
        let should_stop = || stop.load(Ordering::SeqCst);
        let loop_outcome = run_tool_loop_with_stop(
            &client,
            &executor,
            vec![ChatMessage::user("do a long task")],
            7,
            &|t| capture.tokens.lock().unwrap().push_str(t),
            &|_| {},
            &|e| capture.events.lock().unwrap().push(e.clone()),
            &should_stop,
        )
        .await
        .unwrap();

        assert!(loop_outcome.stopped, "a mid-loop stop must surface a typed stopped outcome");
        assert!(
            loop_outcome.outcome.tool_calls.is_empty(),
            "a stopped run must not leak tool calls",
        );
        // Only round 0 was issued: the loop stopped at the top of round 1,
        // before the second request and before any round-1 dispatch.
        assert_eq!(client.requests().len(), 1, "no request may be issued after the stop");
        assert_eq!(capture.events().len(), 2, "no tool dispatch past the stop (round 0 only)");
    }

    #[tokio::test]
    async fn no_stop_signal_leaves_the_loop_exactly_bounded() {
        // With a never-stopping signal the with-stop loop is the S01-S03 loop:
        // a normal one-round search resolves un-stopped.
        let client = ScriptedClient::new(vec![
            tool_call_outcome(vec![search_call("call_1", r#"{"query":"broadcast lag"}"#)]),
            text_outcome("answered"),
        ]);
        let capture = Capture::new();
        let loop_outcome = run_tool_loop_with_stop(
            &client,
            &seeded_tool(),
            vec![ChatMessage::user("what was I working on?")],
            7,
            &|t| capture.tokens.lock().unwrap().push_str(t),
            &|_| {},
            &|e| capture.events.lock().unwrap().push(e.clone()),
            &|| false,
        )
        .await
        .unwrap();
        assert!(!loop_outcome.stopped, "a natural finish is never flagged stopped");
        assert_eq!(loop_outcome.outcome.text, "answered");
    }

    #[tokio::test]
    async fn client_errors_propagate_unchanged() {
        let client = ScriptedClient::new(vec![Err(LlmError::ToolsUnsupported {
            endpoint: "http://scripted.invalid".into(),
            detail: "model does not support tools".into(),
        })]);
        let capture = Capture::new();
        let err = run(&client, &seeded_tool(), &capture).await.unwrap_err();
        assert_eq!(err.kind(), "tools-unsupported");
        assert!(capture.events().is_empty());
    }

    #[tokio::test]
    async fn malformed_arguments_feed_typed_error_to_model_and_ui() {
        let client = ScriptedClient::new(vec![
            tool_call_outcome(vec![search_call("call_1", "{not json")]),
            text_outcome("answered without memory"),
        ]);
        let capture = Capture::new();
        let outcome = run(&client, &seeded_tool(), &capture).await.unwrap();
        assert_eq!(outcome.text, "answered without memory", "loop must survive bad arguments");

        let ToolEvent::Result(result) = &capture.events()[1] else { panic!("expected Result") };
        assert!(!result.ok);
        assert_eq!(result.failure.as_deref(), Some("invalid-arguments"));
        assert_eq!(result.result_count, None);

        // The model sees a structured error payload, not silence.
        let followup = &client.requests()[1].messages;
        let body: serde_json::Value = serde_json::from_str(&followup[2].content).unwrap();
        assert!(body["error"].as_str().unwrap().contains("invalid memory_search arguments"));
    }

    #[tokio::test]
    async fn unknown_tool_name_is_a_typed_failure() {
        let client = ScriptedClient::new(vec![
            tool_call_outcome(vec![ToolCall {
                id: "call_1".into(),
                name: "delete_everything".into(),
                arguments: "{}".into(),
            }]),
            text_outcome("done"),
        ]);
        let capture = Capture::new();
        run(&client, &seeded_tool(), &capture).await.unwrap();
        let ToolEvent::Result(result) = &capture.events()[1] else { panic!("expected Result") };
        assert!(!result.ok);
        assert_eq!(result.failure.as_deref(), Some("unknown-tool"));
        let followup = &client.requests()[1].messages;
        assert!(followup[2].content.contains("delete_everything"));
    }

    #[tokio::test]
    async fn parallel_calls_in_one_round_each_get_result_turns_in_order() {
        let client = ScriptedClient::new(vec![
            tool_call_outcome(vec![
                search_call("call_a", r#"{"query":"alpha"}"#),
                search_call("call_b", r#"{"query":"beta"}"#),
            ]),
            text_outcome("combined answer"),
        ]);
        let capture = Capture::new();
        run(&client, &seeded_tool(), &capture).await.unwrap();

        let events = capture.events();
        assert_eq!(events.len(), 4, "call+result per requested call");
        let followup = &client.requests()[1].messages;
        // user + one assistant echo (both calls) + two tool results.
        assert_eq!(followup.len(), 4);
        assert_eq!(followup[1].tool_calls.len(), 2);
        assert_eq!(followup[2].tool_call_id.as_deref(), Some("call_a"));
        assert_eq!(followup[3].tool_call_id.as_deref(), Some("call_b"));
    }

    #[tokio::test]
    async fn memory_search_limit_is_clamped_like_the_ipc_command() {
        let tool = seeded_tool();
        // A hostile limit does not error and does not exceed the ceiling.
        let outcome = tool
            .execute(&search_call("c", r#"{"query":"watcher","limit":10000}"#))
            .await;
        assert!(outcome.ok);
        assert!(outcome.result_count.unwrap() <= MAX_SEARCH_LIMIT);

        // limit 0 clamps up to 1 rather than searching for nothing.
        let outcome = tool.execute(&search_call("c", r#"{"query":"watcher","limit":0}"#)).await;
        assert!(outcome.ok);
    }

    #[tokio::test]
    async fn memory_search_content_is_the_search_outcome_json() {
        let outcome = seeded_tool()
            .execute(&search_call("c", r#"{"query":"broadcast lag"}"#))
            .await;
        let v: serde_json::Value = serde_json::from_str(&outcome.content).unwrap();
        assert_eq!(v["mode"], "keyword");
        assert_eq!(v["results"][0]["summary"], "Debugged the tokio broadcast lag in the watcher loop");
        assert!(
            v["results"][0].get("embedding").is_none(),
            "embeddings must never ride to the model"
        );
    }

    #[test]
    fn definition_is_the_openai_function_envelope() {
        let def = MemorySearchTool::definition();
        assert_eq!(def.name, MEMORY_SEARCH_TOOL);
        let v = serde_json::to_value(&def).unwrap();
        assert_eq!(v["type"], "function");
        assert_eq!(v["function"]["name"], "memory_search");
        assert_eq!(v["function"]["parameters"]["required"][0], "query");
    }

    #[test]
    fn event_names_are_the_ipc_contract() {
        assert_eq!(TOOL_CALL_EVENT, "llm://tool-call");
        assert_eq!(TOOL_RESULT_EVENT, "llm://tool-result");
    }

    #[test]
    fn tool_events_serialize_camel_case() {
        let call = ToolCallEvent {
            request_id: 7,
            round: 0,
            call: search_call("call_1", r#"{"query":"x"}"#),
        };
        let v = serde_json::to_value(&call).unwrap();
        assert_eq!(v["requestId"], 7);
        assert_eq!(v["round"], 0);
        assert_eq!(v["call"]["id"], "call_1");
        assert_eq!(v["call"]["name"], "memory_search");

        let result = ToolResultEvent {
            request_id: 7,
            round: 0,
            call_id: "call_1".into(),
            name: "memory_search".into(),
            ok: true,
            result_count: Some(3),
            mode: Some(SearchMode::Semantic),
            failure: None,
        };
        let v = serde_json::to_value(&result).unwrap();
        assert_eq!(v["requestId"], 7);
        assert_eq!(v["callId"], "call_1");
        assert_eq!(v["name"], "memory_search");
        assert_eq!(v["ok"], true);
        assert_eq!(v["resultCount"], 3);
        assert_eq!(v["mode"], "semantic");
        assert!(v["failure"].is_null());
    }

    #[test]
    fn args_summary_bounds_unbounded_model_output() {
        assert_eq!(args_summary("{}"), "{}");
        let long = "x".repeat(500);
        let summary = args_summary(&long);
        assert!(summary.chars().count() <= 121);
        assert!(summary.ends_with('…'));
    }

    // --- InputTool + CompositeExecutor (M005 S01/T05) --------------------

    use crate::input::commands::HidArmState;
    use crate::input::fallback::FallbackInput;
    use crate::input::{InputAction, InputControl, InputError, InputPermission, MouseButton};

    /// Records the last performed action so delegation through the tool +
    /// composite can be asserted without touching real HID.
    struct RecordingInput {
        last: Mutex<Option<InputAction>>,
    }

    impl RecordingInput {
        fn new() -> Self {
            Self { last: Mutex::new(None) }
        }
    }

    #[async_trait]
    impl InputControl for RecordingInput {
        fn permission(&self) -> InputPermission {
            InputPermission { granted: true, supported: true }
        }

        fn request_permission(&self) -> bool {
            true
        }

        async fn perform(&self, action: InputAction) -> Result<(), InputError> {
            *self.last.lock().unwrap() = Some(action);
            Ok(())
        }
    }

    fn input_call(id: &str, args: &str) -> ToolCall {
        ToolCall { id: id.into(), name: INPUT_ACTION_TOOL.into(), arguments: args.into() }
    }

    /// An armed arm-state — the default posture for the delegation tests below.
    /// The structural-gate tests build their own disarmed holder.
    fn armed_arm() -> Arc<HidArmState> {
        Arc::new(HidArmState::new(true))
    }

    #[test]
    fn input_definition_is_the_openai_function_envelope() {
        let def = InputTool::definition();
        assert_eq!(def.name, INPUT_ACTION_TOOL);
        let v = serde_json::to_value(&def).unwrap();
        assert_eq!(v["type"], "function");
        assert_eq!(v["function"]["name"], "input_action");
        assert_eq!(v["function"]["parameters"]["required"][0], "action");
    }

    #[tokio::test]
    async fn input_tool_performs_a_valid_action_and_reports_ok() {
        let backend = Arc::new(RecordingInput::new());
        let tool = InputTool::new(backend.clone(), armed_arm());
        let outcome = tool
            .execute(&input_call("c1", r#"{"action":"mouse-click","button":"right"}"#))
            .await;
        assert!(outcome.ok);
        assert_eq!(outcome.failure, None);
        // The action really reached the backend.
        assert_eq!(
            *backend.last.lock().unwrap(),
            Some(InputAction::click(MouseButton::Right)),
        );
        // The model sees a structured confirmation echoing what was synthesized.
        let v: serde_json::Value = serde_json::from_str(&outcome.content).unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["performed"]["action"], "mouse-click");
        assert_eq!(v["performed"]["button"], "right");
    }

    #[tokio::test]
    async fn input_tool_malformed_arguments_are_typed_invalid_arguments() {
        let tool = InputTool::new(Arc::new(RecordingInput::new()), armed_arm());
        // Unknown action tag: serde rejects it before any HID is touched.
        let outcome = tool.execute(&input_call("c1", r#"{"action":"self-destruct"}"#)).await;
        assert!(!outcome.ok);
        assert_eq!(outcome.failure.as_deref(), Some("invalid-arguments"));
        let outcome = tool.execute(&input_call("c1", "{not json")).await;
        assert_eq!(outcome.failure.as_deref(), Some("invalid-arguments"));
    }

    #[tokio::test]
    async fn input_tool_wrong_name_is_unknown_tool() {
        let tool = InputTool::new(Arc::new(RecordingInput::new()), armed_arm());
        let outcome = tool
            .execute(&ToolCall {
                id: "c1".into(),
                name: "memory_search".into(),
                arguments: "{}".into(),
            })
            .await;
        assert!(!outcome.ok);
        assert_eq!(outcome.failure.as_deref(), Some("unknown-tool"));
    }

    #[tokio::test]
    async fn input_tool_propagates_typed_backend_error_kind() {
        // FallbackInput returns the typed `unsupported` error on every platform;
        // its kind must ride back to the model/UI unchanged (R007).
        let tool = InputTool::new(Arc::new(FallbackInput), armed_arm());
        let outcome =
            tool.execute(&input_call("c1", r#"{"action":"type-text","text":"hi"}"#)).await;
        assert!(!outcome.ok);
        assert_eq!(outcome.failure.as_deref(), Some("unsupported"));
        // The detail rides in the content so the model can explain to the user.
        assert!(outcome.content.contains("error"));
    }

    // --- Structural gate (M005 S03/T02, D038) ----------------------------

    #[test]
    fn disarmed_input_tool_advertises_no_definitions() {
        // Structural gate (D038): a disarmed tool contributes zero definitions,
        // so the CompositeExecutor never offers input_action to the model.
        let arm = Arc::new(HidArmState::disarmed());
        let tool = InputTool::new(Arc::new(RecordingInput::new()), arm.clone());
        assert!(tool.definitions().is_empty(), "disarmed tool must advertise nothing");
        // Arming the shared holder flips the advertised set live — no re-mount.
        arm.set_armed(true);
        assert_eq!(
            tool.definitions().len(),
            1,
            "arming makes the tool advertise input_action live via the shared handle"
        );
    }

    #[tokio::test]
    async fn disarmed_input_execute_refuses_with_disabled_before_touching_backend() {
        // The core safety requirement: a disarmed execute() is refused with the
        // typed `disabled` error and the InputControl backend is never touched.
        let backend = Arc::new(RecordingInput::new());
        let tool = InputTool::new(backend.clone(), Arc::new(HidArmState::disarmed()));
        let outcome = tool
            .execute(&input_call("c1", r#"{"action":"mouse-click","button":"left"}"#))
            .await;
        assert!(!outcome.ok);
        assert_eq!(outcome.failure.as_deref(), Some("disabled"));
        // Structural inertness, not a UI hint: nothing reached the backend.
        assert!(
            backend.last.lock().unwrap().is_none(),
            "disarmed execute must refuse BEFORE the backend is touched"
        );
        // The refusal is a visible, typed tool result (R007), never silence.
        let v: serde_json::Value = serde_json::from_str(&outcome.content).unwrap();
        assert!(v["error"].as_str().unwrap().contains("Settings"));
    }

    #[test]
    fn composite_omits_input_action_when_disarmed() {
        // The exact production mount with HID disarmed: the composite advertises
        // only memory_search + screen_query — input_action is withheld entirely.
        let composite = CompositeExecutor::new(vec![
            Box::new(seeded_tool()),
            Box::new(InputTool::new(
                Arc::new(RecordingInput::new()),
                Arc::new(HidArmState::disarmed()),
            )),
            Box::new(ScreenQueryTool::new(Arc::new(ScriptedScreen::ok()), Arc::new(ScreenSeen::new()), Arc::new(FocusedApp::new()))),
        ]);
        let names: Vec<String> =
            composite.definitions().into_iter().map(|d| d.name).collect();
        assert_eq!(names, vec![MEMORY_SEARCH_TOOL, SCREEN_QUERY_TOOL]);
        assert!(
            !names.contains(&INPUT_ACTION_TOOL.to_string()),
            "disarmed HID must not be advertised to the model"
        );
    }

    #[tokio::test]
    async fn composite_disarmed_input_call_is_unknown_tool_and_never_dispatched() {
        // With HID disarmed the tool is unadvertised, so a stray input_action
        // call routes to nobody: the composite returns unknown-tool and the HID
        // backend is never reached.
        let backend = Arc::new(RecordingInput::new());
        let composite = CompositeExecutor::new(vec![Box::new(InputTool::new(
            backend.clone(),
            Arc::new(HidArmState::disarmed()),
        ))]);
        let outcome = composite
            .execute(&input_call("c1", r#"{"action":"mouse-move","x":1,"y":2}"#))
            .await;
        assert!(!outcome.ok);
        assert_eq!(outcome.failure.as_deref(), Some("unknown-tool"));
        assert!(backend.last.lock().unwrap().is_none(), "disarmed HID backend must stay untouched");
    }

    #[test]
    fn composite_concatenates_every_sub_executor_definition() {
        let composite = CompositeExecutor::new(vec![
            Box::new(seeded_tool()),
            Box::new(InputTool::new(Arc::new(RecordingInput::new()), armed_arm())),
        ]);
        let names: Vec<String> = composite.definitions().into_iter().map(|d| d.name).collect();
        assert_eq!(names, vec![MEMORY_SEARCH_TOOL, INPUT_ACTION_TOOL]);
    }

    #[tokio::test]
    async fn composite_routes_each_call_to_its_owner() {
        let backend = Arc::new(RecordingInput::new());
        let composite = CompositeExecutor::new(vec![
            Box::new(seeded_tool()),
            Box::new(InputTool::new(backend.clone(), armed_arm())),
        ]);

        // memory_search dispatches to the memory tool, unchanged.
        let mem = composite.execute(&search_call("c1", r#"{"query":"broadcast lag"}"#)).await;
        assert!(mem.ok);
        assert_eq!(mem.result_count, Some(1));

        // input_action dispatches to the input tool and reaches its backend.
        let hid = composite
            .execute(&input_call("c2", r#"{"action":"mouse-move","x":5,"y":6}"#))
            .await;
        assert!(hid.ok);
        assert_eq!(
            *backend.last.lock().unwrap(),
            Some(InputAction::MouseMove { x: 5, y: 6 }),
        );
    }

    #[tokio::test]
    async fn composite_unknown_tool_is_typed_and_lists_available_tools() {
        let composite = CompositeExecutor::new(vec![
            Box::new(seeded_tool()),
            Box::new(InputTool::new(Arc::new(RecordingInput::new()), armed_arm())),
        ]);
        let outcome = composite
            .execute(&ToolCall {
                id: "c1".into(),
                name: "delete_everything".into(),
                arguments: "{}".into(),
            })
            .await;
        assert!(!outcome.ok);
        assert_eq!(outcome.failure.as_deref(), Some("unknown-tool"));
        // The failure names both mounted tools so the model can retry correctly.
        assert!(outcome.content.contains(MEMORY_SEARCH_TOOL));
        assert!(outcome.content.contains(INPUT_ACTION_TOOL));
    }

    // --- ScreenQueryTool + CompositeExecutor (M005 S02/T03) --------------

    use crate::screenquery::{ScreenElement, ScreenQuery, ScreenQueryError};

    /// Scripted screen-query backend: returns a fixed element set or a typed
    /// failure so the tool's ok/typed-failure paths can be asserted without
    /// touching the real screen.
    struct ScriptedScreen {
        result: Result<Vec<ScreenElement>, ScreenQueryError>,
    }

    impl ScriptedScreen {
        fn ok() -> Self {
            Self {
                result: Ok(vec![ScreenElement {
                    text: "Submit".into(),
                    x: 100,
                    y: 200,
                    width: 60,
                    height: 24,
                    app: None,
                }]),
            }
        }

        fn failing(err: ScreenQueryError) -> Self {
            Self { result: Err(err) }
        }

        /// A fixture whose elements carry the given `(text, app)` attributions —
        /// for the focused-app filtering tests. Coordinates are irrelevant here,
        /// so they are stubbed uniformly.
        fn with_apps(items: &[(&str, Option<&str>)]) -> Self {
            Self {
                result: Ok(items
                    .iter()
                    .map(|(text, app)| ScreenElement {
                        text: (*text).into(),
                        x: 1,
                        y: 2,
                        width: 3,
                        height: 4,
                        app: app.map(str::to_owned),
                    })
                    .collect()),
            }
        }
    }

    #[async_trait]
    impl ScreenQuery for ScriptedScreen {
        async fn query(&self) -> Result<Vec<ScreenElement>, ScreenQueryError> {
            self.result.clone()
        }
    }

    fn screen_call(id: &str) -> ToolCall {
        ToolCall { id: id.into(), name: SCREEN_QUERY_TOOL.into(), arguments: "{}".into() }
    }

    #[test]
    fn screen_query_definition_is_the_openai_function_envelope() {
        let def = ScreenQueryTool::definition();
        assert_eq!(def.name, SCREEN_QUERY_TOOL);
        let v = serde_json::to_value(&def).unwrap();
        assert_eq!(v["type"], "function");
        assert_eq!(v["function"]["name"], "screen_query");
        // No required arguments — the model can call it with an empty object.
        assert_eq!(v["function"]["parameters"]["required"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn screen_query_ok_returns_element_json_with_coordinates() {
        let tool = ScreenQueryTool::new(Arc::new(ScriptedScreen::ok()), Arc::new(ScreenSeen::new()), Arc::new(FocusedApp::new()));
        let outcome = tool.execute(&screen_call("c1")).await;
        assert!(outcome.ok);
        assert_eq!(outcome.failure, None);
        assert_eq!(outcome.result_count, Some(1));
        // The content is the JSON array of elements the model reads to aim a
        // click — x/y/width/height ride to the model, camelCase.
        let v: serde_json::Value = serde_json::from_str(&outcome.content).unwrap();
        assert_eq!(v[0]["text"], "Submit");
        assert_eq!(v[0]["x"], 100);
        assert_eq!(v[0]["y"], 200);
        assert_eq!(v[0]["width"], 60);
        assert_eq!(v[0]["height"], 24);
    }

    #[test]
    fn focused_app_filter_is_identity_before_any_focus() {
        // Before any focus_app the model needs the full screen survey to decide
        // what to focus, so filter() with None returns everything untouched.
        let focused = FocusedApp::new();
        let els = vec![
            ScreenElement { text: "a".into(), x: 0, y: 0, width: 1, height: 1, app: Some("Chrome".into()) },
            ScreenElement { text: "b".into(), x: 0, y: 0, width: 1, height: 1, app: None },
        ];
        assert_eq!(focused.filter(els.clone()), els);
    }

    #[test]
    fn focused_app_filter_keeps_only_the_focused_app_and_drops_unattributed() {
        // Once an app is focused, only its elements survive — case-insensitively
        // on the localized name — and unattributed (app=None) desktop/menu-bar
        // chrome is dropped so the model can never aim a click at the wallpaper.
        let focused = FocusedApp::new();
        focused.set("Google Chrome");
        let kept = focused.filter(vec![
            ScreenElement { text: "addr".into(), x: 0, y: 0, width: 1, height: 1, app: Some("google chrome".into()) },
            ScreenElement { text: "other".into(), x: 0, y: 0, width: 1, height: 1, app: Some("Finder".into()) },
            ScreenElement { text: "desktop".into(), x: 0, y: 0, width: 1, height: 1, app: None },
        ]);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].text, "addr");
    }

    #[tokio::test]
    async fn screen_query_filters_results_to_the_focused_app() {
        // End-to-end through the tool: with a focused app set, the JSON handed to
        // the model contains ONLY that app's elements — the structural guarantee
        // that the model can't click the desktop (M005). The screen_seen flag is
        // still marked so a subsequent aimed click is not blocked.
        let seen = Arc::new(ScreenSeen::new());
        let focused = Arc::new(FocusedApp::new());
        focused.set("Google Chrome");
        let tool = ScreenQueryTool::new(
            Arc::new(ScriptedScreen::with_apps(&[
                ("address bar", Some("Google Chrome")),
                ("dock item", Some("Dock")),
                ("wallpaper text", None),
            ])),
            seen.clone(),
            focused,
        );
        let outcome = tool.execute(&screen_call("c1")).await;
        assert!(outcome.ok);
        assert!(seen.seen(), "a successful query must still ground coordinates");
        assert_eq!(outcome.result_count, Some(1), "only the Chrome element survives");
        let v: serde_json::Value = serde_json::from_str(&outcome.content).unwrap();
        assert_eq!(v.as_array().unwrap().len(), 1);
        assert_eq!(v[0]["text"], "address bar");
        assert_eq!(v[0]["app"], "Google Chrome");
    }

    #[tokio::test]
    async fn screen_query_returns_all_apps_before_any_focus() {
        // With no focus set, the pre-focus survey returns every element so the
        // model can choose which app to focus.
        let tool = ScreenQueryTool::new(
            Arc::new(ScriptedScreen::with_apps(&[
                ("a", Some("Google Chrome")),
                ("b", Some("Finder")),
                ("c", None),
            ])),
            Arc::new(ScreenSeen::new()),
            Arc::new(FocusedApp::new()),
        );
        let outcome = tool.execute(&screen_call("c1")).await;
        assert!(outcome.ok);
        assert_eq!(outcome.result_count, Some(3));
    }

    #[tokio::test]
    async fn screen_query_typed_failure_rides_the_kind_back() {
        // A backend permission failure surfaces as an ok:false outcome carrying
        // the screen-query kind — the UI's walkthrough keys on it (R007).
        let tool = ScreenQueryTool::new(
            Arc::new(ScriptedScreen::failing(ScreenQueryError::PermissionDenied {
                detail: "TCC denied".into(),
            })),
            Arc::new(ScreenSeen::new()),
            Arc::new(FocusedApp::new()),
        );
        let outcome = tool.execute(&screen_call("c1")).await;
        assert!(!outcome.ok);
        assert_eq!(outcome.failure.as_deref(), Some("permission-denied"));
        assert!(outcome.content.contains("error"));

        // The unsupported class (fallback platform) rides its own kind too.
        let tool = ScreenQueryTool::new(
            Arc::new(ScriptedScreen::failing(ScreenQueryError::unsupported_here())),
            Arc::new(ScreenSeen::new()),
            Arc::new(FocusedApp::new()),
        );
        let outcome = tool.execute(&screen_call("c2")).await;
        assert!(!outcome.ok);
        assert_eq!(outcome.failure.as_deref(), Some("unsupported"));
    }

    #[tokio::test]
    async fn screen_query_wrong_name_is_unknown_tool() {
        let tool = ScreenQueryTool::new(Arc::new(ScriptedScreen::ok()), Arc::new(ScreenSeen::new()), Arc::new(FocusedApp::new()));
        let outcome = tool
            .execute(&ToolCall {
                id: "c1".into(),
                name: "memory_search".into(),
                arguments: "{}".into(),
            })
            .await;
        assert!(!outcome.ok);
        assert_eq!(outcome.failure.as_deref(), Some("unknown-tool"));
    }

    #[tokio::test]
    async fn composite_routes_screen_query_to_its_owner() {
        // The exact production mount shape: memory_search + input_action +
        // screen_query, dispatched by name.
        let composite = CompositeExecutor::new(vec![
            Box::new(seeded_tool()),
            Box::new(InputTool::new(Arc::new(RecordingInput::new()), armed_arm())),
            Box::new(ScreenQueryTool::new(Arc::new(ScriptedScreen::ok()), Arc::new(ScreenSeen::new()), Arc::new(FocusedApp::new()))),
        ]);
        let names: Vec<String> =
            composite.definitions().into_iter().map(|d| d.name).collect();
        assert_eq!(names, vec![MEMORY_SEARCH_TOOL, INPUT_ACTION_TOOL, SCREEN_QUERY_TOOL]);

        // A screen_query call routes to the screen tool and returns its elements.
        let outcome = composite.execute(&screen_call("c1")).await;
        assert!(outcome.ok);
        assert_eq!(outcome.result_count, Some(1));
        let v: serde_json::Value = serde_json::from_str(&outcome.content).unwrap();
        assert_eq!(v[0]["x"], 100);
        assert_eq!(v[0]["y"], 200);
    }

    // --- ApprovalGate (M005 S04/T03) -------------------------------------

    /// Scripted approval prompt: pops a queued verdict per request and records
    /// the (kind, summary) it was asked to approve — the Tauri-free stand-in for
    /// the overlay round-trip. An exhausted queue panics loudly so a test that
    /// prompts more than it scripted fails visibly.
    struct ScriptedApprover {
        verdicts: Mutex<VecDeque<ApprovalVerdict>>,
        requests: Mutex<Vec<(ActionKind, String)>>,
    }

    impl ScriptedApprover {
        fn new(verdicts: Vec<ApprovalVerdict>) -> Self {
            Self { verdicts: Mutex::new(verdicts.into()), requests: Mutex::new(Vec::new()) }
        }

        fn prompt_count(&self) -> usize {
            self.requests.lock().unwrap().len()
        }

        fn last_summary(&self) -> Option<String> {
            self.requests.lock().unwrap().last().map(|(_, s)| s.clone())
        }
    }

    #[async_trait]
    impl ApprovalPrompt for ScriptedApprover {
        async fn request(&self, kind: ActionKind, summary: String) -> ApprovalVerdict {
            self.requests.lock().unwrap().push((kind, summary));
            self.verdicts
                .lock()
                .unwrap()
                .pop_front()
                .expect("approver script exhausted: the gate prompted more than expected")
        }
    }

    /// Records the last focused app name so gating of focus_app through the
    /// ApprovalGate can be asserted without touching a real workspace. Any name
    /// is treated as a match (returns it verbatim).
    struct RecordingFocus {
        last: Mutex<Option<String>>,
    }

    impl RecordingFocus {
        fn new() -> Self {
            Self { last: Mutex::new(None) }
        }
    }

    #[async_trait]
    impl AppFocus for RecordingFocus {
        async fn focus(&self, app_name: &str) -> Result<crate::appfocus::FocusedApp, AppFocusError> {
            *self.last.lock().unwrap() = Some(app_name.to_string());
            Ok(crate::appfocus::FocusedApp { app: app_name.to_string(), launched: false })
        }

        async fn running_apps(&self) -> Vec<String> {
            vec!["Google Chrome".into(), "Finder".into()]
        }
    }

    fn focus_call(id: &str, args: &str) -> ToolCall {
        ToolCall { id: id.into(), name: FOCUS_APP_TOOL.into(), arguments: args.into() }
    }

    /// A gate over a recording backend, its inner tool armed (so a Perform truly
    /// reaches HID), plus the shared session whitelist for post-hoc assertions.
    /// The app-focus surface is wired to a scripted no-op backend so the input
    /// path assertions are unaffected.
    fn gate_over(
        mode: HidRunMode,
        backend: Arc<RecordingInput>,
        approver: Arc<ScriptedApprover>,
    ) -> (ApprovalGate, Arc<std::sync::Mutex<SessionWhitelist>>) {
        let whitelist = Arc::new(std::sync::Mutex::new(SessionWhitelist::new()));
        let inner = InputTool::new(backend, armed_arm());
        let focus = FocusAppTool::new(Arc::new(RecordingFocus::new()));
        // Mark the screen already seen — with a screen-spanning box so any aimed
        // coordinate these approval-path tests use lands on-target — so they
        // exercise the Off/Ask/AutoRun gate directly; the targeting guards
        // (no-screen-query, off-target) have their own dedicated tests.
        let screen_seen = Arc::new(ScreenSeen::new());
        screen_seen.mark_seen(vec![SeenBox { x: 0, y: 0, width: 100_000, height: 100_000 }]);
        (
            ApprovalGate::new(
                inner,
                focus,
                mode,
                whitelist.clone(),
                approver,
                screen_seen,
                Arc::new(FocusedApp::new()),
            ),
            whitelist,
        )
    }

    #[tokio::test]
    async fn gate_off_refuses_with_disabled_before_touching_backend_or_prompting() {
        // Off is structurally inert (D038): the gate refuses BEFORE parsing,
        // never prompts, and never touches the backend — even with an armed inner
        // tool, the mode gate wins.
        let backend = Arc::new(RecordingInput::new());
        let approver = Arc::new(ScriptedApprover::new(vec![]));
        let (gate, _wl) = gate_over(HidRunMode::Off, backend.clone(), approver.clone());
        let outcome = gate
            .execute(&input_call("c1", r#"{"action":"mouse-click","button":"left"}"#))
            .await;
        assert!(!outcome.ok);
        assert_eq!(outcome.failure.as_deref(), Some("disabled"));
        assert!(backend.last.lock().unwrap().is_none(), "Off must not reach the backend");
        assert_eq!(approver.prompt_count(), 0, "Off must never prompt");
    }

    #[tokio::test]
    async fn gate_auto_run_performs_without_prompting() {
        // Auto-run performs every action straight through — no prompt, no
        // whitelist consult.
        let backend = Arc::new(RecordingInput::new());
        let approver = Arc::new(ScriptedApprover::new(vec![]));
        let (gate, _wl) = gate_over(HidRunMode::AutoRun, backend.clone(), approver.clone());
        let outcome = gate
            .execute(&input_call("c1", r#"{"action":"mouse-move","x":5,"y":6}"#))
            .await;
        assert!(outcome.ok);
        assert_eq!(approver.prompt_count(), 0, "Auto-run must never prompt");
        assert_eq!(
            *backend.last.lock().unwrap(),
            Some(InputAction::MouseMove { x: 5, y: 6 }),
            "Auto-run must perform the action",
        );
    }

    #[tokio::test]
    async fn gate_ask_deny_never_reaches_the_backend() {
        // Ask + new kind prompts; a Deny returns the typed approval-denied result
        // and never touches HID.
        let backend = Arc::new(RecordingInput::new());
        let approver = Arc::new(ScriptedApprover::new(vec![ApprovalVerdict::Deny]));
        let (gate, _wl) = gate_over(HidRunMode::Ask, backend.clone(), approver.clone());
        let outcome = gate
            .execute(&input_call("c1", r#"{"action":"mouse-click","button":"left"}"#))
            .await;
        assert!(!outcome.ok);
        assert_eq!(outcome.failure.as_deref(), Some(APPROVAL_DENIED_KIND));
        assert!(backend.last.lock().unwrap().is_none(), "Deny must not reach the backend");
        assert_eq!(approver.prompt_count(), 1, "Ask + new kind must prompt exactly once");
        // The overlay saw a human summary naming the action.
        assert!(approver.last_summary().unwrap().contains("Click"));
    }

    #[tokio::test]
    async fn gate_ask_allow_once_performs_but_prompts_again_next_time() {
        // Allow-once performs this action without whitelisting the kind, so the
        // same kind prompts again on the next request.
        let backend = Arc::new(RecordingInput::new());
        let approver = Arc::new(ScriptedApprover::new(vec![
            ApprovalVerdict::AllowOnce,
            ApprovalVerdict::AllowOnce,
        ]));
        let (gate, wl) = gate_over(HidRunMode::Ask, backend.clone(), approver.clone());

        let first = gate
            .execute(&input_call("c1", r#"{"action":"mouse-click","button":"left"}"#))
            .await;
        assert!(first.ok);
        assert_eq!(approver.prompt_count(), 1);

        let second = gate
            .execute(&input_call("c2", r#"{"action":"mouse-click","button":"left"}"#))
            .await;
        assert!(second.ok);
        assert_eq!(approver.prompt_count(), 2, "allow-once must prompt again for the same kind");
        assert!(wl.lock().unwrap().is_empty(), "allow-once must not whitelist the kind");
    }

    #[tokio::test]
    async fn gate_ask_allow_kind_suppresses_the_second_prompt() {
        // "Always allow this kind" performs AND whitelists, so the second action
        // of that kind performs without prompting (the queue has only one verdict;
        // a second prompt would panic on the exhausted script).
        let backend = Arc::new(RecordingInput::new());
        let approver = Arc::new(ScriptedApprover::new(vec![ApprovalVerdict::AllowKind]));
        let (gate, wl) = gate_over(HidRunMode::Ask, backend.clone(), approver.clone());

        let first = gate
            .execute(&input_call("c1", r#"{"action":"key-press","key":"return"}"#))
            .await;
        assert!(first.ok);
        assert_eq!(approver.prompt_count(), 1);
        assert!(wl.lock().unwrap().contains(ActionKind::KeyPress), "allow-kind must whitelist");

        let second = gate
            .execute(&input_call("c2", r#"{"action":"key-press","key":"tab"}"#))
            .await;
        assert!(second.ok, "a whitelisted kind must perform without prompting");
        assert_eq!(approver.prompt_count(), 1, "the whitelisted kind must not prompt again");
        // A different kind still prompts (by-kind, not blanket) — but the script
        // is exhausted, so we assert via the whitelist rather than prompting.
        assert!(!wl.lock().unwrap().contains(ActionKind::MouseClick));
    }

    #[tokio::test]
    async fn gate_ask_malformed_action_is_invalid_arguments_not_a_prompt() {
        // A malformed action never prompts and never touches HID — it is a typed
        // invalid-arguments failure, just like the ungated InputTool.
        let backend = Arc::new(RecordingInput::new());
        let approver = Arc::new(ScriptedApprover::new(vec![]));
        let (gate, _wl) = gate_over(HidRunMode::Ask, backend.clone(), approver.clone());

        let bad_tag = gate.execute(&input_call("c1", r#"{"action":"self-destruct"}"#)).await;
        assert_eq!(bad_tag.failure.as_deref(), Some("invalid-arguments"));
        let bad_json = gate.execute(&input_call("c2", "{not json")).await;
        assert_eq!(bad_json.failure.as_deref(), Some("invalid-arguments"));

        assert_eq!(approver.prompt_count(), 0, "a malformed action must never prompt");
        assert!(backend.last.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn composite_gates_input_but_never_memory_search_or_screen_query() {
        // The exact production shape: memory + gated input + screen_query. Only
        // input_action is gated — memory_search and screen_query dispatch to their
        // own executors and never reach the approver.
        let backend = Arc::new(RecordingInput::new());
        let approver = Arc::new(ScriptedApprover::new(vec![ApprovalVerdict::AllowOnce]));
        let whitelist = Arc::new(std::sync::Mutex::new(SessionWhitelist::new()));
        // One shared targeting gate: the screen_query below sets it so the
        // subsequent mouse-move is allowed to aim — the real focus→query→click
        // wiring, not a bypass.
        let screen_seen = Arc::new(ScreenSeen::new());
        let focused_app = Arc::new(FocusedApp::new());
        let gate = ApprovalGate::new(
            InputTool::new(backend.clone(), armed_arm()),
            FocusAppTool::new(Arc::new(RecordingFocus::new())),
            HidRunMode::Ask,
            whitelist,
            approver.clone(),
            screen_seen.clone(),
            focused_app.clone(),
        );
        let composite = CompositeExecutor::new(vec![
            Box::new(seeded_tool()),
            Box::new(gate),
            Box::new(ScreenQueryTool::new(Arc::new(ScriptedScreen::ok()), screen_seen, focused_app)),
        ]);

        // memory_search: succeeds, never gated.
        let mem = composite.execute(&search_call("c1", r#"{"query":"broadcast lag"}"#)).await;
        assert!(mem.ok);
        assert_eq!(approver.prompt_count(), 0, "memory_search must never be gated");

        // screen_query: succeeds, never gated — and it grounds the coordinates
        // the mouse-move below needs.
        let scr = composite.execute(&screen_call("c2")).await;
        assert!(scr.ok);
        assert_eq!(approver.prompt_count(), 0, "screen_query must never be gated");

        // input_action: gated through the approver, then performed. The aim must
        // land inside a real screen_query element — ScriptedScreen::ok() returns
        // one box [100,160)×[200,224), so (120, 210) is on-target.
        let hid = composite
            .execute(&input_call("c3", r#"{"action":"mouse-move","x":120,"y":210}"#))
            .await;
        assert!(hid.ok);
        assert_eq!(approver.prompt_count(), 1, "input_action must be gated");
        assert_eq!(*backend.last.lock().unwrap(), Some(InputAction::MouseMove { x: 120, y: 210 }));
    }

    #[tokio::test]
    async fn gate_drives_an_approved_input_action_through_the_full_tool_loop() {
        // End-to-end through run_tool_loop: the model calls input_action, the gate
        // prompts, the scripted user allows once, the backend performs, then the
        // model answers in text — the founding aimed-control round.
        let backend = Arc::new(RecordingInput::new());
        let approver = Arc::new(ScriptedApprover::new(vec![ApprovalVerdict::AllowOnce]));
        let whitelist = Arc::new(std::sync::Mutex::new(SessionWhitelist::new()));
        // mouse-click carries no coordinate, so it is not gated on the targeting
        // flag; a fresh (unseen) gate still performs the click.
        let gate = ApprovalGate::new(
            InputTool::new(backend.clone(), armed_arm()),
            FocusAppTool::new(Arc::new(RecordingFocus::new())),
            HidRunMode::Ask,
            whitelist,
            approver.clone(),
            Arc::new(ScreenSeen::new()),
            Arc::new(FocusedApp::new()),
        );
        let composite = CompositeExecutor::new(vec![Box::new(gate)]);
        let client = ScriptedClient::new(vec![
            tool_call_outcome(vec![input_call("c1", r#"{"action":"mouse-click","button":"left"}"#)]),
            text_outcome("clicked the button"),
        ]);
        let capture = Capture::new();
        let outcome = run(&client, &composite, &capture).await.unwrap();
        assert_eq!(outcome.text, "clicked the button");
        assert_eq!(approver.prompt_count(), 1);
        assert_eq!(
            *backend.last.lock().unwrap(),
            Some(InputAction::click(MouseButton::Left)),
        );
        // The tool-result event rode back ok:true after the approval.
        let ToolEvent::Result(result) = &capture.events()[1] else { panic!("expected Result") };
        assert!(result.ok);
    }

    // --- Screen-query targeting guard (M005) -----------------------------

    #[tokio::test]
    async fn mouse_move_is_refused_until_screen_query_grounds_coordinates() {
        // The structural targeting fix: a mouse-move on a guessed coordinate is
        // refused (typed no-screen-query), never reaching the backend, until a
        // successful screen_query has grounded coordinates. AutoRun so approval
        // never masks the guard.
        let backend = Arc::new(RecordingInput::new());
        let approver = Arc::new(ScriptedApprover::new(vec![]));
        let whitelist = Arc::new(std::sync::Mutex::new(SessionWhitelist::new()));
        let screen_seen = Arc::new(ScreenSeen::new());
        let focused_app = Arc::new(FocusedApp::new());
        let gate = ApprovalGate::new(
            InputTool::new(backend.clone(), armed_arm()),
            FocusAppTool::new(Arc::new(RecordingFocus::new())),
            HidRunMode::AutoRun,
            whitelist,
            approver.clone(),
            screen_seen.clone(),
            focused_app.clone(),
        );
        let composite = CompositeExecutor::new(vec![
            Box::new(gate),
            Box::new(ScreenQueryTool::new(Arc::new(ScriptedScreen::ok()), screen_seen, focused_app)),
        ]);

        // Blind mouse-move: refused, backend untouched, approver never consulted.
        let blind = composite
            .execute(&input_call("c1", r#"{"action":"mouse-move","x":9,"y":9}"#))
            .await;
        assert!(!blind.ok);
        assert_eq!(blind.failure.as_deref(), Some(NO_SCREEN_QUERY_KIND));
        assert_eq!(*backend.last.lock().unwrap(), None, "a guessed move must never reach HID");
        assert_eq!(approver.prompt_count(), 0);

        // screen_query grounds the coordinates.
        assert!(composite.execute(&screen_call("c2")).await.ok);

        // Now the same mouse-move lands.
        let aimed = composite
            .execute(&input_call("c3", r#"{"action":"mouse-move","x":100,"y":200}"#))
            .await;
        assert!(aimed.ok, "mouse-move must perform once the screen has been queried");
        assert_eq!(
            *backend.last.lock().unwrap(),
            Some(InputAction::MouseMove { x: 100, y: 200 }),
        );
    }

    #[tokio::test]
    async fn coordinate_bearing_click_is_refused_until_screen_query_grounds_coordinates() {
        // The real small-model failure mode (M005): the model puts the target on
        // the CLICK — {"action":"mouse-click","x":..,"y":..} — never a separate
        // mouse-move. Before this fix serde dropped x/y and the click fired at the
        // cursor (landing on the tray). Now a coordinate-bearing click is gated on
        // screen_query exactly like a move, and once grounded it moves-then-clicks.
        let backend = Arc::new(RecordingInput::new());
        let approver = Arc::new(ScriptedApprover::new(vec![]));
        let whitelist = Arc::new(std::sync::Mutex::new(SessionWhitelist::new()));
        let screen_seen = Arc::new(ScreenSeen::new());
        let focused_app = Arc::new(FocusedApp::new());
        let gate = ApprovalGate::new(
            InputTool::new(backend.clone(), armed_arm()),
            FocusAppTool::new(Arc::new(RecordingFocus::new())),
            HidRunMode::AutoRun,
            whitelist,
            approver.clone(),
            screen_seen.clone(),
            focused_app.clone(),
        );
        let composite = CompositeExecutor::new(vec![
            Box::new(gate),
            Box::new(ScreenQueryTool::new(Arc::new(ScriptedScreen::ok()), screen_seen, focused_app)),
        ]);

        // Blind aimed click: refused, backend untouched — the coordinate is no
        // longer silently dropped into a click-at-cursor.
        let blind = composite
            .execute(&input_call("c1", r#"{"action":"mouse-click","x":840,"y":240,"button":"left"}"#))
            .await;
        assert!(!blind.ok);
        assert_eq!(blind.failure.as_deref(), Some(NO_SCREEN_QUERY_KIND));
        assert_eq!(*backend.last.lock().unwrap(), None, "a guessed click must never reach HID");
        assert_eq!(approver.prompt_count(), 0);

        // screen_query grounds coordinates; the same aimed click now lands, and it
        // carries the coordinate through to the backend (move-then-click).
        assert!(composite.execute(&screen_call("c2")).await.ok);
        let aimed = composite
            .execute(&input_call("c3", r#"{"action":"mouse-click","x":100,"y":200,"button":"left"}"#))
            .await;
        assert!(aimed.ok, "an aimed click must perform once the screen has been queried");
        assert_eq!(
            *backend.last.lock().unwrap(),
            Some(InputAction::click_at(MouseButton::Left, 100, 200)),
            "the coordinate must reach the backend, not be dropped",
        );
    }

    #[tokio::test]
    async fn click_off_every_screen_query_box_is_refused_as_off_target() {
        // The actual M005 miss: the model DID call screen_query, but then clicked a
        // coordinate that is not inside any returned element — bare desktop between
        // windows, which reveals the desktop and hides the user's windows. The gate
        // must refuse it (typed off-target), backend untouched, even though the
        // screen was seen. ScriptedScreen::ok() returns one box [100,160)×[200,224).
        let backend = Arc::new(RecordingInput::new());
        let approver = Arc::new(ScriptedApprover::new(vec![]));
        let whitelist = Arc::new(std::sync::Mutex::new(SessionWhitelist::new()));
        let screen_seen = Arc::new(ScreenSeen::new());
        let focused_app = Arc::new(FocusedApp::new());
        let gate = ApprovalGate::new(
            InputTool::new(backend.clone(), armed_arm()),
            FocusAppTool::new(Arc::new(RecordingFocus::new())),
            HidRunMode::AutoRun,
            whitelist,
            approver.clone(),
            screen_seen.clone(),
            focused_app.clone(),
        );
        let composite = CompositeExecutor::new(vec![
            Box::new(gate),
            Box::new(ScreenQueryTool::new(Arc::new(ScriptedScreen::ok()), screen_seen, focused_app)),
        ]);

        // Ground the screen, then aim at (5, 5) — well outside the only element.
        assert!(composite.execute(&screen_call("c1")).await.ok);
        let off = composite
            .execute(&input_call("c2", r#"{"action":"mouse-click","x":5,"y":5,"button":"left"}"#))
            .await;
        assert!(!off.ok, "a click on bare desktop must be refused");
        assert_eq!(off.failure.as_deref(), Some(OFF_TARGET_KIND));
        assert_eq!(*backend.last.lock().unwrap(), None, "an off-target click must never reach HID");
        assert_eq!(approver.prompt_count(), 0, "refused before the approval prompt");

        // A click INSIDE the element's box performs (regression guard: enforcement
        // is not blanket-refusing every aimed click).
        let on = composite
            .execute(&input_call("c3", r#"{"action":"mouse-click","x":130,"y":210,"button":"left"}"#))
            .await;
        assert!(on.ok, "a click inside a real element must perform");
        assert_eq!(
            *backend.last.lock().unwrap(),
            Some(InputAction::click_at(MouseButton::Left, 130, 210)),
        );
    }

    #[tokio::test]
    async fn focus_change_clears_seen_boxes_so_a_stale_coordinate_is_refused() {
        // A successful focus_app invalidates the seen boxes (the screen changed):
        // a coordinate that was on-target before the focus must now be refused
        // (no-screen-query, since nothing is grounded) until the model re-queries.
        let screen_seen = Arc::new(ScreenSeen::new());
        screen_seen.mark_seen(vec![SeenBox { x: 100, y: 200, width: 60, height: 24 }]);
        assert!(screen_seen.on_target(120, 210));
        screen_seen.invalidate();
        assert!(!screen_seen.seen(), "focus change clears the seen flag");
        assert!(!screen_seen.on_target(120, 210), "and clears the boxes with it");
    }

    #[tokio::test]
    async fn half_specified_click_coordinate_is_rejected_as_invalid() {
        // x-without-y (or the reverse) would silently degrade to a click-at-cursor
        // — the exact failure we are closing — so it is rejected as invalid before
        // the screen_query gate even runs, and never touches the backend.
        let backend = Arc::new(RecordingInput::new());
        let approver = Arc::new(ScriptedApprover::new(vec![]));
        let whitelist = Arc::new(std::sync::Mutex::new(SessionWhitelist::new()));
        let screen_seen = Arc::new(ScreenSeen::new());
        screen_seen.mark_seen(vec![SeenBox { x: 0, y: 0, width: 100_000, height: 100_000 }]); // even with the screen grounded, half-aim is invalid.
        let gate = ApprovalGate::new(
            InputTool::new(backend.clone(), armed_arm()),
            FocusAppTool::new(Arc::new(RecordingFocus::new())),
            HidRunMode::AutoRun,
            whitelist,
            approver,
            screen_seen,
            Arc::new(FocusedApp::new()),
        );
        let outcome = gate
            .execute(&input_call("c1", r#"{"action":"mouse-click","x":840,"button":"left"}"#))
            .await;
        assert!(!outcome.ok);
        assert_eq!(outcome.failure.as_deref(), Some("invalid-arguments"));
        assert_eq!(*backend.last.lock().unwrap(), None, "a half-aimed click must never reach HID");
    }

    #[tokio::test]
    async fn bare_click_type_and_key_are_never_gated_on_screen_query() {
        // A bare mouse-click (no x/y), type, and key carry no coordinate, so they
        // perform on a fresh (never-queried) gate. Only coordinate-bearing actions
        // are gated (see coordinate_bearing_click_is_refused_until_screen_query).
        let backend = Arc::new(RecordingInput::new());
        let approver = Arc::new(ScriptedApprover::new(vec![]));
        let whitelist = Arc::new(std::sync::Mutex::new(SessionWhitelist::new()));
        let gate = ApprovalGate::new(
            InputTool::new(backend.clone(), armed_arm()),
            FocusAppTool::new(Arc::new(RecordingFocus::new())),
            HidRunMode::AutoRun,
            whitelist,
            approver,
            Arc::new(ScreenSeen::new()),
            Arc::new(FocusedApp::new()),
        );
        for (args, expect) in [
            (r#"{"action":"mouse-click","button":"left"}"#, InputAction::click(MouseButton::Left)),
            (r#"{"action":"type-text","text":"hi"}"#, InputAction::TypeText { text: "hi".into() }),
            (
                r#"{"action":"key-press","key":"return"}"#,
                InputAction::KeyPress { key: "return".into() },
            ),
        ] {
            let outcome = gate.execute(&input_call("c", args)).await;
            assert!(outcome.ok, "{args} must not be gated on screen_query");
            assert_eq!(*backend.last.lock().unwrap(), Some(expect));
        }
    }

    #[tokio::test]
    async fn focus_app_invalidates_prior_screen_query() {
        // A successful focus_app changes the frontmost app, so coordinates the
        // model already holds are stale: the next mouse-move must be refused until
        // a fresh screen_query. This enforces focus → query → click ordering.
        let backend = Arc::new(RecordingInput::new());
        let approver = Arc::new(ScriptedApprover::new(vec![]));
        let whitelist = Arc::new(std::sync::Mutex::new(SessionWhitelist::new()));
        let screen_seen = Arc::new(ScreenSeen::new());
        let focused_app = Arc::new(FocusedApp::new());
        let gate = ApprovalGate::new(
            InputTool::new(backend.clone(), armed_arm()),
            FocusAppTool::new(Arc::new(RecordingFocus::new())),
            HidRunMode::AutoRun,
            whitelist,
            approver,
            screen_seen.clone(),
            focused_app.clone(),
        );
        // The fixture element is attributed to Google Chrome so it survives the
        // post-focus filter; its box is [1,4)×[2,6), so (2, 3) is on-target.
        let composite = CompositeExecutor::new(vec![
            Box::new(gate),
            Box::new(ScreenQueryTool::new(
                Arc::new(ScriptedScreen::with_apps(&[("addr", Some("Google Chrome"))])),
                screen_seen,
                focused_app,
            )),
        ]);

        // Query grounds coordinates; a move onto the element now lands.
        assert!(composite.execute(&screen_call("c1")).await.ok);
        assert!(composite
            .execute(&input_call("c2", r#"{"action":"mouse-move","x":2,"y":3}"#))
            .await
            .ok);

        // focus_app succeeds → coordinates invalidated.
        assert!(composite.execute(&focus_call("c3", r#"{"app":"Google Chrome"}"#)).await.ok);

        // The next move is refused until a fresh query.
        let stale = composite
            .execute(&input_call("c4", r#"{"action":"mouse-move","x":2,"y":3}"#))
            .await;
        assert!(!stale.ok, "a move after focus_app must be refused until re-query");
        assert_eq!(stale.failure.as_deref(), Some(NO_SCREEN_QUERY_KIND));

        // Re-query re-grounds (still scoped to the focused Chrome element); the
        // move onto it lands again.
        assert!(composite.execute(&screen_call("c5")).await.ok);
        assert!(composite
            .execute(&input_call("c6", r#"{"action":"mouse-move","x":2,"y":3}"#))
            .await
            .ok);
    }

    #[tokio::test]
    async fn focus_app_scopes_the_next_screen_query_to_that_app() {
        // The end-to-end targeting guarantee (M005): after a successful focus_app,
        // screen_query returns ONLY the focused app's elements — the desktop and
        // other apps are gone, so the model structurally cannot aim at wallpaper.
        let backend = Arc::new(RecordingInput::new());
        let approver = Arc::new(ScriptedApprover::new(vec![]));
        let whitelist = Arc::new(std::sync::Mutex::new(SessionWhitelist::new()));
        let screen_seen = Arc::new(ScreenSeen::new());
        let focused_app = Arc::new(FocusedApp::new());
        // RecordingFocus echoes the requested name back as the resolved app.
        let gate = ApprovalGate::new(
            InputTool::new(backend.clone(), armed_arm()),
            FocusAppTool::new(Arc::new(RecordingFocus::new())),
            HidRunMode::AutoRun,
            whitelist,
            approver,
            screen_seen.clone(),
            focused_app.clone(),
        );
        let screen = Arc::new(ScriptedScreen::with_apps(&[
            ("address bar", Some("Google Chrome")),
            ("desktop icon", None),
            ("menu item", Some("Finder")),
        ]));
        let composite = CompositeExecutor::new(vec![
            Box::new(gate),
            Box::new(ScreenQueryTool::new(screen, screen_seen, focused_app)),
        ]);

        // Before focus: the pre-focus survey returns all three elements.
        let survey = composite.execute(&screen_call("c1")).await;
        assert_eq!(survey.result_count, Some(3), "pre-focus survey returns everything");

        // Focus Chrome, then re-query: only Chrome's element survives.
        assert!(composite.execute(&focus_call("c2", r#"{"app":"Google Chrome"}"#)).await.ok);
        let scoped = composite.execute(&screen_call("c3")).await;
        assert_eq!(scoped.result_count, Some(1), "post-focus query is scoped to Chrome");
        let v: serde_json::Value = serde_json::from_str(&scoped.content).unwrap();
        assert_eq!(v[0]["text"], "address bar");
        assert_eq!(v[0]["app"], "Google Chrome");
    }

    #[test]
    fn hid_system_prompt_names_the_tool_sequence() {
        // The prose the model reads must spell out the focus→query→click order and
        // the no-guessing rule; drift here silently degrades small-model targeting.
        assert!(HID_SYSTEM_PROMPT.contains(FOCUS_APP_TOOL));
        assert!(HID_SYSTEM_PROMPT.contains(SCREEN_QUERY_TOOL));
        assert!(HID_SYSTEM_PROMPT.contains(INPUT_ACTION_TOOL));
        // It teaches the model's natural one-action aim: click with the x,y.
        assert!(HID_SYSTEM_PROMPT.contains("mouse-click"));
        assert!(HID_SYSTEM_PROMPT.to_lowercase().contains("never guess"));
    }

    // --- FocusAppTool (M005) ---------------------------------------------

    #[test]
    fn focus_app_definition_is_the_openai_function_envelope() {
        let def = FocusAppTool::definition();
        assert_eq!(def.name, FOCUS_APP_TOOL);
        let v = serde_json::to_value(&def).unwrap();
        assert_eq!(v["type"], "function");
        assert_eq!(v["function"]["name"], "focus_app");
        assert_eq!(v["function"]["parameters"]["required"][0], "app");
    }

    #[tokio::test]
    async fn focus_app_ok_reports_the_activated_name() {
        let backend = Arc::new(RecordingFocus::new());
        let tool = FocusAppTool::new(backend.clone());
        let outcome = tool.execute(&focus_call("c1", r#"{"app":"Google Chrome"}"#)).await;
        assert!(outcome.ok);
        assert_eq!(outcome.failure, None);
        assert_eq!(*backend.last.lock().unwrap(), Some("Google Chrome".to_string()));
        let v: serde_json::Value = serde_json::from_str(&outcome.content).unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["focused"], "Google Chrome");
        assert_eq!(v["launched"], false);
    }

    #[tokio::test]
    async fn focus_app_not_found_lists_candidates_for_retry() {
        // A not-found rides its kind back and carries the running-app candidates
        // so the model can retry against a real name (R007).
        struct MissBackend;
        #[async_trait]
        impl AppFocus for MissBackend {
            async fn focus(
                &self,
                app_name: &str,
            ) -> Result<crate::appfocus::FocusedApp, AppFocusError> {
                Err(AppFocusError::NotFound {
                    requested: app_name.to_string(),
                    candidates: vec!["Zed".into(), "Finder".into()],
                })
            }
            async fn running_apps(&self) -> Vec<String> {
                vec!["Zed".into(), "Finder".into()]
            }
        }
        let tool = FocusAppTool::new(Arc::new(MissBackend));
        let outcome = tool.execute(&focus_call("c1", r#"{"app":"Firefox"}"#)).await;
        assert!(!outcome.ok);
        assert_eq!(outcome.failure.as_deref(), Some("not-found"));
        let v: serde_json::Value = serde_json::from_str(&outcome.content).unwrap();
        assert_eq!(v["candidates"][0], "Zed");
        assert_eq!(v["candidates"][1], "Finder");
    }

    #[tokio::test]
    async fn focus_app_malformed_arguments_are_typed_invalid_arguments() {
        let tool = FocusAppTool::new(Arc::new(RecordingFocus::new()));
        let outcome = tool.execute(&focus_call("c1", "{not json")).await;
        assert!(!outcome.ok);
        assert_eq!(outcome.failure.as_deref(), Some("invalid-arguments"));
    }

    #[tokio::test]
    async fn focus_app_wrong_name_is_unknown_tool() {
        let tool = FocusAppTool::new(Arc::new(RecordingFocus::new()));
        let outcome = tool
            .execute(&ToolCall { id: "c1".into(), name: "memory_search".into(), arguments: "{}".into() })
            .await;
        assert!(!outcome.ok);
        assert_eq!(outcome.failure.as_deref(), Some("unknown-tool"));
    }

    // --- FocusApp gating through the ApprovalGate (M005) ------------------

    /// A gate whose app-focus surface is a recording backend, so a gated
    /// activation can be asserted to have (not) reached it.
    fn focus_gate_over(
        mode: HidRunMode,
        focus: Arc<RecordingFocus>,
        approver: Arc<ScriptedApprover>,
    ) -> (ApprovalGate, Arc<std::sync::Mutex<SessionWhitelist>>) {
        let whitelist = Arc::new(std::sync::Mutex::new(SessionWhitelist::new()));
        let inner = InputTool::new(Arc::new(RecordingInput::new()), armed_arm());
        (
            ApprovalGate::new(
                inner,
                FocusAppTool::new(focus),
                mode,
                whitelist.clone(),
                approver,
                Arc::new(ScreenSeen::new()),
                Arc::new(FocusedApp::new()),
            ),
            whitelist,
        )
    }

    #[test]
    fn gate_off_withholds_focus_app_from_the_definitions() {
        // Structural gate (D038): HID Off advertises neither input_action nor
        // focus_app; an armed mode advertises both.
        let (off, _wl) = focus_gate_over(
            HidRunMode::Off,
            Arc::new(RecordingFocus::new()),
            Arc::new(ScriptedApprover::new(vec![])),
        );
        assert!(off.definitions().is_empty(), "Off must advertise no HID tools");

        let (ask, _wl) = focus_gate_over(
            HidRunMode::Ask,
            Arc::new(RecordingFocus::new()),
            Arc::new(ScriptedApprover::new(vec![])),
        );
        let names: Vec<String> = ask.definitions().into_iter().map(|d| d.name).collect();
        assert!(names.contains(&INPUT_ACTION_TOOL.to_string()));
        assert!(names.contains(&FOCUS_APP_TOOL.to_string()), "an armed mode advertises focus_app");
    }

    #[tokio::test]
    async fn gate_off_refuses_focus_app_with_disabled_before_activating() {
        // Off is structurally inert: the gate refuses focus_app BEFORE parsing or
        // activating — never prompts, never reaches the backend (D038).
        let focus = Arc::new(RecordingFocus::new());
        let approver = Arc::new(ScriptedApprover::new(vec![]));
        let (gate, _wl) = focus_gate_over(HidRunMode::Off, focus.clone(), approver.clone());
        let outcome = gate.execute(&focus_call("c1", r#"{"app":"Google Chrome"}"#)).await;
        assert!(!outcome.ok);
        assert_eq!(outcome.failure.as_deref(), Some("disabled"));
        assert!(focus.last.lock().unwrap().is_none(), "Off must not activate anything");
        assert_eq!(approver.prompt_count(), 0, "Off must never prompt");
    }

    #[tokio::test]
    async fn gate_ask_deny_never_activates_the_app() {
        // Ask + new kind prompts; a Deny returns approval-denied and never fronts.
        let focus = Arc::new(RecordingFocus::new());
        let approver = Arc::new(ScriptedApprover::new(vec![ApprovalVerdict::Deny]));
        let (gate, _wl) = focus_gate_over(HidRunMode::Ask, focus.clone(), approver.clone());
        let outcome = gate.execute(&focus_call("c1", r#"{"app":"Google Chrome"}"#)).await;
        assert!(!outcome.ok);
        assert_eq!(outcome.failure.as_deref(), Some(APPROVAL_DENIED_KIND));
        assert!(focus.last.lock().unwrap().is_none(), "Deny must not activate the app");
        assert_eq!(approver.prompt_count(), 1);
        // The overlay saw a human summary naming the target app.
        assert!(approver.last_summary().unwrap().contains("Google Chrome"));
    }

    #[tokio::test]
    async fn gate_ask_allow_kind_whitelists_focus_app_for_the_session() {
        // "Always allow this kind" performs AND whitelists FocusApp, so a second
        // focus_app performs without prompting (the queue has one verdict; a
        // second prompt would panic on the exhausted script).
        let focus = Arc::new(RecordingFocus::new());
        let approver = Arc::new(ScriptedApprover::new(vec![ApprovalVerdict::AllowKind]));
        let (gate, wl) = focus_gate_over(HidRunMode::Ask, focus.clone(), approver.clone());

        let first = gate.execute(&focus_call("c1", r#"{"app":"Google Chrome"}"#)).await;
        assert!(first.ok);
        assert_eq!(approver.prompt_count(), 1);
        assert!(wl.lock().unwrap().contains(ActionKind::FocusApp), "allow-kind must whitelist FocusApp");

        let second = gate.execute(&focus_call("c2", r#"{"app":"Finder"}"#)).await;
        assert!(second.ok, "a whitelisted FocusApp must perform without prompting");
        assert_eq!(approver.prompt_count(), 1, "the whitelisted kind must not prompt again");
        assert_eq!(*focus.last.lock().unwrap(), Some("Finder".to_string()));
    }

    #[tokio::test]
    async fn gate_auto_run_activates_focus_app_without_prompting() {
        let focus = Arc::new(RecordingFocus::new());
        let approver = Arc::new(ScriptedApprover::new(vec![]));
        let (gate, _wl) = focus_gate_over(HidRunMode::AutoRun, focus.clone(), approver.clone());
        let outcome = gate.execute(&focus_call("c1", r#"{"app":"Google Chrome"}"#)).await;
        assert!(outcome.ok);
        assert_eq!(approver.prompt_count(), 0, "Auto-run must never prompt");
        assert_eq!(*focus.last.lock().unwrap(), Some("Google Chrome".to_string()));
    }
}
