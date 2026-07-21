//! M007 S02 T04 integration proof (R029): a scripted model drives the production
//! `run_tool_loop` through an [`McpExecutor`] mounted in a [`CompositeExecutor`]
//! to a fake in-process MCP server and back — the first concrete proof an
//! external rmcp tool registers into the agent tool-loop. No child process, no
//! network: the model endpoint is a scripted HTTP/SSE server and the MCP server
//! is a `tokio::io::duplex`-served [`ServerHandler`] (both in-process), so this
//! runs in the default `cargo test`.
//!
//! It mirrors `chat_tool_calling::composite_routes_input_action_through_the_loop`
//! (the S01 CompositeExecutor loop proof) but for the MCP surface:
//! - round 0: the model streams a `mcp__echo` tool call, its `arguments` JSON
//!   fragmented across SSE deltas (the LM Studio shape, MEM080);
//! - the loop dispatches it through the CompositeExecutor to the McpExecutor,
//!   which round-trips a real rmcp `tools/call` over the duplex to the fake
//!   server;
//! - round 1: the model reads the echoed tool result and streams its answer.
//!
//! The wire capture proves request 0 advertised the namespaced `mcp__echo`
//! tool (no built-in collision) and request 1 carried the tool-role turn with
//! the server's echoed payload — the model → loop → McpExecutor → fake-server
//! round-trip, end to end.

use std::sync::Mutex;

use rmcp::model::{
    CallToolRequestParams, CallToolResult, ContentBlock, ListToolsResult, PaginatedRequestParams,
    ServerCapabilities, ServerInfo, Tool,
};
use rmcp::service::{RequestContext, RunningService};
use rmcp::{ErrorData as McpError, RoleClient, RoleServer, ServerHandler, ServiceExt};

use third_eye_lib::llm::mcp::{McpExecutor, MCP_TOOL_PREFIX};
use third_eye_lib::llm::openai::OpenAiClient;
use third_eye_lib::llm::toolloop::{run_tool_loop, CompositeExecutor, ToolEvent};
use third_eye_lib::llm::ChatMessage;

/// The fake tool the served handler advertises for the happy round-trip: `echo`
/// returns its `message` argument, so the loop's tool-role turn carries a
/// payload the model (and this test) can key on.
const ECHO_TOOL: &str = "echo";
const ALWAYS_FAILS_TOOL: &str = "always_fails";

// ---------------------------------------------------------------------------
// Fake in-process MCP server (same shape as the T03 contract test): no macros,
// no child process. It advertises two tools over `tools/list` and answers
// `tools/call`, so the loop drives the real rmcp protocol against a handler we
// fully control.
// ---------------------------------------------------------------------------

#[derive(Clone, Default)]
struct FakeMcpServer;

impl FakeMcpServer {
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
            ALWAYS_FAILS_TOOL => Ok(CallToolResult::error(vec![ContentBlock::text(
                "this tool always fails on purpose",
            )])),
            other => Err(McpError::invalid_params(
                format!("no such tool: {other}"),
                None,
            )),
        }
    }
}

/// Stand up the fake server + a real rmcp client over a `tokio::io::duplex`
/// pair, build an [`McpExecutor`] from the client's peer (which performs the
/// real `tools/list` handshake), and return it alongside the running services
/// that must stay alive for the connection to survive. No child, no network.
async fn connect_fake() -> (
    McpExecutor,
    RunningService<RoleClient, ()>,
    RunningService<RoleServer, FakeMcpServer>,
) {
    let (server_transport, client_transport) = tokio::io::duplex(4096);

    // The server's `.serve()` blocks until the initialize handshake completes,
    // which needs the client — so spawn it concurrently rather than await it
    // before the client exists (else it deadlocks).
    let server_task = tokio::spawn(async move { FakeMcpServer.serve(server_transport).await });

    let client = ()
        .serve(client_transport)
        .await
        .expect("rmcp client failed to complete the initialize handshake");

    let server = server_task
        .await
        .expect("fake MCP server task panicked")
        .expect("fake MCP server failed to complete the initialize handshake");

    let executor = McpExecutor::connect(client.peer().clone())
        .await
        .expect("McpExecutor::connect failed over the in-process fake transport");

    (executor, client, server)
}

// ---------------------------------------------------------------------------
// Scripted HTTP/SSE model server (same pattern as chat_tool_calling.rs): one
// pre-baked response per connection, in order, capturing every request's raw
// bytes. The tool loop makes one HTTP request per round, so this scripts a
// whole conversation.
// ---------------------------------------------------------------------------

mod scripted {
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// Serve `responses[i]` on the i-th accepted connection (each
    /// `connection: close`), and expose the captured request bytes per
    /// connection.
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

    /// True once `buf` holds the full request: complete headers plus
    /// `content-length` bytes of body.
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

    /// The JSON body of the i-th captured request.
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

    /// One streamed `delta.tool_calls` SSE event in the OpenAI shape: id and
    /// name on the first delta for an index, `arguments` string fragments on
    /// follow-ups (MEM080). Omitted fields are absent from the JSON.
    pub fn sse_tool_delta(
        index: u64,
        id: Option<&str>,
        name: Option<&str>,
        arguments: Option<&str>,
    ) -> String {
        let mut call = serde_json::json!({ "index": index });
        if let Some(id) = id {
            call["id"] = id.into();
        }
        let mut function = serde_json::Map::new();
        if let Some(name) = name {
            function.insert("name".into(), name.into());
        }
        if let Some(args) = arguments {
            function.insert("arguments".into(), args.into());
        }
        if !function.is_empty() {
            call["function"] = function.into();
        }
        format!(
            "data: {}\n\n",
            serde_json::json!({"choices": [{"delta": {"content": null, "tool_calls": [call]}}]})
        )
    }

    /// HTTP/1.1 200 chunked SSE response, terminated and connection-closed.
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
}

/// Collects the streamed answer + tool-phase events the loop emits.
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
}

/// R029: the scripted model calls the namespaced `mcp__echo` tool; the
/// production `run_tool_loop` dispatches it through the CompositeExecutor to the
/// McpExecutor, which round-trips a real rmcp `tools/call` to the fake server;
/// the echoed payload rides back as the tool-role turn and the model answers.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tool_loop_round_trips_an_mcp_tool_through_the_composite() {
    // Round 0: the model stops to call mcp__echo — id/name on the first delta,
    // the arguments JSON fragmented across two more (the LM Studio shape).
    let round0 = scripted::sse_200(&[
        scripted::sse_tool_delta(0, Some("call_mcp_1"), Some("mcp__echo"), None),
        scripted::sse_tool_delta(0, None, None, Some(r#"{"message":"hello "#)),
        scripted::sse_tool_delta(0, None, None, Some(r#"from the loop"}"#)),
        "data: [DONE]\n\n".to_string(),
    ]);
    // Round 1: having read the echoed tool result, the model streams its answer.
    let round1 = scripted::sse_200(&[
        scripted::sse_token("The MCP tool echoed: hello from the loop."),
        "data: [DONE]\n\n".to_string(),
    ]);
    let (endpoint, captured) = scripted::spawn(vec![round0, round1]).await;

    // The MCP side: a real rmcp client over a fake in-process server, wrapped in
    // the exact production CompositeExecutor mount shape. `_client`/`_server` are
    // held so the connection stays open for the whole loop.
    let (mcp_executor, _client, _server) = connect_fake().await;
    let executor = CompositeExecutor::new(vec![Box::new(mcp_executor)]);

    let client = OpenAiClient::new(&endpoint);
    let capture = Capture::new();
    let outcome = run_tool_loop(
        &client,
        &executor,
        vec![ChatMessage::user(
            "echo hello from the loop through the MCP tool",
        )],
        7,
        &|t| capture.tokens.lock().unwrap().push_str(t),
        &|e| capture.events.lock().unwrap().push(e.clone()),
    )
    .await
    .expect("scripted MCP conversation must resolve");

    // The final answer streamed through and landed on the outcome.
    assert_eq!(outcome.text, "The MCP tool echoed: hello from the loop.");
    assert_eq!(*capture.tokens.lock().unwrap(), outcome.text);
    assert!(
        outcome.tool_calls.is_empty(),
        "resolved loops leak no pending calls"
    );

    // Tool phases: one Call announcing the reassembled namespaced call, one ok
    // Result — the model → loop → McpExecutor → fake-server round-trip.
    let events = capture.events.lock().unwrap().clone();
    assert_eq!(events.len(), 2, "one call + one result: {events:?}");
    let ToolEvent::Call(call) = &events[0] else {
        panic!("first event must be Call")
    };
    assert_eq!(call.request_id, 7);
    assert_eq!(call.round, 0);
    assert_eq!(call.call.id, "call_mcp_1");
    assert_eq!(
        call.call.name, "mcp__echo",
        "the loop dispatched the namespaced MCP tool"
    );
    assert!(
        call.call.name.starts_with(MCP_TOOL_PREFIX),
        "the dispatched call carries the mcp__ prefix"
    );
    assert_eq!(
        call.call.arguments, r#"{"message":"hello from the loop"}"#,
        "split argument deltas must reassemble byte-for-byte"
    );
    let ToolEvent::Result(result) = &events[1] else {
        panic!("second event must be Result")
    };
    assert!(
        result.ok,
        "the happy echo round-trip must be ok: {result:?}"
    );
    assert_eq!(result.call_id, "call_mcp_1");
    assert_eq!(result.name, "mcp__echo");
    assert_eq!(result.failure, None);

    // Wire-level proof. Request 0 advertised the namespaced MCP tool (no
    // un-prefixed name reaches the model), a plain user message, no tool turns.
    let req0 = scripted::body_json(&captured, 0);
    let tool_names: Vec<&str> = req0["tools"]
        .as_array()
        .expect("round 0 advertises tools")
        .iter()
        .map(|t| t["function"]["name"].as_str().unwrap())
        .collect();
    assert!(
        tool_names.contains(&"mcp__echo"),
        "round 0 must advertise the namespaced mcp__echo tool: {tool_names:?}"
    );
    assert!(
        tool_names.iter().all(|n| n.starts_with(MCP_TOOL_PREFIX)),
        "every advertised tool is an MCP tool namespaced under mcp__: {tool_names:?}"
    );
    assert_eq!(req0["messages"].as_array().unwrap().len(), 1);
    assert_eq!(req0["messages"][0]["role"], "user");

    // Request 1: the OpenAI round-trip — assistant echo carrying the raw
    // arguments, then the tool-role result with the server's echoed payload.
    let req1 = scripted::body_json(&captured, 1);
    let messages = req1["messages"].as_array().unwrap();
    assert_eq!(
        messages.len(),
        3,
        "user + assistant echo + tool result: {messages:?}"
    );
    assert_eq!(messages[1]["role"], "assistant");
    assert_eq!(messages[1]["tool_calls"][0]["id"], "call_mcp_1");
    assert_eq!(
        messages[1]["tool_calls"][0]["function"]["name"],
        "mcp__echo"
    );
    assert_eq!(messages[2]["role"], "tool");
    assert_eq!(messages[2]["tool_call_id"], "call_mcp_1");
    let tool_content = messages[2]["content"].as_str().unwrap();
    assert!(
        tool_content.contains("echo: hello from the loop"),
        "the server's echoed payload must ride back through tools/call: {tool_content}"
    );
}
