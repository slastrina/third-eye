//! M007 S04 T05 live end-to-end proof: the roadmap acceptance for this slice is
//! "demonstrated live end-to-end against the reference stdio server" — a REAL
//! spawned child whose tools reach the agent through the guarded executor, and a
//! clean shutdown via the portable rmcp cancellation path (R020). The prior tasks
//! built the machinery (`McpState` seams, `McpExecutor`, `McpApprovalGate`,
//! `mcp_spawn::launch`); this test exercises it against a real child.
//!
//! Nothing here is simulated: the server under test is the reference
//! `@modelcontextprotocol/server-everything` launched with `npx -y` over a real
//! stdio transport (the same child `mcp_spawn::launch` spawns in production). The
//! test replays `launch()`'s exact sequence against the `McpState` seams without a
//! Tauri `AppHandle` (which cannot be constructed in a plain integration test):
//! spawn → bounded handshake → `tools/list` → `set_peer`/`set_shutdown_handle`/
//! `mark_ready` → drive a tool call through `McpApprovalGate` → cancel. Following
//! the repo's `#[ignore]`-gated live-test convention (mirrors `mcp_stdio_live.rs`
//! and `memory_live.rs`), both tests are skipped by default and require `npx` +
//! network (npm fetches the server on first run). Run them explicitly:
//!
//!   cd src-tauri && cargo test --locked --test mcp_lifecycle_live -- --ignored --nocapture
//!
//! Proven live, end to end:
//! - **Test A (happy path + tools reach the agent + clean shutdown):** a real
//!   child handshakes; its tool catalogue lands on the health value
//!   (`phase=ready`, `toolCount>0`); a real `mcp__echo` call driven through the
//!   `McpApprovalGate` in `AutoRun` reaches the server and the payload rides back;
//!   the same tool in `Off` mode is blocked with the typed
//!   `mcp-action-blocked` kind before the wire (fail-closed); then
//!   `take_shutdown_handle().cancel()` shuts the child down and the supervisor's
//!   `waiting()` resolves to `QuitReason::Cancelled` (the clean R020 path) —
//!   after which the peer is cleared.
//! - **Test B (real mid-session crash degrades, never a panic):** the real child
//!   is `SIGKILL`ed mid-session; the supervisor's `waiting()` returns a
//!   NON-`Cancelled` reason, so health degrades to `crashed` with the cause named
//!   and the peer is cleared; a subsequent tool call rides back a typed
//!   `mcp-transport-error` outcome (never an `Err`, never a panic — R006/R007).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use rmcp::service::QuitReason;
use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
use rmcp::ServiceExt;
use tokio::process::Command;

use third_eye_lib::llm::mcp::{
    McpAllowlist, McpApprovalGate, McpApprovalPrompt, McpApprovalVerdict, McpExecutor, McpPhase,
    McpRunMode, McpState, MCP_ACTION_BLOCKED_KIND, MCP_TOOL_PREFIX, MCP_TRANSPORT_ERROR_KIND,
};
use third_eye_lib::llm::toolloop::ToolExecutor;
use third_eye_lib::llm::ToolCall;

// First-run `npx -y` may download the server from npm, so the handshake gets a
// generous ceiling (mirrors the production `mcp_spawn::HANDSHAKE_TIMEOUT`);
// steady-state operations get a tighter one. Both exist so a wedged child fails
// as a named timeout, not an indefinite hang (R006).
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(120);
const OP_TIMEOUT: Duration = Duration::from_secs(30);

/// An `McpApprovalPrompt` that must never be asked — used in `AutoRun`/`Off` mode
/// tests where the pure resolver never returns `Prompt`, so any call here is a
/// gate-logic regression surfaced as a loud panic rather than a silent hang.
struct NeverPrompt;

#[async_trait::async_trait]
impl McpApprovalPrompt for NeverPrompt {
    async fn request(&self, tool_name: String, _summary: String) -> McpApprovalVerdict {
        panic!("approval prompt was invoked for {tool_name} — AutoRun/Off must never prompt");
    }
}

/// Spawn the reference stdio server as a REAL child (the production
/// `mcp_spawn::launch` shape), returning the transport plus the child pid. The pid
/// (captured before the transport is moved into `.serve()`) is how Test B forces a
/// genuine mid-session crash with a signal.
fn spawn_reference_server() -> (TokioChildProcess, Option<u32>) {
    let transport = TokioChildProcess::new(Command::new("npx").configure(|cmd| {
        cmd.arg("-y").arg("@modelcontextprotocol/server-everything");
    }))
    .expect("failed to spawn `npx` child process for the reference MCP server");
    let pid = transport.id();
    (transport, pid)
}

/// Build a namespaced echo `ToolCall` — the reference server advertises `echo`, so
/// through the executor's `mcp__` namespace it is `mcp__echo`.
fn echo_call(message: &str) -> ToolCall {
    ToolCall {
        id: "call_live_1".to_string(),
        name: format!("{MCP_TOOL_PREFIX}echo"),
        arguments: serde_json::json!({ "message": message }).to_string(),
    }
}

/// TEST A — the primary S04 acceptance: a real child's tools reach the agent
/// through the guarded executor, and the child shuts down cleanly on the portable
/// R020 cancellation path.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires npx + network to fetch and run @modelcontextprotocol/server-everything over stdio"]
async fn mcp_lifecycle_ready_tools_reach_agent_then_clean_shutdown() {
    let state = McpState::new();

    // --- mark_spawning: the launch task's first transition ------------------
    state.mark_spawning();
    let s = state.status();
    assert_eq!(s.phase, McpPhase::Spawning, "launch marks spawning before the child comes up");
    assert_eq!(s.tool_count, 0, "no tools reachable while spawning");

    // --- spawn a REAL child + bounded handshake -----------------------------
    let (transport, _pid) = spawn_reference_server();
    let client = tokio::time::timeout(HANDSHAKE_TIMEOUT, ().serve(transport))
        .await
        .expect("MCP initialize handshake did not complete before timeout (silent hang?)")
        .expect("MCP initialize handshake returned an error");
    eprintln!("[handshake] OK — peer_info={:?}", client.peer_info().map(|i| i.server_info.clone()));

    // --- tools/list via the production McpExecutor::connect path ------------
    // This is the exact seam `mcp_spawn::launch` uses: connect performs the real
    // `tools/list` over the wire and namespaces the catalogue for the model.
    let executor = McpExecutor::connect(client.peer().clone())
        .await
        .expect("McpExecutor::connect must complete tools/list against the real server");
    let tool_count = executor.definitions().len();
    assert!(tool_count > 0, "the reference server must advertise at least one tool");
    assert!(
        executor.definitions().iter().all(|d| d.name.starts_with(MCP_TOOL_PREFIX)),
        "every advertised tool is namespaced under mcp__ (no built-in collision)"
    );
    assert!(
        executor.definitions().iter().any(|d| d.name == format!("{MCP_TOOL_PREFIX}echo")),
        "the reference server-everything must advertise an echo tool"
    );

    // --- inject the peer + retain the shutdown handle (launch()'s sequence) --
    let shutdown = client.cancellation_token();
    state.set_peer(client.peer().clone());
    state.set_shutdown_handle(shutdown);
    state.mark_ready(tool_count);

    let s = state.status();
    assert_eq!(s.phase, McpPhase::Ready, "a handshaked child is ready");
    assert_eq!(s.tool_count, tool_count, "ready records the advertised tool count");
    assert_eq!(s.last_error, None, "a healthy child carries no error");
    assert!(s.updated_at > 0, "a lifecycle transition stamps a real timestamp");
    assert!(state.peer().is_some(), "the injected peer lights up the gate's Some(peer) branch");
    eprintln!("[ready] OK — {tool_count} tools injected, health=ready");

    // --- tools reach the agent: a REAL echo call through the guarded gate ---
    // AutoRun so the pure resolver returns Perform (never Prompt); the gate is the
    // exact production mount wrap. This is the "external tool reaches the agent
    // through the single guarded choke point" proof against a real child.
    let auto_gate = McpApprovalGate::new(
        executor,
        McpRunMode::AutoRun,
        Arc::new(Mutex::new(McpAllowlist::new())),
        Arc::new(NeverPrompt),
    );
    let outcome = tokio::time::timeout(OP_TIMEOUT, auto_gate.execute(&echo_call("hi from S04 live")))
        .await
        .expect("gate-driven echo call timed out");
    assert!(outcome.ok, "the real echo round-trip through the gate must be ok: {outcome:?}");
    assert_eq!(outcome.failure, None, "a successful gated call carries no failure kind");
    assert!(
        outcome.content.contains("hi from S04 live"),
        "the server's echoed payload must ride back through the guarded call_tool: {:?}",
        outcome.content
    );
    eprintln!("[gate AutoRun echo] OK — payload echoed back through the real child");

    // --- fail-closed: Off blocks the same real tool before the wire ---------
    // A fresh executor over a clone of the SAME live peer, wrapped in an Off gate:
    // the call is refused with the typed blocked kind and never reaches call_tool
    // (D038 — the allowlist cannot un-inert a disabled surface).
    let off_executor = McpExecutor::connect(state.peer().expect("peer still injected"))
        .await
        .expect("second McpExecutor::connect over the live peer must succeed");
    let off_gate = McpApprovalGate::new(
        off_executor,
        McpRunMode::Off,
        Arc::new(Mutex::new(McpAllowlist::new())),
        Arc::new(NeverPrompt),
    );
    let blocked = off_gate.execute(&echo_call("should be blocked")).await;
    assert!(!blocked.ok, "an Off-mode MCP call must be blocked");
    assert_eq!(
        blocked.failure.as_deref(),
        Some(MCP_ACTION_BLOCKED_KIND),
        "Off blocks with the attributable mcp-action-blocked kind, not a wire failure: {blocked:?}"
    );
    assert!(
        !blocked.content.contains("should be blocked"),
        "a blocked call must not echo the payload — the wire was never reached: {:?}",
        blocked.content
    );
    eprintln!("[gate Off echo] OK — blocked before the wire, typed mcp-action-blocked");

    // --- clean shutdown via the portable R020 cancellation path -------------
    // Replay `supervise()` + the RunEvent exit hook: a task owns the RunningService
    // and awaits waiting(); the exit hook takes the stored token and cancels it.
    // A clean app exit MUST classify as QuitReason::Cancelled (no crash mark).
    let supervisor = tokio::spawn(async move { client.waiting().await });
    let token = state.take_shutdown_handle().expect("a spawned child stored a shutdown handle");
    assert!(
        state.take_shutdown_handle().is_none(),
        "the handle is taken exactly once so the child is cancelled once"
    );
    token.cancel();

    let quit = tokio::time::timeout(OP_TIMEOUT, supervisor)
        .await
        .expect("supervisor did not observe the service loop end before timeout")
        .expect("supervisor task panicked")
        .expect("waiting() returned a join error");
    assert!(
        matches!(quit, QuitReason::Cancelled),
        "a token-cancelled shutdown is the clean app-exit path, not a crash: {quit:?}"
    );
    // supervise()'s clean-shutdown branch clears the (now dead) peer.
    state.clear_peer();
    assert!(state.peer().is_none(), "the peer is cleared after a clean shutdown");
    eprintln!("[shutdown] OK — QuitReason::Cancelled, peer cleared (portable R020 path)");
}

/// TEST B — a REAL mid-session crash (the child is killed out from under us)
/// degrades to "tools unavailable" and never panics the app. This is the live
/// counterpart to the value-level `crash_after_ready_degrades_health_and_drops_the_peer`
/// unit test (mcp.rs): here the child actually dies.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires npx + network to fetch and run @modelcontextprotocol/server-everything over stdio"]
async fn mcp_lifecycle_mid_session_crash_degrades_not_panic() {
    let state = McpState::new();

    // --- spawn + handshake + inject (a ready child) -------------------------
    let (transport, pid) = spawn_reference_server();
    let pid = pid.expect("the spawned child must expose a pid to force a real crash");
    let client = tokio::time::timeout(HANDSHAKE_TIMEOUT, ().serve(transport))
        .await
        .expect("handshake timed out")
        .expect("handshake errored");
    let executor = McpExecutor::connect(client.peer().clone())
        .await
        .expect("tools/list must complete");
    let tool_count = executor.definitions().len();
    state.set_peer(client.peer().clone());
    state.mark_ready(tool_count);
    assert_eq!(state.status().phase, McpPhase::Ready);
    eprintln!("[ready] OK — child pid={pid}, {tool_count} tools; about to kill it");

    // --- supervise the RunningService, then SIGKILL the real child ----------
    let supervisor = tokio::spawn(async move { client.waiting().await });
    // A real, forceful crash — the child dies without a clean cancel, so the
    // stdio transport closes underneath the service loop (a genuine mid-session
    // drop, not our cancellation token). `npx` execs a node WRAPPER that spawns
    // the actual server as a grandchild holding the stdout pipe, so killing the
    // root pid alone would orphan the pipe-holder and the transport would never
    // see EOF. Kill the whole descendant tree (leaves first, root last) so every
    // pipe end closes and the service loop observes the drop.
    let killed = Command::new("bash")
        .arg("-c")
        .arg(format!(
            "kill_tree() {{ for c in $(pgrep -P \"$1\"); do kill_tree \"$c\"; done; \
             kill -KILL \"$1\" 2>/dev/null; }}; kill_tree {pid}"
        ))
        .status()
        .await
        .expect("failed to run the kill-tree");
    assert!(killed.success(), "kill-tree for pid {pid} must succeed");

    // The service loop ends with a NON-Cancelled reason (transport closed = child
    // exited). supervise() funnels this through fail(): clear_peer + mark_crashed.
    let quit = tokio::time::timeout(OP_TIMEOUT, supervisor)
        .await
        .expect("supervisor did not observe the child's death before timeout")
        .expect("supervisor task panicked");
    match quit {
        Ok(QuitReason::Cancelled) => {
            panic!("a killed child must NOT classify as a clean cancel — that would hide the crash")
        }
        Ok(other) => eprintln!("[crash] OK — service loop ended non-cleanly: {other:?}"),
        Err(join) => eprintln!("[crash] OK — service task join error (child died): {join}"),
    }

    // supervise()'s crash branch: degrade to "tools unavailable" — never a panic.
    state.clear_peer();
    state.mark_crashed(format!("MCP reference server (pid {pid}) exited mid-session (SIGKILL)"));
    let s = state.status();
    assert_eq!(s.phase, McpPhase::Crashed, "a mid-session drop degrades to crashed");
    assert_eq!(s.tool_count, 0, "crashed tools degrade to unavailable");
    assert!(s.last_error.is_some(), "the crash cause is named on the health line");
    assert!(state.peer().is_none(), "the dead peer is cleared so the next run degrades");
    eprintln!("[degrade] OK — health=crashed, peer cleared, app still running");

    // --- an in-flight call after the crash rides back a typed error ---------
    // The executor still holds a peer handle to the now-dead child; a call maps the
    // rmcp transport error to a typed ToolOutcome (never an Err, never a panic —
    // R006). This is what an already-dispatched call sees during a crash.
    let after = tokio::time::timeout(OP_TIMEOUT, executor.execute(&echo_call("post-crash")))
        .await
        .expect("post-crash call must resolve to a typed outcome, not hang");
    assert!(!after.ok, "a call to a dead child cannot succeed: {after:?}");
    assert_eq!(
        after.failure.as_deref(),
        Some(MCP_TRANSPORT_ERROR_KIND),
        "a dead-child call rides back the typed mcp-transport-error kind: {after:?}"
    );
    eprintln!("[post-crash call] OK — typed mcp-transport-error, no panic");
}
