//! External MCP server spawn/handshake/inject lifecycle (M007 S04 T03).
//!
//! This is the code that turns the S03 seams into a running feature: at startup
//! a launch task reads the persisted enabled server list, spawns the child over
//! stdio, performs the initialize handshake (bounded so a wedged child fails as a
//! named timeout, never a silent hang — R006), fetches its tool catalogue, and
//! injects the `Peer` into [`McpState`] so the already-mounted gate's
//! `Some(peer)` branch at `commands.rs` lights up and the agent tool-loop SEES
//! the server's tools. Every lifecycle transition is surfaced on the health value
//! ([`McpState::mark_spawning`]/`mark_ready`/`mark_crashed`) and broadcast on
//! `mcp://state`.
//!
//! Two failure contracts (R006/R007):
//! - **Spawn / handshake failure** → health `crashed` with the cause named, the
//!   peer left absent, the app keeps running (never a panic). The next chat run
//!   simply has no MCP tools.
//! - **Mid-session child drop** → a supervisor task awaiting the `RunningService`
//!   detects the service loop ending, clears the (now dead) peer, and marks
//!   `crashed`. A subsequent run degrades to "tools unavailable"; an in-flight
//!   call already rode back a typed `mcp-transport-error` (mcp.rs), never `Err`.
//!
//! Clean shutdown is the portable rmcp path (R020 — no unix/windows-only kill):
//! the `RunningService`'s cancellation token is stored in [`McpState`] and the
//! RunEvent exit hook in `lib.rs` cancels it, stopping the service loop and
//! terminating the child.
//!
//! S04 scope: the roadmap says "a server" (singular) and [`McpState::peer`] is a
//! single `Option<Peer>`, so this launches exactly the FIRST enabled server; a
//! keyed multi-server registry is a deliberate S05 forward seam (RESEARCH C).
//!
//! This module never invokes the rmcp `call_tool` wire method — it only
//! spawns/handshakes/injects a `Peer`; the single guarded tool-call choke point
//! stays in `mcp.rs` (`scripts/check-mcp-guard.sh`).

use std::time::Duration;

use rmcp::service::{QuitReason, RunningService};
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::{
    ConfigureCommandExt, IntoTransport, StreamableHttpClientTransport, TokioChildProcess,
};
use rmcp::{RoleClient, ServiceExt};
use tauri::{AppHandle, Emitter, Manager};
use tokio::process::Command;

use super::commands::McpAuthState;
use super::mcp::{McpServerConfig, McpState, McpTransport};

/// First-run `npx -y` may download the server from npm, so the handshake gets a
/// generous ceiling (mirrors the S01 live test); a wedged child fails as this
/// named timeout → `crashed` health, never an indefinite hang (R006).
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(120);

/// Tighter ceiling for steady-state operations (the post-handshake `tools/list`)
/// so a hung catalogue fetch also fails as a named timeout rather than wedging
/// the launch task forever.
const OP_TIMEOUT: Duration = Duration::from_secs(30);

/// Spawn the startup MCP launch task (called from `setup()`). Sync wrapper over
/// the async [`launch`] so it joins the `async_runtime::spawn` family
/// (`watcher::spawn_loop`/`memory::ingest::spawn`/`nudge::spawn`) rather than
/// blocking the sync `setup()` on the up-to-2-minute handshake.
pub fn launch_on_startup(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        launch(app).await;
    });
}

/// Read the persisted enabled server list, connect the first enabled server +
/// handshake, inject its peer, and start the crash supervisor. Only the peer
/// *acquisition* differs by transport — a [`Stdio`](McpTransport::Stdio) server
/// spawns a child ([`connect_stdio`]) and an [`Http`](McpTransport::Http) server
/// dials a remote streamable-HTTP / SSE endpoint ([`connect_http`]); both
/// converge on the SAME bounded serve → [`finish_launch`] tail (list_all_tools →
/// inject → supervise). Every exit path leaves the app running: no
/// configured/enabled server is a logged no-op; any connect/handshake/catalogue
/// failure degrades to `crashed` health with the cause named (never a panic).
async fn launch(app: AppHandle) {
    let servers = crate::config::load_mcp_servers(&app).unwrap_or_default();
    let Some(cfg) = select_enabled_server(&servers) else {
        log::info!(
            "llm: no enabled MCP server configured — the agent runs with built-in tools only"
        );
        return;
    };
    let cfg = cfg.clone();
    match cfg.transport {
        McpTransport::Stdio => log::info!(
            "llm: launching stdio MCP server '{}' (command={} args={:?})",
            cfg.id,
            cfg.command,
            cfg.args
        ),
        McpTransport::Http => log::info!(
            "llm: connecting http MCP server '{}' (url={})",
            cfg.id,
            cfg.url.as_deref().unwrap_or("<none>")
        ),
    }

    {
        let state = app.state::<McpState>();
        state.mark_spawning();
    }
    broadcast(&app);

    // --- acquire the peer (transport-specific), then the shared tail -------
    // The ONLY per-transport branch: stdio spawns a child, http dials a remote
    // endpoint. Both return a handshaked `RunningService<RoleClient, ()>`; a
    // `None` means the connect already funnelled through `fail()` → crashed.
    let client = match cfg.transport {
        McpTransport::Stdio => connect_stdio(&app, &cfg).await,
        McpTransport::Http => connect_http(&app, &cfg).await,
    };
    let Some(client) = client else { return };

    finish_launch(app, cfg, client).await;
}

/// Build the stdio [`TokioChildProcess`] transport and run the bounded handshake.
/// A spawn failure (bad command) funnels through [`fail`] → `crashed` and yields
/// `None` (the S04 path, unchanged in behaviour).
async fn connect_stdio(
    app: &AppHandle,
    cfg: &McpServerConfig,
) -> Option<RunningService<RoleClient, ()>> {
    let transport = match TokioChildProcess::new(Command::new(&cfg.command).configure(|c| {
        for a in &cfg.args {
            c.arg(a);
        }
    })) {
        Ok(t) => t,
        Err(e) => {
            fail(
                app,
                format!(
                    "failed to spawn MCP server '{}' (command `{}`): {e}",
                    cfg.id, cfg.command
                ),
            );
            return None;
        }
    };
    serve_bounded(app, cfg, transport).await
}

/// Build the remote [`StreamableHttpClientTransport`] and run the bounded
/// handshake (S05). rmcp owns SSE parsing, JSON-RPC framing, the session id, and
/// bounded reconnect — we only supply the url and (optionally) the bearer token
/// read from the OS keychain for the `Authorization` header. A missing/blank url,
/// a keychain read error, or a handshake-time auth 401 all funnel through
/// [`fail`] → `crashed` with the cause named (never a panic, never the token in a
/// log line).
async fn connect_http(
    app: &AppHandle,
    cfg: &McpServerConfig,
) -> Option<RunningService<RoleClient, ()>> {
    // Resolve the optional bearer token from the OS keychain (the one
    // side-effecting step). The non-secret `auth_ref` names the account; the
    // secret bytes are read crate-internally and NEVER logged.
    let token = if let Some(auth_ref) = cfg
        .auth_ref
        .as_deref()
        .map(str::trim)
        .filter(|r| !r.is_empty())
    {
        match app.state::<McpAuthState>().store().get_token(auth_ref) {
            Ok(Some(token)) => {
                log::info!(
                    "llm: MCP server '{}' using keychain bearer token from account '{}'",
                    cfg.id,
                    auth_ref
                );
                Some(token)
            }
            Ok(None) => {
                // The user named an auth account but stored no token yet. Connect
                // unauthenticated — a server that requires auth fails as a named
                // 401 in the handshake below, which is the visible reason.
                log::warn!(
                    "llm: MCP server '{}' names auth account '{}' but no token is stored — connecting unauthenticated",
                    cfg.id,
                    auth_ref
                );
                None
            }
            Err(e) => {
                fail(
                    app,
                    format!(
                        "MCP server '{}' auth token read failed for account '{}' ({})",
                        cfg.id,
                        auth_ref,
                        e.kind()
                    ),
                );
                return None;
            }
        }
    } else {
        None
    };

    let config = match build_http_config(cfg.url.as_deref(), token.as_deref()) {
        Ok(config) => config,
        Err(reason) => {
            // An http server with no url is a corrupt entry config-repair should
            // have dropped; guard here too so a hand-edited settings.json cannot
            // panic — it degrades to `crashed` with the reason named.
            fail(app, format!("MCP server '{}' {reason}", cfg.id));
            return None;
        }
    };

    let transport = StreamableHttpClientTransport::from_config(config);
    serve_bounded(app, cfg, transport).await
}

/// Assemble the streamable-http transport config from the non-secret url and the
/// already-resolved bearer token. Pure / side-effect-free (the keychain read
/// lives in [`connect_http`]) so the url-validation and RAW-token-attachment
/// invariants are unit-testable without an `AppHandle` or a network. A
/// blank/missing url is an `Err` (the corrupt-http-entry guard); the token, when
/// present, is attached RAW via `auth_header` — reqwest applies `bearer_auth`
/// (`Authorization: Bearer <token>`), so we must NOT prepend "Bearer " ourselves.
///
/// Exposed `pub` for the S05 T06 contract test (`tests/mcp_http_live.rs`), which
/// drives this production builder into a REAL `StreamableHttpClientTransport`
/// against a fake HTTP server and asserts the resulting request actually carries
/// `Authorization: Bearer <token>` on the wire — proving the auth-attachment
/// contract end-to-end, not just the config field.
pub fn build_http_config(
    url: Option<&str>,
    token: Option<&str>,
) -> Result<StreamableHttpClientTransportConfig, String> {
    let url = url
        .map(str::trim)
        .filter(|u| !u.is_empty())
        .ok_or_else(|| "is http but has no url configured".to_string())?;
    let mut config = StreamableHttpClientTransportConfig::with_uri(url.to_string());
    if let Some(token) = token {
        config = config.auth_header(token.to_string());
    }
    Ok(config)
}

/// Run the initialize handshake bounded by [`HANDSHAKE_TIMEOUT`] over any rmcp
/// transport (stdio child or remote http), so a wedged/unreachable server fails
/// as a NAMED timeout → `crashed` health rather than an indefinite hang (R006).
/// Generic over the transport so the stdio and http paths share one bounded-serve
/// site; a handshake error (including an http auth 401) funnels through [`fail`].
async fn serve_bounded<T, E, A>(
    app: &AppHandle,
    cfg: &McpServerConfig,
    transport: T,
) -> Option<RunningService<RoleClient, ()>>
where
    T: IntoTransport<RoleClient, E, A>,
    E: std::error::Error + Send + Sync + 'static,
{
    match tokio::time::timeout(HANDSHAKE_TIMEOUT, ().serve(transport)).await {
        Err(_) => {
            fail(
                app,
                format!(
                    "MCP server '{}' handshake did not complete within {:?} (unreachable endpoint or wedged server?)",
                    cfg.id, HANDSHAKE_TIMEOUT
                ),
            );
            None
        }
        Ok(Err(e)) => {
            fail(
                app,
                format!("MCP server '{}' handshake returned an error: {e}", cfg.id),
            );
            None
        }
        Ok(Ok(client)) => Some(client),
    }
}

/// The shared post-handshake tail both transports converge on: a bounded
/// `tools/list` (the health `tool_count` + a liveness proof), inject the peer +
/// retain the cancellation token, mark `ready`, and spawn the crash supervisor.
/// Reused verbatim from the S04 stdio path — the http peer is a `Peer<RoleClient>`
/// indistinguishable from a stdio one here.
async fn finish_launch(
    app: AppHandle,
    cfg: McpServerConfig,
    client: RunningService<RoleClient, ()>,
) {
    // --- tools/list (bounded) — the health tool_count and a liveness proof --
    let tools = match tokio::time::timeout(OP_TIMEOUT, client.list_all_tools()).await {
        Ok(Ok(tools)) => tools,
        Ok(Err(e)) => {
            fail(
                &app,
                format!("MCP server '{}' tools/list returned an error: {e}", cfg.id),
            );
            // Cancel the just-connected server so a handshaked-but-unusable server
            // does not leak (its peer is never injected).
            let _ = client.cancel().await;
            return;
        }
        Err(_) => {
            fail(
                &app,
                format!(
                    "MCP server '{}' tools/list did not complete within {:?}",
                    cfg.id, OP_TIMEOUT
                ),
            );
            let _ = client.cancel().await;
            return;
        }
    };
    let tool_count = tools.len();

    // --- inject the peer + retain the shutdown handle ----------------------
    // `peer()` is the clonable handle the executor mounts from; the cancellation
    // token is the ONLY thing that can shut the server down (RESEARCH constraint 1).
    let peer = client.peer().clone();
    let shutdown = client.cancellation_token();
    {
        let state = app.state::<McpState>();
        state.set_peer(peer);
        state.set_shutdown_handle(shutdown);
        state.mark_ready(tool_count);
    }
    log::info!(
        "llm: MCP server '{}' ready — {} tool(s) injected, gate now sees external tools",
        cfg.id,
        tool_count
    );
    broadcast(&app);

    // --- supervise the server for a mid-session drop -----------------------
    let sup_app = app.clone();
    let server_id = cfg.id.clone();
    tauri::async_runtime::spawn(async move {
        supervise(sup_app, server_id, client).await;
    });
}

/// Await the `RunningService` for the app's lifetime; when the service loop ends
/// classify why. A `Cancelled` quit is the clean app-exit shutdown (the RunEvent
/// hook cancelled the token) — no crash. Any other end (transport closed = child
/// exited, or a join error) is a mid-session drop: clear the dead peer and mark
/// `crashed` so a subsequent run degrades to "tools unavailable" (R006/R007),
/// never a panic.
async fn supervise(app: AppHandle, server_id: String, client: RunningService<RoleClient, ()>) {
    match client.waiting().await {
        Ok(QuitReason::Cancelled) => {
            log::info!(
                "llm: MCP server '{server_id}' service loop ended (cancelled — clean shutdown)"
            );
            app.state::<McpState>().clear_peer();
        }
        Ok(reason) => {
            fail(
                &app,
                format!(
                    "MCP server '{server_id}' exited mid-session ({reason:?}) — tools unavailable"
                ),
            );
        }
        Err(join) => {
            fail(
                &app,
                format!(
                    "MCP server '{server_id}' service task join error: {join} — tools unavailable"
                ),
            );
        }
    }
}

/// Select the single server to launch for S04: the first `enabled` entry. When
/// more than one is enabled, log that only the first is launched (the single
/// active-peer cap — a keyed multi-server registry is the S05 seam). Pure /
/// side-effect-free apart from the multi-server notice, so it is unit-testable.
fn select_enabled_server(servers: &[McpServerConfig]) -> Option<&McpServerConfig> {
    let enabled: Vec<&McpServerConfig> = servers.iter().filter(|s| s.enabled).collect();
    match enabled.as_slice() {
        [] => None,
        [only] => Some(only),
        [first, rest @ ..] => {
            log::warn!(
                "llm: {} enabled MCP servers configured; S04 launches only the first ('{}'). \
                 Others ({}) are not spawned — multi-server support is a later slice.",
                enabled.len(),
                first.id,
                rest.iter()
                    .map(|s| s.id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            Some(first)
        }
    }
}

/// Record a lifecycle failure (spawn/handshake/catalogue/mid-session): log it,
/// clear any injected peer so runs degrade to "tools unavailable", mark the
/// health value `crashed` with the cause, and broadcast. The app keeps running —
/// this is the never-a-panic path (R006/R007).
fn fail(app: &AppHandle, message: String) {
    log::warn!("llm: {message}");
    let state = app.state::<McpState>();
    state.clear_peer();
    state.mark_crashed(message);
    broadcast(app);
}

/// Broadcast the current MCP health value on `mcp://state` (the same event the
/// run-mode applier fires). A broadcast failure is cosmetic — the truth stays
/// queryable via `mcp_status` — so it is logged, never bubbled.
fn broadcast(app: &AppHandle) {
    let status = app.state::<McpState>().status();
    if let Err(e) = app.emit(crate::llm::commands::MCP_STATE_EVENT, status) {
        log::warn!("llm: MCP state broadcast failed: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::super::mcp::McpTransport;
    use super::*;

    fn cfg(id: &str, enabled: bool) -> McpServerConfig {
        McpServerConfig {
            id: id.to_string(),
            command: "npx".to_string(),
            args: vec![
                "-y".to_string(),
                "@modelcontextprotocol/server-everything".to_string(),
            ],
            enabled,
            transport: McpTransport::Stdio,
            url: None,
            auth_ref: None,
        }
    }

    #[test]
    fn select_returns_none_when_no_server_is_enabled() {
        // Absent list and all-disabled both mean "no external server" — the
        // fail-closed no-op launch path (nothing spawned).
        assert!(select_enabled_server(&[]).is_none());
        let disabled = vec![cfg("a", false), cfg("b", false)];
        assert!(select_enabled_server(&disabled).is_none());
    }

    #[test]
    fn select_picks_the_first_enabled_entry_skipping_disabled() {
        // A disabled entry ahead of an enabled one is skipped — enabled is the
        // gate on whether the startup task spawns a child.
        let servers = vec![
            cfg("disabled", false),
            cfg("wanted", true),
            cfg("later", true),
        ];
        let picked = select_enabled_server(&servers).expect("an enabled server exists");
        assert_eq!(picked.id, "wanted", "the first ENABLED server is chosen");
    }

    #[test]
    fn select_with_multiple_enabled_returns_the_first_enabled() {
        // The single active-peer cap: with several enabled the first wins
        // (others are logged as not-spawned, the S05 multi-server seam).
        let servers = vec![cfg("one", true), cfg("two", true)];
        let picked = select_enabled_server(&servers).expect("an enabled server exists");
        assert_eq!(picked.id, "one");
    }

    #[test]
    fn http_config_rejects_a_missing_or_blank_url() {
        // A corrupt http entry (no/blank url) must NOT build a transport — it is
        // the reason string that funnels through fail() → crashed, never a panic.
        assert!(build_http_config(None, None).is_err());
        for blank in ["", "   ", "\n\t"] {
            let err = build_http_config(Some(blank), None).expect_err("blank url is corrupt");
            assert!(
                err.contains("url"),
                "the reason names the missing url: {err}"
            );
        }
    }

    #[test]
    fn http_config_attaches_the_raw_bearer_token_without_a_prefix() {
        // The auth-header invariant: the RAW token is handed to rmcp (reqwest
        // applies `bearer_auth` → "Authorization: Bearer <token>"), so we must
        // never see a "Bearer " prefix baked in here.
        let config =
            build_http_config(Some("https://mcp.example.com/mcp"), Some("tok-XYZ")).unwrap();
        assert_eq!(config.auth_header.as_deref(), Some("tok-XYZ"));
        assert_eq!(&*config.uri, "https://mcp.example.com/mcp");
    }

    #[test]
    fn http_config_without_a_token_leaves_auth_header_absent() {
        // An unauthenticated http server (no auth_ref / no stored token) attaches
        // no Authorization header at all.
        let config = build_http_config(Some("https://open.example.com/mcp"), None).unwrap();
        assert!(config.auth_header.is_none());
    }

    #[test]
    fn http_config_trims_surrounding_whitespace_from_the_url() {
        // A url with stray whitespace (hand-edited settings.json) is trimmed, not
        // rejected — the trimmed form is what reaches rmcp.
        let config = build_http_config(Some("  https://mcp.example.com/mcp  "), None).unwrap();
        assert_eq!(&*config.uri, "https://mcp.example.com/mcp");
    }
}
