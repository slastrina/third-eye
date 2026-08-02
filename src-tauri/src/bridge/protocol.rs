//! Bridge protocol v1 (coding-agent S7): the pure half of the loopback
//! bridge — auth checking and app-event → bridge-message mapping, no io.
//!
//! The bridge is a FORWARDER: it taps the app-wide Tauri events the tools
//! already emit (`llm://tool-call`, `llm://tool-result`,
//! `llm://terminal-chunk`, `llm://run-state`) and translates the coding
//! subset into a small, versioned JSON message set for the VS Code
//! extension. Nothing here grants the extension any control over the app —
//! the only inbound message is `auth` (and the app-initiated
//! `debug-request` flows outbound, approved by the user inside VS Code).

/// Protocol version, sent in `hello` — the extension refuses a mismatch
/// rather than mis-parsing.
pub const BRIDGE_PROTOCOL_VERSION: u32 = 1;

/// Whether `raw` is a valid auth message carrying exactly `token`.
/// Constant-shape parse: `{"type":"auth","token":"…"}`; anything else —
/// wrong type tag, missing token, malformed JSON — fails closed.
pub fn auth_ok(raw: &str, token: &str) -> bool {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(raw) else {
        return false;
    };
    value.get("type").and_then(|t| t.as_str()) == Some("auth")
        && value.get("token").and_then(|t| t.as_str()) == Some(token)
}

/// The `hello` message a freshly authenticated client receives.
pub fn hello() -> String {
    serde_json::json!({
        "type": "hello",
        "app": "third-eye",
        "version": BRIDGE_PROTOCOL_VERSION,
    })
    .to_string()
}

/// Map one app-wide Tauri event to its bridge message, if the extension
/// cares about it. `event` is the Tauri event name, `payload` its JSON
/// payload string. Returns `None` for everything the bridge does not
/// forward (unknown events, non-coding tools, malformed payloads — fail
/// quiet, never fail loud on the forwarding path).
pub fn forward(event: &str, payload: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(payload).ok()?;
    match event {
        "llm://tool-call" => {
            let call = value.get("call")?;
            let name = call.get("name")?.as_str()?;
            let call_id = call.get("id")?.as_str()?;
            let args: serde_json::Value = call
                .get("arguments")
                .and_then(|a| a.as_str())
                .and_then(|a| serde_json::from_str(a).ok())
                .unwrap_or(serde_json::Value::Null);
            match name {
                // The extension opens/reveals the file as the agent edits it.
                "write_file" => Some(
                    serde_json::json!({
                        "type": "file-editing",
                        "callId": call_id,
                        "path": args.get("path").and_then(|p| p.as_str()).unwrap_or(""),
                    })
                    .to_string(),
                ),
                "run_in_workspace" => Some(
                    serde_json::json!({
                        "type": "run",
                        "phase": "started",
                        "callId": call_id,
                        "command": args.get("command").and_then(|c| c.as_str()).unwrap_or(""),
                    })
                    .to_string(),
                ),
                _ => None,
            }
        }
        "llm://tool-result" => {
            let name = value.get("name")?.as_str()?;
            let call_id = value.get("callId")?.as_str()?;
            let ok = value.get("ok").and_then(|o| o.as_bool()).unwrap_or(false);
            match name {
                "write_file" => Some(
                    serde_json::json!({
                        "type": "file-edited",
                        "callId": call_id,
                        "ok": ok,
                    })
                    .to_string(),
                ),
                "workspace_diff" if ok => Some(
                    serde_json::json!({
                        "type": "diff",
                        "callId": call_id,
                        "report": value.get("preview").and_then(|p| p.as_str()).unwrap_or(""),
                    })
                    .to_string(),
                ),
                "run_in_workspace" => Some(
                    serde_json::json!({
                        "type": "run",
                        "phase": "done",
                        "callId": call_id,
                        "ok": ok,
                    })
                    .to_string(),
                ),
                _ => None,
            }
        }
        "llm://terminal-chunk" => Some(
            serde_json::json!({
                "type": "run",
                "phase": "output",
                "callId": value.get("callId")?.as_str()?,
                "chunk": value.get("chunk")?.as_str()?,
            })
            .to_string(),
        ),
        "llm://run-state" => Some(
            serde_json::json!({
                "type": "run-state",
                "phase": value.get("phase")?.as_str()?,
            })
            .to_string(),
        ),
        _ => None,
    }
}

/// One inbound client command (protocol v2, spec 2026-08-02 N3): what an
/// AUTHENTICATED local client — the `thirdeye` CLI — may ask the app to do.
/// Additive: v1 clients (VS Code) never send these and are unaffected.
#[derive(Debug, Clone, PartialEq)]
pub enum ClientCommand {
    /// Add an absolute directory to the workspace roots (persisted).
    AddWorkspace { path: String },
    /// Bring the overlay up, optionally pre-filling the input.
    ShowOverlay { prefill: Option<String> },
    /// Submit a chat AS IF typed in the overlay; the sender then receives
    /// the `chat-*` stream frames for the run.
    Ask { text: String },
}

/// Parse one inbound frame as a v2 command. `None` for auth frames (handled
/// before this), unknown types, and malformed JSON — fail quiet.
pub fn parse_command(raw: &str) -> Option<ClientCommand> {
    let value: serde_json::Value = serde_json::from_str(raw).ok()?;
    let kind = value.get("type")?.as_str()?;
    match kind {
        "add-workspace" => Some(ClientCommand::AddWorkspace {
            path: value.get("path")?.as_str()?.to_string(),
        }),
        "show-overlay" => Some(ClientCommand::ShowOverlay {
            prefill: value
                .get("prefill")
                .and_then(|p| p.as_str())
                .map(String::from),
        }),
        "ask" => Some(ClientCommand::Ask {
            text: value.get("text")?.as_str()?.to_string(),
        }),
        _ => None,
    }
}

/// Ack for one handled command: `{"type":"ok"/"err","cmd",…}`.
pub fn command_ack(cmd: &str, result: &Result<String, String>) -> String {
    match result {
        Ok(detail) => serde_json::json!({"type": "ok", "cmd": cmd, "detail": detail}),
        Err(e) => serde_json::json!({"type": "err", "cmd": cmd, "detail": e}),
    }
    .to_string()
}

/// Map one chat-stream app event to its bridge frame for ask-subscribed
/// clients (never sent to clients that did not `ask` — the VS Code
/// extension's read surface stays the coding subset only).
pub fn forward_chat(event: &str, payload: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(payload).ok()?;
    match event {
        "llm://token" => Some(
            serde_json::json!({"type": "chat-token", "token": value.get("token")?.as_str()?})
                .to_string(),
        ),
        "llm://done" => Some(
            serde_json::json!({"type": "chat-done", "text": value.get("text")?.as_str()?})
                .to_string(),
        ),
        "llm://error" => Some(
            serde_json::json!({
                "type": "chat-error",
                "detail": value
                    .get("error")
                    .map(|e| e.to_string())
                    .unwrap_or_default(),
            })
            .to_string(),
        ),
        _ => None,
    }
}

/// The `debug-request` message the `vscode_debug` tool sends: the extension
/// asks the user inside VS Code before starting anything.
pub fn debug_request(config: Option<&str>) -> String {
    serde_json::json!({
        "type": "debug-request",
        "config": config,
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_fails_closed_on_everything_but_the_exact_token() {
        assert!(auth_ok(r#"{"type":"auth","token":"secret"}"#, "secret"));
        assert!(!auth_ok(r#"{"type":"auth","token":"wrong"}"#, "secret"));
        assert!(!auth_ok(r#"{"type":"auth"}"#, "secret"));
        assert!(!auth_ok(r#"{"type":"hello","token":"secret"}"#, "secret"));
        assert!(!auth_ok("not json", "secret"));
        assert!(!auth_ok("", "secret"));
    }

    #[test]
    fn write_file_calls_forward_as_file_editing_with_the_path() {
        let payload = serde_json::json!({
            "requestId": 1, "round": 0,
            "call": {"id": "c1", "name": "write_file",
                     "arguments": "{\"path\":\"src/main.rs\",\"content\":\"x\"}"}
        })
        .to_string();
        let msg = forward("llm://tool-call", &payload).expect("forwarded");
        let value: serde_json::Value = serde_json::from_str(&msg).unwrap();
        assert_eq!(value["type"], "file-editing");
        assert_eq!(value["path"], "src/main.rs");
        assert_eq!(value["callId"], "c1");
    }

    #[test]
    fn diff_results_forward_the_report_and_failures_stay_home() {
        let ok = serde_json::json!({
            "requestId": 1, "round": 0, "callId": "d1", "name": "workspace_diff",
            "ok": true, "resultCount": null, "mode": null, "failure": null,
            "preview": "+new\n-old"
        })
        .to_string();
        let msg = forward("llm://tool-result", &ok).expect("forwarded");
        let value: serde_json::Value = serde_json::from_str(&msg).unwrap();
        assert_eq!(value["type"], "diff");
        assert_eq!(value["report"], "+new\n-old");
        // A FAILED diff never rides to the extension (nothing to render).
        let failed = ok.replace("true", "false");
        assert!(forward("llm://tool-result", &failed).is_none());
    }

    #[test]
    fn run_lifecycle_forwards_started_output_done() {
        let call = serde_json::json!({
            "call": {"id": "r1", "name": "run_in_workspace",
                     "arguments": "{\"command\":\"cargo test\"}"}
        })
        .to_string();
        let started = forward("llm://tool-call", &call).unwrap();
        assert!(started.contains("\"started\"") && started.contains("cargo test"));
        let chunk =
            serde_json::json!({"requestId": 1, "callId": "r1", "chunk": "Compiling"}).to_string();
        let output = forward("llm://terminal-chunk", &chunk).unwrap();
        assert!(output.contains("\"output\"") && output.contains("Compiling"));
        let result = serde_json::json!({
            "callId": "r1", "name": "run_in_workspace", "ok": true
        })
        .to_string();
        let done = forward("llm://tool-result", &result).unwrap();
        assert!(done.contains("\"done\""));
    }

    #[test]
    fn v2_commands_parse_and_acks_carry_the_result() {
        assert_eq!(
            parse_command(r#"{"type":"add-workspace","path":"/tmp/x"}"#),
            Some(ClientCommand::AddWorkspace {
                path: "/tmp/x".into()
            })
        );
        assert_eq!(
            parse_command(r#"{"type":"show-overlay"}"#),
            Some(ClientCommand::ShowOverlay { prefill: None })
        );
        assert_eq!(
            parse_command(r#"{"type":"ask","text":"2+2?"}"#),
            Some(ClientCommand::Ask {
                text: "2+2?".into()
            })
        );
        assert_eq!(parse_command(r#"{"type":"auth","token":"x"}"#), None);
        assert_eq!(parse_command("junk"), None);
        let ok = command_ack("ask", &Ok("submitted".into()));
        assert!(ok.contains("\"ok\"") && ok.contains("submitted"));
        let err = command_ack("add-workspace", &Err("not a directory".into()));
        assert!(err.contains("\"err\"") && err.contains("not a directory"));
    }

    #[test]
    fn chat_stream_maps_token_done_error_only() {
        let token = serde_json::json!({"requestId": 1, "token": "Hel"}).to_string();
        assert!(forward_chat("llm://token", &token)
            .unwrap()
            .contains("chat-token"));
        let done = serde_json::json!({"requestId": 1, "text": "4", "tokenCount": 1}).to_string();
        assert!(forward_chat("llm://done", &done)
            .unwrap()
            .contains("chat-done"));
        let error = serde_json::json!({"requestId": 1, "error": {"kind": "offline"}}).to_string();
        assert!(forward_chat("llm://error", &error)
            .unwrap()
            .contains("chat-error"));
        assert!(forward_chat("llm://tool-call", "{}").is_none());
    }

    #[test]
    fn non_coding_tools_and_unknown_events_never_leak() {
        // R011 posture: the bridge forwards the coding subset ONLY — screen
        // queries, memory searches, chat text never reach the socket.
        let screen = serde_json::json!({
            "call": {"id": "s1", "name": "screen_query", "arguments": "{}"}
        })
        .to_string();
        assert!(forward("llm://tool-call", &screen).is_none());
        let memory = serde_json::json!({
            "callId": "m1", "name": "memory_search", "ok": true
        })
        .to_string();
        assert!(forward("llm://tool-result", &memory).is_none());
        assert!(forward("llm://token", r#"{"token":"secret text"}"#).is_none());
        assert!(forward("llm://tool-call", "not json").is_none());
    }
}
