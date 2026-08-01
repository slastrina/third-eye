//! Loopback WS bridge for the VS Code extension (coding-agent S7).
//!
//! R016 posture, structurally enforced:
//!
//! - binds `127.0.0.1:0` ONLY — never a routable interface; the ephemeral
//!   port plus a per-boot random token land in `bridge.json` (0600) inside
//!   the app-data dir, which is how the extension — and nothing else —
//!   discovers it;
//! - every connection must authenticate with the token as its FIRST
//!   message (5s deadline) or the socket closes; the token never travels
//!   anywhere else;
//! - the bridge only FORWARDS the coding subset of app events
//!   ([`protocol::forward`]) — screen content, memory, chat text never
//!   reach the socket — and accepts no inbound control (auth aside);
//!   the one app→VS Code request (`debug-request`) is approved by the
//!   user inside VS Code.

pub mod debug_tool;
pub mod protocol;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use futures_util::{SinkExt, StreamExt};
use tauri::{AppHandle, Listener, Manager};
use tokio::net::TcpListener;
use tokio::sync::broadcast;

/// Bounded outbound fan-out; a slow client drops messages, never blocks
/// the app (lagged receivers skip ahead).
const BROADCAST_CAPACITY: usize = 256;

/// How long a fresh connection has to authenticate before the socket closes.
const AUTH_DEADLINE_SECS: u64 = 5;

/// The events the bridge taps. Kept in one place so the forwarding surface
/// is auditable at a glance.
const TAPPED_EVENTS: &[&str] = &[
    "llm://tool-call",
    "llm://tool-result",
    "llm://terminal-chunk",
    "llm://run-state",
];

pub struct BridgeState {
    port: Mutex<Option<u16>>,
    token: String,
    outbound: broadcast::Sender<String>,
    connected: AtomicUsize,
}

impl BridgeState {
    pub fn new() -> Self {
        Self {
            port: Mutex::new(None),
            token: random_token(),
            outbound: broadcast::channel(BROADCAST_CAPACITY).0,
            connected: AtomicUsize::new(0),
        }
    }

    pub fn port(&self) -> Option<u16> {
        *self.port.lock().unwrap()
    }

    pub fn connected(&self) -> usize {
        self.connected.load(Ordering::SeqCst)
    }

    /// Send one message to every connected client. Returns whether at least
    /// one client was connected to receive it.
    pub fn send(&self, message: String) -> bool {
        self.outbound.send(message).is_ok()
    }
}

impl Default for BridgeState {
    fn default() -> Self {
        Self::new()
    }
}

/// Per-boot random token: 32 bytes of OS entropy, hex. `/dev/urandom` keeps
/// this dependency-free (macOS/unix — the bridge is desktop-only). Read
/// EXACTLY 32 bytes — `fs::read` would stream the infinite device to OOM.
fn random_token() -> String {
    let bytes = std::fs::File::open("/dev/urandom")
        .ok()
        .and_then(|mut f| {
            use std::io::Read;
            let mut buf = [0u8; 32];
            f.read_exact(&mut buf).ok().map(|()| buf.to_vec())
        })
        .unwrap_or_else(|| {
            // Fallback entropy: hasher over time + pid. Strictly worse, but
            // the bridge still only ever listens on loopback.
            let mut bytes = Vec::with_capacity(32);
            let mut seed = std::process::id() as u64
                ^ std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.subsec_nanos() as u64 ^ d.as_secs())
                    .unwrap_or(0);
            for _ in 0..32 {
                seed = seed
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                bytes.push((seed >> 32) as u8);
            }
            bytes
        });
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Where the discovery file lives: `<app-data>/bridge.json`.
pub fn discovery_path(app: &AppHandle) -> Option<std::path::PathBuf> {
    app.path()
        .app_data_dir()
        .ok()
        .map(|d| d.join("bridge.json"))
}

/// Start the bridge: bind loopback, write the discovery file, tap the app
/// events, accept clients. Failure is logged and leaves the app fully
/// functional without a bridge (health-as-value; Settings shows "off").
pub fn start_bridge(app: &AppHandle) {
    let state = app.state::<Arc<BridgeState>>().inner().clone();
    // Tap the app-wide events once, forwarding the coding subset. The
    // listener callbacks are sync; broadcast::send is sync — no runtime hop.
    for event in TAPPED_EVENTS {
        let tap = state.clone();
        app.listen(*event, move |e| {
            if let Some(message) = protocol::forward(event, e.payload()) {
                let _ = tap.outbound.send(message);
            }
        });
    }
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let listener = match TcpListener::bind("127.0.0.1:0").await {
            Ok(listener) => listener,
            Err(e) => {
                log::error!("bridge: loopback bind failed: {e}");
                return;
            }
        };
        let port = match listener.local_addr() {
            Ok(addr) => addr.port(),
            Err(e) => {
                log::error!("bridge: local_addr failed: {e}");
                return;
            }
        };
        *state.port.lock().unwrap() = Some(port);
        if let Err(e) = write_discovery(&app, port, &state.token) {
            log::error!("bridge: discovery file write failed: {e}");
            // Keep serving: a manually-configured client can still connect.
        }
        log::info!("bridge: listening on 127.0.0.1:{port}");
        loop {
            let Ok((socket, peer)) = listener.accept().await else {
                log::warn!("bridge: accept failed; stopping");
                return;
            };
            let state = state.clone();
            tauri::async_runtime::spawn(async move {
                if let Err(e) = serve_client(socket, &state).await {
                    log::debug!("bridge: client {peer} ended: {e}");
                }
            });
        }
    });
}

/// Write `bridge.json` (port + token) with owner-only permissions.
fn write_discovery(app: &AppHandle, port: u16, token: &str) -> Result<(), String> {
    let path = discovery_path(app).ok_or("no app-data dir")?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let body = serde_json::json!({
        "port": port,
        "token": token,
        "version": protocol::BRIDGE_PROTOCOL_VERSION,
    })
    .to_string();
    std::fs::write(&path, body).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    log::info!("bridge: discovery file at {}", path.display());
    Ok(())
}

/// One client: WS handshake → auth-first (deadline) → hello → forward the
/// broadcast until either side closes. Inbound frames after auth are
/// ignored (v1 has no client→app control surface).
async fn serve_client(
    socket: tokio::net::TcpStream,
    state: &Arc<BridgeState>,
) -> Result<(), String> {
    let ws = tokio_tungstenite::accept_async(socket)
        .await
        .map_err(|e| format!("handshake: {e}"))?;
    let (mut sink, mut stream) = ws.split();
    // Auth must be the FIRST message, within the deadline.
    let first = tokio::time::timeout(
        std::time::Duration::from_secs(AUTH_DEADLINE_SECS),
        stream.next(),
    )
    .await
    .map_err(|_| "auth timeout".to_string())?
    .ok_or("closed before auth")?
    .map_err(|e| format!("auth read: {e}"))?;
    let authed = matches!(&first, tokio_tungstenite::tungstenite::Message::Text(text)
        if protocol::auth_ok(text, &state.token));
    if !authed {
        let _ = sink
            .send(tokio_tungstenite::tungstenite::Message::Close(None))
            .await;
        return Err("bad auth".into());
    }
    sink.send(tokio_tungstenite::tungstenite::Message::Text(
        protocol::hello(),
    ))
    .await
    .map_err(|e| format!("hello: {e}"))?;
    state.connected.fetch_add(1, Ordering::SeqCst);
    log::info!("bridge: client connected ({} total)", state.connected());
    let mut rx = state.outbound.subscribe();
    let result = loop {
        tokio::select! {
            outbound = rx.recv() => match outbound {
                Ok(message) => {
                    if let Err(e) = sink
                        .send(tokio_tungstenite::tungstenite::Message::Text(message))
                        .await
                    {
                        break Err(format!("send: {e}"));
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    log::warn!("bridge: client lagged, {n} messages dropped");
                }
                Err(broadcast::error::RecvError::Closed) => break Ok(()),
            },
            inbound = stream.next() => match inbound {
                None => break Ok(()),
                Some(Err(e)) => break Err(format!("recv: {e}")),
                Some(Ok(tokio_tungstenite::tungstenite::Message::Close(_))) => break Ok(()),
                // v1: no client→app control; other frames are ignored.
                Some(Ok(_)) => {}
            },
        }
    };
    state.connected.fetch_sub(1, Ordering::SeqCst);
    log::info!("bridge: client disconnected ({} left)", state.connected());
    result
}

/// Settings snapshot (health-as-value): whether the bridge listens, where,
/// how many clients, and whether VS Code looks installed on this machine.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BridgeStatus {
    pub running: bool,
    pub port: Option<u16>,
    pub connected: usize,
    pub discovery_path: Option<String>,
    pub vscode_detected: bool,
}

/// Best-effort VS Code detection: the app bundle or the `code` CLI.
pub fn vscode_detected() -> bool {
    std::path::Path::new("/Applications/Visual Studio Code.app").exists()
        || std::process::Command::new("/usr/bin/which")
            .arg("code")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
}

#[tauri::command]
pub fn bridge_status(app: AppHandle, state: tauri::State<'_, Arc<BridgeState>>) -> BridgeStatus {
    BridgeStatus {
        running: state.port().is_some(),
        port: state.port(),
        connected: state.connected(),
        discovery_path: discovery_path(&app).map(|p| p.display().to_string()),
        vscode_detected: vscode_detected(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_long_random_hex_and_per_instance() {
        let a = BridgeState::new();
        let b = BridgeState::new();
        assert_eq!(a.token.len(), 64);
        assert!(a.token.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a.token, b.token, "two boots must never share a token");
    }

    #[test]
    fn send_reports_whether_anyone_listens() {
        let state = BridgeState::new();
        assert!(!state.send("x".into()), "no clients yet");
        let _rx = state.outbound.subscribe();
        assert!(state.send("x".into()));
    }

    #[tokio::test]
    async fn end_to_end_auth_gate_and_forwarding() {
        use futures_util::{SinkExt, StreamExt};
        // A real loopback server with a scripted state (no Tauri runtime):
        // bad token → closed before hello; good token → hello then a
        // broadcast message arrives.
        let state = Arc::new(BridgeState::new());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let serve_state = state.clone();
        tokio::spawn(async move {
            loop {
                let Ok((socket, _)) = listener.accept().await else {
                    return;
                };
                let state = serve_state.clone();
                tokio::spawn(async move {
                    let _ = serve_client(socket, &state).await;
                });
            }
        });

        // Wrong token: the server closes without a hello.
        let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}"))
            .await
            .unwrap();
        ws.send(tokio_tungstenite::tungstenite::Message::Text(
            r#"{"type":"auth","token":"wrong"}"#.to_string(),
        ))
        .await
        .unwrap();
        let reply = ws.next().await;
        assert!(
            !matches!(
                &reply,
                Some(Ok(tokio_tungstenite::tungstenite::Message::Text(t))) if t.contains("hello")
            ),
            "bad auth must never be greeted: {reply:?}"
        );

        // Right token: hello, then a forwarded message.
        let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}"))
            .await
            .unwrap();
        ws.send(tokio_tungstenite::tungstenite::Message::Text(format!(
            r#"{{"type":"auth","token":"{}"}}"#,
            state.token
        )))
        .await
        .unwrap();
        let hello = ws.next().await.unwrap().unwrap();
        assert!(hello.to_string().contains("hello"), "{hello:?}");
        // Wait for the subscriber count to include this client, then send.
        for _ in 0..50 {
            if state.connected() == 1 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(state.send(r#"{"type":"diff","report":"+x"}"#.into()));
        let forwarded = ws.next().await.unwrap().unwrap().to_string();
        assert!(forwarded.contains("diff"), "{forwarded}");
    }
}
