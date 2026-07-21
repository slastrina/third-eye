//! M007 S01 live wire proof: drive the official rmcp Rust SDK's stdio
//! child-process client against a REAL spawned MCP server and prove the full
//! round-trip — initialize handshake, `tools/list`, `tools/call`, and a clean
//! child shutdown via `client.cancel()`. This is the seam S02's `McpExecutor`
//! will build on; nothing here is simulated (no in-process mock, no faked
//! JSON-RPC), so a protocol regression surfaces as a concrete assertion
//! failure — or, if the child hangs, as a `tokio::time::timeout` expiry rather
//! than a silent test hang.
//!
//! The server under test is the reference `@modelcontextprotocol/server-everything`
//! launched with `npx -y`. Following the repo's `#[ignore]`-gated live-test
//! convention (mirrors `memory_live.rs`, which requires LM Studio), this test
//! is skipped by default and requires `npx` + network (npm fetches the server
//! on first run). Run it explicitly:
//!
//!   cd src-tauri && cargo test --locked --test mcp_stdio_live -- --ignored --nocapture
//!
//! Proven over the real stdio wire:
//! - the initialize handshake completes (`peer_info()` is `Some`, i.e. the
//!   server returned an `InitializeResult`);
//! - `tools/list` returns at least one tool and includes the `echo` tool;
//! - `tools/call echo { message }` succeeds (`is_error != Some(true)`) and the
//!   returned text content carries the payload back;
//! - a `tools/call` on an unknown tool does NOT succeed (protocol error or an
//!   `is_error: true` result) — the negative-path assertion;
//! - `client.cancel()` shuts the service down and terminates the child cleanly.

use std::time::Duration;

use rmcp::{
    model::CallToolRequestParams,
    transport::{ConfigureCommandExt, TokioChildProcess},
    ServiceExt,
};
use tokio::process::Command;

// First-run `npx -y` may download the server from npm, so the handshake gets a
// generous ceiling; steady-state operations get a tighter one. Both exist so a
// wedged child fails as a named timeout, not an indefinite hang.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(120);
const OP_TIMEOUT: Duration = Duration::from_secs(30);

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires npx + network to fetch and run @modelcontextprotocol/server-everything over stdio"]
async fn mcp_stdio_roundtrip_against_real_server() {
    // --- spawn a REAL child MCP server over stdio ---------------------------
    // TokioChildProcess wires the child's stdin/stdout into rmcp's transport;
    // `.configure` is rmcp's ConfigureCommandExt builder over tokio's Command.
    let transport = TokioChildProcess::new(Command::new("npx").configure(|cmd| {
        cmd.arg("-y").arg("@modelcontextprotocol/server-everything");
    }))
    .expect("failed to spawn `npx` child process for the MCP server");

    // --- initialize handshake ----------------------------------------------
    // `()` is a unit ClientHandler (no client-side callbacks needed). `.serve()`
    // performs the initialize/initialized handshake and yields a RunningService.
    let client = tokio::time::timeout(HANDSHAKE_TIMEOUT, ().serve(transport))
        .await
        .expect("MCP initialize handshake did not complete before timeout (silent hang?)")
        .expect("MCP initialize handshake returned an error");

    let server_info = client
        .peer_info()
        .expect("handshake produced no peer_info (server sent no InitializeResult)");
    eprintln!(
        "[handshake] OK — protocol_version={:?} server={:?}",
        server_info.protocol_version, server_info.server_info
    );

    // --- tools/list --------------------------------------------------------
    let tools = tokio::time::timeout(OP_TIMEOUT, client.list_all_tools())
        .await
        .expect("tools/list timed out")
        .expect("tools/list returned an error");
    assert!(!tools.is_empty(), "server advertised zero tools");
    let tool_names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
    eprintln!("[tools/list] OK — {} tools: {:?}", tools.len(), tool_names);
    assert!(
        tool_names.contains(&"echo"),
        "expected an `echo` tool from server-everything, got {tool_names:?}"
    );

    // --- tools/call (happy path) -------------------------------------------
    // Build the arguments JsonObject directly; the `object!` macro lives behind
    // rmcp's `macros` feature, which our minimal client feature set omits.
    let mut args = serde_json::Map::new();
    args.insert(
        "message".to_string(),
        serde_json::Value::String("hi from rmcp".to_string()),
    );
    let call = client.call_tool(CallToolRequestParams::new("echo").with_arguments(args));
    let result = tokio::time::timeout(OP_TIMEOUT, call)
        .await
        .expect("tools/call echo timed out")
        .expect("tools/call echo returned an error");
    assert_ne!(
        result.is_error,
        Some(true),
        "echo tool reported is_error=true: {result:?}"
    );
    let echoed = result
        .content
        .iter()
        .find_map(|block| block.as_text())
        .map(|text| text.text.clone())
        .expect("echo tool returned no text content block");
    assert!(
        echoed.contains("hi from rmcp"),
        "echoed text did not carry the payload back: {echoed:?}"
    );
    eprintln!("[tools/call echo] OK — echoed: {echoed:?}");

    // --- tools/call (negative path) ----------------------------------------
    // An unknown tool must NOT succeed: rmcp surfaces this either as a transport
    // /protocol error (Err) or as a tool result flagged is_error=true. Either is
    // acceptable; a plain successful result is not.
    let bogus = client.call_tool(CallToolRequestParams::new("no_such_tool_xyz"));
    match tokio::time::timeout(OP_TIMEOUT, bogus).await {
        Err(_) => panic!("unknown-tool call timed out"),
        Ok(Err(err)) => eprintln!("[tools/call unknown] OK — rejected with protocol error: {err}"),
        Ok(Ok(res)) => {
            assert_eq!(
                res.is_error,
                Some(true),
                "unknown tool call unexpectedly succeeded: {res:?}"
            );
            eprintln!("[tools/call unknown] OK — rejected with is_error=true");
        }
    }

    // --- clean shutdown ----------------------------------------------------
    // `cancel()` cancels the service task and terminates the child; it is the
    // clean-shutdown path (no manual Child::kill needed).
    let quit = tokio::time::timeout(OP_TIMEOUT, client.cancel())
        .await
        .expect("client.cancel() timed out")
        .expect("client.cancel() failed to join the service task");
    eprintln!("[shutdown] OK — clean cancel: {quit:?}");
}
