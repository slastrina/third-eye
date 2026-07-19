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
use serde::Serialize;

use crate::memory::commands::{DEFAULT_SEARCH_LIMIT, MAX_SEARCH_LIMIT};
use crate::memory::{search, Embedder, MemoryStore, SearchMode};

use super::{
    ChatMessage, ChatRequest, LlmClient, LlmError, StreamOutcome, TokenSink, ToolCall,
    ToolDefinition,
};

/// Event names — the tool-phase half of the IPC contract with `src/chat.ts`.
pub const TOOL_CALL_EVENT: &str = "llm://tool-call";
pub const TOOL_RESULT_EVENT: &str = "llm://tool-result";

/// Maximum rounds in which the request carries tool definitions. The round
/// after the last one strips tools, forcing a text answer — the structural
/// bound that makes the loop terminate whatever the model does.
pub const MAX_TOOL_ROUNDS: usize = 3;

/// The one tool S03 ships. The name is part of the model-facing contract
/// and the UI's memory-consulted check (T04).
pub const MEMORY_SEARCH_TOOL: &str = "memory_search";

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
    fn failure(kind: &str, detail: impl Into<String>) -> Self {
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

/// Drive one chat request through its tool rounds to a final text answer.
///
/// Each round streams via `on_token`; when the model stops to call tools,
/// every call is announced (`ToolEvent::Call`), executed, answered
/// (`ToolEvent::Result`), and appended as the OpenAI assistant-echo +
/// tool-role turns before the follow-up request. Client errors (offline,
/// tools-unsupported, interrupted) propagate unchanged — the caller's error
/// surface already speaks [`LlmError`]. Runs inside the spawned chat task,
/// so single-flight supersede-abort covers every round.
pub async fn run_tool_loop(
    client: &dyn LlmClient,
    executor: &dyn ToolExecutor,
    mut messages: Vec<ChatMessage>,
    request_id: u64,
    on_token: TokenSink<'_>,
    on_event: ToolEventSink<'_>,
) -> Result<StreamOutcome, LlmError> {
    for round in 0..=MAX_TOOL_ROUNDS {
        let tools = if round < MAX_TOOL_ROUNDS { executor.definitions() } else { Vec::new() };
        let final_round = tools.is_empty();
        let request = ChatRequest { messages: std::mem::take(&mut messages), tools };
        let outcome = client.stream_chat(&request, on_token).await?;
        messages = request.messages;

        if outcome.tool_calls.is_empty() {
            if round > 0 {
                log::info!(
                    "llm: tool loop done after {round} tool round(s) (request={request_id})"
                );
            }
            return Ok(outcome);
        }
        if final_round {
            // The tools-stripped round still "called" a tool the request
            // never offered — terminate with the text we have rather than
            // loop; never silence (R006).
            log::warn!(
                "llm: tool call on the tools-stripped final round ignored (request={request_id})"
            );
            return Ok(StreamOutcome { tool_calls: Vec::new(), ..outcome });
        }

        // First half of the OpenAI round-trip: echo the requested calls.
        messages
            .push(ChatMessage::assistant_tool_calls(outcome.text.clone(), outcome.tool_calls.clone()));

        for call in &outcome.tool_calls {
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
}
