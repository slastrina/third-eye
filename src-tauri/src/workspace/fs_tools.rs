//! Workspace file tools (coding-agent S3): `read_file` / `list_dir` /
//! `write_file`, the coder's hands on disk.
//!
//! Structural inertness (D038): with ZERO workspace roots configured the
//! tools contribute no definitions — the model is never offered them —
//! and a stray call is refused typed (`no-workspaces`). Every path passes
//! the S2 containment choke point before any io. Reads are text-only and
//! capped (the result enters model context); writes are approval-gated on
//! the shared prompt/whitelist plumbing (`ActionKind::WriteFile` — Allow
//! once / session / Always all work) and text-only with a hard size cap.
//! Code contents never enter the memory store (R011) — they stay on disk.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use super::{WorkspaceError, WorkspaceState};
use crate::input::commands::{HidRunMode, SessionWhitelist};
use crate::input::ActionKind;
use crate::llm::toolloop::{ApprovalPrompt, ApprovalVerdict, ToolExecutor, ToolOutcome};
use crate::llm::{ToolCall, ToolDefinition};

pub const READ_FILE_TOOL: &str = "read_file";
pub const LIST_DIR_TOOL: &str = "list_dir";
pub const WRITE_FILE_TOOL: &str = "write_file";

/// Read cap: what one read may put into model context.
const READ_MAX_CHARS: usize = 24_000;
/// Directory listing cap.
const LIST_MAX_ENTRIES: usize = 200;
/// Write cap: memories are one-liners, files are files — but 1 MB of
/// generated text in one call is a runaway, not a program.
pub const WRITE_MAX_BYTES: usize = 1_000_000;

/// The three tools share state; one struct per tool keeps the composite's
/// one-name-per-executor dispatch simple.
pub struct ReadFileTool {
    workspace: Arc<WorkspaceState>,
}

pub struct ListDirTool {
    workspace: Arc<WorkspaceState>,
}

pub struct WriteFileTool {
    workspace: Arc<WorkspaceState>,
    mode: HidRunMode,
    whitelist: Arc<Mutex<SessionWhitelist>>,
    approver: Arc<dyn ApprovalPrompt>,
}

impl ReadFileTool {
    pub fn new(workspace: Arc<WorkspaceState>) -> Self {
        Self { workspace }
    }

    pub fn definition() -> ToolDefinition {
        ToolDefinition {
            name: READ_FILE_TOOL.into(),
            description: "Read a text file inside one of the user's designated workspace \
                          folders. Use it to understand code before changing it. Long files \
                          are truncated (marked); binary files are refused."
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "File path — absolute inside a workspace, or relative to the first workspace."
                    }
                },
                "required": ["path"]
            }),
        }
    }
}

impl ListDirTool {
    pub fn new(workspace: Arc<WorkspaceState>) -> Self {
        Self { workspace }
    }

    pub fn definition() -> ToolDefinition {
        ToolDefinition {
            name: LIST_DIR_TOOL.into(),
            description: "List a directory inside the user's workspace folders (name, kind, \
                          size). Call with no path to list the first workspace root."
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Directory path (optional; defaults to the first workspace root)."
                    }
                },
                "required": []
            }),
        }
    }
}

impl WriteFileTool {
    pub fn new(
        workspace: Arc<WorkspaceState>,
        mode: HidRunMode,
        whitelist: Arc<Mutex<SessionWhitelist>>,
        approver: Arc<dyn ApprovalPrompt>,
    ) -> Self {
        Self {
            workspace,
            mode,
            whitelist,
            approver,
        }
    }

    pub fn definition() -> ToolDefinition {
        ToolDefinition {
            name: WRITE_FILE_TOOL.into(),
            description: "Write (create or fully replace) ONE text file inside a designated \
                          workspace folder. The user approves each write until they grant \
                          always. Write complete file contents — this is not a patch tool. \
                          Parent folders are created. Never claim a file was written unless \
                          this tool returned ok."
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Target file path inside a workspace."
                    },
                    "content": {
                        "type": "string",
                        "description": "The COMPLETE new file contents."
                    }
                },
                "required": ["path", "content"]
            }),
        }
    }
}

fn workspace_failure(err: WorkspaceError) -> ToolOutcome {
    ToolOutcome::failure(err.kind(), err.to_string())
}

#[async_trait]
impl ToolExecutor for ReadFileTool {
    fn definitions(&self) -> Vec<ToolDefinition> {
        if self.workspace.has_roots() {
            vec![Self::definition()]
        } else {
            Vec::new()
        }
    }

    fn claims(&self, name: &str) -> bool {
        name == READ_FILE_TOOL
    }

    async fn execute(&self, call: &ToolCall) -> ToolOutcome {
        let Some(path) = string_arg(call, "path") else {
            return ToolOutcome::failure("invalid-arguments", "path is required");
        };
        let resolved = match self.workspace.resolve(&path) {
            Ok(p) => p,
            Err(e) => return workspace_failure(e),
        };
        let bytes = match std::fs::read(&resolved) {
            Ok(b) => b,
            Err(e) => {
                return ToolOutcome::failure(
                    "io",
                    format!("reading {} failed: {e}", resolved.display()),
                );
            }
        };
        let Ok(text) = String::from_utf8(bytes) else {
            return ToolOutcome::failure(
                "binary-file",
                format!("{path} is not a text file — read_file handles text only"),
            );
        };
        let total = text.chars().count();
        let mut shown: String = text.chars().take(READ_MAX_CHARS).collect();
        if total > READ_MAX_CHARS {
            shown.push_str(&format!("\n[…truncated — {total} chars total]"));
        }
        ToolOutcome::success(format!("[{}]\n{shown}", resolved.display()))
    }
}

#[async_trait]
impl ToolExecutor for ListDirTool {
    fn definitions(&self) -> Vec<ToolDefinition> {
        if self.workspace.has_roots() {
            vec![Self::definition()]
        } else {
            Vec::new()
        }
    }

    fn claims(&self, name: &str) -> bool {
        name == LIST_DIR_TOOL
    }

    async fn execute(&self, call: &ToolCall) -> ToolOutcome {
        let path = string_arg(call, "path").unwrap_or_default();
        let target = if path.trim().is_empty() {
            match self.workspace.roots().first() {
                Some(root) => root.display().to_string(),
                None => return workspace_failure(WorkspaceError::NoWorkspaces),
            }
        } else {
            path
        };
        let resolved = match self.workspace.resolve(&target) {
            Ok(p) => p,
            Err(e) => return workspace_failure(e),
        };
        let entries = match std::fs::read_dir(&resolved) {
            Ok(iter) => iter,
            Err(e) => {
                return ToolOutcome::failure("io", format!("listing {target} failed: {e}"));
            }
        };
        let mut rows: Vec<String> = Vec::new();
        let mut total = 0usize;
        for entry in entries.flatten() {
            total += 1;
            if rows.len() >= LIST_MAX_ENTRIES {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            let meta = entry.metadata().ok();
            let kind = match &meta {
                Some(m) if m.is_dir() => "dir",
                Some(_) => "file",
                None => "?",
            };
            let size = meta.map(|m| m.len()).unwrap_or(0);
            rows.push(format!("{kind}\t{size}\t{name}"));
        }
        rows.sort();
        let mut out = format!("[{}] {total} entr(ies)\n", resolved.display());
        out.push_str(&rows.join("\n"));
        if total > LIST_MAX_ENTRIES {
            out.push_str(&format!("\n[…showing {LIST_MAX_ENTRIES} of {total}]"));
        }
        ToolOutcome::success(out)
    }
}

#[async_trait]
impl ToolExecutor for WriteFileTool {
    fn definitions(&self) -> Vec<ToolDefinition> {
        if self.workspace.has_roots() {
            vec![Self::definition()]
        } else {
            Vec::new()
        }
    }

    fn claims(&self, name: &str) -> bool {
        name == WRITE_FILE_TOOL
    }

    async fn execute(&self, call: &ToolCall) -> ToolOutcome {
        let (Some(path), Some(content)) = (string_arg(call, "path"), string_arg(call, "content"))
        else {
            return ToolOutcome::failure("invalid-arguments", "path and content are required");
        };
        if content.len() > WRITE_MAX_BYTES {
            return ToolOutcome::failure(
                "too-large",
                format!(
                    "{} bytes exceeds the {WRITE_MAX_BYTES}-byte write cap — write files in \
                     smaller units",
                    content.len()
                ),
            );
        }
        let resolved = match self.workspace.resolve(&path) {
            Ok(p) => p,
            Err(e) => return workspace_failure(e),
        };
        // Approval on the shared plumbing (clipboard/run_command precedent):
        // Off refuses, Ask prompts unless session/persistent-granted,
        // AutoRun performs.
        match self.mode {
            HidRunMode::Off => {
                return ToolOutcome::failure(
                    "disabled",
                    "file writing is disabled while input control is Off — the user flips it in \
                     Settings → Input Control",
                );
            }
            HidRunMode::AutoRun => {}
            HidRunMode::Ask => {
                let granted = self
                    .whitelist
                    .lock()
                    .map(|w| w.contains(ActionKind::WriteFile))
                    .unwrap_or(false);
                if !granted {
                    let summary = format!(
                        "Write file: {} ({} bytes)",
                        resolved.display(),
                        content.len()
                    );
                    match self.approver.request(ActionKind::WriteFile, summary).await {
                        ApprovalVerdict::AllowOnce => {}
                        ApprovalVerdict::AllowKind | ApprovalVerdict::AllowAlways => {
                            if let Ok(mut whitelist) = self.whitelist.lock() {
                                whitelist.allow(ActionKind::WriteFile);
                            }
                        }
                        ApprovalVerdict::Deny => {
                            return ToolOutcome::failure(
                                "approval-denied",
                                format!("the user declined writing {path}"),
                            );
                        }
                    }
                }
            }
        }
        if let Some(parent) = resolved.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                return ToolOutcome::failure("io", format!("creating parent dirs failed: {e}"));
            }
        }
        let existed = resolved.exists();
        if let Err(e) = std::fs::write(&resolved, content.as_bytes()) {
            return ToolOutcome::failure("io", format!("writing {path} failed: {e}"));
        }
        log::info!(
            "workspace: wrote {} ({} bytes, {})",
            resolved.display(),
            content.len(),
            if existed { "replaced" } else { "created" }
        );
        ToolOutcome::success(format!(
            "wrote {} bytes to {} ({})",
            content.len(),
            resolved.display(),
            if existed {
                "replaced existing file"
            } else {
                "new file"
            }
        ))
    }
}

fn string_arg(call: &ToolCall, key: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(&call.arguments)
        .ok()?
        .get(key)?
        .as_str()
        .map(String::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    struct AllowAll;

    #[async_trait]
    impl ApprovalPrompt for AllowAll {
        async fn request(&self, _kind: ActionKind, _summary: String) -> ApprovalVerdict {
            ApprovalVerdict::AllowOnce
        }
    }

    fn scratch_ws(tag: &str) -> (Arc<WorkspaceState>, PathBuf) {
        let dir = std::env::temp_dir().join(format!("te-fs-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("hello.txt"), "hello world").unwrap();
        let ws = Arc::new(WorkspaceState::new());
        ws.set_roots(vec![dir.display().to_string()]);
        (ws, dir)
    }

    fn call(name: &str, args: serde_json::Value) -> ToolCall {
        ToolCall {
            id: "c1".into(),
            name: name.into(),
            arguments: args.to_string(),
        }
    }

    #[tokio::test]
    async fn tools_are_inert_without_roots_and_refuse_typed() {
        let ws = Arc::new(WorkspaceState::new());
        let read = ReadFileTool::new(ws.clone());
        assert!(read.definitions().is_empty(), "no roots → not offered");
        let outcome = read
            .execute(&call(READ_FILE_TOOL, serde_json::json!({"path": "x"})))
            .await;
        assert_eq!(outcome.failure.as_deref(), Some("no-workspaces"));
    }

    #[tokio::test]
    async fn read_and_list_stay_inside_and_write_round_trips() {
        let (ws, dir) = scratch_ws("rw");
        let read = ReadFileTool::new(ws.clone());
        let outcome = read
            .execute(&call(
                READ_FILE_TOOL,
                serde_json::json!({"path": "hello.txt"}),
            ))
            .await;
        assert!(outcome.ok, "{:?} {:?}", outcome.failure, outcome.content);
        assert!(outcome.content.contains("hello world"));
        // Escape attempts refuse typed with zero io.
        let escape = read
            .execute(&call(
                READ_FILE_TOOL,
                serde_json::json!({"path": "/etc/passwd"}),
            ))
            .await;
        assert_eq!(escape.failure.as_deref(), Some("outside-workspace"));

        let list = ListDirTool::new(ws.clone());
        let outcome = list
            .execute(&call(LIST_DIR_TOOL, serde_json::json!({})))
            .await;
        assert!(outcome.ok);
        assert!(outcome.content.contains("hello.txt"));

        let write = WriteFileTool::new(
            ws.clone(),
            HidRunMode::Ask,
            Arc::new(Mutex::new(SessionWhitelist::new())),
            Arc::new(AllowAll),
        );
        let outcome = write
            .execute(&call(
                WRITE_FILE_TOOL,
                serde_json::json!({"path": "sub/new.rs", "content": "fn main() {}"}),
            ))
            .await;
        assert!(outcome.ok, "{:?}", outcome.failure);
        assert_eq!(
            std::fs::read_to_string(dir.join("sub/new.rs")).unwrap(),
            "fn main() {}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn write_respects_off_mode_and_size_cap() {
        let (ws, dir) = scratch_ws("gate");
        let off = WriteFileTool::new(
            ws.clone(),
            HidRunMode::Off,
            Arc::new(Mutex::new(SessionWhitelist::new())),
            Arc::new(AllowAll),
        );
        let outcome = off
            .execute(&call(
                WRITE_FILE_TOOL,
                serde_json::json!({"path": "x.txt", "content": "hi"}),
            ))
            .await;
        assert_eq!(outcome.failure.as_deref(), Some("disabled"));

        let big = "x".repeat(WRITE_MAX_BYTES + 1);
        let ask = WriteFileTool::new(
            ws,
            HidRunMode::AutoRun,
            Arc::new(Mutex::new(SessionWhitelist::new())),
            Arc::new(AllowAll),
        );
        let outcome = ask
            .execute(&call(
                WRITE_FILE_TOOL,
                serde_json::json!({"path": "big.txt", "content": big}),
            ))
            .await;
        assert_eq!(outcome.failure.as_deref(), Some("too-large"));
        assert!(!dir.join("big.txt").exists(), "cap refuses before io");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
