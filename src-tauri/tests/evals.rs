//! Behavioural evals for tool use (spec 2026-07-27, feature 5).
//!
//! Deterministic scenarios drive the REAL production tool loop — real
//! [`run_tool_loop`], real [`CompositeExecutor`], real gates — against a
//! scripted model endpoint and recording backends, and assert the
//! *behavioural contract* rather than any one function:
//!
//! - grounding: an ungrounded click is refused typed and the loop recovers
//!   through screen_query into a grounded click;
//! - toggles: a tool disabled in Settings is never offered and refuses typed;
//! - honesty/verification: a click whose hit-test lands in the wrong app
//!   flips to `verification-failed` — the model is told, structurally;
//! - recall: the chat-history surface answers "what did I ask before";
//! - prompt contract: the load-bearing behavioural clauses of
//!   `HID_SYSTEM_PROMPT` and the tool descriptions exist — deleting the
//!   recall paragraph or the cx/cy rule flips an eval red (spec criterion 5).
//!
//! Run: `make evals` (or `cargo test --test evals`). The live twin
//! (`live_eval_recall_behaviour`) sends a scenario to a real LM Studio
//! endpoint and is `#[ignore]` — run explicitly with `-- --ignored`.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use third_eye_lib::appfocus::{AppFocus, AppFocusError, FocusedApp};
use third_eye_lib::input::commands::{HidArmState, HidRunMode, SessionWhitelist};
use third_eye_lib::input::{
    ActionKind, ActionReport, FocusReport, InputAction, InputControl, InputError, InputPermission,
};
use third_eye_lib::llm::openai::OpenAiClient;
use third_eye_lib::llm::toolloop::{
    run_tool_loop, ApprovalGate, ApprovalPrompt, ApprovalVerdict, ChatHistorySearchTool,
    CompositeExecutor, FocusAppTool, FocusedApp as FocusedAppGate, InputTool, MemorySearchTool,
    ReadPageTool, ScreenQueryTool, ScreenSeen, ToolEvent, UrlGroundingExecutor, UrlSeen,
    CHAT_HISTORY_SEARCH_TOOL, HID_SYSTEM_PROMPT, INPUT_ACTION_TOOL, MEMORY_SEARCH_TOOL,
    NO_SCREEN_QUERY_KIND, READ_PAGE_TOOL, SCREEN_QUERY_TOOL, TOO_MANY_OPENS_KIND,
    UNGROUNDED_URL_KIND, VERIFICATION_FAILED_KIND,
};
use third_eye_lib::llm::ChatMessage;
use third_eye_lib::memory::MemoryStore;
use third_eye_lib::screenquery::{ScreenElement, ScreenQuery, ScreenQueryError};
use third_eye_lib::tool_toggles::{ToggleGatedExecutor, ToolToggles};

// ---------------------------------------------------------------------------
// Scripted HTTP server (chat_tool_calling.rs shape): one pre-baked response
// per connection, capturing every request's raw bytes.
// ---------------------------------------------------------------------------

mod scripted {
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    pub async fn spawn(responses: Vec<Vec<u8>>) -> (String, Arc<Mutex<Vec<Vec<u8>>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let captured: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));
        let cap = captured.clone();
        tokio::spawn(async move {
            for response in responses {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let mut buf: Vec<u8> = Vec::new();
                let mut tmp = [0u8; 4096];
                while !request_complete(&buf) {
                    match sock.read(&mut tmp).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => buf.extend_from_slice(&tmp[..n]),
                    }
                }
                cap.lock().unwrap().push(buf);
                let _ = sock.write_all(&response).await;
                let _ = sock.shutdown().await;
            }
        });
        (format!("http://{addr}"), captured)
    }

    fn request_complete(buf: &[u8]) -> bool {
        let text = String::from_utf8_lossy(buf);
        let Some(header_end) = text.find("\r\n\r\n") else {
            return false;
        };
        let content_length = text
            .lines()
            .find_map(|l| {
                l.to_ascii_lowercase()
                    .strip_prefix("content-length:")?
                    .trim()
                    .parse::<usize>()
                    .ok()
            })
            .unwrap_or(0);
        buf.len() >= header_end + 4 + content_length
    }

    pub fn body_json(captured: &Arc<Mutex<Vec<Vec<u8>>>>, i: usize) -> serde_json::Value {
        let raw = captured.lock().unwrap()[i].clone();
        let text = String::from_utf8_lossy(&raw);
        let body = text
            .split("\r\n\r\n")
            .nth(1)
            .expect("captured request has no body");
        serde_json::from_str(body).expect("captured request body is not JSON")
    }

    pub fn sse_token(token: &str) -> String {
        format!(
            "data: {}\n\n",
            serde_json::json!({"choices": [{"delta": {"content": token}}]})
        )
    }

    pub fn sse_tool_call(id: &str, name: &str, arguments: &str) -> String {
        format!(
            "data: {}\n\n",
            serde_json::json!({"choices": [{"delta": {"content": null, "tool_calls": [{
                "index": 0, "id": id,
                "function": {"name": name, "arguments": arguments}
            }]}}]})
        )
    }

    pub fn sse_200(parts: &[String]) -> Vec<u8> {
        let mut resp = String::from(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
             transfer-encoding: chunked\r\nconnection: close\r\n\r\n",
        );
        for part in parts {
            resp.push_str(&format!("{:x}\r\n{part}\r\n", part.len()));
        }
        resp.push_str("0\r\n\r\n");
        resp.into_bytes()
    }

    /// One whole model round: a single tool call, terminated.
    pub fn round_tool(id: &str, name: &str, arguments: &str) -> Vec<u8> {
        sse_200(&[
            sse_tool_call(id, name, arguments),
            "data: [DONE]\n\n".to_string(),
        ])
    }

    /// One whole model round: a plain text answer, terminated.
    pub fn round_text(text: &str) -> Vec<u8> {
        sse_200(&[sse_token(text), "data: [DONE]\n\n".to_string()])
    }
}

// ---------------------------------------------------------------------------
// Recording doubles
// ---------------------------------------------------------------------------

/// Records performed actions; each `perform` returns the next scripted
/// report (default report when the script runs dry).
struct ScriptedInput {
    actions: Mutex<Vec<InputAction>>,
    reports: Mutex<Vec<ActionReport>>,
}

impl ScriptedInput {
    fn new(reports: Vec<ActionReport>) -> Self {
        Self {
            actions: Mutex::new(Vec::new()),
            reports: Mutex::new(reports),
        }
    }
}

#[async_trait]
impl InputControl for ScriptedInput {
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
        self.actions.lock().unwrap().push(action);
        let mut reports = self.reports.lock().unwrap();
        Ok(if reports.is_empty() {
            ActionReport::default()
        } else {
            reports.remove(0)
        })
    }
}

struct AlwaysFocus;

#[async_trait]
impl AppFocus for AlwaysFocus {
    async fn focus(&self, app_name: &str) -> Result<FocusedApp, AppFocusError> {
        Ok(FocusedApp {
            app: app_name.to_string(),
            launched: false,
            visible_windows: Some(1),
        })
    }
    async fn running_apps(&self) -> Vec<String> {
        vec!["Google Chrome".into()]
    }
}

/// One unambiguous on-screen element in Chrome at (640,220) 400x40, and a
/// fixed page text for the continuity eval.
struct FixedScreen;

#[async_trait]
impl ScreenQuery for FixedScreen {
    async fn page_text(&self, _app: &str) -> Option<String> {
        Some(
            "Lasagna!\nIngredients\n2 cups besciamella\n500g beef mince\n375g lasagna sheets"
                .into(),
        )
    }

    async fn query(&self) -> Result<Vec<ScreenElement>, ScreenQueryError> {
        Ok(vec![ScreenElement {
            text: "Search Google or type a URL".into(),
            x: 640,
            y: 220,
            width: 400,
            height: 40,
            cx: 840,
            cy: 240,
            app: Some("Google Chrome".into()),
            role: Some("AXTextField".into()),
        }])
    }
}

/// A screen whose page text carries a REAL result URL — reading it grounds
/// that URL for navigation.
struct SearchResultsScreen;

#[async_trait]
impl ScreenQuery for SearchResultsScreen {
    async fn page_text(&self, _app: &str) -> Option<String> {
        Some(
            "Lasagna! - RecipeTin Eats\nhttps://recipetineats.com/lasagna/\n4.9 stars\n\
             Carbonara - RecipeTin Eats\nhttps://recipetineats.com/carbonara/\n5.0 stars"
                .into(),
        )
    }

    async fn query(&self) -> Result<Vec<ScreenElement>, ScreenQueryError> {
        Ok(Vec::new())
    }
}

/// Approves everything — the URL-gate eval tests the gate, not approvals.
struct AllowAll;

#[async_trait]
impl ApprovalPrompt for AllowAll {
    async fn request(&self, _kind: ActionKind, _summary: String) -> ApprovalVerdict {
        ApprovalVerdict::AllowOnce
    }
}

/// Auto-run never prompts; panic proves it.
struct NeverPrompt;

#[async_trait]
impl ApprovalPrompt for NeverPrompt {
    async fn request(&self, kind: ActionKind, summary: String) -> ApprovalVerdict {
        panic!("auto-run must never prompt (kind={kind:?}, summary={summary:?})");
    }
}

/// The production HID mount: gate over input+focus with screen/focus state
/// shared with a screen-query tool.
fn hid_gate(
    input: Arc<ScriptedInput>,
    screen_seen: Arc<ScreenSeen>,
    focused_app: Arc<FocusedAppGate>,
) -> (ApprovalGate, ScreenQueryTool) {
    let gate = ApprovalGate::new(
        InputTool::new(input, Arc::new(HidArmState::new(true)), focused_app.clone()),
        FocusAppTool::new(Arc::new(AlwaysFocus)),
        HidRunMode::AutoRun,
        Arc::new(Mutex::new(SessionWhitelist::new())),
        Arc::new(NeverPrompt),
        screen_seen.clone(),
        focused_app.clone(),
    );
    let screen = ScreenQueryTool::new(Arc::new(FixedScreen), screen_seen, focused_app);
    (gate, screen)
}

fn collect_results(events: &[ToolEvent]) -> Vec<(String, bool, Option<String>)> {
    events
        .iter()
        .filter_map(|e| match e {
            ToolEvent::Result(r) => Some((r.name.clone(), r.ok, r.failure.clone())),
            ToolEvent::Call(_) => None,
        })
        .collect()
}

struct Capture {
    events: Mutex<Vec<ToolEvent>>,
}

async fn run_scenario(
    endpoint: &str,
    executor: &CompositeExecutor,
    ask: &str,
) -> (String, Vec<ToolEvent>) {
    let client = OpenAiClient::new(endpoint);
    let capture = Capture {
        events: Mutex::new(Vec::new()),
    };
    let outcome = run_tool_loop(
        &client,
        executor,
        vec![
            ChatMessage::system(HID_SYSTEM_PROMPT),
            ChatMessage::user(ask),
        ],
        7,
        &|_| {},
        &|e| capture.events.lock().unwrap().push(e.clone()),
    )
    .await
    .expect("scripted scenario must resolve");
    let events = capture.events.lock().unwrap().clone();
    (outcome.text, events)
}

/// Scratch sqlite path cleaned on drop.
struct ScratchDb {
    path: PathBuf,
}

impl ScratchDb {
    fn new(tag: &str) -> Self {
        Self {
            path: std::env::temp_dir()
                .join(format!("third-eye-eval-{tag}-{}.db", std::process::id())),
        }
    }
}

impl Drop for ScratchDb {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

// ---------------------------------------------------------------------------
// Evals
// ---------------------------------------------------------------------------

/// GROUNDING: an ungrounded click is refused with the typed
/// `no-screen-query` kind, and the loop recovers — screen_query grounds the
/// coordinates and the SAME click at the element's cx/cy then reaches the
/// backend. Exactly one real action fires, at exactly the served center.
#[tokio::test(flavor = "multi_thread")]
async fn eval_grounding_ungrounded_click_refused_then_recovers() {
    let (endpoint, _captured) = scripted::spawn(vec![
        scripted::round_tool(
            "c1",
            INPUT_ACTION_TOOL,
            r#"{"action":"mouse-click","button":"left","x":500,"y":500}"#,
        ),
        scripted::round_tool("c2", SCREEN_QUERY_TOOL, "{}"),
        scripted::round_tool(
            "c3",
            INPUT_ACTION_TOOL,
            r#"{"action":"mouse-click","button":"left","x":840,"y":240}"#,
        ),
        scripted::round_text("Clicked the address bar."),
    ])
    .await;

    let input = Arc::new(ScriptedInput::new(Vec::new()));
    let screen_seen = Arc::new(ScreenSeen::new());
    let focused_app = Arc::new(FocusedAppGate::new());
    let (gate, screen) = hid_gate(input.clone(), screen_seen, focused_app);
    let executor = CompositeExecutor::new(vec![Box::new(gate), Box::new(screen)]);

    let (text, events) = run_scenario(&endpoint, &executor, "click the address bar").await;
    assert_eq!(text, "Clicked the address bar.");
    let results = collect_results(&events);
    assert_eq!(results.len(), 3, "{results:?}");
    assert_eq!(
        (results[0].1, results[0].2.as_deref()),
        (false, Some(NO_SCREEN_QUERY_KIND)),
        "a blind click must be refused typed"
    );
    assert!(results[1].1, "screen_query grounds");
    assert!(results[2].1, "the grounded click succeeds");
    // Exactly ONE action reached the backend, at the served center.
    let actions = input.actions.lock().unwrap().clone();
    assert_eq!(actions.len(), 1, "{actions:?}");
    match &actions[0] {
        InputAction::MouseClick { x, y, .. } => {
            assert_eq!((x.unwrap(), y.unwrap()), (840, 240));
        }
        other => panic!("expected a click, got {other:?}"),
    }
}

/// TOGGLES: a tool turned off in Settings is structurally inert through the
/// whole loop — never offered in the request's tools array, and a call that
/// names it anyway refuses typed with the Settings language.
#[tokio::test(flavor = "multi_thread")]
async fn eval_disabled_tool_never_offered_and_refuses_typed() {
    let (endpoint, captured) = scripted::spawn(vec![
        scripted::round_tool("c1", MEMORY_SEARCH_TOOL, r#"{"query":"recipes"}"#),
        scripted::round_text("I can't search memory — it is disabled in Settings."),
    ])
    .await;

    let scratch = ScratchDb::new("toggles");
    let store = Arc::new(MemoryStore::open(&scratch.path).unwrap());
    let toggles = Arc::new(ToolToggles::new());
    toggles.set_enabled(MEMORY_SEARCH_TOOL, false);
    let executor = CompositeExecutor::new(vec![
        Box::new(ToggleGatedExecutor::new(
            Box::new(MemorySearchTool::new(
                store.clone(),
                Arc::new(third_eye_lib::memory::OpenAiEmbedder::new(
                    "http://127.0.0.1:1".to_string(),
                )),
            )),
            toggles.clone(),
        )),
        Box::new(ToggleGatedExecutor::new(
            Box::new(ChatHistorySearchTool::new(store)),
            toggles,
        )),
    ]);

    let (text, events) = run_scenario(&endpoint, &executor, "what recipes did I ask about?").await;
    assert!(text.contains("disabled"));
    let results = collect_results(&events);
    assert_eq!(results.len(), 1);
    assert_eq!(
        (results[0].1, results[0].2.as_deref()),
        (false, Some("disabled"))
    );
    // The request never offered the disabled tool; the enabled sibling rode.
    let req0 = scripted::body_json(&captured, 0);
    let offered: Vec<&str> = req0["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["function"]["name"].as_str().unwrap())
        .collect();
    assert!(!offered.contains(&MEMORY_SEARCH_TOOL), "{offered:?}");
    assert!(offered.contains(&CHAT_HISTORY_SEARCH_TOOL), "{offered:?}");
    // The refusal text tells the model where the switch lives.
    let req1 = scripted::body_json(&captured, 1);
    let tool_turn = req1["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "tool")
        .expect("refusal must ride back as the tool turn");
    assert!(tool_turn["content"].as_str().unwrap().contains("Settings"));
}

/// VERIFICATION: a grounded click whose post-hoc hit-test lands in ANOTHER
/// app flips to the typed `verification-failed` — the model cannot narrate
/// past a click that hit the wrong window.
#[tokio::test(flavor = "multi_thread")]
async fn eval_wrong_app_click_flips_to_verification_failed() {
    let (endpoint, _captured) = scripted::spawn(vec![
        scripted::round_tool("c1", "focus_app", r#"{"app":"Google Chrome"}"#),
        scripted::round_tool("c2", SCREEN_QUERY_TOOL, "{}"),
        scripted::round_tool(
            "c3",
            INPUT_ACTION_TOOL,
            r#"{"action":"mouse-click","button":"left","x":840,"y":240}"#,
        ),
        scripted::round_text("The click landed in Finder — let me re-read the screen."),
    ])
    .await;

    // The scripted report: the hit-test says the element under the click
    // belonged to Finder, not the focused Chrome.
    let wrong_app_report = ActionReport {
        clicked_element: Some(FocusReport {
            app: Some("Finder".into()),
            role: Some("AXButton".into()),
            title: Some("Trash".into()),
            value: None,
        }),
        ..ActionReport::default()
    };
    let input = Arc::new(ScriptedInput::new(vec![wrong_app_report]));
    let screen_seen = Arc::new(ScreenSeen::new());
    let focused_app = Arc::new(FocusedAppGate::new());
    let (gate, screen) = hid_gate(input, screen_seen, focused_app);
    let executor = CompositeExecutor::new(vec![Box::new(gate), Box::new(screen)]);

    let (_text, events) = run_scenario(&endpoint, &executor, "click the address bar").await;
    let results = collect_results(&events);
    assert_eq!(results.len(), 3, "{results:?}");
    assert!(results[0].1, "focus_app ok");
    assert!(results[1].1, "screen_query ok");
    assert_eq!(
        (results[2].1, results[2].2.as_deref()),
        (false, Some(VERIFICATION_FAILED_KIND)),
        "a wrong-app hit-test must flip the click to a typed failure"
    );
}

/// RECALL: the chat-history surface answers "what did I ask before" from
/// the stored transcripts — offered, matched, excerpted, honest on a miss.
#[tokio::test(flavor = "multi_thread")]
async fn eval_recall_surfaces_past_questions() {
    let (endpoint, captured) = scripted::spawn(vec![
        scripted::round_tool("c1", CHAT_HISTORY_SEARCH_TOOL, r#"{"query":"recipe"}"#),
        scripted::round_text("You asked about carbonara recipes."),
    ])
    .await;

    let scratch = ScratchDb::new("recall");
    let store = Arc::new(MemoryStore::open(&scratch.path).unwrap());
    let session = store.chat_session_create(1_000).unwrap();
    store
        .chat_append_exchange(
            session,
            "find me a good carbonara recipe on google",
            "Found RecipeTinEats' carbonara (5.0 stars).",
            1_753_500_000_000,
        )
        .unwrap();
    let executor = CompositeExecutor::new(vec![Box::new(ChatHistorySearchTool::new(store))]);

    let (text, events) =
        run_scenario(&endpoint, &executor, "what recipes have I asked about?").await;
    assert_eq!(text, "You asked about carbonara recipes.");
    let results = collect_results(&events);
    assert_eq!(results.len(), 1);
    assert!(results[0].1);
    // The stored question rode back to the model verbatim.
    let req1 = scripted::body_json(&captured, 1);
    let tool_turn = req1["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "tool")
        .unwrap();
    assert!(tool_turn["content"]
        .as_str()
        .unwrap()
        .contains("find me a good carbonara recipe on google"));
}

/// CONTINUITY: "what are the ingredients in this recipe" is answered by
/// READING the open page — focus_app then read_page returns the page's
/// text into the tool turn; before any focus, read_page refuses typed with
/// the pointer to focus_app.
#[tokio::test(flavor = "multi_thread")]
async fn eval_follow_up_reads_the_open_page() {
    let (endpoint, captured) = scripted::spawn(vec![
        scripted::round_tool("c1", READ_PAGE_TOOL, "{}"),
        scripted::round_tool("c2", "focus_app", r#"{"app":"Google Chrome"}"#),
        scripted::round_tool("c3", READ_PAGE_TOOL, "{}"),
        scripted::round_text("The recipe calls for besciamella, beef mince, and lasagna sheets."),
    ])
    .await;

    let input = Arc::new(ScriptedInput::new(Vec::new()));
    let screen_seen = Arc::new(ScreenSeen::new());
    let focused_app = Arc::new(FocusedAppGate::new());
    let (gate, screen) = hid_gate(input, screen_seen, focused_app.clone());
    let read_page = ReadPageTool::new(Arc::new(FixedScreen), focused_app);
    let executor =
        CompositeExecutor::new(vec![Box::new(gate), Box::new(screen), Box::new(read_page)]);

    let (text, events) = run_scenario(
        &endpoint,
        &executor,
        "what are the ingredients in this recipe?",
    )
    .await;
    assert!(text.contains("besciamella"));
    let results = collect_results(&events);
    assert_eq!(results.len(), 3, "{results:?}");
    // Before any focus the tool refuses typed, naming the fix.
    assert_eq!(
        (results[0].1, results[0].2.as_deref()),
        (false, Some("no-focused-app"))
    );
    assert!(results[1].1, "focus_app ok");
    assert!(results[2].1, "read_page ok once focused");
    // The page's actual text rode to the model in the tool turn.
    let req3 = scripted::body_json(&captured, 3);
    let tool_turns: Vec<&str> = req3["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "tool")
        .map(|m| m["content"].as_str().unwrap())
        .collect();
    assert!(
        tool_turns.iter().any(|t| t.contains("500g beef mince")),
        "{tool_turns:?}"
    );
}

/// NAVIGATION: invented deep URLs refuse typed; the search-results page
/// opens; a URL that appeared in a tool result becomes grounded and opens;
/// the third navigation hits the tab budget. The one-shot recipe bug,
/// made structurally impossible.
#[tokio::test(flavor = "multi_thread")]
async fn eval_url_grounding_blocks_invented_pages_and_tab_floods() {
    let (endpoint, _captured) = scripted::spawn(vec![
        scripted::round_tool(
            "c1",
            "run_command",
            r#"{"command":"open \"https://www.allrecipes.com/recipe/23600/worlds-best-lasagna/\""}"#,
        ),
        scripted::round_tool(
            "c2",
            "run_command",
            r#"{"command":"open \"https://www.google.com/search?q=lasagne+recipe\""}"#,
        ),
        scripted::round_tool("c3", "focus_app", r#"{"app":"Google Chrome"}"#),
        scripted::round_tool("c4", READ_PAGE_TOOL, "{}"),
        scripted::round_tool(
            "c5",
            "run_command",
            r#"{"command":"open \"https://recipetineats.com/lasagna\""}"#,
        ),
        scripted::round_tool(
            "c6",
            "run_command",
            r#"{"command":"open \"https://recipetineats.com/carbonara\""}"#,
        ),
        scripted::round_text("Opened the RecipeTin Eats lasagna page from the search results."),
    ])
    .await;

    let input = Arc::new(ScriptedInput::new(Vec::new()));
    let screen_seen = Arc::new(ScreenSeen::new());
    let focused_app = Arc::new(FocusedAppGate::new());
    let (gate, screen) = hid_gate(input, screen_seen, focused_app.clone());
    let read_page = ReadPageTool::new(Arc::new(SearchResultsScreen), focused_app);
    let commands = Arc::new(third_eye_lib::command_runner::CommandState::new());
    commands.set_enabled(true);
    let run_command = third_eye_lib::command_runner::RunCommandTool::new(
        commands,
        Arc::new(Mutex::new(SessionWhitelist::new())),
        Arc::new(AllowAll),
    );
    let composite = CompositeExecutor::new(vec![
        Box::new(gate),
        Box::new(screen),
        Box::new(read_page),
        Box::new(run_command),
    ]);
    let url_seen = Arc::new(UrlSeen::new());
    let executor_inner = UrlGroundingExecutor::new(composite, url_seen);
    let executor = CompositeExecutor::new(vec![Box::new(executor_inner)]);

    let (_text, events) =
        run_scenario(&endpoint, &executor, "find me a good lasagna recipe online").await;
    let results = collect_results(&events);
    assert_eq!(results.len(), 6, "{results:?}");
    // 1: invented deep URL → refused typed, nothing ran.
    assert_eq!(
        (results[0].1, results[0].2.as_deref()),
        (false, Some(UNGROUNDED_URL_KIND))
    );
    // 2: the search-results page opens (open-by-default).
    assert!(results[1].1, "search page must open: {:?}", results[1]);
    // 3+4: focus + read the page (whose text contains the real recipe URL).
    assert!(results[2].1 && results[3].1);
    // 5: the URL read from the page is now grounded and opens.
    assert!(results[4].1, "grounded URL must open: {:?}", results[4]);
    // 6: the budget (2 opens) is spent — tab flooding refused typed.
    assert_eq!(
        (results[5].1, results[5].2.as_deref()),
        (false, Some(TOO_MANY_OPENS_KIND))
    );
}

/// TYPED NAVIGATION: typing a guessed deep URL into the address bar is
/// the same one-shot as opening it — refused typed through the full loop;
/// typing ordinary text is untouched.
#[tokio::test(flavor = "multi_thread")]
async fn eval_typed_urls_follow_the_same_grounding() {
    let (endpoint, _captured) = scripted::spawn(vec![
        scripted::round_tool("c1", "focus_app", r#"{"app":"Google Chrome"}"#),
        scripted::round_tool("c2", SCREEN_QUERY_TOOL, "{}"),
        scripted::round_tool(
            "c3",
            INPUT_ACTION_TOOL,
            r#"{"action":"type-text","text":"https://recipetineats.com/carbonara"}"#,
        ),
        scripted::round_tool(
            "c4",
            INPUT_ACTION_TOOL,
            r#"{"action":"type-text","text":"lasagne recipe"}"#,
        ),
        scripted::round_text("Searched instead."),
    ])
    .await;

    let input = Arc::new(ScriptedInput::new(Vec::new()));
    let screen_seen = Arc::new(ScreenSeen::new());
    let focused_app = Arc::new(FocusedAppGate::new());
    let (gate, screen) = hid_gate(input.clone(), screen_seen, focused_app);
    let composite = CompositeExecutor::new(vec![Box::new(gate), Box::new(screen)]);
    let executor = CompositeExecutor::new(vec![Box::new(UrlGroundingExecutor::new(
        composite,
        Arc::new(UrlSeen::new()),
    ))]);

    let (_text, events) = run_scenario(&endpoint, &executor, "find me a carbonara recipe").await;
    let results = collect_results(&events);
    assert_eq!(results.len(), 4, "{results:?}");
    assert!(results[0].1 && results[1].1);
    // The guessed URL never reaches the keyboard…
    assert_eq!(
        (results[2].1, results[2].2.as_deref()),
        (false, Some(UNGROUNDED_URL_KIND))
    );
    // …while plain text types fine.
    assert!(results[3].1, "ordinary typing must pass: {:?}", results[3]);
    let actions = input.actions.lock().unwrap().clone();
    assert_eq!(
        actions.len(),
        1,
        "only the search text was typed: {actions:?}"
    );
}

/// ON-DEMAND MEMORY: "remember that my name is Alex" stores a Told-sourced
/// memory the moment the user asks, and memory_search finds it — the exact
/// conversation that used to end in "I don't have the ability to save".
#[tokio::test(flavor = "multi_thread")]
async fn eval_remember_stores_and_recall_finds() {
    let (endpoint, _captured) = scripted::spawn(vec![
        scripted::round_tool("c1", "remember", r#"{"fact":"The user's name is Alex"}"#),
        scripted::round_tool("c2", MEMORY_SEARCH_TOOL, r#"{"query":"name"}"#),
        scripted::round_text("Saved — your name is Sam."),
    ])
    .await;

    let scratch = ScratchDb::new("remember");
    let store = Arc::new(MemoryStore::open(&scratch.path).unwrap());
    let dead_embedder = Arc::new(third_eye_lib::memory::OpenAiEmbedder::new(
        "http://127.0.0.1:1".to_string(),
    ));
    let executor = CompositeExecutor::new(vec![
        Box::new(third_eye_lib::llm::toolloop::RememberTool::new(
            store.clone(),
            dead_embedder.clone(),
        )),
        Box::new(MemorySearchTool::new(store.clone(), dead_embedder)),
    ]);

    let (text, events) = run_scenario(
        &endpoint,
        &executor,
        "can you save my name in your memory? It's Sam",
    )
    .await;
    assert_eq!(text, "Saved — your name is Sam.");
    let results = collect_results(&events);
    assert_eq!(results.len(), 2, "{results:?}");
    assert!(results[0].1, "remember must succeed: {:?}", results[0]);
    assert!(results[1].1, "search must succeed");
    // The fact is durably in the store with Told provenance…
    let records = store.list(10, 0).unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].summary, "The user's name is Alex");
    assert_eq!(records[0].source, third_eye_lib::memory::MemorySource::Told);
}

/// PROMPT CONTRACT: the load-bearing behavioural clauses exist. Each assert
/// names the behaviour it protects — deleting the clause flips this red
/// (spec success criterion 5).
#[test]
fn eval_prompt_contract_load_bearing_clauses_present() {
    // Recall: never claim no access to past conversations without searching.
    assert!(
        HID_SYSTEM_PROMPT.contains("never claim you have no access to past conversations"),
        "recall paragraph missing"
    );
    // Grounding: coordinates come from screen_query, centers precomputed.
    assert!(
        HID_SYSTEM_PROMPT.contains("never guess coordinates"),
        "grounding clause missing"
    );
    assert!(
        HID_SYSTEM_PROMPT.contains("cx,cy"),
        "server-computed center rule missing"
    );
    // Honesty: refused tool = did not happen; verified evidence gates claims.
    assert!(
        HID_SYSTEM_PROMPT.contains("the action DID NOT HAPPEN"),
        "refusal-honesty clause missing"
    );
    assert!(
        HID_SYSTEM_PROMPT.contains("EVALUATE THE GOAL"),
        "goal self-evaluation clause missing"
    );
    // Screenshot save honesty.
    assert!(
        HID_SYSTEM_PROMPT.contains("NOT save a file"),
        "screenshot save-honesty clause missing"
    );
    // Hit-test check on clicks.
    assert!(
        HID_SYSTEM_PROMPT.contains("verified.clickedElement"),
        "clickedElement check missing"
    );
    // Continuity: follow-ups mean the open page — read it, never punt.
    assert!(
        HID_SYSTEM_PROMPT.contains("CONTINUITY"),
        "continuity paragraph missing"
    );
    assert!(
        HID_SYSTEM_PROMPT.contains("Never claim you cannot see a page you opened"),
        "read-the-page rule missing"
    );
    // On-demand memory: remember-on-request, never "I cannot store".
    assert!(
        HID_SYSTEM_PROMPT.contains("never claim you cannot store information"),
        "remember clause missing"
    );
    // Task follow-ups continue in the same app — never re-clarify.
    assert!(
        HID_SYSTEM_PROMPT.contains("CONTINUE in the same app and page"),
        "task-continuation clause missing"
    );
    // Answers summarize FINDINGS, not process.
    assert!(
        HID_SYSTEM_PROMPT.contains("must summarize WHAT YOU FOUND"),
        "findings-first answer clause missing"
    );
    // On-page search boxes beat hand-built query URLs.
    assert!(
        HID_SYSTEM_PROMPT.contains("USE that search box"),
        "on-page search clause missing"
    );
    // Navigation reuses the open browser window (new tab), never a second
    // window from `open`.
    assert!(
        HID_SYSTEM_PROMPT.contains("navigate IN that window"),
        "reuse-the-window clause missing"
    );
    assert!(
        HID_SYSTEM_PROMPT.contains("Typed URLs follow the same grounding rules"),
        "typed-URL grounding clause missing"
    );
    // Web navigation: search-then-choose, never invented deep URLs.
    assert!(
        HID_SYSTEM_PROMPT.contains("NEVER invent specific page URLs"),
        "URL-guessing prohibition missing"
    );
    assert!(
        HID_SYSTEM_PROMPT.contains("open ONE search-results URL"),
        "search-then-choose flow missing"
    );
    // Tool descriptions carry their halves of the contract.
    let screen_desc = ScreenQueryTool::definition().description;
    assert!(
        screen_desc.contains("cx as x and cy as y") && screen_desc.contains("role"),
        "screen_query description lost its aiming rules: {screen_desc}"
    );
    let input_desc = InputTool::definition().description;
    assert!(
        input_desc.contains("clickedElement"),
        "input_action description lost the hit-test rule"
    );
}

/// LIVE twin (opt-in): the real model, asked the real recipes question over
/// a seeded transcript store, should reach for chat_history_search. Needs
/// LM Studio serving a tool-capable model at the default endpoint:
/// ```sh
/// cargo test --manifest-path src-tauri/Cargo.toml --test evals \
///   -- --ignored --nocapture live_eval_recall_behaviour
/// ```
#[tokio::test]
#[ignore = "requires LM Studio serving a tool-capable chat model"]
async fn live_eval_recall_behaviour() {
    let scratch = ScratchDb::new("live-recall");
    let store = Arc::new(MemoryStore::open(&scratch.path).unwrap());
    let session = store.chat_session_create(1_000).unwrap();
    store
        .chat_append_exchange(
            session,
            "find me a good carbonara recipe on google",
            "Found RecipeTinEats' carbonara (5.0 stars).",
            1_753_500_000_000,
        )
        .unwrap();
    let executor = CompositeExecutor::new(vec![Box::new(ChatHistorySearchTool::new(store))]);
    let (text, events) = run_scenario(
        third_eye_lib::llm::openai::DEFAULT_ENDPOINT,
        &executor,
        "what recipes have I asked you about before?",
    )
    .await;
    let called_recall = events
        .iter()
        .any(|e| matches!(e, ToolEvent::Call(c) if c.call.name == CHAT_HISTORY_SEARCH_TOOL));
    eprintln!("live recall eval: called_recall={called_recall} answer={text:?}");
    assert!(
        called_recall,
        "the model must search past chats before answering: {text:?}"
    );
    assert!(
        text.to_lowercase().contains("carbonara"),
        "the answer should surface the stored question: {text:?}"
    );
}
