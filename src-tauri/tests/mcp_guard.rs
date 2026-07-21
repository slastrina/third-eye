//! M007 S03 T04 runtime gate-integration test: drive the wired
//! [`McpApprovalGate`] (the guard the production mount wraps `McpExecutor` in)
//! against the SAME in-process *fake* rmcp transport the S02 contract test uses
//! (`tests/mcp_executor_contract.rs`) — a `tokio::io::duplex` pair with a tiny
//! hand-written `ServerHandler` on one end and a REAL rmcp client on the other.
//! No child process, no network, so it runs in the default `cargo test`.
//!
//! What this proves that the pure resolver unit tests (`mcp.rs`) and the
//! structural check (`scripts/check-mcp-guard.sh`) cannot: the *wired* gate
//! actually blocks / allows at RUNTIME, and — the slice's central claim — a
//! blocked MCP tool call **never reaches the server's `call_tool`**. The fake
//! server counts every `call_tool` it receives, so each test asserts the
//! observable side effect directly, not just the returned outcome:
//!
//! - Off mode → the call is refused with the typed `mcp-action-blocked` kind
//!   and the server's `call_tool` counter stays at 0 (the wire is never
//!   touched, D038);
//! - Ask mode + a `Deny` verdict from the injected prompt seam → the same
//!   typed `mcp-action-blocked` block, counter still 0 (a denied confirmation
//!   never reaches the server);
//! - AutoRun (and Ask + `AllowOnce` / `AllowTool`) → the call reaches the
//!   server, returns its real result (`ok: true`), and the counter increments;
//! - `AllowTool` grants the tool name in the session allowlist so the *second*
//!   call to the same tool performs WITHOUT re-prompting (the prompt seam is
//!   consulted once, the server is reached twice).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, ContentBlock, ListToolsResult, PaginatedRequestParams,
    ServerCapabilities, ServerInfo, Tool,
};
use rmcp::service::{RequestContext, RunningService};
use rmcp::{ErrorData as McpError, RoleClient, RoleServer, ServerHandler, ServiceExt};

use third_eye_lib::llm::mcp::{
    McpAllowlist, McpApprovalGate, McpApprovalPrompt, McpApprovalVerdict, McpExecutor, McpRunMode,
    MCP_ACTION_BLOCKED_KIND,
};
use third_eye_lib::llm::toolloop::{ToolExecutor, ToolOutcome};
use third_eye_lib::llm::ToolCall;

/// The single tool the fake advertises — a happy `echo` whose invocation the
/// gate either blocks before the wire or lets through to the server.
const ECHO_TOOL: &str = "echo";

/// A minimal in-process MCP server that COUNTS every `call_tool` it receives.
/// The counter is the observable the gate-integration tests assert on: a
/// blocked call must leave it at 0 (the server was never reached), an allowed
/// call must increment it. Cloneable because `.serve()` takes the handler by
/// value; the counter is an `Arc` so a handle held outside the served instance
/// observes the same count.
#[derive(Clone)]
struct CountingMcpServer {
    call_count: Arc<AtomicUsize>,
}

impl CountingMcpServer {
    fn new() -> Self {
        Self {
            call_count: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn message_schema() -> rmcp::model::JsonObject {
        let serde_json::Value::Object(map) = serde_json::json!({
            "type": "object",
            "properties": { "message": { "type": "string" } },
            "required": ["message"]
        }) else {
            unreachable!("object literal is always a JSON object")
        };
        map
    }
}

impl ServerHandler for CountingMcpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        Ok(ListToolsResult::with_all_items(vec![Tool::new(
            ECHO_TOOL,
            "Echo the `message` argument straight back",
            Self::message_schema(),
        )]))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        // Record that the wire choke point was reached — the observable a
        // blocked call must NOT produce.
        self.call_count.fetch_add(1, Ordering::SeqCst);
        match request.name.as_ref() {
            ECHO_TOOL => {
                let message = request
                    .arguments
                    .as_ref()
                    .and_then(|args| args.get("message"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                Ok(CallToolResult::success(vec![ContentBlock::text(format!(
                    "echo: {message}"
                ))]))
            }
            other => Err(McpError::invalid_params(format!("no such tool: {other}"), None)),
        }
    }
}

/// An injected [`McpApprovalPrompt`] that returns a scripted verdict and counts
/// how many times it was consulted. Lets a test prove the prompt seam is
/// reached exactly when the resolver says `Prompt` (Ask + ungranted) and NOT
/// when the mode short-circuits (Off / AutoRun / already-allowlisted).
struct ScriptedPrompt {
    verdict: McpApprovalVerdict,
    prompts: Arc<AtomicUsize>,
}

impl ScriptedPrompt {
    fn new(verdict: McpApprovalVerdict) -> (Arc<Self>, Arc<AtomicUsize>) {
        let prompts = Arc::new(AtomicUsize::new(0));
        (
            Arc::new(Self {
                verdict,
                prompts: prompts.clone(),
            }),
            prompts,
        )
    }
}

#[async_trait]
impl McpApprovalPrompt for ScriptedPrompt {
    async fn request(&self, _tool_name: String, _summary: String) -> McpApprovalVerdict {
        self.prompts.fetch_add(1, Ordering::SeqCst);
        self.verdict
    }
}

/// Everything a test keeps alive: the wired gate under test plus a handle on the
/// server's `call_tool` counter. Dropping `_client`/`_server` tears the
/// connection down, so they are held for the test's duration.
struct Harness {
    gate: McpApprovalGate,
    call_count: Arc<AtomicUsize>,
    _client: RunningService<RoleClient, ()>,
    _server: RunningService<RoleServer, CountingMcpServer>,
}

/// Stand up the counting fake server + a real rmcp client over a
/// `tokio::io::duplex` pair, build an [`McpExecutor`] from the client peer, and
/// WRAP it in an [`McpApprovalGate`] with the given mode + prompt seam — the
/// exact composition the production `commands.rs` mount performs, so the test
/// exercises the real guarded surface.
async fn connect_gated(
    mode: McpRunMode,
    approver: Arc<dyn McpApprovalPrompt>,
) -> (Harness, Arc<Mutex<McpAllowlist>>) {
    let server = CountingMcpServer::new();
    let call_count = server.call_count.clone();

    let (server_transport, client_transport) = tokio::io::duplex(4096);

    // Spawn the server concurrently: its `.serve()` blocks until the client
    // drives the initialize handshake, so it cannot be awaited first.
    let server_task = tokio::spawn(async move { server.serve(server_transport).await });

    let client = ()
        .serve(client_transport)
        .await
        .expect("rmcp client failed to complete the initialize handshake");

    let served = server_task
        .await
        .expect("counting MCP server task panicked")
        .expect("counting MCP server failed to complete the initialize handshake");

    let executor = McpExecutor::connect(client.peer().clone())
        .await
        .expect("McpExecutor::connect failed over the in-process fake transport");

    // The session allowlist the gate mutates on "Always allow this tool" — a
    // handle is returned so a test can inspect the grant.
    let allowlist = Arc::new(Mutex::new(McpAllowlist::new()));
    let gate = McpApprovalGate::new(executor, mode, allowlist.clone(), approver);

    (
        Harness {
            gate,
            call_count,
            _client: client,
            _server: served,
        },
        allowlist,
    )
}

/// Build a `ToolCall` the way the tool-loop dispatcher hands one to an executor:
/// a namespaced name plus the raw JSON `arguments` string.
fn tool_call(name: &str, arguments: &str) -> ToolCall {
    ToolCall {
        id: "call_test".to_string(),
        name: name.to_string(),
        arguments: arguments.to_string(),
    }
}

// A prompt seam that must never be consulted (Off / AutoRun paths never
// resolve to Prompt). Panics if reached so a wrongful prompt is a hard failure.
struct UnreachablePrompt;

#[async_trait]
impl McpApprovalPrompt for UnreachablePrompt {
    async fn request(&self, tool_name: String, _summary: String) -> McpApprovalVerdict {
        panic!("the prompt seam must not be consulted for tool {tool_name}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn off_mode_blocks_before_the_wire_and_the_server_is_never_reached() {
    let (harness, _allowlist) = connect_gated(McpRunMode::Off, Arc::new(UnreachablePrompt)).await;

    let outcome: ToolOutcome = harness
        .gate
        .execute(&tool_call("mcp__echo", r#"{"message":"hello"}"#))
        .await;

    // Typed, visible block — never a silent no-op (R006/R007).
    assert!(!outcome.ok, "an Off-mode MCP call must be blocked: {outcome:?}");
    assert_eq!(
        outcome.failure.as_deref(),
        Some(MCP_ACTION_BLOCKED_KIND),
        "a blocked call must carry the distinct mcp-action-blocked kind"
    );
    // The slice's central claim: the server's call_tool was NEVER reached.
    assert_eq!(
        harness.call_count.load(Ordering::SeqCst),
        0,
        "a blocked MCP call must never reach the server's call_tool"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ask_mode_with_a_deny_verdict_blocks_and_the_server_is_never_reached() {
    let (approver, prompts) = ScriptedPrompt::new(McpApprovalVerdict::Deny);
    let (harness, _allowlist) = connect_gated(McpRunMode::Ask, approver).await;

    let outcome = harness
        .gate
        .execute(&tool_call("mcp__echo", r#"{"message":"hello"}"#))
        .await;

    // The user was prompted exactly once, denied, and the block is the typed
    // attributable kind.
    assert_eq!(
        prompts.load(Ordering::SeqCst),
        1,
        "Ask mode must consult the prompt seam once for an ungranted tool"
    );
    assert!(!outcome.ok, "a denied MCP call must be blocked: {outcome:?}");
    assert_eq!(outcome.failure.as_deref(), Some(MCP_ACTION_BLOCKED_KIND));
    assert_eq!(
        harness.call_count.load(Ordering::SeqCst),
        0,
        "a prompt-denied MCP call must never reach the server's call_tool"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auto_run_mode_reaches_the_server_and_returns_its_result_unprompted() {
    let (harness, _allowlist) =
        connect_gated(McpRunMode::AutoRun, Arc::new(UnreachablePrompt)).await;

    let outcome = harness
        .gate
        .execute(&tool_call("mcp__echo", r#"{"message":"hello mcp"}"#))
        .await;

    // AutoRun performs without a prompt: the call reaches the server and its
    // real result rides back.
    assert!(outcome.ok, "an AutoRun MCP call must run: {outcome:?}");
    assert_eq!(outcome.failure, None);
    assert_eq!(
        outcome.content, "echo: hello mcp",
        "the server's real result must ride back through the gate"
    );
    assert_eq!(
        harness.call_count.load(Ordering::SeqCst),
        1,
        "an allowed MCP call must reach the server's call_tool exactly once"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ask_mode_with_allow_once_reaches_the_server_but_does_not_grant() {
    let (approver, prompts) = ScriptedPrompt::new(McpApprovalVerdict::AllowOnce);
    let (harness, allowlist) = connect_gated(McpRunMode::Ask, approver).await;

    let outcome = harness
        .gate
        .execute(&tool_call("mcp__echo", r#"{"message":"once"}"#))
        .await;

    assert_eq!(prompts.load(Ordering::SeqCst), 1, "AllowOnce must be prompted");
    assert!(outcome.ok, "an AllowOnce MCP call must run: {outcome:?}");
    assert_eq!(outcome.content, "echo: once");
    assert_eq!(harness.call_count.load(Ordering::SeqCst), 1);
    // AllowOnce performs WITHOUT allowlisting: the tool is not granted for the
    // session, so a subsequent call would prompt again.
    assert!(
        allowlist.lock().unwrap().is_empty(),
        "AllowOnce must not grant the tool for the session"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ask_mode_with_allow_tool_grants_and_the_second_call_runs_unprompted() {
    let (approver, prompts) = ScriptedPrompt::new(McpApprovalVerdict::AllowTool);
    let (harness, allowlist) = connect_gated(McpRunMode::Ask, approver).await;

    // First call: prompted, "Always allow this tool" → runs and grants.
    let first = harness
        .gate
        .execute(&tool_call("mcp__echo", r#"{"message":"first"}"#))
        .await;
    assert!(first.ok, "first AllowTool call must run: {first:?}");
    assert_eq!(first.content, "echo: first");
    assert!(
        allowlist.lock().unwrap().contains("mcp__echo"),
        "AllowTool must grant the tool name for the session"
    );

    // Second call to the SAME tool: the resolver now says Perform (allowlisted),
    // so the prompt seam is NOT consulted again — yet the server IS reached.
    let second = harness
        .gate
        .execute(&tool_call("mcp__echo", r#"{"message":"second"}"#))
        .await;
    assert!(second.ok, "second call must run unprompted: {second:?}");
    assert_eq!(second.content, "echo: second");

    assert_eq!(
        prompts.load(Ordering::SeqCst),
        1,
        "a session-granted tool must be prompted only once"
    );
    assert_eq!(
        harness.call_count.load(Ordering::SeqCst),
        2,
        "both allowed calls must reach the server's call_tool"
    );
}
