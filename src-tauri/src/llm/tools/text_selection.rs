//! `text_selection` (S4): the user's current selection as context, and as
//! a target — "fix this paragraph", "translate what I highlighted" in ANY
//! app. `get` reads AXSelectedText of the focused element (falling back
//! to cmd+c with the clipboard preserved); `replace` / `insert` set
//! AXSelectedText (inserting at the caret when nothing is selected),
//! falling back to cmd+v the same way. Mutations are HID-class and typed
//! like typing; teach mode keeps `get` and refuses mutations.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;

use crate::input::commands::{resolve_approval, ApprovalDecision, HidRunMode, SessionWhitelist};
use crate::input::{ActionKind, FocusReport, InputAction, InputControl};
use crate::llm::toolloop::{ApprovalPrompt, ApprovalVerdict, ToolExecutor, ToolOutcome};
use crate::llm::{ToolCall, ToolDefinition};

pub const TEXT_SELECTION_TOOL: &str = "text_selection";
const SELECTION_MAX_CHARS: usize = 16_000;

/// The accessibility half of the seam (blocking, tests stub it).
pub trait SelectionAx: Send + Sync {
    fn selected_text(&self) -> Option<(Option<String>, FocusReport)>;
    /// `Ok(false)` = unsupported here (fall back to the keyboard).
    fn set_selected_text(&self, text: &str) -> Result<bool, String>;
}

/// The clipboard half (the fallback path saves and restores it).
pub trait ClipboardIo: Send + Sync {
    fn read(&self) -> Result<Option<String>, String>;
    fn write(&self, text: &str) -> Result<(), String>;
}

pub struct MacosSelectionAx;
impl SelectionAx for MacosSelectionAx {
    fn selected_text(&self) -> Option<(Option<String>, FocusReport)> {
        crate::input::macos::selected_text_blocking()
    }
    fn set_selected_text(&self, text: &str) -> Result<bool, String> {
        crate::input::macos::set_selected_text_blocking(text)
    }
}

pub struct SystemClipboard;
impl ClipboardIo for SystemClipboard {
    fn read(&self) -> Result<Option<String>, String> {
        crate::clipboard_tool::read_text()
    }
    fn write(&self, text: &str) -> Result<(), String> {
        crate::clipboard_tool::write_text(text)
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Args {
    action: String,
    #[serde(default)]
    text: Option<String>,
}

pub struct TextSelectionTool {
    ax: Arc<dyn SelectionAx>,
    clipboard: Arc<dyn ClipboardIo>,
    input: Arc<dyn InputControl>,
    mode: HidRunMode,
    whitelist: Arc<Mutex<SessionWhitelist>>,
    approver: Arc<dyn ApprovalPrompt>,
    read_only: bool,
    settle: Duration,
}

impl TextSelectionTool {
    pub fn new(
        ax: Arc<dyn SelectionAx>,
        clipboard: Arc<dyn ClipboardIo>,
        input: Arc<dyn InputControl>,
        mode: HidRunMode,
        whitelist: Arc<Mutex<SessionWhitelist>>,
        approver: Arc<dyn ApprovalPrompt>,
        read_only: bool,
    ) -> Self {
        Self {
            ax,
            clipboard,
            input,
            mode,
            whitelist,
            approver,
            read_only,
            settle: Duration::from_millis(200),
        }
    }

    /// Test seam: the copy/paste settle.
    pub fn with_settle(mut self, settle: Duration) -> Self {
        self.settle = settle;
        self
    }

    pub fn definition() -> ToolDefinition {
        ToolDefinition {
            name: TEXT_SELECTION_TOOL.into(),
            description: "The text the user has selected in the focused app, and a way to change \
                          it: get (read the selection), replace {text} (replace the selection with \
                          new text), insert {text} (insert at the caret). Works in any app — use it \
                          for 'fix / translate / summarise this' on highlighted text and to write \
                          the result back in place."
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["get", "replace", "insert"] },
                    "text": { "type": "string", "description": "replace/insert: the text to put there." }
                },
                "required": ["action"]
            }),
        }
    }

    async fn gate(&self, summary: String) -> Result<(), ToolOutcome> {
        if self.read_only {
            return Err(ToolOutcome::failure(
                "teach-mode",
                "Teach Me mode: type the change yourself with input_action so the user sees it",
            ));
        }
        let decision = {
            let wl = self.whitelist.lock().unwrap();
            resolve_approval(self.mode, ActionKind::TypeText, &wl)
        };
        match decision {
            ApprovalDecision::Refuse => Err(ToolOutcome::failure(
                "disabled",
                "input control is off — the user can enable it in Settings → Automation",
            )),
            ApprovalDecision::Perform => Ok(()),
            ApprovalDecision::Prompt => match self
                .approver
                .request(ActionKind::TypeText, summary.clone())
                .await
            {
                ApprovalVerdict::AllowOnce => Ok(()),
                ApprovalVerdict::AllowKind | ApprovalVerdict::AllowAlways => {
                    if let Ok(mut wl) = self.whitelist.lock() {
                        wl.allow(ActionKind::TypeText);
                    }
                    Ok(())
                }
                ApprovalVerdict::Deny => Err(ToolOutcome::failure(
                    "approval-denied",
                    format!("the user declined: {summary}"),
                )),
            },
        }
    }

    async fn key(&self, key: &str) -> Result<FocusReport, ToolOutcome> {
        self.input
            .perform(InputAction::KeyPress {
                key: key.into(),
                modifiers: Some(vec!["cmd".into()]),
            })
            .await
            .map(|r| r.focus.unwrap_or_default())
            .map_err(|e| ToolOutcome::failure(e.kind(), e.to_string()))
    }

    async fn get(&self) -> ToolOutcome {
        let (ax_text, focus) = match self.ax.selected_text() {
            Some((Some(text), focus)) => (Some(text), Some(focus)),
            Some((None, focus)) => (None, Some(focus)),
            None => (None, None),
        };
        if let Some(text) = ax_text {
            return ToolOutcome::success(
                serde_json::json!({ "ok": true, "text": clip(&text), "via": "accessibility", "focus": focus }).to_string(),
            );
        }
        // Fallback: copy, read, restore — the user's clipboard survives.
        let saved = self.clipboard.read().unwrap_or(None);
        let focus = match self.key("c").await {
            Ok(f) => f,
            Err(o) => return o,
        };
        tokio::time::sleep(self.settle).await;
        let got = self.clipboard.read().unwrap_or(None);
        let _ = self.clipboard.write(saved.as_deref().unwrap_or(""));
        match got {
            Some(text) if !text.is_empty() && Some(&text) != saved.as_ref() => ToolOutcome::success(
                serde_json::json!({ "ok": true, "text": clip(&text), "via": "copy", "focus": focus }).to_string(),
            ),
            _ => ToolOutcome::failure(
                "no-selection",
                "nothing is selected in the focused app (or it exposes no text) — ask the user to select the text, or click into the field first",
            ),
        }
    }

    async fn put(&self, text: &str, insert: bool) -> ToolOutcome {
        let verb = if insert {
            "Insert"
        } else {
            "Replace the selection with"
        };
        let shown: String = text.chars().take(60).collect();
        if let Err(r) = self.gate(format!("{verb} {shown:?}")).await {
            return r;
        }
        match self.ax.set_selected_text(text) {
            Ok(true) => {
                let focus = self.ax.selected_text().map(|(_, f)| f);
                return ToolOutcome::success(
                    serde_json::json!({ "ok": true, "via": "accessibility", "verified": { "focus": focus } }).to_string(),
                );
            }
            Ok(false) => {}
            Err(e) => log::warn!("text_selection: AX set failed ({e}); pasting instead"),
        }
        // Fallback: paste with the clipboard preserved.
        let saved = self.clipboard.read().unwrap_or(None);
        if let Err(e) = self.clipboard.write(text) {
            return ToolOutcome::failure("clipboard-failed", e);
        }
        let focus = match self.key("v").await {
            Ok(f) => f,
            Err(o) => return o,
        };
        tokio::time::sleep(self.settle).await;
        let _ = self.clipboard.write(saved.as_deref().unwrap_or(""));
        let landed = focus
            .value
            .as_deref()
            .is_some_and(|v| v.contains(text.chars().take(64).collect::<String>().as_str()));
        ToolOutcome::success(
            serde_json::json!({ "ok": true, "via": "paste", "verified": { "focus": focus, "textEntered": landed } }).to_string(),
        )
    }
}

fn clip(s: &str) -> String {
    if s.chars().count() > SELECTION_MAX_CHARS {
        format!(
            "{}… [{} chars]",
            s.chars().take(SELECTION_MAX_CHARS).collect::<String>(),
            s.chars().count()
        )
    } else {
        s.to_string()
    }
}

#[async_trait]
impl ToolExecutor for TextSelectionTool {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![Self::definition()]
    }

    fn claims(&self, name: &str) -> bool {
        name == TEXT_SELECTION_TOOL
    }

    async fn execute(&self, call: &ToolCall) -> ToolOutcome {
        let args: Args = match serde_json::from_str(&call.arguments) {
            Ok(a) => a,
            Err(e) => {
                return ToolOutcome::failure(
                    "invalid-arguments",
                    format!("invalid {TEXT_SELECTION_TOOL} arguments: {e}"),
                )
            }
        };
        match args.action.as_str() {
            "get" => self.get().await,
            "replace" | "insert" => match args.text {
                Some(text) => self.put(&text, args.action == "insert").await,
                None => {
                    ToolOutcome::failure("invalid-arguments", format!("{} needs text", args.action))
                }
            },
            other => ToolOutcome::failure(
                "invalid-arguments",
                format!("unknown action {other:?} (get | replace | insert)"),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::{ActionReport, InputError, InputPermission};

    struct Ax {
        selected: Option<String>,
        settable: bool,
        sets: Mutex<Vec<String>>,
    }
    impl SelectionAx for Ax {
        fn selected_text(&self) -> Option<(Option<String>, FocusReport)> {
            Some((
                self.selected.clone(),
                FocusReport {
                    app: Some("Notes".into()),
                    ..FocusReport::default()
                },
            ))
        }
        fn set_selected_text(&self, text: &str) -> Result<bool, String> {
            self.sets.lock().unwrap().push(text.into());
            Ok(self.settable)
        }
    }
    struct Clip(Mutex<Option<String>>, Mutex<Vec<String>>);
    impl ClipboardIo for Clip {
        fn read(&self) -> Result<Option<String>, String> {
            Ok(self.0.lock().unwrap().clone())
        }
        fn write(&self, t: &str) -> Result<(), String> {
            self.1.lock().unwrap().push(t.into());
            *self.0.lock().unwrap() = Some(t.into());
            Ok(())
        }
    }
    /// cmd+c "copies" the given text into the clipboard; cmd+v echoes it into the focus value.
    struct Keys {
        copy_yields: Option<String>,
        clip: Arc<Clip>,
        presses: Mutex<Vec<String>>,
    }
    #[async_trait]
    impl InputControl for Keys {
        fn permission(&self) -> InputPermission {
            InputPermission {
                granted: true,
                supported: true,
            }
        }
        fn request_permission(&self) -> bool {
            true
        }
        async fn perform(&self, action: InputAction) -> Result<ActionReport, InputError> {
            let InputAction::KeyPress { key, .. } = &action else {
                panic!()
            };
            self.presses.lock().unwrap().push(key.clone());
            let mut value = None;
            if key == "c" {
                if let Some(t) = &self.copy_yields {
                    *self.clip.0.lock().unwrap() = Some(t.clone());
                }
            }
            if key == "v" {
                value = self
                    .clip
                    .0
                    .lock()
                    .unwrap()
                    .clone()
                    .map(|v| format!("before {v} after"));
            }
            Ok(ActionReport {
                cursor: None,
                focus: Some(FocusReport {
                    app: Some("Notes".into()),
                    value,
                    ..FocusReport::default()
                }),
                text_entered: None,
                clicked_element: None,
            })
        }
    }
    struct Allow;
    #[async_trait]
    impl ApprovalPrompt for Allow {
        async fn request(&self, _k: ActionKind, _s: String) -> ApprovalVerdict {
            ApprovalVerdict::AllowOnce
        }
    }
    fn tool(
        selected: Option<&str>,
        settable: bool,
        copy_yields: Option<&str>,
        mode: HidRunMode,
        read_only: bool,
    ) -> (TextSelectionTool, Arc<Ax>, Arc<Clip>, Arc<Keys>) {
        let ax = Arc::new(Ax {
            selected: selected.map(String::from),
            settable,
            sets: Mutex::new(vec![]),
        });
        let clip = Arc::new(Clip(
            Mutex::new(Some("user clipboard".into())),
            Mutex::new(vec![]),
        ));
        let keys = Arc::new(Keys {
            copy_yields: copy_yields.map(String::from),
            clip: clip.clone(),
            presses: Mutex::new(vec![]),
        });
        let t = TextSelectionTool::new(
            ax.clone(),
            clip.clone(),
            keys.clone(),
            mode,
            Arc::new(Mutex::new(SessionWhitelist::new())),
            Arc::new(Allow),
            read_only,
        )
        .with_settle(Duration::from_millis(1));
        (t, ax, clip, keys)
    }
    fn call(args: serde_json::Value) -> ToolCall {
        ToolCall {
            id: "c".into(),
            name: TEXT_SELECTION_TOOL.into(),
            arguments: args.to_string(),
        }
    }

    #[tokio::test]
    async fn get_prefers_accessibility_and_never_touches_the_keyboard() {
        let (t, _, _, keys) = tool(Some("hello world"), true, None, HidRunMode::AutoRun, false);
        let out = t.execute(&call(serde_json::json!({"action":"get"}))).await;
        assert!(
            out.ok
                && out.content.contains("hello world")
                && out.content.contains("\"via\":\"accessibility\""),
            "{}",
            out.content
        );
        assert!(keys.presses.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn get_falls_back_to_copy_and_restores_the_clipboard() {
        let (t, _, clip, keys) = tool(None, true, Some("copied text"), HidRunMode::AutoRun, false);
        let out = t.execute(&call(serde_json::json!({"action":"get"}))).await;
        assert!(
            out.ok
                && out.content.contains("copied text")
                && out.content.contains("\"via\":\"copy\""),
            "{}",
            out.content
        );
        assert_eq!(keys.presses.lock().unwrap().as_slice(), ["c"]);
        assert_eq!(
            clip.0.lock().unwrap().as_deref(),
            Some("user clipboard"),
            "restored"
        );
        // Nothing selected: copy yields nothing new → typed.
        let (t, ..) = tool(None, true, None, HidRunMode::AutoRun, false);
        let out = t.execute(&call(serde_json::json!({"action":"get"}))).await;
        assert_eq!(out.failure.as_deref(), Some("no-selection"));
    }

    #[tokio::test]
    async fn replace_uses_accessibility_when_supported_else_pastes_and_restores() {
        let (t, ax, _, keys) = tool(Some("old"), true, None, HidRunMode::AutoRun, false);
        let out = t
            .execute(&call(serde_json::json!({"action":"replace","text":"new"})))
            .await;
        assert!(
            out.ok && out.content.contains("\"via\":\"accessibility\""),
            "{}",
            out.content
        );
        assert_eq!(ax.sets.lock().unwrap().as_slice(), ["new"]);
        assert!(keys.presses.lock().unwrap().is_empty());
        let (t, _, clip, keys) = tool(Some("old"), false, None, HidRunMode::AutoRun, false);
        let out = t
            .execute(&call(
                serde_json::json!({"action":"insert","text":"pasted"}),
            ))
            .await;
        assert!(
            out.ok
                && out.content.contains("\"via\":\"paste\"")
                && out.content.contains("\"textEntered\":true"),
            "{}",
            out.content
        );
        assert_eq!(keys.presses.lock().unwrap().as_slice(), ["v"]);
        assert_eq!(clip.0.lock().unwrap().as_deref(), Some("user clipboard"));
    }

    #[tokio::test]
    async fn mutations_gate_reads_do_not_and_teach_is_read_only() {
        let (t, ax, ..) = tool(Some("x"), true, None, HidRunMode::Off, false);
        assert_eq!(
            t.execute(&call(serde_json::json!({"action":"replace","text":"y"})))
                .await
                .failure
                .as_deref(),
            Some("disabled")
        );
        assert!(
            t.execute(&call(serde_json::json!({"action":"get"})))
                .await
                .ok
        );
        assert!(ax.sets.lock().unwrap().is_empty());
        let (t, ..) = tool(Some("x"), true, None, HidRunMode::AutoRun, true);
        assert_eq!(
            t.execute(&call(serde_json::json!({"action":"insert","text":"y"})))
                .await
                .failure
                .as_deref(),
            Some("teach-mode")
        );
        assert_eq!(
            t.execute(&call(serde_json::json!({"action":"replace"})))
                .await
                .failure
                .as_deref(),
            Some("invalid-arguments")
        );
    }
}
