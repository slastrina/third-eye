//! `ui_action` (S2): press, set the value of, or focus a control by its
//! NAME in the focused app — through the accessibility tree, not the
//! mouse. screen_query already harvests real AXButton/AXLink/AXTextField
//! elements; clicking them is where the cursor-commit race, occlusion and
//! wrong-app misses live. AXPress / AXValue have none of that, and the
//! readback (the element's value, the system focus) is the verification.
//! HID-class: run mode + approval like a click. Teach mode strips it —
//! the human way is visible.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde::Deserialize;

use crate::input::commands::{resolve_approval, ApprovalDecision, HidRunMode, SessionWhitelist};
use crate::input::ActionKind;
use crate::llm::toolloop::{
    ApprovalPrompt, ApprovalVerdict, FocusedApp, ToolExecutor, ToolOutcome,
};
use crate::llm::{ToolCall, ToolDefinition};
use crate::screenquery::ax::{AxAct, AxActionError, AxActionReport};

pub const UI_ACTION_TOOL: &str = "ui_action";

/// The AX-actions seam: tests script it, macOS walks the real tree.
#[async_trait]
pub trait AxActions: Send + Sync {
    async fn act(
        &self,
        app: &str,
        title: &str,
        role: Option<&str>,
        act: AxAct,
    ) -> Result<AxActionReport, AxActionError>;
}

/// Production: the focused app's pid, the blocking walk on a worker, and a
/// hard timeout above the walk's own budget (a wedged AX tree ends this
/// ACTION typed, never the run).
pub struct MacosAxActions;

#[async_trait]
impl AxActions for MacosAxActions {
    async fn act(
        &self,
        app: &str,
        title: &str,
        role: Option<&str>,
        act: AxAct,
    ) -> Result<AxActionReport, AxActionError> {
        let Some(pid) = crate::appfocus::macos::pid_for_app_name(app) else {
            return Err(AxActionError::Failed {
                detail: format!("{app} is not running"),
            });
        };
        let title = title.to_string();
        let role = role.map(String::from);
        let task = tokio::task::spawn_blocking(move || {
            crate::screenquery::ax::perform_ui_action_blocking(pid, &title, role.as_deref(), act)
        });
        match tokio::time::timeout(std::time::Duration::from_secs(6), task).await {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => Err(AxActionError::Failed {
                detail: format!("ui_action task failed: {e}"),
            }),
            Err(_) => Err(AxActionError::Failed {
                detail: "the app's accessibility tree did not answer in time".into(),
            }),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Args {
    action: String,
    element: String,
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    value: Option<String>,
}

pub struct UiActionTool {
    backend: Arc<dyn AxActions>,
    focused_app: Arc<FocusedApp>,
    mode: HidRunMode,
    whitelist: Arc<Mutex<SessionWhitelist>>,
    approver: Arc<dyn ApprovalPrompt>,
}

impl UiActionTool {
    pub fn new(
        backend: Arc<dyn AxActions>,
        focused_app: Arc<FocusedApp>,
        mode: HidRunMode,
        whitelist: Arc<Mutex<SessionWhitelist>>,
        approver: Arc<dyn ApprovalPrompt>,
    ) -> Self {
        Self {
            backend,
            focused_app,
            mode,
            whitelist,
            approver,
        }
    }

    pub fn definition() -> ToolDefinition {
        ToolDefinition {
            name: UI_ACTION_TOOL.into(),
            description: "Act on a control in the focused app BY NAME through accessibility — \
                          no mouse, no coordinates: press a button/link/menu item, set a text \
                          field's value, or focus a field. Prefer this over mouse-click for any \
                          element screen_query listed with a role. Name the element exactly as \
                          screen_query showed it; add role (button, link, textfield, checkbox) \
                          when names repeat. Fails typed when nothing (or several things) match."
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["press", "set_value", "focus"] },
                    "element": { "type": "string", "description": "The control's title/label as screen_query showed it." },
                    "role": { "type": "string", "description": "Optional role filter: button, link, textfield, checkbox, menuitem, …" },
                    "value": { "type": "string", "description": "set_value: the text to put in the field." }
                },
                "required": ["action", "element"]
            }),
        }
    }
}

#[async_trait]
impl ToolExecutor for UiActionTool {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![Self::definition()]
    }

    fn claims(&self, name: &str) -> bool {
        name == UI_ACTION_TOOL
    }

    async fn execute(&self, call: &ToolCall) -> ToolOutcome {
        let args: Args = match serde_json::from_str(&call.arguments) {
            Ok(a) => a,
            Err(e) => {
                return ToolOutcome::failure(
                    "invalid-arguments",
                    format!("invalid {UI_ACTION_TOOL} arguments: {e}"),
                )
            }
        };
        let act = match args.action.as_str() {
            "press" => AxAct::Press,
            "focus" => AxAct::Focus,
            "set_value" => match args.value.clone() {
                Some(v) => AxAct::SetValue(v),
                None => return ToolOutcome::failure("invalid-arguments", "set_value needs value"),
            },
            other => {
                return ToolOutcome::failure(
                    "invalid-arguments",
                    format!("unknown action {other:?} (press | set_value | focus)"),
                )
            }
        };
        if args.element.trim().is_empty() {
            return ToolOutcome::failure("invalid-arguments", "element must not be empty");
        }
        // The element lives in the app the model focused — that is where
        // the tree is walked; nothing focused, nothing to act on.
        let Some(app) = self.focused_app.current() else {
            return ToolOutcome::failure(
                "no-focused-app",
                "focus_app the app first, then screen_query to see its controls",
            );
        };
        let summary = format!(
            "{} {:?} in {app}",
            match &act {
                AxAct::Press => "Press".to_string(),
                AxAct::SetValue(v) => format!("Set to {v:?}:"),
                AxAct::Focus => "Focus".to_string(),
            },
            args.element.trim()
        );
        let decision = {
            let wl = self.whitelist.lock().unwrap();
            resolve_approval(self.mode, ActionKind::UiAction, &wl)
        };
        match decision {
            ApprovalDecision::Refuse => {
                return ToolOutcome::failure(
                    "disabled",
                    "input control is off — the user can enable it in Settings → Automation",
                )
            }
            ApprovalDecision::Perform => {}
            ApprovalDecision::Prompt => match self
                .approver
                .request(ActionKind::UiAction, summary.clone())
                .await
            {
                ApprovalVerdict::AllowOnce => {}
                ApprovalVerdict::AllowKind | ApprovalVerdict::AllowAlways => {
                    if let Ok(mut wl) = self.whitelist.lock() {
                        wl.allow(ActionKind::UiAction);
                    }
                }
                ApprovalVerdict::Deny => {
                    return ToolOutcome::failure(
                        "approval-denied",
                        format!("the user declined: {summary}"),
                    )
                }
            },
        }
        match self
            .backend
            .act(&app, args.element.trim(), args.role.as_deref(), act)
            .await
        {
            Ok(report) => ToolOutcome::success(
                serde_json::json!({ "ok": true, "app": app, "verified": report }).to_string(),
            ),
            Err(e) => ToolOutcome::failure(e.kind(), e.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type Recorded = (String, String, Option<String>, AxAct);
    struct Scripted(Mutex<Vec<Recorded>>, Result<AxActionReport, AxActionError>);
    #[async_trait]
    impl AxActions for Scripted {
        async fn act(
            &self,
            app: &str,
            title: &str,
            role: Option<&str>,
            act: AxAct,
        ) -> Result<AxActionReport, AxActionError> {
            self.0
                .lock()
                .unwrap()
                .push((app.into(), title.into(), role.map(String::from), act));
            self.1.clone()
        }
    }
    struct Prompt(ApprovalVerdict, Mutex<Vec<String>>);
    #[async_trait]
    impl ApprovalPrompt for Prompt {
        async fn request(&self, _k: ActionKind, s: String) -> ApprovalVerdict {
            self.1.lock().unwrap().push(s);
            self.0
        }
    }
    fn ok_report() -> AxActionReport {
        AxActionReport {
            matched_role: "AXButton".into(),
            matched_title: "Save".into(),
            value_after: None,
            focused_after: Some("AXButton: Save".into()),
        }
    }
    fn tool(
        mode: HidRunMode,
        verdict: ApprovalVerdict,
        result: Result<AxActionReport, AxActionError>,
        focused: Option<&str>,
    ) -> (UiActionTool, Arc<Scripted>, Arc<Prompt>) {
        let backend = Arc::new(Scripted(Mutex::new(vec![]), result));
        let prompt = Arc::new(Prompt(verdict, Mutex::new(vec![])));
        let fa = Arc::new(FocusedApp::new());
        if let Some(f) = focused {
            fa.set(f);
        }
        let t = UiActionTool::new(
            backend.clone(),
            fa,
            mode,
            Arc::new(Mutex::new(SessionWhitelist::new())),
            prompt.clone(),
        );
        (t, backend, prompt)
    }
    fn call(args: serde_json::Value) -> ToolCall {
        ToolCall {
            id: "c".into(),
            name: UI_ACTION_TOOL.into(),
            arguments: args.to_string(),
        }
    }

    #[tokio::test]
    async fn press_by_name_reports_the_readback_and_asks_in_ask_mode() {
        let (t, backend, prompt) = tool(
            HidRunMode::Ask,
            ApprovalVerdict::AllowOnce,
            Ok(ok_report()),
            Some("TextEdit"),
        );
        let out = t
            .execute(&call(
                serde_json::json!({"action":"press","element":"Save","role":"button"}),
            ))
            .await;
        assert!(out.ok, "{out:?}");
        assert!(
            out.content.contains("\"matchedRole\":\"AXButton\""),
            "{}",
            out.content
        );
        let calls = backend.0.lock().unwrap();
        assert_eq!(calls[0].0, "TextEdit");
        assert_eq!(calls[0].2.as_deref(), Some("button"));
        assert_eq!(calls[0].3, AxAct::Press);
        assert_eq!(prompt.1.lock().unwrap()[0], "Press \"Save\" in TextEdit");
    }

    #[tokio::test]
    async fn needs_a_focused_app_and_a_value_for_set_value() {
        let (t, ..) = tool(
            HidRunMode::AutoRun,
            ApprovalVerdict::AllowOnce,
            Ok(ok_report()),
            None,
        );
        let out = t
            .execute(&call(
                serde_json::json!({"action":"press","element":"Save"}),
            ))
            .await;
        assert_eq!(out.failure.as_deref(), Some("no-focused-app"));
        let (t, ..) = tool(
            HidRunMode::AutoRun,
            ApprovalVerdict::AllowOnce,
            Ok(ok_report()),
            Some("X"),
        );
        let out = t
            .execute(&call(
                serde_json::json!({"action":"set_value","element":"Search"}),
            ))
            .await;
        assert_eq!(out.failure.as_deref(), Some("invalid-arguments"));
    }

    #[tokio::test]
    async fn off_and_deny_refuse_typed_before_touching_the_tree() {
        let (t, backend, _) = tool(
            HidRunMode::Off,
            ApprovalVerdict::AllowOnce,
            Ok(ok_report()),
            Some("X"),
        );
        let out = t
            .execute(&call(
                serde_json::json!({"action":"press","element":"Save"}),
            ))
            .await;
        assert_eq!(out.failure.as_deref(), Some("disabled"));
        assert!(backend.0.lock().unwrap().is_empty());
        let (t, backend, _) = tool(
            HidRunMode::Ask,
            ApprovalVerdict::Deny,
            Ok(ok_report()),
            Some("X"),
        );
        let out = t
            .execute(&call(
                serde_json::json!({"action":"press","element":"Save"}),
            ))
            .await;
        assert_eq!(out.failure.as_deref(), Some("approval-denied"));
        assert!(backend.0.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn backend_errors_ride_back_typed_with_candidates() {
        let err = AxActionError::Ambiguous {
            candidates: vec![
                "AXButton \"Add to cart\"".into(),
                "AXButton \"Add to wishlist\"".into(),
            ],
        };
        let (t, ..) = tool(
            HidRunMode::AutoRun,
            ApprovalVerdict::AllowOnce,
            Err(err),
            Some("Safari"),
        );
        let out = t
            .execute(&call(
                serde_json::json!({"action":"press","element":"Add to"}),
            ))
            .await;
        assert_eq!(out.failure.as_deref(), Some("ambiguous"));
        assert!(out.content.contains("Add to wishlist"));
    }
}
