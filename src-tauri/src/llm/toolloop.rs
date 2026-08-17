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
use crate::input::{ActionKind, ActionReport, InputAction, InputControl, InputError, MouseButton};
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

/// How many times ONE exact (tool, arguments) call may execute per run
/// before further repeats are refused typed. A model stuck in a loop
/// re-issues the SAME failing call verbatim (the pi-script incident: the
/// same `run_in_workspace` command failed round after round while the
/// model narrated "let me fix it" and rewrote the same file) — three
/// attempts is generous for legitimate retries; the fourth identical call
/// cannot produce new information.
pub const REPEATED_CALL_LIMIT: u32 = 3;

/// Typed failure kind for the repeat breaker — the model is told to change
/// strategy or report the blocker honestly, structurally (D038), instead
/// of narrating another lap.
pub const REPEATED_CALL_KIND: &str = "repeated-call";

/// The one tool S03 ships. The name is part of the model-facing contract
/// and the UI's memory-consulted check (T04).
pub const MEMORY_SEARCH_TOOL: &str = "memory_search";

pub const CHAT_HISTORY_SEARCH_TOOL: &str = "chat_history_search";

pub const READ_PAGE_TOOL: &str = "read_page";

pub const REMEMBER_TOOL: &str = "remember";

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
/// The lane-independent core of the agent contract (2026-08-17 prompt
/// split): tool mechanics, grounding, verified-honesty, recall,
/// continuity, refusal honesty. Browsing and coding doctrine ride only
/// the lanes that need them — a 9B follows a short prompt with no
/// internal tensions far better than one wall of imperatives.
pub const HID_SYSTEM_PROMPT_CORE: &str = r#"You can control this computer for the user with tools: focus_app (open an app or bring it to the front — it launches the app if it is not running), screen_query (read the text on screen with each item's exact pixel coordinates), and input_action (move/click the mouse, type text, press a key).

To operate any app, follow this order every time:
1. Call focus_app with the app name (e.g. "Google Chrome") to open it / bring it to the front.
2. Call screen_query to see what is on screen and get the real pixel coordinates of the element you want. Each element carries cx,cy — its exact centre, precomputed for you. That pair IS the click target; never do your own arithmetic on x/width. Elements with a role (AXButton, AXLink, AXTextField, …) are the app's real controls with exact frames — prefer them over plain text when both match what you want to click.
3. To click a target, call input_action with action "mouse-click" and pass that element's cx as x and cy as y verbatim — the click moves to that point and clicks it in one step.
4. Use action "type-text" or "key-press" to enter text. Typing goes into whatever you last clicked, so ALWAYS mouse-click the exact field you want to fill before you type.

More vocabulary: mouse-click with clicks 2 double-clicks (open a file, select a word) and clicks 3 selects a whole line; mouse-drag (fromX,fromY -> toX,toY) selects text ranges and drags things — both endpoints are cx,cy pairs from screen_query; scroll with deltaY (positive = further down the page) reaches content that is off screen — scroll then screen_query again to read what appeared; key-press with modifiers gives shortcuts: ["cmd"]+"a" select all, ["cmd"]+"c" copy, ["cmd"]+"v" paste. Prefer keyboard shortcuts over pixel work when both can do the job (cmd+a beats drag-selecting a whole document). For LONG text, clipboard write + click the field + cmd+v beats type-text; to extract text from an app: select it, cmd+c, clipboard read. After opening an app or triggering an animation, call wait (default 500ms) before the next screen_query instead of retrying a stale read.

Every x,y you pass MUST come from the most recent screen_query — never guess coordinates. A click or move to a coordinate you did not read from screen_query will be refused. After you focus_app the screen changes, so call screen_query again before you click.

Always focus_app FIRST, before screen_query. Once you have focused an app, screen_query returns only that app's on-screen elements — so only ever aim at an element screen_query returned. Never click a coordinate that is not one of those elements: an empty spot is the desktop wallpaper, and clicking it hides the user's windows instead of doing what they asked.

Every input_action result carries a `verified` block — what ACTUALLY happened, measured from the OS after the action: `cursor` is where the mouse really ended up, `focus` names the app and UI element that now holds keyboard focus (its role, title, and current value), and for type-text `textEntered` reports whether your text was really observed in the focused field. VALIDATE every action against it before moving on: after a mouse-click, verified.clickedElement names the UI element that was actually under the click (its role and title) — if it is not the thing you aimed at (wrong title, role AXGroup instead of the link/button you wanted), the click landed off target: screen_query again and re-aim at a better element. After clicking a text field, verified.focus should name that field in the app you focused; after type-text, verified.textEntered should be true and verified.focus.value should contain what you typed. If verified contradicts your intent — the focused app is wrong, textEntered is false, the value is missing your text — the action landed in the wrong place: call screen_query again, re-aim, and retry instead of continuing the sequence. When the evidence shows your action landed in a DIFFERENT app than the one you focused, the tool fails it for you (kind verification-failed) — treat that like any failed step: screen_query, re-aim, retry.

Report tool results honestly, using the `verified` evidence: only claim an action worked when its verified block confirms it. If a tool call returns an error, tell the user it failed and why — never claim an action succeeded when the tool reported a failure.

EVALUATE THE GOAL before finishing: after the last action of any on-screen task, take_screenshot (or screen_query) and CHECK the screen actually shows what the user asked for — the result state, not just your last action's success. If focus_app reports visibleWindows 0, the app is frontmost but the user sees NOTHING — open a window (key-press "n" with modifiers ["cmd"]) and verify again, or say plainly that the app has no window open. Only declare the task done when the final look confirms it; otherwise describe what you see and what is still missing. Your FINAL ANSWER must summarize WHAT YOU FOUND — the actual content: item names, prices, ratings, key facts read from the page (read_page before answering a find-task) — not a list of the steps you took; the user watched the steps happen. Write answers in plain prose/markdown — never LaTeX math notation ($$…$$); this is a chat overlay, not a paper. take_screenshot does NOT save a file unless you pass save: true (Desktop by default, `directory` for a user-named folder) — when the user asks to save a screenshot pass save: true and quote the exact saved path from the result; never claim a screenshot was saved anywhere else.

You also have find_programs (search what is installed on this machine — GUI apps and terminal tools) and run_command (run one shell command; the user approves each one). For simple machine facts — the time (`date`), public IP (`curl -s ifconfig.me`), hostname, disk space (`df -h`), battery (`pmset -g batt`) — prefer ONE short read-only run_command over driving the screen. Check find_programs before claiming an app or tool is or is not installed, and before running a CLI tool you are not sure exists.

You CAN recall the past: memory_search finds distilled memories of the user's activity and earlier conversations; chat_history_search finds the verbatim messages of past chat sessions. When the user asks what they said, asked, or discussed before ("what recipes have I asked about?"), call chat_history_search with a short keyword (e.g. "recipe") and answer from the matches — never claim you have no access to past conversations without searching first. When the user asks you to REMEMBER something ("remember that…"), call remember with one concise self-contained fact — never claim you cannot store information. The same applies to PERSONAL FACTS: "what is my name", "where do I work", "what do I like" — memory_search FIRST (and chat_history_search if memory finds nothing); only after both come back empty may you say you do not know, and then offer to remember it.

CONTINUITY: follow-up questions usually refer to what you JUST did and what is on screen right now. A page you opened earlier in this conversation is still open — "this recipe", "the ingredients", "read it to me" mean THAT page. Answer by looking: focus_app the browser if needed, then read_page for the page's full text (or screen_query/take_screenshot for layout). Never claim you cannot see a page you opened — read it. The same goes for the TASK: a follow-up that refines what you just did ("now the PC version", "only ones under $50", "sort by price") means CONTINUE in the same app and page — refine the search there, use the site's filters, read the results, answer. Never ask whether the user wants you to "actually search" — they just asked; act.

When a tool refuses — kind `disabled`, `approval-denied`, `verification-failed`, or any error — the action DID NOT HAPPEN. Tell the user exactly what you completed, what failed, and why (e.g. "I opened Chrome, but input control is disabled so I could not type the search"). NEVER describe an outcome you did not verify from a successful tool result: claiming an unperformed action happened is the worst possible answer."#;

/// The browsing playbook (thin + heavy lanes). One search doctrine —
/// web_search — replacing the three competing strategies (hand-built
/// URLs vs. on-page search box vs. address-bar typing) whose per-run
/// coin flips were the ebay inconsistency (user report 2026-08-17).
pub const HID_SYSTEM_PROMPT_BROWSING: &str = r#"WEB SEARCH — the ONE way to find things online: call web_search with the query, and site "ebay", "amazon", or "youtube" when the user wants that site (default google). It opens the results page and returns what is on screen with exact click coordinates: pick the best result, mouse-click its cx,cy, then read_page to extract the actual information before answering. NEVER compose search or product URLs by hand, never type URLs into the address bar, and never google another site's listings when web_search can search that site directly.
Direct navigation is only for URLs the user gave you or that appeared in a page or tool result (run_command `open <url>`). READ the page that is already open (screen_query or read_page) before opening another one — opening page after page without reading is refused (too-many-opens); prefer clicking links on the open page over new navigations.
When the browser already shows the site you need, work IN that window — click its controls, use its filters. A follow-up that refines a search ("only under $50", "now the PC version") means refine on the SAME site: another web_search there or the site's own filters."#;

/// The coding contract (coder lane only).
pub const HID_SYSTEM_PROMPT_CODING: &str = r#"CODING: read_file, list_dir, write_file and run_in_workspace work ANYWHERE on this machine. Relative paths resolve against the ACTIVE working directory (named in your Environment); if none is set, a folder chooser asks the user where to work — wait for their pick. list_dir first to learn a layout, read_file before changing a file — never write over content you have not read. write_file replaces the WHOLE file, so pass the complete new contents. Writes and commands in a directory the user has not yet approved prompt them (approving "this session" covers that directory); tmp (/tmp, the system temp dir) is ALWAYS writable with no prompt — use it for scratch work. To compile, test, or run code, use run_in_workspace (never run_command): output streams into the chat and timeoutSecs goes up to 600 for long builds — after writing code, BUILD AND TEST it with run_in_workspace and report the real result. There is NO persistent shell: every command starts fresh in the active working directory — a bare `cd` does nothing and is refused; pass cwd for another folder. When a result header names a directory in [brackets], that IS the directory you listed or read — describe it as that path, never as a folder you merely intended to be in. When the build is clean, call workspace_diff and REVIEW the diff before declaring the task done: confirm it contains exactly the intended changes, then summarize them for the user. NEVER git-commit or git-push changes unless the user explicitly asks you to."#;

/// Teach Me mode (user request 2026-08-18): the Wolfram-Alpha "show the
/// proof" of computer use. Replaces the browsing playbook — the efficient
/// shortcuts it teaches are structurally stripped in this mode.
pub const HID_SYSTEM_PROMPT_TEACH: &str = r#"TEACH ME MODE is ON: the user wants to LEARN how to do this themselves — show the human way, not just the result. Work exactly like a person at the keyboard: visible clicks, typing, and standard keyboard shortcuts only. The terminal and one-shot search shortcuts are unavailable on purpose — do not mention wanting them.
NARRATE as you go: before each action, one short line saying what you are doing and why ("Opening Chrome", "Clicking the search box", "Pressing cmd+t for a new tab"). To search the web the human way: focus the browser, key-press "t" with ["cmd"] for a new tab, click the address bar if needed, type-text the search words, key-press "return" — then read the results on screen like a person would.
FINISH with a numbered "Do it yourself" recap: the exact steps — app names, what to click, what to type, which shortcuts — so the user can repeat the whole task without you."#;

/// Assemble the system prompt for one routed lane + mode: coder runs carry
/// the coding contract; teach-mode runs carry the human-way teaching
/// contract instead of the browsing playbook; everything else browses. The
/// full concatenation stays available as [`struct@HID_SYSTEM_PROMPT`] for
/// the prompt-contract evals.
pub fn system_prompt_for_lane(lane: &str, teach: bool) -> String {
    let section = if lane == "coder" {
        HID_SYSTEM_PROMPT_CODING
    } else if teach {
        HID_SYSTEM_PROMPT_TEACH
    } else {
        HID_SYSTEM_PROMPT_BROWSING
    };
    format!("{HID_SYSTEM_PROMPT_CORE}\n\n{section}")
}

/// The tools Teach Me mode structurally REMOVES: the invisible shortcuts a
/// human at the keyboard does not have. Pure — the assembly filter and the
/// tests share it.
pub fn teach_mode_strips(tool: &str) -> bool {
    tool == "run_command" || tool == WEB_SEARCH_TOOL || tool == "run_in_workspace"
}

/// Every clause in one string — the prompt-contract eval surface (each
/// load-bearing clause is pinned there regardless of which lane carries it).
pub static HID_SYSTEM_PROMPT: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
    format!(
        "{HID_SYSTEM_PROMPT_CORE}\n\n{HID_SYSTEM_PROMPT_BROWSING}\n\n{HID_SYSTEM_PROMPT_CODING}\n\n{HID_SYSTEM_PROMPT_TEACH}"
    )
});

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

/// The typed failure an `input_action` gets when the event was synthesized but
/// its post-action `verified` evidence CONTRADICTS the intent: keyboard focus
/// ended up in a different app than the one the model `focus_app`'d. This is
/// the structural half of the verification surface (the reinforcement loop):
/// the `verified` block alone relies on the model reading it, and a small model
/// happily sails past `focus.app = "Third Eye"` while narrating success.
/// Flipping the result to a typed failure forces the observe → re-aim → retry
/// cycle the same way [`NO_SCREEN_QUERY_KIND`]/[`OFF_TARGET_KIND`] force
/// grounded aiming (M005/M008 pattern: structure over prose).
pub const VERIFICATION_FAILED_KIND: &str = "verification-failed";

/// Compare an action's post-hoc [`ActionReport`] against the run's focused-app
/// intent and return the contradiction, if any. Pure and deliberately narrow:
/// it fires ONLY on positive evidence of wrongness — a focus readback naming a
/// DIFFERENT app than `focused`. Absent evidence (`focus`/`app` = `None`, no
/// app focused yet) passes: the report's fields are best-effort observations,
/// and refusing on missing data would fail honest actions on targets the OS
/// cannot attribute. Cursor mismatch needs no rule here — the backend already
/// fails a move that never committed — and `textEntered: false` stays soft
/// (some targets never echo text; the model sees it in `verified`).
pub fn verify_against_intent(report: &ActionReport, focused: Option<&str>) -> Option<String> {
    let focused = focused?;
    // A click's hit-test is the sharper signal: links and buttons often take
    // no keyboard focus, so `focus` stays silent while `clicked_element`
    // names exactly what was under the mousedown. Same positive-evidence
    // rule: only a DIFFERENT app fails it.
    if let Some(hit) = report
        .clicked_element
        .as_ref()
        .and_then(|e| e.app.as_deref())
    {
        if !hit.eq_ignore_ascii_case(focused) {
            return Some(format!(
                "the click was synthesized, but the element under it belongs to {hit:?}, not the \
                 focused app {focused:?} — the click hit the wrong app's window (or the desktop). \
                 Call screen_query to re-read the screen, then re-aim; do not assume this step \
                 worked."
            ));
        }
    }
    let observed = report.focus.as_ref()?.app.as_deref()?;
    if observed.eq_ignore_ascii_case(focused) {
        return None;
    }
    Some(format!(
        "the action was synthesized, but keyboard focus now sits in {observed:?}, not the focused \
         app {focused:?} — it landed in the wrong app. Call screen_query to re-read the screen, \
         then re-aim; do not assume this step worked."
    ))
}

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
        px >= self.x && px < self.x + self.width && py >= self.y && py < self.y + self.height
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
    /// Bounded human-facing output preview (computer-control I2): populated
    /// for `run_command` results so the chat transcript's terminal block can
    /// show what the command printed — the visible-terminal requirement.
    /// Absent for every other tool (their results are model-facing data).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
}

/// Cap on the UI preview riding a tool-result event.
const RESULT_PREVIEW_CHARS: usize = 2000;

/// The UI preview for one executed call: run_command's report, bounded on a
/// char boundary; `None` for every other tool.
fn result_preview(call_name: &str, content: &str) -> Option<String> {
    // String literals, not the cfg(desktop) modules' consts (this module is
    // not cfg(desktop)); each pair is pinned equal by a unit test in its
    // own module (command_runner / workspace::exec_tool).
    if call_name != "run_command"
        && call_name != "run_in_workspace"
        && call_name != "workspace_diff"
    {
        return None;
    }
    if content.len() <= RESULT_PREVIEW_CHARS {
        return Some(content.to_string());
    }
    let mut cut = RESULT_PREVIEW_CHARS;
    while !content.is_char_boundary(cut) {
        cut -= 1;
    }
    Some(format!("{}…", &content[..cut]))
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
    /// Screenshot payload (take_screenshot): tool-role messages are
    /// text-only in the chat API, so the loop injects this as a follow-up
    /// vision user turn. Transient model context — never stored (R011).
    pub attachment_png: Option<String>,
}

impl ToolOutcome {
    /// A plain successful outcome: `content` rides to the model verbatim.
    pub fn success(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            ok: true,
            result_count: None,
            mode: None,
            failure: None,
            attachment_png: None,
        }
    }

    /// A typed failure: the model sees `{"error": detail}` (so it can
    /// recover or answer without the tool), the UI sees the kind.
    pub fn failure(kind: &str, detail: impl Into<String>) -> Self {
        let detail = detail.into();
        Self {
            content: serde_json::json!({ "error": detail }).to_string(),
            ok: false,
            result_count: None,
            mode: None,
            failure: Some(kind.to_string()),
            attachment_png: None,
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
    /// Whether this executor is the home of `name` — the composite's routing
    /// question. Defaults to "advertises it right now", which is correct for
    /// every plain tool; gates that HIDE a tool from the model but still own
    /// its refusal (the per-tool switchboard) override this so a call to a
    /// disabled tool reaches the gate's typed "disabled" answer instead of
    /// falling through to the composite's generic unknown-tool.
    fn claims(&self, name: &str) -> bool {
        self.definitions().iter().any(|d| d.name == name)
    }
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
            description: "Search the user's stored memories: summaries of their on-screen \
                          activity (app names, time spans) AND distilled one-liners from past \
                          chat conversations with you. Call this when the user asks about \
                          their earlier work, activity, or things discussed before. For exact \
                          quotes of past questions/answers, chat_history_search searches the \
                          verbatim transcripts."
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
                format!(
                    "unknown tool: {} (available: {MEMORY_SEARCH_TOOL})",
                    call.name
                ),
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
        let limit = args
            .limit
            .unwrap_or(DEFAULT_SEARCH_LIMIT)
            .clamp(1, MAX_SEARCH_LIMIT);
        match search(&self.store, self.embedder.as_ref(), &args.query, limit).await {
            Ok(outcome) => {
                let content = serde_json::to_string(&outcome).unwrap_or_else(|e| {
                    format!(r#"{{"error":"result serialization failed: {e}"}}"#)
                });
                ToolOutcome {
                    content,
                    ok: true,
                    result_count: Some(outcome.results.len()),
                    mode: Some(outcome.mode),
                    failure: None,
                    attachment_png: None,
                }
            }
            // Store failure: typed to model and UI, stream keeps going —
            // the model can still answer from context.
            Err(err) => ToolOutcome::failure(err.kind(), err.to_string()),
        }
    }
}

/// The typed refusal an `open <deep-url>` gets when that URL never appeared
/// in the user's words or a real tool result — the model invented it. The
/// structural half of the search-then-choose rule (the prose alone did not
/// hold: the small model kept one-shotting guessed recipe URLs).
pub const UNGROUNDED_URL_KIND: &str = "ungrounded-url";

/// Per-run set of URLs the model has legitimately SEEN: harvested from the
/// user's messages at run start and from every tool result as it lands.
/// The `ScreenSeen` pattern applied to navigation — "never open a URL you
/// did not read somewhere real."
#[derive(Default)]
pub struct UrlSeen(std::sync::Mutex<std::collections::HashSet<String>>);

impl UrlSeen {
    pub fn new() -> Self {
        Self::default()
    }

    /// Harvest every URL in `text` into the seen set.
    pub fn harvest(&self, text: &str) {
        let mut seen = self.0.lock().unwrap();
        for url in extract_urls(text) {
            seen.insert(url);
        }
    }

    pub fn contains(&self, normalized: &str) -> bool {
        self.0.lock().unwrap().contains(normalized)
    }
}

/// Pull normalized URLs out of free text (tool results, user messages).
pub fn extract_urls(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for start in text
        .match_indices("http")
        .map(|(i, _)| i)
        .collect::<Vec<_>>()
    {
        let rest = &text[start..];
        if !(rest.starts_with("http://") || rest.starts_with("https://")) {
            continue;
        }
        let end = rest
            .find(|c: char| c.is_whitespace() || "\"'<>)]}".contains(c))
            .unwrap_or(rest.len());
        let raw = rest[..end].trim_end_matches(['.', ',', ';', ':', '!', '?']);
        if raw.len() > 10 {
            out.push(normalize_url(raw));
        }
    }
    out
}

/// Normalization for grounded-set membership: lowercase scheme+host, strip
/// one trailing slash. Exact-match beyond that — the model copies URLs
/// verbatim when it has really seen them.
pub fn normalize_url(url: &str) -> String {
    let trimmed = url.trim_end_matches('/');
    match trimmed.find("://").map(|i| i + 3) {
        Some(host_start) => {
            let host_end = trimmed[host_start..]
                .find('/')
                .map(|i| host_start + i)
                .unwrap_or(trimmed.len());
            format!(
                "{}{}",
                trimmed[..host_end].to_lowercase(),
                &trimmed[host_end..]
            )
        }
        None => trimmed.to_lowercase(),
    }
}

/// URLs that are always openable without grounding: any homepage (no path),
/// and search-results pages on the major engines — the search-then-choose
/// flow's entry points.
pub fn url_is_open_by_default(normalized: &str) -> bool {
    let Some(host_start) = normalized.find("://").map(|i| i + 3) else {
        return false;
    };
    let after = &normalized[host_start..];
    let (host, path) = match after.find('/') {
        Some(i) => (&after[..i], &after[i..]),
        None => (after, ""),
    };
    if path.is_empty() || path == "/" {
        return true;
    }
    let path_only = path.split('?').next().unwrap_or(path);
    let is_engine = host.ends_with("google.com")
        || host.ends_with("bing.com")
        || host.ends_with("duckduckgo.com");
    is_engine && (path_only == "/search" || path_only == "/html" || path_only == "/")
}

/// The URL inside an `open …` shell command, if the command is a browser
/// navigation. Non-`open` commands (curl, etc.) are out of scope — the
/// one-shot failure mode is guessed browser tabs.
pub fn open_command_url(command: &str) -> Option<String> {
    let trimmed = command.trim_start();
    if trimmed != "open" && !trimmed.starts_with("open ") {
        return None;
    }
    extract_urls(trimmed).into_iter().next()
}

/// Wraps the whole composite: refuses ungrounded `open <deep-url>` commands
/// BEFORE they run, and harvests URLs out of every tool result so anything
/// the model legitimately read becomes openable.
pub struct UrlGroundingExecutor {
    inner: CompositeExecutor,
    seen: Arc<UrlSeen>,
    /// Successful `open <url>` navigations this run — the tab-flood brake.
    opens: std::sync::atomic::AtomicUsize,
    /// Whether the model has READ a page (screen_query / read_page /
    /// web_search) since its last navigation — the progress rule
    /// (2026-08-17): reading earns another open, so multi-hop research is
    /// unbounded while blind tab-flooding still stops at the budget.
    read_since_open: std::sync::atomic::AtomicBool,
}

/// Browser navigations allowed per run: the search page plus one more.
/// Anything past that is tab flooding — the model should be CLICKING
/// results and reading the open page, not opening more.
pub const MAX_OPENS_PER_RUN: usize = 2;

/// The typed refusal further `open <url>` commands get past the budget.
pub const TOO_MANY_OPENS_KIND: &str = "too-many-opens";

impl UrlGroundingExecutor {
    pub fn new(inner: CompositeExecutor, seen: Arc<UrlSeen>) -> Self {
        Self {
            inner,
            seen,
            opens: std::sync::atomic::AtomicUsize::new(0),
            read_since_open: std::sync::atomic::AtomicBool::new(true),
        }
    }
}

#[async_trait]
impl ToolExecutor for UrlGroundingExecutor {
    fn definitions(&self) -> Vec<ToolDefinition> {
        self.inner.definitions()
    }

    fn claims(&self, name: &str) -> bool {
        self.inner.claims(name)
    }

    async fn execute(&self, call: &ToolCall) -> ToolOutcome {
        // Typed navigation is navigation: a type-text carrying a deep URL
        // the model never saw is the same one-shot guess as `open`ing it —
        // the address-bar path must not be a grounding loophole.
        if call.name == INPUT_ACTION_TOOL {
            let typed = serde_json::from_str::<serde_json::Value>(&call.arguments)
                .ok()
                .filter(|v| v.get("action").and_then(|a| a.as_str()) == Some("type-text"))
                .and_then(|v| v.get("text").and_then(|t| t.as_str()).map(String::from));
            if let Some(text) = typed {
                for url in extract_urls(&text) {
                    if !url_is_open_by_default(&url) && !self.seen.contains(&url) {
                        log::warn!("llm: type-text refused — ungrounded url {url:?}");
                        return ToolOutcome::failure(
                            UNGROUNDED_URL_KIND,
                            format!(
                                "{url} was never given by the user or read from a page — typing \
                                 a guessed URL is the same as opening one. Search first, then \
                                 CLICK the result you want (or type a search-results URL)."
                            ),
                        );
                    }
                }
            }
        }
        if call.name == crate::command_runner::RUN_COMMAND_TOOL {
            let command = serde_json::from_str::<serde_json::Value>(&call.arguments)
                .ok()
                .and_then(|v| v.get("command").and_then(|c| c.as_str()).map(String::from));
            if let Some(url) = command.as_deref().and_then(open_command_url) {
                if !url_is_open_by_default(&url) && !self.seen.contains(&url) {
                    log::warn!("llm: run_command refused — ungrounded url {url:?}");
                    return ToolOutcome::failure(
                        UNGROUNDED_URL_KIND,
                        format!(
                            "{url} was never given by the user or read from a page — you cannot \
                             know it exists, and opening guessed URLs floods the browser with \
                             dead tabs. To FIND something: open ONE search-results URL (e.g. \
                             https://www.google.com/search?q=…), then screen_query and CLICK \
                             the result you want. URLs the user typed, or that appeared in a \
                             tool result, open fine."
                        ),
                    );
                }
                // The progress rule (2026-08-17, replacing the fixed cap's
                // premature quitting): past the free budget, another open
                // is EARNED by reading the page you already have — blind
                // open-after-open still stops.
                let past_budget =
                    self.opens.load(std::sync::atomic::Ordering::SeqCst) >= MAX_OPENS_PER_RUN;
                if past_budget
                    && !self
                        .read_since_open
                        .load(std::sync::atomic::Ordering::SeqCst)
                {
                    log::warn!("llm: run_command refused — unread page open ({url:?})");
                    return ToolOutcome::failure(
                        TOO_MANY_OPENS_KIND,
                        "you opened a page and have not read it — screen_query or read_page \
                         the page that is open (and click its links) before navigating \
                         anywhere else."
                            .to_string(),
                    );
                }
                let outcome = self.inner.execute(call).await;
                if outcome.ok {
                    self.opens.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    self.read_since_open
                        .store(false, std::sync::atomic::Ordering::SeqCst);
                }
                self.seen.harvest(&outcome.content);
                return outcome;
            }
        }
        let outcome = self.inner.execute(call).await;
        // A successful read earns the next navigation (progress rule); a
        // web_search both opened and read, so it satisfies itself.
        if outcome.ok
            && matches!(
                call.name.as_str(),
                SCREEN_QUERY_TOOL | READ_PAGE_TOOL | WEB_SEARCH_TOOL
            )
        {
            self.read_since_open
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
        // Everything the model just read is now legitimately navigable.
        self.seen.harvest(&outcome.content);
        outcome
    }
}

/// One-call web/site search (2026-08-17 consistency work): the model's
/// riskiest freeform sequence — compose a URL vs. find the search box vs.
/// type into the address bar — collapsed into deterministic code. The URL
/// comes from a TEMPLATE (never the model's imagination), the page opens,
/// and after a settle pause the same screen harvest as `screen_query`
/// returns the results with grounded click coordinates. The cx/cy pattern,
/// applied to search.
pub struct WebSearchTool {
    screen: ScreenQueryTool,
    url_seen: Arc<UrlSeen>,
    opener: Arc<dyn Opener>,
}

pub const WEB_SEARCH_TOOL: &str = "web_search";

/// How long the results page gets to render before the screen harvest.
const WEB_SEARCH_SETTLE_MS: u64 = 2500;

/// Browser-open seam so tests never launch a real browser.
#[async_trait]
pub trait Opener: Send + Sync {
    async fn open(&self, url: &str) -> Result<(), String>;
}

/// Production opener: macOS `open` — the default browser takes the URL.
pub struct SystemOpener;

#[async_trait]
impl Opener for SystemOpener {
    async fn open(&self, url: &str) -> Result<(), String> {
        let status = tokio::process::Command::new("/usr/bin/open")
            .arg(url)
            .status()
            .await
            .map_err(|e| format!("could not run open: {e}"))?;
        if status.success() {
            Ok(())
        } else {
            Err(format!("open exited {status}"))
        }
    }
}

/// The search-results URL for one engine/site — templates live HERE, in
/// code, so every run builds the identical URL for the identical ask.
pub fn search_url(site: &str, query: &str) -> String {
    let encoded = url_encode_query(query);
    match site {
        "ebay" => format!("https://www.ebay.com/sch/i.html?_nkw={encoded}"),
        "amazon" => format!("https://www.amazon.com/s?k={encoded}"),
        "youtube" => format!("https://www.youtube.com/results?search_query={encoded}"),
        _ => format!("https://www.google.com/search?q={encoded}"),
    }
}

/// Minimal query percent-encoding (space → `+`, unreserved kept, the rest
/// `%XX`) — dependency-free and stable.
pub fn url_encode_query(query: &str) -> String {
    let mut out = String::with_capacity(query.len());
    for byte in query.trim().bytes() {
        match byte {
            b' ' => out.push('+'),
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

impl WebSearchTool {
    pub fn new(screen: ScreenQueryTool, url_seen: Arc<UrlSeen>, opener: Arc<dyn Opener>) -> Self {
        Self {
            screen,
            url_seen,
            opener,
        }
    }

    pub fn definition() -> ToolDefinition {
        ToolDefinition {
            name: WEB_SEARCH_TOOL.into(),
            description: "Search the web — or a specific site — in ONE call: opens the \
                          results page in the browser and returns what is on screen with \
                          exact click coordinates (cx,cy). THE way to find anything online: \
                          never build search URLs by hand or type into the address bar. \
                          Then mouse-click the best result and read_page it."
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "The search words, e.g. \"half life 2\"."
                    },
                    "site": {
                        "type": "string",
                        "enum": ["google", "ebay", "amazon", "youtube"],
                        "description": "Where to search (default google). Use the site the user named — searching ebay searches EBAY's listings directly."
                    }
                },
                "required": ["query"]
            }),
        }
    }
}

#[async_trait]
impl ToolExecutor for WebSearchTool {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![Self::definition()]
    }

    fn claims(&self, name: &str) -> bool {
        name == WEB_SEARCH_TOOL
    }

    async fn execute(&self, call: &ToolCall) -> ToolOutcome {
        let args: serde_json::Value = match serde_json::from_str(&call.arguments) {
            Ok(args) => args,
            Err(e) => {
                return ToolOutcome::failure(
                    "invalid-arguments",
                    format!("invalid {WEB_SEARCH_TOOL} arguments: {e}"),
                )
            }
        };
        let query = args
            .get("query")
            .and_then(|q| q.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        if query.is_empty() {
            return ToolOutcome::failure("invalid-arguments", "query must not be empty");
        }
        let site = args
            .get("site")
            .and_then(|s| s.as_str())
            .unwrap_or("google")
            .to_lowercase();
        let url = search_url(&site, &query);
        // The templated URL is by construction legitimate navigation.
        self.url_seen.harvest(&url);
        if let Err(e) = self.opener.open(&url).await {
            return ToolOutcome::failure("open-failed", format!("could not open {url}: {e}"));
        }
        log::info!("llm: web_search site={site} url={url}");
        tokio::time::sleep(std::time::Duration::from_millis(WEB_SEARCH_SETTLE_MS)).await;
        // The same harvest as screen_query: results with grounded cx/cy.
        let screen_call = ToolCall {
            id: call.id.clone(),
            name: SCREEN_QUERY_TOOL.into(),
            arguments: "{}".into(),
        };
        let screen = self.screen.execute(&screen_call).await;
        if !screen.ok {
            return ToolOutcome::failure(
                "screen-read-failed",
                format!(
                    "opened {url}, but reading the results screen failed: {}",
                    screen.content
                ),
            );
        }
        ToolOutcome::success(format!(
            "Opened the {site} search results for \"{query}\" ({url}). What is on screen now \
             (click a result via its cx,cy, then read_page):\n{}",
            screen.content
        ))
    }
}

/// On-demand memory (user request 2026-07-31): "remember that my name is
/// Alex" becomes a stored memory the moment the user asks — deterministic,
/// unlike hoping the passive chat distiller keeps the detail. Stored with
/// `Told` provenance (the user chose these words), embedded best-effort so
/// semantic recall works, visible and deletable in the memory window like
/// every other memory.
pub struct RememberTool {
    store: Arc<MemoryStore>,
    embedder: Arc<dyn Embedder>,
}

/// Longest fact the tool stores — memories are one-liners, not documents.
const REMEMBER_MAX_CHARS: usize = 500;

impl RememberTool {
    pub fn new(store: Arc<MemoryStore>, embedder: Arc<dyn Embedder>) -> Self {
        Self { store, embedder }
    }

    pub fn definition() -> ToolDefinition {
        ToolDefinition {
            name: REMEMBER_TOOL.into(),
            description: "Store one fact in persistent memory, exactly when the user asks you \
                          to remember something (\"remember that…\", \"save this\", \"don't \
                          forget…\"). Write the fact as ONE concise self-contained sentence \
                          (\"The user's name is Alex\"), never a whole conversation. It survives \
                          restarts and is found later by memory_search. Do not store secrets \
                          (passwords, keys) — tell the user you won't keep those."
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "fact": {
                        "type": "string",
                        "description": "The one-sentence fact to keep, self-contained."
                    },
                    "category": {
                        "type": "string",
                        "enum": ["development", "browsing", "communication", "writing", "media",
                                 "shopping", "reference", "personal", "system", "other"],
                        "description": "Where this fact belongs; default personal."
                    },
                    "tags": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Up to 5 short lowercase keywords (optional)."
                    }
                },
                "required": ["fact"]
            }),
        }
    }
}

#[derive(Debug, serde::Deserialize)]
struct RememberArgs {
    fact: String,
    #[serde(default)]
    category: Option<String>,
    #[serde(default)]
    tags: Option<Vec<String>>,
}

#[async_trait]
impl ToolExecutor for RememberTool {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![Self::definition()]
    }

    async fn execute(&self, call: &ToolCall) -> ToolOutcome {
        if call.name != REMEMBER_TOOL {
            return ToolOutcome::failure(
                "unknown-tool",
                format!("unknown tool: {} (available: {REMEMBER_TOOL})", call.name),
            );
        }
        let args: RememberArgs = match serde_json::from_str(&call.arguments) {
            Ok(args) => args,
            Err(e) => {
                return ToolOutcome::failure(
                    "invalid-arguments",
                    format!("invalid {REMEMBER_TOOL} arguments: {e}"),
                )
            }
        };
        let fact = args.fact.trim();
        if fact.is_empty() {
            return ToolOutcome::failure(
                "invalid-arguments",
                "fact must not be empty — one concise sentence to keep",
            );
        }
        let fact: String = fact.chars().take(REMEMBER_MAX_CHARS).collect();
        // Best-effort embedding: a down embedder means keyword-only recall
        // for this row, never a failed save.
        let embedding = match self.embedder.embed(std::slice::from_ref(&fact)).await {
            Ok(mut vectors) if !vectors.is_empty() => Some(vectors.remove(0)),
            Ok(_) => None,
            Err(e) => {
                log::debug!("remember: embedding unavailable ({e}); keyword recall only");
                None
            }
        };
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        match self.store.insert(crate::memory::store::NewMemory {
            summary: fact.clone(),
            apps: Vec::new(),
            span_start_ms: now_ms,
            span_end_ms: now_ms,
            embedding,
            source: crate::memory::store::MemorySource::Told,
            category: args.category.unwrap_or_else(|| "personal".into()),
            tags: args.tags.unwrap_or_default(),
            // A fact the user asked to keep must not silently expire.
            pinned: true,
            expires_at_ms: None,
        }) {
            Ok(record) => ToolOutcome::success(format!(
                "remembered (memory #{}): {fact} — recall it later with memory_search; the user \
                 can see and delete it in the memory window",
                record.id
            )),
            Err(e) => ToolOutcome::failure(e.kind(), format!("saving the memory failed: {e}")),
        }
    }
}

/// Verbatim recall over stored chat transcripts (computer-control I3 made
/// the sessions; this exposes them to the model). "What recipes have I
/// asked about?" is answerable only from the actual past messages — the
/// distilled memories may have dropped the detail. Read-only over the same
/// store the Settings transcript search uses; no gate needed (it discloses
/// the user's own chat history to the user's own assistant).
pub struct ChatHistorySearchTool {
    store: Arc<MemoryStore>,
}

/// Cap on messages returned per search — the tool result enters model
/// context, so a broad query must page, not flood.
const CHAT_HISTORY_MAX_RESULTS: usize = 20;
const CHAT_HISTORY_DEFAULT_RESULTS: usize = 8;

/// Cap on one matched message's text in the result (long assistant replies
/// would blow the context for no recall value — the match is what matters).
const CHAT_HISTORY_EXCERPT_CHARS: usize = 280;

impl ChatHistorySearchTool {
    pub fn new(store: Arc<MemoryStore>) -> Self {
        Self { store }
    }

    pub fn definition() -> ToolDefinition {
        ToolDefinition {
            name: CHAT_HISTORY_SEARCH_TOOL.into(),
            description: "Search the user's PAST chat conversations with you — the stored \
                          verbatim transcripts of earlier sessions. Call this when the user \
                          asks what they asked or discussed before (e.g. \"what recipes have I \
                          asked you about?\" -> query \"recipe\"). Returns matching messages \
                          (who said it, when) newest first. Search a short keyword, not a \
                          whole sentence."
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Keyword to find in past messages, e.g. \"recipe\""
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum messages to return (optional)",
                        "minimum": 1,
                        "maximum": CHAT_HISTORY_MAX_RESULTS
                    }
                },
                "required": ["query"]
            }),
        }
    }
}

/// Head excerpt on a char boundary, `…`-suffixed when truncated.
fn head_excerpt(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let head: String = s.chars().take(max_chars).collect();
    format!("{head}…")
}

#[derive(Debug, serde::Deserialize)]
struct ChatHistorySearchArgs {
    query: String,
    #[serde(default)]
    limit: Option<usize>,
}

#[async_trait]
impl ToolExecutor for ChatHistorySearchTool {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![Self::definition()]
    }

    async fn execute(&self, call: &ToolCall) -> ToolOutcome {
        if call.name != CHAT_HISTORY_SEARCH_TOOL {
            return ToolOutcome::failure(
                "unknown-tool",
                format!(
                    "unknown tool: {} (available: {CHAT_HISTORY_SEARCH_TOOL})",
                    call.name
                ),
            );
        }
        let args: ChatHistorySearchArgs = match serde_json::from_str(&call.arguments) {
            Ok(args) => args,
            Err(e) => {
                return ToolOutcome::failure(
                    "invalid-arguments",
                    format!("invalid {CHAT_HISTORY_SEARCH_TOOL} arguments: {e}"),
                )
            }
        };
        if args.query.trim().is_empty() {
            return ToolOutcome::failure(
                "invalid-arguments",
                "query must not be empty — search a keyword like \"recipe\"",
            );
        }
        let limit = args
            .limit
            .unwrap_or(CHAT_HISTORY_DEFAULT_RESULTS)
            .clamp(1, CHAT_HISTORY_MAX_RESULTS);
        match self.store.chat_messages_matching(&args.query, limit) {
            Ok(rows) => {
                let shaped: Vec<serde_json::Value> = rows
                    .iter()
                    .map(|m| {
                        serde_json::json!({
                            "sessionId": m.session_id,
                            "role": m.role,
                            "text": head_excerpt(&m.text, CHAT_HISTORY_EXCERPT_CHARS),
                            "at": format_at_ms(m.at_ms),
                        })
                    })
                    .collect();
                let count = shaped.len();
                let content = serde_json::to_string(&shaped).unwrap_or_else(|e| {
                    format!(r#"{{"error":"result serialization failed: {e}"}}"#)
                });
                ToolOutcome {
                    content,
                    ok: true,
                    result_count: Some(count),
                    mode: None,
                    failure: None,
                    attachment_png: None,
                }
            }
            Err(err) => ToolOutcome::failure(err.kind(), err.to_string()),
        }
    }
}

/// Epoch ms → local "YYYY-MM-DD HH:MM" — the model reasons about "last
/// Tuesday", not epoch integers.
fn format_at_ms(at_ms: i64) -> String {
    use chrono::TimeZone;
    match chrono::Local.timestamp_millis_opt(at_ms) {
        chrono::LocalResult::Single(t) => t.format("%Y-%m-%d %H:%M").to_string(),
        _ => at_ms.to_string(),
    }
}

/// Read the focused app's full visible text via its accessibility tree —
/// the continuity primitive (2026-07-27): a page the model opened last
/// turn is still on screen, and "what are the ingredients in this recipe"
/// is answered by READING that page, not by claiming no access. Shares the
/// screen-query backend seam; read-only, no gate (it discloses what is
/// already on the user's screen to the user's own assistant).
pub struct ReadPageTool {
    backend: Arc<dyn ScreenQuery>,
    focused_app: Arc<FocusedApp>,
}

impl ReadPageTool {
    pub fn new(backend: Arc<dyn ScreenQuery>, focused_app: Arc<FocusedApp>) -> Self {
        Self {
            backend,
            focused_app,
        }
    }

    pub fn definition() -> ToolDefinition {
        ToolDefinition {
            name: READ_PAGE_TOOL.into(),
            description: "Read the FULL text content of the focused app's window (the whole \
                          page, not just what fits on screen) — recipes, articles, documents. \
                          Use this whenever the user asks about the content of a page that is \
                          open ('what are the ingredients', 'summarize this article', 'read it \
                          to me'), including pages you opened in an earlier turn — they are \
                          still open. Requires an app to be focused (focus_app first if none \
                          is). Returns plain text in reading order; it has no coordinates — \
                          clicking still needs screen_query."
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
impl ToolExecutor for ReadPageTool {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![Self::definition()]
    }

    async fn execute(&self, call: &ToolCall) -> ToolOutcome {
        if call.name != READ_PAGE_TOOL {
            return ToolOutcome::failure(
                "unknown-tool",
                format!("unknown tool: {} (available: {READ_PAGE_TOOL})", call.name),
            );
        }
        let Some(app) = self.focused_app.current() else {
            return ToolOutcome::failure(
                "no-focused-app",
                "no app is focused this run — call focus_app with the app whose page you want \
                 to read (e.g. the browser), then read_page",
            );
        };
        match self.backend.page_text(&app).await {
            Some(text) => {
                let chars = text.chars().count();
                ToolOutcome::success(format!(
                    "[text of the frontmost {app} content, {chars} chars]\n{text}"
                ))
            }
            None => ToolOutcome::failure(
                "no-content",
                format!(
                    "{app} exposed no readable text (no accessibility content, or nothing is \
                     open) — take_screenshot to LOOK at the screen instead"
                ),
            ),
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
    /// The app the model last `focus_app`'d this run — the INTENT the
    /// post-action `verified` evidence is checked against. When the readback
    /// shows focus in a different app, the result flips to a typed
    /// [`VERIFICATION_FAILED_KIND`] failure so the model must re-aim instead
    /// of narrating success (the reinforcement loop, M008). Same shared holder
    /// the `ScreenQueryTool` filter and `ApprovalGate` use.
    focused_app: Arc<FocusedApp>,
}

impl InputTool {
    pub fn new(
        backend: Arc<dyn InputControl>,
        arm: Arc<HidArmState>,
        focused_app: Arc<FocusedApp>,
    ) -> Self {
        Self {
            backend,
            arm,
            focused_app,
        }
    }

    /// The model-facing definition. `action` is required and discriminates the
    /// remaining fields; the per-field descriptions name which action each
    /// belongs to so a small model can fill just the ones it needs.
    pub fn definition() -> ToolDefinition {
        ToolDefinition {
            name: INPUT_ACTION_TOOL.into(),
            description: "Drive this computer's mouse and keyboard: click (single, double, or \
                          triple), drag (press-glide-release — select text, move things), scroll \
                          the wheel, move the mouse, type text, or press a key with optional \
                          modifiers (cmd/ctrl/alt/shift — e.g. cmd+c to copy, cmd+a to select all). Coordinates are absolute screen pixels and MUST come from a \
                          screen_query result — never guess an x/y. To click something: call \
                          focus_app to bring the app to the front, then screen_query to read its \
                          on-screen elements, then mouse-click passing the target element's cx,cy \
                          verbatim as x,y (the click moves there and clicks in one step). A click \
                          or move to a guessed coordinate is refused. Every result includes a \
                          `verified` block measured from the OS AFTER the action: `cursor` (where \
                          the mouse really is), for clicks `clickedElement` (the UI element that \
                          was under the click — check its role/title matches what you aimed at), \
                          `focus` (the app and UI element that now holds \
                          keyboard focus), and for type-text `textEntered` (whether the typed text \
                          was observed in the focused field). ALWAYS check `verified` before your \
                          next step: if it does not match what you intended (wrong app in focus, \
                          textEntered false), the action landed somewhere else — re-run \
                          screen_query and correct instead of continuing."
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["mouse-move", "mouse-click", "mouse-drag", "scroll", "type-text", "key-press"],
                        "description": "Which HID action to perform."
                    },
                    "x": {
                        "type": "integer",
                        "description": "mouse-move / mouse-click: absolute screen X — pass the \
                                        target element's cx from screen_query verbatim. For \
                                        mouse-click, pass x and y together to move to the target \
                                        then click it."
                    },
                    "y": {
                        "type": "integer",
                        "description": "mouse-move / mouse-click: absolute screen Y — pass the \
                                        target element's cy from screen_query verbatim. For \
                                        mouse-click, pass x and y together to move to the target \
                                        then click it."
                    },
                    "button": {
                        "type": "string",
                        "enum": ["left", "right", "middle"],
                        "description": "mouse-click / mouse-drag: which mouse button (default left)."
                    },
                    "clicks": {
                        "type": "integer",
                        "enum": [1, 2, 3],
                        "description": "mouse-click: 2 double-clicks (open a file, select a word), \
                                        3 triple-clicks (select a whole line). Default 1."
                    },
                    "fromX": { "type": "integer", "description": "mouse-drag: drag start X — the start element's cx from screen_query." },
                    "fromY": { "type": "integer", "description": "mouse-drag: drag start Y — the start element's cy from screen_query." },
                    "toX": { "type": "integer", "description": "mouse-drag: drag end X — the end element's cx from screen_query." },
                    "toY": { "type": "integer", "description": "mouse-drag: drag end Y — the end element's cy from screen_query." },
                    "deltaX": {
                        "type": "integer",
                        "description": "scroll: horizontal wheel lines (positive scrolls right)."
                    },
                    "deltaY": {
                        "type": "integer",
                        "description": "scroll: vertical wheel lines — positive scrolls the content \
                                        DOWN (to see further), negative scrolls back up. Use with \
                                        optional x/y to aim the pane first."
                    },
                    "modifiers": {
                        "type": "array",
                        "items": { "type": "string", "enum": ["cmd", "ctrl", "alt", "shift"] },
                        "description": "key-press: modifiers held while pressing the key — e.g. \
                                        [\"cmd\"] with key \"c\" copies, [\"cmd\"] + \"a\" selects all, \
                                        [\"cmd\"] + \"v\" pastes."
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
            log::warn!(
                "llm: input_action refused — HID disarmed (kind={})",
                err.kind()
            );
            return ToolOutcome::failure(err.kind(), err.to_string());
        }
        if call.name != INPUT_ACTION_TOOL {
            return ToolOutcome::failure(
                "unknown-tool",
                format!(
                    "unknown tool: {} (available: {INPUT_ACTION_TOOL})",
                    call.name
                ),
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
            // `verified` is the backend's post-action readback (cursor, focused
            // element, text-entry confirmation) — evidence of the action's
            // EFFECT, so the model can validate instead of assuming (R007).
            Ok(report) => {
                // The reinforcement loop's structural half: when the evidence
                // positively contradicts the intent (focus landed in a
                // different app than the one the model focused), the result
                // itself becomes a typed failure — the model cannot sail past
                // it, it must observe → re-aim → retry. The event DID fire;
                // `verified` rides along so the model sees what really
                // happened.
                if let Some(contradiction) =
                    verify_against_intent(&report, self.focused_app.current().as_deref())
                {
                    log::warn!("llm: input_action {VERIFICATION_FAILED_KIND}: {contradiction}");
                    return ToolOutcome {
                        content: serde_json::json!({
                            "ok": false,
                            "error": contradiction,
                            "performed": performed,
                            "verified": report,
                        })
                        .to_string(),
                        ok: false,
                        result_count: None,
                        mode: None,
                        failure: Some(VERIFICATION_FAILED_KIND.to_string()),
                        attachment_png: None,
                    };
                }
                ToolOutcome {
                    content: serde_json::json!({
                        "ok": true,
                        "performed": performed,
                        "verified": report,
                    })
                    .to_string(),
                    ok: true,
                    result_count: None,
                    mode: None,
                    failure: None,
                    attachment_png: None,
                }
            }
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
        Self {
            backend,
            screen_seen,
            focused_app,
        }
    }

    /// The model-facing definition. No arguments: a screen query is a snapshot
    /// of whatever is on screen right now, so a small model can call it with an
    /// empty object.
    pub fn definition() -> ToolDefinition {
        ToolDefinition {
            name: SCREEN_QUERY_TOOL.into(),
            description: "Return the focused app's on-screen elements, each with absolute \
                          screen coordinates: cx, cy is the element's exact centre — the \
                          ready-made click target — plus x, y (top-left corner), width and \
                          height for context. Elements with a `role` (AXButton, AXLink, \
                          AXTextField, …) are the app's REAL interactive controls with exact \
                          frames — ALWAYS prefer clicking one of those over plain recognized \
                          text when both match your target. To click an element, pass its cx as \
                          x and cy as y to input_action verbatim; do not compute your own \
                          coordinates."
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
                format!(
                    "unknown tool: {} (available: {SCREEN_QUERY_TOOL})",
                    call.name
                ),
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
                // Accuracy v2: harvest the focused app's REAL interactive
                // controls from its accessibility tree and put them first —
                // exact frames, no OCR quantization. OCR lines centered
                // inside an AX control are that control's label and drop.
                let elements = match &focused {
                    Some(app) => {
                        let ax = self.backend.interactive(app).await;
                        if !ax.is_empty() {
                            log::debug!(
                                "screen_query: merged {} AX element(s) for {app:?}",
                                ax.len()
                            );
                        }
                        crate::screenquery::merge_ax_and_ocr(ax, elements)
                    }
                    None => elements,
                };
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
                    attachment_png: None,
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
                // truthfully; `visibleWindows` is the "frontmost but nothing
                // on screen" detector — 0 means the app has NO open window
                // and the user sees nothing until one is opened (cmd+n).
                content: serde_json::json!({
                    "ok": true,
                    "focused": focused.app,
                    "launched": focused.launched,
                    "visibleWindows": focused.visible_windows,
                    "warning": if focused.visible_windows == Some(0) {
                        Some("the app is frontmost but has ZERO visible windows — the user sees nothing; open a window (key-press \"n\" with modifiers [\"cmd\"]) or tell the user")
                    } else { None },
                })
                .to_string(),
                ok: true,
                result_count: None,
                mode: None,
                failure: None,
                attachment_png: None,
            },
            // A not-found carries the running-app candidates back to the model so
            // it can retry against a real name; other typed errors ride their
            // kind back unchanged (R007).
            Err(AppFocusError::NotFound {
                requested,
                candidates,
            }) => ToolOutcome {
                content: serde_json::json!({
                    "error": format!("no running or installed app matched {requested:?}"),
                    "candidates": candidates,
                })
                .to_string(),
                ok: false,
                result_count: None,
                mode: None,
                failure: Some("not-found".to_string()),
                attachment_png: None,
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
        self.executors
            .iter()
            .flat_map(|e| e.definitions())
            .collect()
    }

    async fn execute(&self, call: &ToolCall) -> ToolOutcome {
        for executor in &self.executors {
            if executor.claims(&call.name) {
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
    /// Perform this action and grant it PERMANENTLY (user request
    /// 2026-07-27): the production prompt persists the grant — the action
    /// kind for HID kinds, the command's first token into the command
    /// allowlist for `run_command` — then downgrades the verdict before the
    /// gate sees it, so gate logic never changes. Revocable in Settings.
    AllowAlways,
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
        Self {
            inner,
            focus,
            mode,
            whitelist,
            approver,
            screen_seen,
            focused_app,
        }
    }

    /// A human sentence describing the pending action — what the overlay shows so
    /// the user knows exactly what they are approving. Pixel coordinates and typed
    /// text are transient prompt context only; they never persist (R011/R023).
    fn summary(action: &InputAction) -> String {
        match action {
            InputAction::MouseMove { x, y } => format!("Move the mouse to ({x}, {y})"),
            InputAction::MouseDrag {
                button,
                from_x,
                from_y,
                to_x,
                to_y,
            } => format!(
                "Drag the {} mouse button from ({from_x}, {from_y}) to ({to_x}, {to_y})",
                button_name(*button)
            ),
            InputAction::Scroll {
                delta_x, delta_y, ..
            } => format!(
                "Scroll the page (deltaX {}, deltaY {})",
                delta_x.unwrap_or(0),
                delta_y.unwrap_or(0)
            ),
            InputAction::MouseClick {
                button,
                x: Some(x),
                y: Some(y),
                clicks,
            } => {
                let times = match clicks.unwrap_or(1) {
                    2 => "Double-click",
                    3 => "Triple-click",
                    _ => "Click",
                };
                format!(
                    "{times} the {} mouse button at ({x}, {y})",
                    button_name(*button)
                )
            }
            InputAction::MouseClick { button, .. } => {
                format!("Click the {} mouse button", button_name(*button))
            }
            InputAction::TypeText { text } => format!("Type {}", quote_preview(text)),
            InputAction::KeyPress { key, modifiers } => match modifiers {
                Some(mods) if !mods.is_empty() => {
                    format!("Press {}+{}", mods.join("+"), key)
                }
                _ => format!("Press the {key} key"),
            },
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
                    ApprovalVerdict::AllowAlways | ApprovalVerdict::AllowKind => {
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
            let summary = format!(
                "Open {} (bring to front, launching if needed)",
                quote_preview(&app)
            );
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
                         screen_query and pass its cx,cy verbatim as the click's x,y. If the \
                         target is not among them, call screen_query again (or focus_app then \
                         screen_query) — never invent a coordinate."
                    ),
                );
            }
        }
        let kind = action.kind();
        let summary = Self::summary(&action);
        self.gate_and_run(INPUT_ACTION_TOOL, kind, summary, || {
            self.inner.execute(call)
        })
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
        Self {
            outcome,
            stopped: false,
        }
    }

    /// A user-stopped run: whatever streamed before the stop, no tool calls
    /// leaking out, flagged so the caller surfaces the `stopped` run-state.
    fn stopped(text: String, token_count: usize) -> Self {
        Self {
            outcome: StreamOutcome {
                text,
                token_count,
                tool_calls: Vec::new(),
                prompt_tokens: None,
                completion_tokens: None,
            },
            stopped: true,
        }
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
// Three sinks + a stop signal are the injected-effects contract, not a design
// smell to bundle away; the arity is deliberate.
#[allow(clippy::too_many_arguments)]
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
    // Real token spend, SUMMED across every round of the run (2026-08-03):
    // a tool run is many requests, each re-paying the prompt.
    let mut total_prompt: u64 = 0;
    let mut total_completion: u64 = 0;
    let mut saw_usage = false;
    // Repeat breaker: exact (tool, arguments) execution counts this run.
    let mut call_counts: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
    for round in 0..=MAX_TOOL_ROUNDS {
        // Stop observed between rounds: terminate before issuing the next
        // request, with the text streamed so far (R006 — never silent).
        if should_stop() {
            log::info!(
                "llm: tool loop stopped by user before round {round} (request={request_id})"
            );
            return Ok(LoopOutcome::stopped(streamed_text, streamed_tokens));
        }
        let tools = if round < MAX_TOOL_ROUNDS {
            executor.definitions()
        } else {
            Vec::new()
        };
        let final_round = tools.is_empty();
        let request = ChatRequest {
            messages: std::mem::take(&mut messages),
            tools,
        };
        let outcome = client
            .stream_chat_reasoning(&request, on_token, on_reasoning)
            .await?;
        messages = request.messages;
        streamed_text = outcome.text.clone();
        streamed_tokens = outcome.token_count;
        if let (Some(prompt), Some(completion)) = (outcome.prompt_tokens, outcome.completion_tokens)
        {
            saw_usage = true;
            total_prompt += prompt;
            total_completion += completion;
        }
        let run_usage = |outcome: StreamOutcome| StreamOutcome {
            prompt_tokens: saw_usage.then_some(total_prompt),
            completion_tokens: saw_usage.then_some(total_completion),
            ..outcome
        };

        if outcome.tool_calls.is_empty() {
            if round > 0 {
                log::info!(
                    "llm: tool loop done after {round} tool round(s) (request={request_id})"
                );
            }
            return Ok(LoopOutcome::done(run_usage(outcome)));
        }
        if final_round {
            // The tools-stripped round still "called" a tool the request
            // never offered — terminate with the text we have rather than
            // loop; never silence (R006).
            log::warn!(
                "llm: tool call on the tools-stripped final round ignored (request={request_id})"
            );
            return Ok(LoopOutcome::done(run_usage(StreamOutcome {
                tool_calls: Vec::new(),
                ..outcome
            })));
        }

        // First half of the OpenAI round-trip: echo the requested calls.
        messages.push(ChatMessage::assistant_tool_calls(
            outcome.text.clone(),
            outcome.tool_calls.clone(),
        ));

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
                return Ok(LoopOutcome::stopped(
                    outcome.text.clone(),
                    outcome.token_count,
                ));
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

            let repeat_key = format!("{}\x01{}", call.name, call.arguments.trim());
            let seen = call_counts.entry(repeat_key).or_insert(0);
            *seen += 1;
            let result = if *seen > REPEATED_CALL_LIMIT {
                // The same exact call for the (LIMIT+1)th time: executing it
                // again cannot produce new information — refuse typed and
                // force a strategy change (or an honest report).
                ToolOutcome::failure(
                    REPEATED_CALL_KIND,
                    format!(
                        "you have already called {} with these EXACT arguments {} times this                          run — repeating it will give the same result. STOP repeating. Either                          solve the underlying problem a genuinely different way (different                          content, different command, different tool), or end the run and tell                          the user plainly what you tried and what is failing.",
                        call.name, REPEATED_CALL_LIMIT
                    ),
                )
            } else {
                executor.execute(call).await
            };
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
                preview: result_preview(&call.name, &result.content),
            }));

            // Second half of the round-trip: the tool-role answer.
            messages.push(ChatMessage::tool_result(&call.id, result.content));
            // A screenshot rides as a follow-up vision user turn — the chat
            // API's tool role is text-only. Marked so the model knows the
            // image answers its own call, not a new user question.
            if let Some(base64_png) = result.attachment_png {
                messages.push(
                    ChatMessage::user(
                        "[the screenshot your take_screenshot call captured — look at it]",
                    )
                    .with_attachments(vec![crate::llm::Attachment { base64_png }]),
                );
            }
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
            LlmHealth {
                online: true,
                endpoint: self.endpoint().into(),
            }
        }
    }

    fn text_outcome(text: &str) -> Result<StreamOutcome, LlmError> {
        Ok(StreamOutcome {
            text: text.into(),
            token_count: 1,
            tool_calls: Vec::new(),
            prompt_tokens: None,
            completion_tokens: None,
        })
    }

    fn tool_call_outcome(calls: Vec<ToolCall>) -> Result<StreamOutcome, LlmError> {
        Ok(StreamOutcome {
            text: String::new(),
            token_count: 0,
            tool_calls: calls,
            prompt_tokens: None,
            completion_tokens: None,
        })
    }

    fn search_call(id: &str, args: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: MEMORY_SEARCH_TOOL.into(),
            arguments: args.into(),
        }
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
            Err(LlmError::Offline {
                endpoint: self.endpoint().into(),
                detail: "down".into(),
            })
        }
    }

    #[tokio::test]
    async fn chat_history_search_returns_matching_past_messages() {
        let store = MemoryStore::open_in_memory().unwrap();
        let session = store.chat_session_create(1_000).unwrap();
        store
            .chat_append_exchange(
                session,
                "find me a good carbonara recipe",
                &format!("Here is RecipeTinEats carbonara. {}", "x".repeat(400)),
                1_753_500_000_000,
            )
            .unwrap();
        let tool = ChatHistorySearchTool::new(Arc::new(store));
        let outcome = tool
            .execute(&ToolCall {
                id: "c1".into(),
                name: CHAT_HISTORY_SEARCH_TOOL.into(),
                arguments: r#"{"query":"recipe"}"#.into(),
            })
            .await;
        assert!(outcome.ok, "{:?}", outcome.failure);
        assert_eq!(outcome.result_count, Some(2));
        let rows: Vec<serde_json::Value> = serde_json::from_str(&outcome.content).unwrap();
        // Newest first: the long assistant reply is excerpted, the user
        // question verbatim; both carry a readable local timestamp.
        assert_eq!(rows[1]["role"], "user");
        assert_eq!(rows[1]["text"], "find me a good carbonara recipe");
        assert!(rows[0]["text"].as_str().unwrap().ends_with('…'));
        assert!(rows[0]["text"].as_str().unwrap().chars().count() <= 281);
        assert!(rows[0]["at"].as_str().unwrap().starts_with("20"));

        // A miss is an honest empty result, not an error.
        let miss = tool
            .execute(&ToolCall {
                id: "c2".into(),
                name: CHAT_HISTORY_SEARCH_TOOL.into(),
                arguments: r#"{"query":"gnocchi"}"#.into(),
            })
            .await;
        assert!(miss.ok);
        assert_eq!(miss.result_count, Some(0));

        // Empty/blank query is a typed refusal.
        let blank = tool
            .execute(&ToolCall {
                id: "c3".into(),
                name: CHAT_HISTORY_SEARCH_TOOL.into(),
                arguments: r#"{"query":"  "}"#.into(),
            })
            .await;
        assert!(!blank.ok);
        assert_eq!(blank.failure.as_deref(), Some("invalid-arguments"));
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
                source: crate::memory::store::MemorySource::Watcher,
                category: "other".into(),
                tags: Vec::new(),
                pinned: false,
                expires_at_ms: None,
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
            Self {
                events: Mutex::new(Vec::new()),
                tokens: Mutex::new(String::new()),
            }
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
        assert_eq!(
            requests[0].tools.len(),
            1,
            "first round must advertise memory_search"
        );
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
        let ToolEvent::Call(call) = &events[0] else {
            panic!("first event must be Call")
        };
        assert_eq!(call.request_id, 7);
        assert_eq!(call.round, 0);
        assert_eq!(call.call.name, MEMORY_SEARCH_TOOL);
        let ToolEvent::Result(result) = &events[1] else {
            panic!("second event must be Result")
        };
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
        #[allow(clippy::assertions_on_constants)] // the constant relation IS the documented claim
        {
            assert!(ROUNDS > 3, "must exceed the retired S03 3-round cap");
            assert!(
                ROUNDS < MAX_TOOL_ROUNDS,
                "must resolve on the model's stop, not the ceiling"
            );
        }
        let mut responses: Vec<Result<StreamOutcome, LlmError>> = (0..ROUNDS)
            .map(|i| {
                tool_call_outcome(vec![search_call(
                    &format!("call_{i}"),
                    r#"{"query":"again"}"#,
                )])
            })
            .collect();
        responses.push(text_outcome("done after six rounds"));
        let client = ScriptedClient::new(responses);
        let capture = Capture::new();
        let outcome = run(&client, &seeded_tool(), &capture).await.unwrap();
        assert_eq!(outcome.text, "done after six rounds");

        let requests = client.requests();
        assert_eq!(
            requests.len(),
            ROUNDS + 1,
            "six tool rounds plus the model's text answer"
        );
        // Every issued round still carried tools — the ceiling was never reached.
        for req in &requests {
            assert_eq!(
                req.tools.len(),
                1,
                "the loop stopped on the model, not the tools-strip"
            );
        }
        assert_eq!(
            capture.events().len(),
            ROUNDS * 2,
            "one call + one result per round"
        );
    }

    #[tokio::test]
    async fn tool_call_on_stripped_final_round_terminates_without_dispatch() {
        // Defensive bound: even if the model "calls" a tool when none were
        // offered, the loop ends — no dispatch, no extra request.
        let mut responses: Vec<Result<StreamOutcome, LlmError>> = (0..MAX_TOOL_ROUNDS)
            .map(|i| tool_call_outcome(vec![search_call(&format!("call_{i}"), r#"{"query":"q"}"#)]))
            .collect();
        responses.push(tool_call_outcome(vec![search_call(
            "call_zombie",
            r#"{"query":"q"}"#,
        )]));
        let client = ScriptedClient::new(responses);
        let capture = Capture::new();
        let outcome = run(&client, &seeded_tool(), &capture).await.unwrap();
        assert!(
            outcome.tool_calls.is_empty(),
            "zombie calls must not leak out of the loop"
        );
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
        let executor = StopAfterExecute {
            inner: seeded_tool(),
            stop: stop.clone(),
        };

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

        assert!(
            loop_outcome.stopped,
            "a mid-loop stop must surface a typed stopped outcome"
        );
        assert!(
            loop_outcome.outcome.tool_calls.is_empty(),
            "a stopped run must not leak tool calls",
        );
        // Only round 0 was issued: the loop stopped at the top of round 1,
        // before the second request and before any round-1 dispatch.
        assert_eq!(
            client.requests().len(),
            1,
            "no request may be issued after the stop"
        );
        assert_eq!(
            capture.events().len(),
            2,
            "no tool dispatch past the stop (round 0 only)"
        );
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
        assert!(
            !loop_outcome.stopped,
            "a natural finish is never flagged stopped"
        );
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
        assert_eq!(
            outcome.text, "answered without memory",
            "loop must survive bad arguments"
        );

        let ToolEvent::Result(result) = &capture.events()[1] else {
            panic!("expected Result")
        };
        assert!(!result.ok);
        assert_eq!(result.failure.as_deref(), Some("invalid-arguments"));
        assert_eq!(result.result_count, None);

        // The model sees a structured error payload, not silence.
        let followup = &client.requests()[1].messages;
        let body: serde_json::Value = serde_json::from_str(&followup[2].content).unwrap();
        assert!(body["error"]
            .as_str()
            .unwrap()
            .contains("invalid memory_search arguments"));
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
        let ToolEvent::Result(result) = &capture.events()[1] else {
            panic!("expected Result")
        };
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
        let outcome = tool
            .execute(&search_call("c", r#"{"query":"watcher","limit":0}"#))
            .await;
        assert!(outcome.ok);
    }

    #[tokio::test]
    async fn memory_search_content_is_the_search_outcome_json() {
        let outcome = seeded_tool()
            .execute(&search_call("c", r#"{"query":"broadcast lag"}"#))
            .await;
        let v: serde_json::Value = serde_json::from_str(&outcome.content).unwrap();
        assert_eq!(v["mode"], "keyword");
        assert_eq!(
            v["results"][0]["summary"],
            "Debugged the tokio broadcast lag in the watcher loop"
        );
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
            preview: None,
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
    use crate::input::{
        ActionReport, FocusReport, InputAction, InputControl, InputError, InputPermission,
        MouseButton,
    };

    /// Records the last performed action so delegation through the tool +
    /// composite can be asserted without touching real HID. Returns a
    /// distinctive [`ActionReport`] so the `verified` passthrough is pinnable.
    struct RecordingInput {
        last: Mutex<Option<InputAction>>,
    }

    impl RecordingInput {
        fn new() -> Self {
            Self {
                last: Mutex::new(None),
            }
        }
    }

    #[async_trait]
    impl InputControl for RecordingInput {
        fn permission(&self) -> InputPermission {
            InputPermission {
                granted: true,
                supported: true,
            }
        }

        fn request_permission(&self) -> bool {
            true
        }

        async fn perform(&self, action: InputAction) -> Result<ActionReport, InputError> {
            *self.last.lock().unwrap() = Some(action);
            Ok(ActionReport {
                cursor: None,
                focus: Some(FocusReport {
                    app: Some("Mock App".into()),
                    role: Some("AXTextField".into()),
                    title: None,
                    value: None,
                }),
                text_entered: None,
                clicked_element: None,
            })
        }
    }

    fn input_call(id: &str, args: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: INPUT_ACTION_TOOL.into(),
            arguments: args.into(),
        }
    }

    /// An armed arm-state — the default posture for the delegation tests below.
    /// The structural-gate tests build their own disarmed holder.
    fn armed_arm() -> Arc<HidArmState> {
        Arc::new(HidArmState::new(true))
    }

    /// A never-focused FocusedApp — post-action verification is inert until a
    /// focus_app stores an intent, so this is the delegation tests' default.
    /// The verification tests build their own focused holder.
    fn unfocused() -> Arc<FocusedApp> {
        Arc::new(FocusedApp::new())
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
        let tool = InputTool::new(backend.clone(), armed_arm(), unfocused());
        let outcome = tool
            .execute(&input_call(
                "c1",
                r#"{"action":"mouse-click","button":"right"}"#,
            ))
            .await;
        assert!(outcome.ok);
        assert_eq!(outcome.failure, None);
        // The action really reached the backend.
        assert_eq!(
            *backend.last.lock().unwrap(),
            Some(InputAction::click(MouseButton::Right)),
        );
        // The model sees a structured confirmation echoing what was synthesized
        // PLUS the backend's post-action verification evidence.
        let v: serde_json::Value = serde_json::from_str(&outcome.content).unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["performed"]["action"], "mouse-click");
        assert_eq!(v["performed"]["button"], "right");
        assert_eq!(
            v["verified"]["focus"]["app"], "Mock App",
            "the backend's ActionReport must ride the result as `verified`"
        );
        assert_eq!(v["verified"]["focus"]["role"], "AXTextField");
    }

    #[tokio::test]
    async fn input_tool_focus_in_another_app_is_a_typed_verification_failure() {
        // The reinforcement loop's structural half: RecordingInput reports
        // post-action focus in "Mock App"; with Chrome as the focused intent,
        // the contradiction must flip the result to a typed failure carrying
        // the evidence — the model cannot narrate past it.
        let backend = Arc::new(RecordingInput::new());
        let focused = Arc::new(FocusedApp::new());
        focused.set("Google Chrome");
        let tool = InputTool::new(backend.clone(), armed_arm(), focused);
        let outcome = tool
            .execute(&input_call(
                "c1",
                r#"{"action":"type-text","text":"farts"}"#,
            ))
            .await;
        assert!(
            !outcome.ok,
            "a wrong-app focus readback must fail the action"
        );
        assert_eq!(outcome.failure.as_deref(), Some(VERIFICATION_FAILED_KIND));
        // The action DID reach the backend — the failure is about its EFFECT,
        // after the fact, not a refusal to act.
        assert!(
            backend.last.lock().unwrap().is_some(),
            "the action itself was performed"
        );
        let v: serde_json::Value = serde_json::from_str(&outcome.content).unwrap();
        assert_eq!(v["ok"], false);
        assert_eq!(
            v["verified"]["focus"]["app"], "Mock App",
            "the evidence must ride the failure so the model sees what happened"
        );
        let error = v["error"].as_str().unwrap();
        assert!(
            error.contains("Mock App") && error.contains("Google Chrome"),
            "the detail must name both the observed and intended app: {error}"
        );
        assert!(
            error.contains("screen_query"),
            "the detail must teach the recovery: {error}"
        );
    }

    #[tokio::test]
    async fn input_tool_matching_focus_passes_verification_case_insensitively() {
        // Same app in a different casing is the SAME intent — localized names
        // match case-insensitively everywhere else (FocusedApp::filter).
        let focused = Arc::new(FocusedApp::new());
        focused.set("mock APP");
        let tool = InputTool::new(Arc::new(RecordingInput::new()), armed_arm(), focused);
        let outcome = tool
            .execute(&input_call(
                "c1",
                r#"{"action":"mouse-click","button":"left"}"#,
            ))
            .await;
        assert!(
            outcome.ok,
            "focus inside the focused app must pass: {}",
            outcome.content
        );
        assert_eq!(outcome.failure, None);
    }

    #[test]
    fn url_extraction_normalizes_and_trims_punctuation() {
        let urls = extract_urls(
            "see https://RecipeTinEats.com/lasagna/, and (https://a.com/b?q=1) or text.",
        );
        assert_eq!(
            urls,
            vec![
                "https://recipetineats.com/lasagna".to_string(),
                "https://a.com/b?q=1".to_string(),
            ]
        );
        assert!(extract_urls("no urls here, not even http alone").is_empty());
    }

    #[test]
    fn open_by_default_is_homepages_and_search_results_only() {
        assert!(url_is_open_by_default("https://recipetineats.com"));
        assert!(url_is_open_by_default(
            "https://www.google.com/search?q=lasagne+recipes"
        ));
        assert!(url_is_open_by_default("https://duckduckgo.com/html?q=x"));
        // Deep content paths need grounding.
        assert!(!url_is_open_by_default("https://recipetineats.com/lasagna"));
        assert!(!url_is_open_by_default(
            "https://www.allrecipes.com/recipe/23600/worlds-best-lasagna"
        ));
        // A search-looking path on a random host is still deep.
        assert!(!url_is_open_by_default("https://evil.com/search?q=x"));
    }

    #[test]
    fn open_command_url_scopes_to_open_commands() {
        assert_eq!(
            open_command_url("open \"https://a.com/b\""),
            Some("https://a.com/b".to_string())
        );
        assert_eq!(open_command_url("open -a \"Google Chrome\""), None);
        assert_eq!(open_command_url("curl https://a.com/deep/path"), None);
        assert_eq!(open_command_url("date"), None);
    }

    #[test]
    fn url_seen_grounds_exact_normalized_urls() {
        let seen = UrlSeen::new();
        seen.harvest("the page linked https://RecipeTinEats.com/lasagna/ today");
        assert!(seen.contains("https://recipetineats.com/lasagna"));
        assert!(!seen.contains("https://recipetineats.com/carbonara"));
    }

    #[test]
    fn verify_against_intent_fires_only_on_positive_contradiction() {
        let observed = |app: &str| ActionReport {
            focus: Some(FocusReport {
                app: Some(app.into()),
                ..FocusReport::default()
            }),
            ..ActionReport::default()
        };
        // No focused app yet (pre-focus actions) → inert.
        assert_eq!(verify_against_intent(&observed("Other"), None), None);
        // No readback at all (fallback backends, mouse-move) → inert.
        assert_eq!(
            verify_against_intent(&ActionReport::default(), Some("Chrome")),
            None
        );
        // Readback present but the app unattributed → inert: absence of
        // evidence is not evidence of wrongness.
        let no_app = ActionReport {
            focus: Some(FocusReport::default()),
            ..ActionReport::default()
        };
        assert_eq!(verify_against_intent(&no_app, Some("Chrome")), None);
        // Same app, any casing → pass.
        assert_eq!(
            verify_against_intent(&observed("google chrome"), Some("Google Chrome")),
            None
        );
        // A DIFFERENT app is the one positive contradiction — named on both ends.
        let detail = verify_against_intent(&observed("Third Eye"), Some("Google Chrome"))
            .expect("a wrong-app readback must contradict");
        assert!(detail.contains("Third Eye") && detail.contains("Google Chrome"));
    }

    #[test]
    fn verify_against_intent_checks_the_click_hit_test_first() {
        let hit = |app: Option<&str>| ActionReport {
            clicked_element: Some(FocusReport {
                app: app.map(Into::into),
                role: Some("AXLink".into()),
                ..FocusReport::default()
            }),
            ..ActionReport::default()
        };
        // The element under the click belongs to another app → contradiction,
        // even though keyboard focus reported nothing (links take no focus).
        let detail = verify_against_intent(&hit(Some("Finder")), Some("Google Chrome"))
            .expect("a wrong-app hit-test must contradict");
        assert!(detail.contains("Finder") && detail.contains("Google Chrome"));
        // Same app (any casing) or an unattributed hit → inert.
        assert_eq!(
            verify_against_intent(&hit(Some("google chrome")), Some("Google Chrome")),
            None
        );
        assert_eq!(
            verify_against_intent(&hit(None), Some("Google Chrome")),
            None
        );
    }

    #[tokio::test]
    async fn input_tool_malformed_arguments_are_typed_invalid_arguments() {
        let tool = InputTool::new(Arc::new(RecordingInput::new()), armed_arm(), unfocused());
        // Unknown action tag: serde rejects it before any HID is touched.
        let outcome = tool
            .execute(&input_call("c1", r#"{"action":"self-destruct"}"#))
            .await;
        assert!(!outcome.ok);
        assert_eq!(outcome.failure.as_deref(), Some("invalid-arguments"));
        let outcome = tool.execute(&input_call("c1", "{not json")).await;
        assert_eq!(outcome.failure.as_deref(), Some("invalid-arguments"));
    }

    #[tokio::test]
    async fn input_tool_wrong_name_is_unknown_tool() {
        let tool = InputTool::new(Arc::new(RecordingInput::new()), armed_arm(), unfocused());
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
        let tool = InputTool::new(Arc::new(FallbackInput), armed_arm(), unfocused());
        let outcome = tool
            .execute(&input_call("c1", r#"{"action":"type-text","text":"hi"}"#))
            .await;
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
        let tool = InputTool::new(Arc::new(RecordingInput::new()), arm.clone(), unfocused());
        assert!(
            tool.definitions().is_empty(),
            "disarmed tool must advertise nothing"
        );
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
        let tool = InputTool::new(
            backend.clone(),
            Arc::new(HidArmState::disarmed()),
            unfocused(),
        );
        let outcome = tool
            .execute(&input_call(
                "c1",
                r#"{"action":"mouse-click","button":"left"}"#,
            ))
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
                unfocused(),
            )),
            Box::new(ScreenQueryTool::new(
                Arc::new(ScriptedScreen::ok()),
                Arc::new(ScreenSeen::new()),
                Arc::new(FocusedApp::new()),
            )),
        ]);
        let names: Vec<String> = composite
            .definitions()
            .into_iter()
            .map(|d| d.name)
            .collect();
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
            unfocused(),
        ))]);
        let outcome = composite
            .execute(&input_call("c1", r#"{"action":"mouse-move","x":1,"y":2}"#))
            .await;
        assert!(!outcome.ok);
        assert_eq!(outcome.failure.as_deref(), Some("unknown-tool"));
        assert!(
            backend.last.lock().unwrap().is_none(),
            "disarmed HID backend must stay untouched"
        );
    }

    #[test]
    fn composite_concatenates_every_sub_executor_definition() {
        let composite = CompositeExecutor::new(vec![
            Box::new(seeded_tool()),
            Box::new(InputTool::new(
                Arc::new(RecordingInput::new()),
                armed_arm(),
                unfocused(),
            )),
        ]);
        let names: Vec<String> = composite
            .definitions()
            .into_iter()
            .map(|d| d.name)
            .collect();
        assert_eq!(names, vec![MEMORY_SEARCH_TOOL, INPUT_ACTION_TOOL]);
    }

    #[tokio::test]
    async fn composite_routes_each_call_to_its_owner() {
        let backend = Arc::new(RecordingInput::new());
        let composite = CompositeExecutor::new(vec![
            Box::new(seeded_tool()),
            Box::new(InputTool::new(backend.clone(), armed_arm(), unfocused())),
        ]);

        // memory_search dispatches to the memory tool, unchanged.
        let mem = composite
            .execute(&search_call("c1", r#"{"query":"broadcast lag"}"#))
            .await;
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
            Box::new(InputTool::new(
                Arc::new(RecordingInput::new()),
                armed_arm(),
                unfocused(),
            )),
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
                    cx: 0,
                    cy: 0,
                    app: None,
                    role: None,
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
                        cx: 0,
                        cy: 0,
                        app: app.map(str::to_owned),
                        role: None,
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
        ToolCall {
            id: id.into(),
            name: SCREEN_QUERY_TOOL.into(),
            arguments: "{}".into(),
        }
    }

    #[test]
    fn screen_query_definition_is_the_openai_function_envelope() {
        let def = ScreenQueryTool::definition();
        assert_eq!(def.name, SCREEN_QUERY_TOOL);
        let v = serde_json::to_value(&def).unwrap();
        assert_eq!(v["type"], "function");
        assert_eq!(v["function"]["name"], "screen_query");
        // No required arguments — the model can call it with an empty object.
        assert_eq!(
            v["function"]["parameters"]["required"]
                .as_array()
                .unwrap()
                .len(),
            0
        );
    }

    #[tokio::test]
    async fn screen_query_ok_returns_element_json_with_coordinates() {
        let tool = ScreenQueryTool::new(
            Arc::new(ScriptedScreen::ok()),
            Arc::new(ScreenSeen::new()),
            Arc::new(FocusedApp::new()),
        );
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
            ScreenElement {
                text: "a".into(),
                x: 0,
                y: 0,
                width: 1,
                height: 1,
                cx: 0,
                cy: 0,
                app: Some("Chrome".into()),
                role: None,
            },
            ScreenElement {
                text: "b".into(),
                x: 0,
                y: 0,
                width: 1,
                height: 1,
                cx: 0,
                cy: 0,
                app: None,
                role: None,
            },
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
            ScreenElement {
                text: "addr".into(),
                x: 0,
                y: 0,
                width: 1,
                height: 1,
                cx: 0,
                cy: 0,
                app: Some("google chrome".into()),
                role: None,
            },
            ScreenElement {
                text: "other".into(),
                x: 0,
                y: 0,
                width: 1,
                height: 1,
                cx: 0,
                cy: 0,
                app: Some("Finder".into()),
                role: None,
            },
            ScreenElement {
                text: "desktop".into(),
                x: 0,
                y: 0,
                width: 1,
                height: 1,
                cx: 0,
                cy: 0,
                app: None,
                role: None,
            },
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
        assert!(
            seen.seen(),
            "a successful query must still ground coordinates"
        );
        assert_eq!(
            outcome.result_count,
            Some(1),
            "only the Chrome element survives"
        );
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
            Arc::new(ScriptedScreen::failing(
                ScreenQueryError::PermissionDenied {
                    detail: "TCC denied".into(),
                },
            )),
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
        let tool = ScreenQueryTool::new(
            Arc::new(ScriptedScreen::ok()),
            Arc::new(ScreenSeen::new()),
            Arc::new(FocusedApp::new()),
        );
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
            Box::new(InputTool::new(
                Arc::new(RecordingInput::new()),
                armed_arm(),
                unfocused(),
            )),
            Box::new(ScreenQueryTool::new(
                Arc::new(ScriptedScreen::ok()),
                Arc::new(ScreenSeen::new()),
                Arc::new(FocusedApp::new()),
            )),
        ]);
        let names: Vec<String> = composite
            .definitions()
            .into_iter()
            .map(|d| d.name)
            .collect();
        assert_eq!(
            names,
            vec![MEMORY_SEARCH_TOOL, INPUT_ACTION_TOOL, SCREEN_QUERY_TOOL]
        );

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
            Self {
                verdicts: Mutex::new(verdicts.into()),
                requests: Mutex::new(Vec::new()),
            }
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
            Self {
                last: Mutex::new(None),
            }
        }
    }

    #[async_trait]
    impl AppFocus for RecordingFocus {
        async fn focus(
            &self,
            app_name: &str,
        ) -> Result<crate::appfocus::FocusedApp, AppFocusError> {
            *self.last.lock().unwrap() = Some(app_name.to_string());
            Ok(crate::appfocus::FocusedApp {
                app: app_name.to_string(),
                launched: false,
                visible_windows: None,
            })
        }

        async fn running_apps(&self) -> Vec<String> {
            vec!["Google Chrome".into(), "Finder".into()]
        }
    }

    fn focus_call(id: &str, args: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: FOCUS_APP_TOOL.into(),
            arguments: args.into(),
        }
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
        let inner = InputTool::new(backend, armed_arm(), unfocused());
        let focus = FocusAppTool::new(Arc::new(RecordingFocus::new()));
        // Mark the screen already seen — with a screen-spanning box so any aimed
        // coordinate these approval-path tests use lands on-target — so they
        // exercise the Off/Ask/AutoRun gate directly; the targeting guards
        // (no-screen-query, off-target) have their own dedicated tests.
        let screen_seen = Arc::new(ScreenSeen::new());
        screen_seen.mark_seen(vec![SeenBox {
            x: 0,
            y: 0,
            width: 100_000,
            height: 100_000,
        }]);
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
            .execute(&input_call(
                "c1",
                r#"{"action":"mouse-click","button":"left"}"#,
            ))
            .await;
        assert!(!outcome.ok);
        assert_eq!(outcome.failure.as_deref(), Some("disabled"));
        assert!(
            backend.last.lock().unwrap().is_none(),
            "Off must not reach the backend"
        );
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
            .execute(&input_call(
                "c1",
                r#"{"action":"mouse-click","button":"left"}"#,
            ))
            .await;
        assert!(!outcome.ok);
        assert_eq!(outcome.failure.as_deref(), Some(APPROVAL_DENIED_KIND));
        assert!(
            backend.last.lock().unwrap().is_none(),
            "Deny must not reach the backend"
        );
        assert_eq!(
            approver.prompt_count(),
            1,
            "Ask + new kind must prompt exactly once"
        );
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
            .execute(&input_call(
                "c1",
                r#"{"action":"mouse-click","button":"left"}"#,
            ))
            .await;
        assert!(first.ok);
        assert_eq!(approver.prompt_count(), 1);

        let second = gate
            .execute(&input_call(
                "c2",
                r#"{"action":"mouse-click","button":"left"}"#,
            ))
            .await;
        assert!(second.ok);
        assert_eq!(
            approver.prompt_count(),
            2,
            "allow-once must prompt again for the same kind"
        );
        assert!(
            wl.lock().unwrap().is_empty(),
            "allow-once must not whitelist the kind"
        );
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
            .execute(&input_call(
                "c1",
                r#"{"action":"key-press","key":"return"}"#,
            ))
            .await;
        assert!(first.ok);
        assert_eq!(approver.prompt_count(), 1);
        assert!(
            wl.lock().unwrap().contains(ActionKind::KeyPress),
            "allow-kind must whitelist"
        );

        let second = gate
            .execute(&input_call("c2", r#"{"action":"key-press","key":"tab"}"#))
            .await;
        assert!(
            second.ok,
            "a whitelisted kind must perform without prompting"
        );
        assert_eq!(
            approver.prompt_count(),
            1,
            "the whitelisted kind must not prompt again"
        );
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

        let bad_tag = gate
            .execute(&input_call("c1", r#"{"action":"self-destruct"}"#))
            .await;
        assert_eq!(bad_tag.failure.as_deref(), Some("invalid-arguments"));
        let bad_json = gate.execute(&input_call("c2", "{not json")).await;
        assert_eq!(bad_json.failure.as_deref(), Some("invalid-arguments"));

        assert_eq!(
            approver.prompt_count(),
            0,
            "a malformed action must never prompt"
        );
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
            InputTool::new(backend.clone(), armed_arm(), unfocused()),
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
            Box::new(ScreenQueryTool::new(
                Arc::new(ScriptedScreen::ok()),
                screen_seen,
                focused_app,
            )),
        ]);

        // memory_search: succeeds, never gated.
        let mem = composite
            .execute(&search_call("c1", r#"{"query":"broadcast lag"}"#))
            .await;
        assert!(mem.ok);
        assert_eq!(
            approver.prompt_count(),
            0,
            "memory_search must never be gated"
        );

        // screen_query: succeeds, never gated — and it grounds the coordinates
        // the mouse-move below needs.
        let scr = composite.execute(&screen_call("c2")).await;
        assert!(scr.ok);
        assert_eq!(
            approver.prompt_count(),
            0,
            "screen_query must never be gated"
        );

        // input_action: gated through the approver, then performed. The aim must
        // land inside a real screen_query element — ScriptedScreen::ok() returns
        // one box [100,160)×[200,224), so (120, 210) is on-target.
        let hid = composite
            .execute(&input_call(
                "c3",
                r#"{"action":"mouse-move","x":120,"y":210}"#,
            ))
            .await;
        assert!(hid.ok);
        assert_eq!(approver.prompt_count(), 1, "input_action must be gated");
        assert_eq!(
            *backend.last.lock().unwrap(),
            Some(InputAction::MouseMove { x: 120, y: 210 })
        );
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
            InputTool::new(backend.clone(), armed_arm(), unfocused()),
            FocusAppTool::new(Arc::new(RecordingFocus::new())),
            HidRunMode::Ask,
            whitelist,
            approver.clone(),
            Arc::new(ScreenSeen::new()),
            Arc::new(FocusedApp::new()),
        );
        let composite = CompositeExecutor::new(vec![Box::new(gate)]);
        let client = ScriptedClient::new(vec![
            tool_call_outcome(vec![input_call(
                "c1",
                r#"{"action":"mouse-click","button":"left"}"#,
            )]),
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
        let ToolEvent::Result(result) = &capture.events()[1] else {
            panic!("expected Result")
        };
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
            InputTool::new(backend.clone(), armed_arm(), unfocused()),
            FocusAppTool::new(Arc::new(RecordingFocus::new())),
            HidRunMode::AutoRun,
            whitelist,
            approver.clone(),
            screen_seen.clone(),
            focused_app.clone(),
        );
        let composite = CompositeExecutor::new(vec![
            Box::new(gate),
            Box::new(ScreenQueryTool::new(
                Arc::new(ScriptedScreen::ok()),
                screen_seen,
                focused_app,
            )),
        ]);

        // Blind mouse-move: refused, backend untouched, approver never consulted.
        let blind = composite
            .execute(&input_call("c1", r#"{"action":"mouse-move","x":9,"y":9}"#))
            .await;
        assert!(!blind.ok);
        assert_eq!(blind.failure.as_deref(), Some(NO_SCREEN_QUERY_KIND));
        assert_eq!(
            *backend.last.lock().unwrap(),
            None,
            "a guessed move must never reach HID"
        );
        assert_eq!(approver.prompt_count(), 0);

        // screen_query grounds the coordinates.
        assert!(composite.execute(&screen_call("c2")).await.ok);

        // Now the same mouse-move lands.
        let aimed = composite
            .execute(&input_call(
                "c3",
                r#"{"action":"mouse-move","x":100,"y":200}"#,
            ))
            .await;
        assert!(
            aimed.ok,
            "mouse-move must perform once the screen has been queried"
        );
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
            InputTool::new(backend.clone(), armed_arm(), unfocused()),
            FocusAppTool::new(Arc::new(RecordingFocus::new())),
            HidRunMode::AutoRun,
            whitelist,
            approver.clone(),
            screen_seen.clone(),
            focused_app.clone(),
        );
        let composite = CompositeExecutor::new(vec![
            Box::new(gate),
            Box::new(ScreenQueryTool::new(
                Arc::new(ScriptedScreen::ok()),
                screen_seen,
                focused_app,
            )),
        ]);

        // Blind aimed click: refused, backend untouched — the coordinate is no
        // longer silently dropped into a click-at-cursor.
        let blind = composite
            .execute(&input_call(
                "c1",
                r#"{"action":"mouse-click","x":840,"y":240,"button":"left"}"#,
            ))
            .await;
        assert!(!blind.ok);
        assert_eq!(blind.failure.as_deref(), Some(NO_SCREEN_QUERY_KIND));
        assert_eq!(
            *backend.last.lock().unwrap(),
            None,
            "a guessed click must never reach HID"
        );
        assert_eq!(approver.prompt_count(), 0);

        // screen_query grounds coordinates; the same aimed click now lands, and it
        // carries the coordinate through to the backend (move-then-click).
        assert!(composite.execute(&screen_call("c2")).await.ok);
        let aimed = composite
            .execute(&input_call(
                "c3",
                r#"{"action":"mouse-click","x":100,"y":200,"button":"left"}"#,
            ))
            .await;
        assert!(
            aimed.ok,
            "an aimed click must perform once the screen has been queried"
        );
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
            InputTool::new(backend.clone(), armed_arm(), unfocused()),
            FocusAppTool::new(Arc::new(RecordingFocus::new())),
            HidRunMode::AutoRun,
            whitelist,
            approver.clone(),
            screen_seen.clone(),
            focused_app.clone(),
        );
        let composite = CompositeExecutor::new(vec![
            Box::new(gate),
            Box::new(ScreenQueryTool::new(
                Arc::new(ScriptedScreen::ok()),
                screen_seen,
                focused_app,
            )),
        ]);

        // Ground the screen, then aim at (5, 5) — well outside the only element.
        assert!(composite.execute(&screen_call("c1")).await.ok);
        let off = composite
            .execute(&input_call(
                "c2",
                r#"{"action":"mouse-click","x":5,"y":5,"button":"left"}"#,
            ))
            .await;
        assert!(!off.ok, "a click on bare desktop must be refused");
        assert_eq!(off.failure.as_deref(), Some(OFF_TARGET_KIND));
        assert_eq!(
            *backend.last.lock().unwrap(),
            None,
            "an off-target click must never reach HID"
        );
        assert_eq!(
            approver.prompt_count(),
            0,
            "refused before the approval prompt"
        );

        // A click INSIDE the element's box performs (regression guard: enforcement
        // is not blanket-refusing every aimed click).
        let on = composite
            .execute(&input_call(
                "c3",
                r#"{"action":"mouse-click","x":130,"y":210,"button":"left"}"#,
            ))
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
        screen_seen.mark_seen(vec![SeenBox {
            x: 100,
            y: 200,
            width: 60,
            height: 24,
        }]);
        assert!(screen_seen.on_target(120, 210));
        screen_seen.invalidate();
        assert!(!screen_seen.seen(), "focus change clears the seen flag");
        assert!(
            !screen_seen.on_target(120, 210),
            "and clears the boxes with it"
        );
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
        screen_seen.mark_seen(vec![SeenBox {
            x: 0,
            y: 0,
            width: 100_000,
            height: 100_000,
        }]); // even with the screen grounded, half-aim is invalid.
        let gate = ApprovalGate::new(
            InputTool::new(backend.clone(), armed_arm(), unfocused()),
            FocusAppTool::new(Arc::new(RecordingFocus::new())),
            HidRunMode::AutoRun,
            whitelist,
            approver,
            screen_seen,
            Arc::new(FocusedApp::new()),
        );
        let outcome = gate
            .execute(&input_call(
                "c1",
                r#"{"action":"mouse-click","x":840,"button":"left"}"#,
            ))
            .await;
        assert!(!outcome.ok);
        assert_eq!(outcome.failure.as_deref(), Some("invalid-arguments"));
        assert_eq!(
            *backend.last.lock().unwrap(),
            None,
            "a half-aimed click must never reach HID"
        );
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
            InputTool::new(backend.clone(), armed_arm(), unfocused()),
            FocusAppTool::new(Arc::new(RecordingFocus::new())),
            HidRunMode::AutoRun,
            whitelist,
            approver,
            Arc::new(ScreenSeen::new()),
            Arc::new(FocusedApp::new()),
        );
        for (args, expect) in [
            (
                r#"{"action":"mouse-click","button":"left"}"#,
                InputAction::click(MouseButton::Left),
            ),
            (
                r#"{"action":"type-text","text":"hi"}"#,
                InputAction::TypeText { text: "hi".into() },
            ),
            (
                r#"{"action":"key-press","key":"return"}"#,
                InputAction::KeyPress {
                    key: "return".into(),
                    modifiers: None,
                },
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
            InputTool::new(backend.clone(), armed_arm(), unfocused()),
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
                Arc::new(ScriptedScreen::with_apps(&[(
                    "addr",
                    Some("Google Chrome"),
                )])),
                screen_seen,
                focused_app,
            )),
        ]);

        // Query grounds coordinates; a move onto the element now lands.
        assert!(composite.execute(&screen_call("c1")).await.ok);
        assert!(
            composite
                .execute(&input_call("c2", r#"{"action":"mouse-move","x":2,"y":3}"#))
                .await
                .ok
        );

        // focus_app succeeds → coordinates invalidated.
        assert!(
            composite
                .execute(&focus_call("c3", r#"{"app":"Google Chrome"}"#))
                .await
                .ok
        );

        // The next move is refused until a fresh query.
        let stale = composite
            .execute(&input_call("c4", r#"{"action":"mouse-move","x":2,"y":3}"#))
            .await;
        assert!(
            !stale.ok,
            "a move after focus_app must be refused until re-query"
        );
        assert_eq!(stale.failure.as_deref(), Some(NO_SCREEN_QUERY_KIND));

        // Re-query re-grounds (still scoped to the focused Chrome element); the
        // move onto it lands again.
        assert!(composite.execute(&screen_call("c5")).await.ok);
        assert!(
            composite
                .execute(&input_call("c6", r#"{"action":"mouse-move","x":2,"y":3}"#))
                .await
                .ok
        );
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
            InputTool::new(backend.clone(), armed_arm(), unfocused()),
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
        assert_eq!(
            survey.result_count,
            Some(3),
            "pre-focus survey returns everything"
        );

        // Focus Chrome, then re-query: only Chrome's element survives.
        assert!(
            composite
                .execute(&focus_call("c2", r#"{"app":"Google Chrome"}"#))
                .await
                .ok
        );
        let scoped = composite.execute(&screen_call("c3")).await;
        assert_eq!(
            scoped.result_count,
            Some(1),
            "post-focus query is scoped to Chrome"
        );
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
        // And the post-action validation discipline: the `verified` block is
        // what the model must check before claiming an action worked.
        assert!(HID_SYSTEM_PROMPT.contains("verified"));
        // ...and names the structural enforcement so the model treats the
        // auto-failed action as a re-aim signal, not a dead end.
        assert!(HID_SYSTEM_PROMPT.contains(VERIFICATION_FAILED_KIND));
        assert!(HID_SYSTEM_PROMPT.contains("textEntered"));
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
        let outcome = tool
            .execute(&focus_call("c1", r#"{"app":"Google Chrome"}"#))
            .await;
        assert!(outcome.ok);
        assert_eq!(outcome.failure, None);
        assert_eq!(
            *backend.last.lock().unwrap(),
            Some("Google Chrome".to_string())
        );
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
        let outcome = tool
            .execute(&focus_call("c1", r#"{"app":"Firefox"}"#))
            .await;
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
            .execute(&ToolCall {
                id: "c1".into(),
                name: "memory_search".into(),
                arguments: "{}".into(),
            })
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
        let inner = InputTool::new(Arc::new(RecordingInput::new()), armed_arm(), unfocused());
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
        assert!(
            off.definitions().is_empty(),
            "Off must advertise no HID tools"
        );

        let (ask, _wl) = focus_gate_over(
            HidRunMode::Ask,
            Arc::new(RecordingFocus::new()),
            Arc::new(ScriptedApprover::new(vec![])),
        );
        let names: Vec<String> = ask.definitions().into_iter().map(|d| d.name).collect();
        assert!(names.contains(&INPUT_ACTION_TOOL.to_string()));
        assert!(
            names.contains(&FOCUS_APP_TOOL.to_string()),
            "an armed mode advertises focus_app"
        );
    }

    #[tokio::test]
    async fn gate_off_refuses_focus_app_with_disabled_before_activating() {
        // Off is structurally inert: the gate refuses focus_app BEFORE parsing or
        // activating — never prompts, never reaches the backend (D038).
        let focus = Arc::new(RecordingFocus::new());
        let approver = Arc::new(ScriptedApprover::new(vec![]));
        let (gate, _wl) = focus_gate_over(HidRunMode::Off, focus.clone(), approver.clone());
        let outcome = gate
            .execute(&focus_call("c1", r#"{"app":"Google Chrome"}"#))
            .await;
        assert!(!outcome.ok);
        assert_eq!(outcome.failure.as_deref(), Some("disabled"));
        assert!(
            focus.last.lock().unwrap().is_none(),
            "Off must not activate anything"
        );
        assert_eq!(approver.prompt_count(), 0, "Off must never prompt");
    }

    #[tokio::test]
    async fn gate_ask_deny_never_activates_the_app() {
        // Ask + new kind prompts; a Deny returns approval-denied and never fronts.
        let focus = Arc::new(RecordingFocus::new());
        let approver = Arc::new(ScriptedApprover::new(vec![ApprovalVerdict::Deny]));
        let (gate, _wl) = focus_gate_over(HidRunMode::Ask, focus.clone(), approver.clone());
        let outcome = gate
            .execute(&focus_call("c1", r#"{"app":"Google Chrome"}"#))
            .await;
        assert!(!outcome.ok);
        assert_eq!(outcome.failure.as_deref(), Some(APPROVAL_DENIED_KIND));
        assert!(
            focus.last.lock().unwrap().is_none(),
            "Deny must not activate the app"
        );
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

        let first = gate
            .execute(&focus_call("c1", r#"{"app":"Google Chrome"}"#))
            .await;
        assert!(first.ok);
        assert_eq!(approver.prompt_count(), 1);
        assert!(
            wl.lock().unwrap().contains(ActionKind::FocusApp),
            "allow-kind must whitelist FocusApp"
        );

        let second = gate.execute(&focus_call("c2", r#"{"app":"Finder"}"#)).await;
        assert!(
            second.ok,
            "a whitelisted FocusApp must perform without prompting"
        );
        assert_eq!(
            approver.prompt_count(),
            1,
            "the whitelisted kind must not prompt again"
        );
        assert_eq!(*focus.last.lock().unwrap(), Some("Finder".to_string()));
    }

    #[tokio::test]
    async fn gate_auto_run_activates_focus_app_without_prompting() {
        let focus = Arc::new(RecordingFocus::new());
        let approver = Arc::new(ScriptedApprover::new(vec![]));
        let (gate, _wl) = focus_gate_over(HidRunMode::AutoRun, focus.clone(), approver.clone());
        let outcome = gate
            .execute(&focus_call("c1", r#"{"app":"Google Chrome"}"#))
            .await;
        assert!(outcome.ok);
        assert_eq!(approver.prompt_count(), 0, "Auto-run must never prompt");
        assert_eq!(
            *focus.last.lock().unwrap(),
            Some("Google Chrome".to_string())
        );
    }
}
