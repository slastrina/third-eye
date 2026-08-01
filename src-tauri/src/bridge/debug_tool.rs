//! `vscode_debug` (coding-agent S7): the agent asks VS Code to start a
//! debug session. The bridge only DELIVERS the request — the user approves
//! or dismisses it inside VS Code (the extension shows the prompt), so no
//! HID-style approval plumbing is needed app-side. Structurally inert
//! without workspace roots (the coding-tool posture) and typed-refusing
//! when no extension is connected — the model must tell the user to open
//! VS Code instead of pretending a session started.

use std::sync::Arc;

use async_trait::async_trait;

use super::{protocol, BridgeState};
use crate::llm::toolloop::{ToolExecutor, ToolOutcome};
use crate::llm::{ToolCall, ToolDefinition};
use crate::workspace::WorkspaceState;

pub const VSCODE_DEBUG_TOOL: &str = "vscode_debug";

pub struct VsCodeDebugTool {
    workspace: Arc<WorkspaceState>,
    bridge: Arc<BridgeState>,
}

impl VsCodeDebugTool {
    pub fn new(workspace: Arc<WorkspaceState>, bridge: Arc<BridgeState>) -> Self {
        Self { workspace, bridge }
    }

    pub fn definition() -> ToolDefinition {
        ToolDefinition {
            name: VSCODE_DEBUG_TOOL.into(),
            description: "Ask VS Code (when its Third Eye extension is connected) to start a \
                          debug session for the workspace. The USER approves the request inside \
                          VS Code — this tool only delivers it. Optionally name a launch \
                          configuration; omit to let VS Code pick the default. Never claim a \
                          debug session is running — only that the request was delivered."
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "config": {
                        "type": "string",
                        "description": "Launch-configuration name from .vscode/launch.json (optional)."
                    }
                },
                "required": []
            }),
        }
    }
}

#[async_trait]
impl ToolExecutor for VsCodeDebugTool {
    fn definitions(&self) -> Vec<ToolDefinition> {
        if self.workspace.has_roots() {
            vec![Self::definition()]
        } else {
            Vec::new()
        }
    }

    fn claims(&self, name: &str) -> bool {
        name == VSCODE_DEBUG_TOOL
    }

    async fn execute(&self, call: &ToolCall) -> ToolOutcome {
        if !self.workspace.has_roots() {
            return ToolOutcome::failure(
                "no-workspaces",
                "no workspace folders are configured — the user adds them in Settings → Workspaces",
            );
        }
        let config = serde_json::from_str::<serde_json::Value>(&call.arguments)
            .ok()
            .and_then(|v| v.get("config").and_then(|c| c.as_str().map(String::from)))
            .filter(|c| !c.trim().is_empty());
        // Delivery truth comes from the send itself: a broadcast with zero
        // receivers fails, which IS the "no extension connected" state.
        let delivered = self.bridge.send(protocol::debug_request(config.as_deref()));
        if delivered {
            ToolOutcome::success(format!(
                "debug request delivered to VS Code ({}) — the user approves it there; do not \
                 assume the session started",
                config.as_deref().unwrap_or("default configuration")
            ))
        } else {
            ToolOutcome::failure(
                "no-vscode",
                "VS Code is not connected — the user needs VS Code open with the Third Eye \
                 extension installed for debug control",
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(args: serde_json::Value) -> ToolCall {
        ToolCall {
            id: "c1".into(),
            name: VSCODE_DEBUG_TOOL.into(),
            arguments: args.to_string(),
        }
    }

    #[tokio::test]
    async fn inert_without_roots_and_typed_without_vscode() {
        let ws = Arc::new(WorkspaceState::new());
        let bridge = Arc::new(BridgeState::new());
        let tool = VsCodeDebugTool::new(ws.clone(), bridge.clone());
        assert!(tool.definitions().is_empty());
        let outcome = tool.execute(&call(serde_json::json!({}))).await;
        assert_eq!(outcome.failure.as_deref(), Some("no-workspaces"));
        // With a root but no connected extension: typed no-vscode.
        let dir = std::env::temp_dir();
        ws.set_roots(vec![dir.display().to_string()]);
        assert_eq!(tool.definitions().len(), 1);
        let outcome = tool.execute(&call(serde_json::json!({}))).await;
        assert_eq!(outcome.failure.as_deref(), Some("no-vscode"));
    }

    #[tokio::test]
    async fn delivers_the_request_when_a_client_listens() {
        let ws = Arc::new(WorkspaceState::new());
        ws.set_roots(vec![std::env::temp_dir().display().to_string()]);
        let bridge = Arc::new(BridgeState::new());
        let mut rx = bridge.outbound.subscribe();
        let tool = VsCodeDebugTool::new(ws, bridge);
        let outcome = tool
            .execute(&call(serde_json::json!({"config": "Debug CLI"})))
            .await;
        assert!(outcome.ok, "{outcome:?}");
        assert!(outcome.content.contains("do not assume"));
        let message = rx.try_recv().unwrap();
        assert!(message.contains("debug-request") && message.contains("Debug CLI"));
    }
}
