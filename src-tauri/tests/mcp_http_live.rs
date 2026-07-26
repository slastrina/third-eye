//! M007 S05 T06 — the remote-HTTP transport's two acceptance proofs.
//!
//! The roadmap requires "a contract test against a fake HTTP transport plus a
//! live connect to a real remote or hosted server". This file carries both,
//! mirroring the stdio slice's split (`mcp_executor_contract.rs` runs in the
//! default `cargo test`; `mcp_lifecycle_live.rs` is `#[ignore]`-gated):
//!
//! - **CONTRACT (default `cargo test`, no network beyond loopback):** drive the
//!   PRODUCTION [`build_http_config`] into a REAL rmcp
//!   [`StreamableHttpClientTransport`] pointed at a tiny hand-written HTTP server
//!   (a raw `tokio::net::TcpListener`, no rmcp server handler). Prove that when a
//!   bearer token is configured the very first request the transport sends
//!   carries `Authorization: Bearer <token>` — exactly one `Bearer ` prefix (our
//!   code hands the RAW token to rmcp; reqwest adds the scheme) — and that with
//!   no token no `Authorization` header is attached at all. This is the
//!   auth-header-on-the-wire assertion the `build_http_config` unit tests in
//!   `mcp_spawn.rs` cannot make (they only inspect the config field).
//!
//! - **LIVE (`#[ignore]`-gated, needs a real remote server):** connect to a real
//!   remote HTTP/SSE MCP server, list its tools through the production
//!   [`McpExecutor::connect`] path, and drive one call through the SAME
//!   [`McpApprovalGate`] the stdio path uses: `AutoRun` lets it reach the wire,
//!   `Off` blocks it fail-closed with the typed `mcp-action-blocked` kind before
//!   the wire. The endpoint, bearer token, tool name, and arguments come from
//!   env vars so the cannot-be-simulated acceptance runs against any real remote
//!   MCP server:
//!
//! ```text
//! THIRD_EYE_MCP_HTTP_URL=https://mcp.example.com/mcp \
//! THIRD_EYE_MCP_HTTP_TOKEN=... \
//! THIRD_EYE_MCP_HTTP_TOOL=echo \
//! THIRD_EYE_MCP_HTTP_ARGS='{"message":"hi"}' \
//! cargo test --locked --test mcp_http_live -- --ignored --nocapture
//! ```

use std::time::Duration;

use rmcp::transport::StreamableHttpClientTransport;
use rmcp::ServiceExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::oneshot;

use third_eye_lib::llm::mcp_spawn::build_http_config;

// A generous handshake ceiling (mirrors the production `mcp_spawn` timeouts) and
// a tighter op ceiling, so a wedged/unreachable endpoint fails as a NAMED
// timeout rather than an indefinite test hang (R006).
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(120);
const OP_TIMEOUT: Duration = Duration::from_secs(30);
// The contract mock is pure loopback; the first request lands in milliseconds,
// so a short ceiling keeps a regression (header never sent) from hanging CI.
const CONTRACT_TIMEOUT: Duration = Duration::from_secs(10);

// ===========================================================================
// CONTRACT TEST — fake HTTP transport, proves auth_header reaches the wire.
// ===========================================================================

/// Stand up a one-shot loopback HTTP server, point the PRODUCTION
/// [`build_http_config`] + a real [`StreamableHttpClientTransport`] at it, and
/// return whatever `Authorization` header value the transport's first request
/// carried (`None` if it sent none). No rmcp server handler and no real MCP
/// handshake — we only need the client to *send* its first request; the mock
/// records the header, replies with a throwaway 200, and closes. The
/// `().serve()` attempt is expected to fail (the mock is not a real MCP server);
/// that failure is irrelevant — the assertion is purely on the captured header,
/// so this is deterministic and never depends on rmcp handshake internals.
async fn capture_first_request_authorization(token: Option<&str>) -> Option<String> {
    // Bind loopback:0 so the OS picks a free port — no fixed-port flakiness.
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("failed to bind the loopback contract HTTP server");
    let addr = listener
        .local_addr()
        .expect("mock server has no local addr");
    let url = format!("http://{addr}/mcp");

    // The mock: accept exactly one connection, read the request head (up to the
    // blank line terminating the headers), extract the Authorization header, and
    // hand it back over a oneshot. Then reply 200 + close so the client's send
    // side completes cleanly.
    let (tx, rx) = oneshot::channel::<Option<String>>();
    tokio::spawn(async move {
        let Ok((mut socket, _peer)) = listener.accept().await else {
            let _ = tx.send(None);
            return;
        };
        // Read until the header terminator `\r\n\r\n` (or a sane cap / EOF). The
        // Authorization header rides in the request head, so we never need the
        // body. reqwest sends headers up front, so this resolves immediately.
        let mut buf = Vec::with_capacity(4096);
        let mut chunk = [0u8; 1024];
        loop {
            match socket.read(&mut chunk).await {
                Ok(0) => break, // EOF before a full head — parse what we have.
                Ok(n) => {
                    buf.extend_from_slice(&chunk[..n]);
                    if buf.windows(4).any(|w| w == b"\r\n\r\n") || buf.len() > 64 * 1024 {
                        break;
                    }
                }
                Err(_) => break,
            }
        }

        // Case-insensitive scan for the `Authorization:` request header, taking
        // its trimmed value. HTTP header names are case-insensitive (reqwest
        // lower-cases them on HTTP/1.1), so match on the lowercased line.
        let head = String::from_utf8_lossy(&buf);
        let auth = head.lines().find_map(|line| {
            let (name, value) = line.split_once(':')?;
            if name.trim().eq_ignore_ascii_case("authorization") {
                Some(value.trim().to_string())
            } else {
                None
            }
        });
        let _ = tx.send(auth);

        // A throwaway response so the client's request completes without a
        // connection-reset race; the body is deliberately not valid MCP.
        let _ = socket
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}")
            .await;
        let _ = socket.flush().await;
    });

    // Drive the PRODUCTION builder into a real streamable-HTTP transport and
    // start the service. We do NOT await serve for success — the mock is not a
    // real MCP server, so serve will error; a bounded spawn just guarantees the
    // client sends its first request (attaching the header) and can never hang.
    let config = build_http_config(Some(&url), token).expect("contract url is valid");
    let transport = StreamableHttpClientTransport::from_config(config);
    let serve = tokio::spawn(async move {
        let _ = tokio::time::timeout(CONTRACT_TIMEOUT, ().serve(transport)).await;
    });

    let captured = tokio::time::timeout(CONTRACT_TIMEOUT, rx)
        .await
        .expect("the contract mock never received a request (auth header never sent?)")
        .expect("the contract mock dropped its sender without capturing a request");

    serve.abort();
    captured
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn contract_attaches_bearer_authorization_header_to_the_first_request() {
    // The core auth contract: a configured token reaches the wire as exactly
    // `Authorization: Bearer <token>` — one `Bearer ` prefix, proving our code
    // hands rmcp the RAW token and rmcp/reqwest adds the scheme (no double
    // prefix, no dropped header).
    let token = "contract-token-abc123";
    let captured = capture_first_request_authorization(Some(token)).await;
    assert_eq!(
        captured.as_deref(),
        Some(format!("Bearer {token}").as_str()),
        "the streamable-HTTP transport must attach `Authorization: Bearer <token>` \
         to its first request when a bearer token is configured"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn contract_attaches_no_authorization_header_without_a_token() {
    // The unauthenticated path: an http server with no token attaches NO
    // Authorization header at all — never an empty `Bearer ` or a stray scheme.
    let captured = capture_first_request_authorization(None).await;
    assert_eq!(
        captured, None,
        "no bearer token configured must mean no Authorization header on the wire, \
         got {captured:?}"
    );
}

// ===========================================================================
// LIVE TEST — a real remote HTTP MCP server through the guarded gate.
// ===========================================================================

use std::sync::{Arc, Mutex};

use third_eye_lib::llm::mcp::{
    McpAllowlist, McpApprovalGate, McpApprovalPrompt, McpApprovalVerdict, McpExecutor, McpRunMode,
    MCP_ACTION_BLOCKED_KIND, MCP_TOOL_PREFIX,
};
use third_eye_lib::llm::toolloop::ToolExecutor;
use third_eye_lib::llm::ToolCall;

/// An `McpApprovalPrompt` that must never be asked — `AutoRun`/`Off` resolve
/// purely, so any prompt here is a gate-logic regression surfaced as a loud
/// panic (mirrors `mcp_lifecycle_live.rs::NeverPrompt`).
struct NeverPrompt;

#[async_trait::async_trait]
impl McpApprovalPrompt for NeverPrompt {
    async fn request(&self, tool_name: String, _summary: String) -> McpApprovalVerdict {
        panic!("approval prompt was invoked for {tool_name} — AutoRun/Off must never prompt");
    }
}

/// LIVE — the S05 cannot-be-simulated acceptance: a REAL remote HTTP/SSE MCP
/// server's tools reach the guarded agent through the SAME executor + gate the
/// stdio path uses. Endpoint/token/tool/args come from env so this drives any
/// real remote server. `AutoRun` reaches the wire; `Off` fails closed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires a real remote HTTP MCP server; set THIRD_EYE_MCP_HTTP_URL (+ _TOKEN/_TOOL/_ARGS)"]
async fn live_remote_http_tools_reach_agent_through_the_guarded_gate() {
    // --- resolve the remote endpoint from env ------------------------------
    let url = std::env::var("THIRD_EYE_MCP_HTTP_URL").expect(
        "set THIRD_EYE_MCP_HTTP_URL to a real remote HTTP MCP server endpoint to run this live test",
    );
    let token = std::env::var("THIRD_EYE_MCP_HTTP_TOKEN").ok();
    // The tool to exercise through the gate and its JSON arguments. Defaults
    // target a server-everything-style `echo`; override for any other server.
    let tool = std::env::var("THIRD_EYE_MCP_HTTP_TOOL").unwrap_or_else(|_| "echo".to_string());
    let args = std::env::var("THIRD_EYE_MCP_HTTP_ARGS")
        .unwrap_or_else(|_| r#"{"message":"hi from S05 live"}"#.to_string());

    // --- connect via the PRODUCTION build_http_config + serve path ----------
    let config = build_http_config(Some(&url), token.as_deref())
        .expect("THIRD_EYE_MCP_HTTP_URL must be a valid non-blank url");
    let transport = StreamableHttpClientTransport::from_config(config);
    let client = tokio::time::timeout(HANDSHAKE_TIMEOUT, ().serve(transport))
        .await
        .expect("remote MCP initialize handshake did not complete before timeout")
        .expect("remote MCP initialize handshake returned an error (bad url/auth?)");
    eprintln!(
        "[handshake] OK — peer_info={:?}",
        client.peer_info().map(|i| i.server_info.clone())
    );

    // --- tools/list via the production McpExecutor::connect path ------------
    let executor = McpExecutor::connect(client.peer().clone())
        .await
        .expect("McpExecutor::connect must complete tools/list against the remote server");
    let defs = executor.definitions();
    assert!(
        !defs.is_empty(),
        "the remote server must advertise at least one tool"
    );
    assert!(
        defs.iter().all(|d| d.name.starts_with(MCP_TOOL_PREFIX)),
        "every advertised remote tool is namespaced under mcp__ (no built-in collision)"
    );
    let tool_names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
    eprintln!("[tools/list] OK — {} tools: {tool_names:?}", defs.len());

    let namespaced = format!("{MCP_TOOL_PREFIX}{tool}");
    assert!(
        tool_names.contains(&namespaced.as_str()),
        "the configured THIRD_EYE_MCP_HTTP_TOOL `{tool}` is not advertised; \
         available: {tool_names:?}"
    );
    let call = ToolCall {
        id: "call_live_http_1".to_string(),
        name: namespaced.clone(),
        arguments: args,
    };

    // --- AutoRun: the call reaches the wire through the gate ----------------
    // AutoRun resolves to Perform, so the gate forwards to call_tool. We assert
    // it was NOT gate-blocked: the outcome is either ok or a typed wire/tool
    // failure, never `mcp-action-blocked`. That is the "remote tool reaches the
    // agent through the single guarded choke point" proof against a real server.
    let auto_gate = McpApprovalGate::new(
        executor,
        McpRunMode::AutoRun,
        Arc::new(Mutex::new(McpAllowlist::new())),
        Arc::new(NeverPrompt),
    );
    let outcome = tokio::time::timeout(OP_TIMEOUT, auto_gate.execute(&call))
        .await
        .expect("gate-driven remote call timed out");
    assert_ne!(
        outcome.failure.as_deref(),
        Some(MCP_ACTION_BLOCKED_KIND),
        "an AutoRun call must reach the wire, not be gate-blocked: {outcome:?}"
    );
    eprintln!(
        "[gate AutoRun] OK — call reached the wire (ok={}, failure={:?})",
        outcome.ok, outcome.failure
    );

    // --- Off: the SAME tool is blocked fail-closed before the wire ----------
    // A fresh executor over a clone of the SAME live peer, wrapped in an Off
    // gate: the call is refused with the typed blocked kind before any wire call
    // (D038 — the allowlist cannot un-inert a disabled surface).
    let off_executor = McpExecutor::connect(client.peer().clone())
        .await
        .expect("second McpExecutor::connect over the live remote peer must succeed");
    let off_gate = McpApprovalGate::new(
        off_executor,
        McpRunMode::Off,
        Arc::new(Mutex::new(McpAllowlist::new())),
        Arc::new(NeverPrompt),
    );
    let blocked = off_gate.execute(&call).await;
    assert!(!blocked.ok, "an Off-mode remote MCP call must be blocked");
    assert_eq!(
        blocked.failure.as_deref(),
        Some(MCP_ACTION_BLOCKED_KIND),
        "Off blocks with the typed mcp-action-blocked kind, before the wire: {blocked:?}"
    );
    eprintln!("[gate Off] OK — blocked before the wire, typed mcp-action-blocked");

    // --- clean shutdown via the portable R020 cancellation path -------------
    let quit = tokio::time::timeout(OP_TIMEOUT, client.cancel())
        .await
        .expect("client.cancel() timed out")
        .expect("client.cancel() failed to join the service task");
    eprintln!("[shutdown] OK — clean cancel: {quit:?}");
}
