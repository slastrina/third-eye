//! Clipboard + wait tools (HID-extensions spec, 2026-07-26).
//!
//! `clipboard` reads or writes the system pasteboard: write + Cmd+V is the
//! reliable way to enter long text (char-by-char typing is slow and
//! error-prone), and select + Cmd+C + read is the reliable way to EXTRACT
//! text from an app. Reading is user data, so the tool rides the same
//! HidRunMode gate as input actions with its own `ActionKind::Clipboard`
//! (Off refuses typed; Ask prompts naming the operation; session grants
//! apply). Clipboard text is transient tool context — never persisted to
//! the memory store.
//!
//! `wait` sleeps a bounded moment (50–3000 ms) so UI animations settle
//! before the next screen_query — ungated: it touches nothing.

use std::sync::{Arc, Mutex};

use crate::input::commands::{resolve_approval, ApprovalDecision, HidRunMode, SessionWhitelist};
use crate::input::ActionKind;
use crate::llm::toolloop::{ApprovalPrompt, ApprovalVerdict, ToolExecutor, ToolOutcome};
use crate::llm::{ToolCall, ToolDefinition};

pub const CLIPBOARD_TOOL: &str = "clipboard";
pub const WAIT_TOOL: &str = "wait";

/// Read cap: a giant clipboard must not flood the model context.
const MAX_READ_CHARS: usize = 16 * 1024;

/// Put text on the system clipboard (the run report's "Copy").
pub fn write_text(text: &str) -> Result<(), String> {
    platform::write(text)
}

#[cfg(target_os = "macos")]
mod platform {
    use objc2_app_kit::{NSPasteboard, NSPasteboardTypeString};
    use objc2_foundation::NSString;

    pub fn read() -> Result<Option<String>, String> {
        unsafe {
            let pasteboard = NSPasteboard::generalPasteboard();
            Ok(pasteboard
                .stringForType(NSPasteboardTypeString)
                .map(|s| s.to_string()))
        }
    }

    pub fn write(text: &str) -> Result<(), String> {
        unsafe {
            let pasteboard = NSPasteboard::generalPasteboard();
            pasteboard.clearContents();
            let ok =
                pasteboard.setString_forType(&NSString::from_str(text), NSPasteboardTypeString);
            if ok {
                Ok(())
            } else {
                Err("NSPasteboard setString returned false".into())
            }
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod platform {
    pub fn read() -> Result<Option<String>, String> {
        Err("clipboard is not supported on this platform".into())
    }

    pub fn write(_text: &str) -> Result<(), String> {
        Err("clipboard is not supported on this platform".into())
    }
}

#[derive(serde::Deserialize)]
struct ClipboardArgs {
    op: String,
    #[serde(default)]
    text: Option<String>,
}

/// The gated clipboard tool. Mode snapshot + whitelist + approver injected
/// per run, mirroring the HID gate's seams.
pub struct ClipboardTool {
    mode: HidRunMode,
    whitelist: Arc<Mutex<SessionWhitelist>>,
    approver: Arc<dyn ApprovalPrompt>,
}

impl ClipboardTool {
    pub fn new(
        mode: HidRunMode,
        whitelist: Arc<Mutex<SessionWhitelist>>,
        approver: Arc<dyn ApprovalPrompt>,
    ) -> Self {
        Self {
            mode,
            whitelist,
            approver,
        }
    }

    pub fn definition() -> ToolDefinition {
        ToolDefinition {
            name: CLIPBOARD_TOOL.into(),
            description: "Read or write the system clipboard. To enter LONG text reliably: \
                          clipboard write it, click the target field, then key-press \"v\" with \
                          modifiers [\"cmd\"] — much faster and safer than type-text. To EXTRACT \
                          text from an app: select it (mouse-drag or cmd+a), key-press \"c\" with \
                          [\"cmd\"], then clipboard read."
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "op": {
                        "type": "string",
                        "enum": ["read", "write"],
                        "description": "read returns the current clipboard text; write replaces it."
                    },
                    "text": {
                        "type": "string",
                        "description": "write: the text to place on the clipboard."
                    }
                },
                "required": ["op"]
            }),
        }
    }
}

#[async_trait::async_trait]
impl ToolExecutor for ClipboardTool {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![Self::definition()]
    }

    async fn execute(&self, call: &ToolCall) -> ToolOutcome {
        if call.name != CLIPBOARD_TOOL {
            return ToolOutcome::failure(
                "unknown-tool",
                format!("unknown tool: {} (available: {CLIPBOARD_TOOL})", call.name),
            );
        }
        let args: ClipboardArgs = match serde_json::from_str(&call.arguments) {
            Ok(args) => args,
            Err(e) => {
                return ToolOutcome::failure(
                    "invalid-arguments",
                    format!("invalid {CLIPBOARD_TOOL} arguments: {e}"),
                )
            }
        };
        let summary = match args.op.as_str() {
            "read" => "Read the clipboard's current contents".to_string(),
            "write" => format!(
                "Put text on the clipboard ({} chars)",
                args.text.as_deref().map(str::len).unwrap_or(0)
            ),
            other => {
                return ToolOutcome::failure(
                    "invalid-arguments",
                    format!("clipboard op must be read or write, got {other:?}"),
                )
            }
        };
        // Same Off/Ask/AutoRun path as HID actions, kind Clipboard.
        let decision = {
            let whitelist = self.whitelist.lock().expect("whitelist lock poisoned");
            resolve_approval(self.mode, ActionKind::Clipboard, &whitelist)
        };
        match decision {
            ApprovalDecision::Refuse => {
                return ToolOutcome::failure(
                    "disabled",
                    "input control is off — the clipboard tool is gated with it \
                     (Settings → Automation → Input Control)",
                );
            }
            ApprovalDecision::Perform => {}
            ApprovalDecision::Prompt => {
                match self.approver.request(ActionKind::Clipboard, summary).await {
                    ApprovalVerdict::AllowOnce => {}
                    // AllowAlways is downgraded by the production prompt;
                    // treat a raw one like the session grant defensively.
                    ApprovalVerdict::AllowKind | ApprovalVerdict::AllowAlways => {
                        if let Ok(mut whitelist) = self.whitelist.lock() {
                            whitelist.allow(ActionKind::Clipboard);
                        }
                    }
                    ApprovalVerdict::Deny => {
                        return ToolOutcome::failure(
                            "approval-denied",
                            "the user declined the clipboard operation",
                        );
                    }
                }
            }
        }
        match args.op.as_str() {
            "read" => match platform::read() {
                Ok(Some(text)) => {
                    let shown = if text.chars().count() > MAX_READ_CHARS {
                        let cut: String = text.chars().take(MAX_READ_CHARS).collect();
                        format!("{cut}\n… [truncated: clipboard is longer]")
                    } else {
                        text
                    };
                    ToolOutcome::success(shown)
                }
                Ok(None) => ToolOutcome::success("(the clipboard has no text)".to_string()),
                Err(e) => ToolOutcome::failure("clipboard-failed", e),
            },
            _ => {
                let Some(text) = args.text else {
                    return ToolOutcome::failure(
                        "invalid-arguments",
                        "clipboard write needs the text field",
                    );
                };
                match platform::write(&text) {
                    Ok(()) => ToolOutcome::success(format!(
                        "clipboard set ({} chars) — click the target field, then press cmd+v",
                        text.len()
                    )),
                    Err(e) => ToolOutcome::failure("clipboard-failed", e),
                }
            }
        }
    }
}

/// Clamp a requested wait into the honest band.
pub fn clamp_wait_ms(requested: Option<u64>) -> u64 {
    requested.unwrap_or(500).clamp(50, 3000)
}

/// The ungated settle tool: sleep a bounded moment.
pub struct WaitTool;

impl WaitTool {
    pub fn definition() -> ToolDefinition {
        ToolDefinition {
            name: WAIT_TOOL.into(),
            description: "Pause briefly (default 500ms, max 3000ms) so the UI can settle — use \
                          after opening an app or triggering an animation, BEFORE the next \
                          screen_query, instead of retrying failed reads."
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "ms": {
                        "type": "integer",
                        "description": "Milliseconds to wait (50–3000, default 500)."
                    }
                }
            }),
        }
    }
}

#[derive(serde::Deserialize)]
struct WaitArgs {
    #[serde(default)]
    ms: Option<u64>,
}

#[async_trait::async_trait]
impl ToolExecutor for WaitTool {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![Self::definition()]
    }

    async fn execute(&self, call: &ToolCall) -> ToolOutcome {
        if call.name != WAIT_TOOL {
            return ToolOutcome::failure(
                "unknown-tool",
                format!("unknown tool: {} (available: {WAIT_TOOL})", call.name),
            );
        }
        let args: WaitArgs = serde_json::from_str(&call.arguments).unwrap_or(WaitArgs { ms: None });
        let ms = clamp_wait_ms(args.ms);
        tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
        ToolOutcome::success(format!("waited {ms}ms"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;

    struct Scripted(ApprovalVerdict);
    #[async_trait]
    impl ApprovalPrompt for Scripted {
        async fn request(&self, _k: ActionKind, _s: String) -> ApprovalVerdict {
            self.0
        }
    }

    fn tool(mode: HidRunMode, verdict: ApprovalVerdict) -> ClipboardTool {
        ClipboardTool::new(
            mode,
            Arc::new(Mutex::new(SessionWhitelist::new())),
            Arc::new(Scripted(verdict)),
        )
    }

    fn call(args: serde_json::Value) -> ToolCall {
        ToolCall {
            id: "c".into(),
            name: CLIPBOARD_TOOL.into(),
            arguments: args.to_string(),
        }
    }

    #[tokio::test]
    async fn off_mode_refuses_typed() {
        let outcome = tool(HidRunMode::Off, ApprovalVerdict::AllowOnce)
            .execute(&call(serde_json::json!({"op": "read"})))
            .await;
        assert_eq!(outcome.failure.as_deref(), Some("disabled"));
    }

    #[tokio::test]
    async fn deny_never_touches_the_pasteboard() {
        let outcome = tool(HidRunMode::Ask, ApprovalVerdict::Deny)
            .execute(&call(serde_json::json!({"op": "write", "text": "x"})))
            .await;
        assert_eq!(outcome.failure.as_deref(), Some("approval-denied"));
    }

    #[tokio::test]
    async fn bad_op_is_typed_invalid() {
        let outcome = tool(HidRunMode::AutoRun, ApprovalVerdict::Deny)
            .execute(&call(serde_json::json!({"op": "paste"})))
            .await;
        assert_eq!(outcome.failure.as_deref(), Some("invalid-arguments"));
    }

    #[test]
    fn wait_clamps_into_the_band() {
        assert_eq!(clamp_wait_ms(None), 500);
        assert_eq!(clamp_wait_ms(Some(1)), 50);
        assert_eq!(clamp_wait_ms(Some(60_000)), 3000);
        assert_eq!(clamp_wait_ms(Some(800)), 800);
    }
}
