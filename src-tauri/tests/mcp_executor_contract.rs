//! M007 S02 T03 contract test: drive [`McpExecutor`] against an in-process
//! *fake* rmcp transport — a `tokio::io::duplex` pair with a tiny hand-written
//! [`ServerHandler`] on one end and a REAL rmcp client on the other. Unlike the
//! S01 live wire proof (`tests/mcp_stdio_live.rs`, `#[ignore]`-gated, needs
//! `npx` + network to spawn a child MCP server), this exercises the true
//! `tools/list` + `tools/call` protocol with NO child process and NO network,
//! so it runs in the default `cargo test`.
//!
//! What the fake proves that the pure-mapper unit tests in `mcp.rs` cannot:
//! - `McpExecutor::connect(peer)` fetches the catalogue over the real wire
//!   (`tools/list`) and namespaces every tool under `mcp__` — no built-in
//!   collision (S02 must-have 5);
//! - `execute()` marshals a [`ToolCall`] into an rmcp `tools/call` and maps the
//!   `CallToolResult` back: a happy call round-trips the payload (`ok: true`);
//! - the three typed failure kinds arrive over a real protocol round-trip, each
//!   as a `ToolOutcome::failure` and never an `Err` (R006):
//!   - a server-side `is_error: true` result → `mcp-tool-error` (`ok: false`);
//!   - a JSON-RPC error from the server (unknown tool) → `mcp-transport-error`;
//!   - a non-object `arguments` string → `invalid-arguments`, refused before
//!     any wire call.
//!
//! The fake advertises three tools so the collision, happy-path, and
//! server-side-error paths are all exercised against one served handler.

use rmcp::model::{
    CallToolRequestParams, CallToolResult, ContentBlock, ListToolsResult, PaginatedRequestParams,
    ServerCapabilities, ServerInfo, Tool,
};
use rmcp::service::{RequestContext, RunningService};
use rmcp::{ErrorData as McpError, RoleClient, RoleServer, ServerHandler, ServiceExt};

use third_eye_lib::llm::mcp::{
    McpExecutor, INVALID_ARGUMENTS_KIND, MCP_TOOL_ERROR_KIND, MCP_TOOL_PREFIX,
    MCP_TRANSPORT_ERROR_KIND,
};
use third_eye_lib::llm::toolloop::{
    ToolExecutor, ToolOutcome, FOCUS_APP_TOOL, INPUT_ACTION_TOOL, MEMORY_SEARCH_TOOL,
    SCREEN_QUERY_TOOL,
};
use third_eye_lib::llm::ToolCall;

/// The four built-in tool names an MCP tool must never structurally collide
/// with (S02 must-have 5) — asserted against the fake's namespaced catalogue.
const BUILTINS: [&str; 4] = [
    MEMORY_SEARCH_TOOL,
    INPUT_ACTION_TOOL,
    SCREEN_QUERY_TOOL,
    FOCUS_APP_TOOL,
];

/// The fake tool names the served handler advertises. `echo` is the happy
/// path; `always_fails` returns a server-side `is_error: true`; a call to any
/// unadvertised name yields a JSON-RPC error (the transport-error path).
const ECHO_TOOL: &str = "echo";
const ALWAYS_FAILS_TOOL: &str = "always_fails";

/// A minimal in-process MCP server: no macros, no child process. It advertises
/// two tools over `tools/list` and answers `tools/call` for each, so the
/// contract test drives the real rmcp protocol against a handler we fully
/// control (letting us script the server-side `is_error` and unknown-tool
/// paths deterministically).
#[derive(Clone, Default)]
struct FakeMcpServer;

impl FakeMcpServer {
    /// A one-string-property input schema shared by both fake tools — enough
    /// for the model (here, the test) to fill and for the server to read back.
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

impl ServerHandler for FakeMcpServer {
    fn get_info(&self) -> ServerInfo {
        // Advertise the tools capability so the client's `tools/list` is a
        // first-class, capability-backed request (mirrors a real server).
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        Ok(ListToolsResult::with_all_items(vec![
            Tool::new(
                ECHO_TOOL,
                "Echo the `message` argument straight back",
                Self::message_schema(),
            ),
            Tool::new(
                ALWAYS_FAILS_TOOL,
                "Always returns a server-side tool error",
                Self::message_schema(),
            ),
        ]))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        match request.name.as_ref() {
            ECHO_TOOL => {
                // Read the `message` the caller supplied and echo it in a text
                // content block — the happy round-trip the executor maps to
                // `ok: true` content.
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
            // The tool RAN and reported its own failure (not a wire fault):
            // a server-side `is_error: true` result the executor maps to
            // `ok: false` with the `mcp-tool-error` kind.
            ALWAYS_FAILS_TOOL => Ok(CallToolResult::error(vec![ContentBlock::text(
                "this tool always fails on purpose",
            )])),
            // An unadvertised tool is a protocol-level error: the client's
            // `call_tool` surfaces this as an `Err`, which the executor maps to
            // the distinct `mcp-transport-error` kind.
            other => Err(McpError::invalid_params(
                format!("no such tool: {other}"),
                None,
            )),
        }
    }
}

/// Everything the harness must keep alive for the duration of a test: the real
/// rmcp client's running service (dropping it cancels the connection, so the
/// peer inside `McpExecutor` would start failing) plus the served fake.
struct Harness {
    executor: McpExecutor,
    // Held only to keep the connection open; never touched directly.
    _client: RunningService<RoleClient, ()>,
    _server: RunningService<RoleServer, FakeMcpServer>,
}

/// Stand up the fake server + a real rmcp client over a `tokio::io::duplex`
/// pair, then build an [`McpExecutor`] from the client's peer (which performs
/// the real `tools/list` handshake). No child process, no network.
async fn connect_fake() -> Harness {
    let (server_transport, client_transport) = tokio::io::duplex(4096);

    // The server's `.serve()` blocks until the initialize handshake completes,
    // and the handshake needs the client — so spawn the server concurrently
    // rather than awaiting it before the client exists (else it deadlocks
    // waiting for an `initialize` that never arrives).
    let server_task = tokio::spawn(async move { FakeMcpServer.serve(server_transport).await });

    // A unit `()` ClientHandler is enough — the executor only issues requests.
    // This drives the handshake: it sends `initialize` + `initialized`, which
    // lets the spawned server's `.serve()` complete.
    let client = ()
        .serve(client_transport)
        .await
        .expect("rmcp client failed to complete the initialize handshake");

    let server = server_task
        .await
        .expect("fake MCP server task panicked")
        .expect("fake MCP server failed to complete the initialize handshake");

    // `Peer` is Clone; the running service keeps owning the connection.
    let executor = McpExecutor::connect(client.peer().clone())
        .await
        .expect("McpExecutor::connect failed over the in-process fake transport");

    Harness {
        executor,
        _client: client,
        _server: server,
    }
}

/// Build a `ToolCall` the way the tool-loop dispatcher hands one to an
/// executor: a namespaced name plus the raw JSON `arguments` string.
fn tool_call(name: &str, arguments: &str) -> ToolCall {
    ToolCall {
        id: "call_test".to_string(),
        name: name.to_string(),
        arguments: arguments.to_string(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn definitions_are_namespaced_and_never_collide_with_builtins() {
    let harness = connect_fake().await;
    let defs = harness.executor.definitions();

    let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
    assert_eq!(
        names,
        vec!["mcp__echo", "mcp__always_fails"],
        "the served catalogue must be namespaced under the mcp__ prefix"
    );

    // Every definition carries the prefix and none equals a built-in name —
    // structural non-collision, not a runtime check (S02 must-have 5).
    for def in &defs {
        assert!(
            def.name.starts_with(MCP_TOOL_PREFIX),
            "definition {} is missing the mcp__ prefix",
            def.name
        );
        assert!(
            !BUILTINS.contains(&def.name.as_str()),
            "definition {} collided with a built-in tool",
            def.name
        );
    }

    // The server's input_schema rides through verbatim as the model-facing
    // parameters object.
    let echo = defs
        .iter()
        .find(|d| d.name == "mcp__echo")
        .expect("mcp__echo must be advertised");
    assert_eq!(echo.parameters["properties"]["message"]["type"], "string");
    assert_eq!(
        echo.description,
        "Echo the `message` argument straight back"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn execute_round_trips_a_happy_call_over_the_real_wire() {
    let harness = connect_fake().await;

    let outcome: ToolOutcome = harness
        .executor
        .execute(&tool_call("mcp__echo", r#"{"message":"hello mcp"}"#))
        .await;

    assert!(outcome.ok, "happy echo call must be ok: {outcome:?}");
    assert_eq!(outcome.failure, None);
    assert_eq!(
        outcome.content, "echo: hello mcp",
        "the echoed payload must round-trip back through tools/call"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn execute_maps_a_server_side_is_error_to_the_mcp_tool_error_kind() {
    let harness = connect_fake().await;

    let outcome = harness
        .executor
        .execute(&tool_call("mcp__always_fails", r#"{"message":"x"}"#))
        .await;

    // The tool ran and reported failure — ok:false with the distinct
    // mcp-tool-error kind, the tool's own error text riding back to the model.
    assert!(!outcome.ok, "a server-side is_error must map to ok:false");
    assert_eq!(outcome.failure.as_deref(), Some(MCP_TOOL_ERROR_KIND));
    assert!(
        outcome.content.contains("always fails on purpose"),
        "the tool's error content must ride back: {:?}",
        outcome.content
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn execute_maps_a_protocol_error_to_the_mcp_transport_error_kind() {
    let harness = connect_fake().await;

    // No such tool on the server — the client's call_tool returns an Err
    // (JSON-RPC error response), which execute() must map to the distinct
    // transport-error kind, NEVER propagate as an Err (R006).
    let outcome = harness
        .executor
        .execute(&tool_call("mcp__does_not_exist", r#"{"message":"x"}"#))
        .await;

    assert!(!outcome.ok, "a protocol error must map to ok:false");
    assert_eq!(outcome.failure.as_deref(), Some(MCP_TRANSPORT_ERROR_KIND));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn execute_refuses_non_object_arguments_before_any_wire_call() {
    let harness = connect_fake().await;

    // A bare array is valid JSON but not a tool-arguments object; it must be
    // refused with the invalid-arguments kind before any tools/call is issued.
    let outcome = harness
        .executor
        .execute(&tool_call("mcp__echo", "[1,2,3]"))
        .await;

    assert!(!outcome.ok, "non-object arguments must map to ok:false");
    assert_eq!(outcome.failure.as_deref(), Some(INVALID_ARGUMENTS_KIND));
}
