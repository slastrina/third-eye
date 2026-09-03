//! `open` (S1): a typed way to open a URL, a file/folder, or an app —
//! replacing `run_command open …`, which a small model reaches for
//! inconsistently and which always spawned a new tab. URLs go through
//! the one-tab browser module (and the SAME grounding the executor applies
//! to shell opens); paths open with Launch Services after an existence
//! check and the approval gate; apps go through the focus_app backend so
//! the result is a verified frontmost report.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde::Deserialize;

use crate::appfocus::AppFocus;
use crate::input::commands::{resolve_approval, ApprovalDecision, HidRunMode, SessionWhitelist};
use crate::input::ActionKind;
use crate::llm::toolloop::{ApprovalPrompt, ApprovalVerdict, Opener, ToolExecutor, ToolOutcome};
use crate::llm::{ToolCall, ToolDefinition};

pub const OPEN_TOOL: &str = "open";

/// Path-open seam so tests never launch anything.
#[async_trait]
pub trait PathOpener: Send + Sync {
    async fn open_path(&self, path: &std::path::Path) -> Result<(), String>;
}

/// Production: macOS `open <path>` (the default app for the file).
pub struct SystemPathOpener;

#[async_trait]
impl PathOpener for SystemPathOpener {
    async fn open_path(&self, path: &std::path::Path) -> Result<(), String> {
        let status = tokio::process::Command::new("/usr/bin/open")
            .arg(path)
            .status()
            .await
            .map_err(|e| format!("could not run open: {e}"))?;
        if status.success() {
            Ok(())
        } else {
            Err(format!("open exited {status}"))
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct OpenArgs {
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    app: Option<String>,
}

pub struct OpenTool {
    opener: Arc<dyn Opener>,
    paths: Arc<dyn PathOpener>,
    focus: Arc<dyn AppFocus>,
    mode: HidRunMode,
    whitelist: Arc<Mutex<SessionWhitelist>>,
    approver: Arc<dyn ApprovalPrompt>,
}

impl OpenTool {
    pub fn new(
        opener: Arc<dyn Opener>,
        paths: Arc<dyn PathOpener>,
        focus: Arc<dyn AppFocus>,
        mode: HidRunMode,
        whitelist: Arc<Mutex<SessionWhitelist>>,
        approver: Arc<dyn ApprovalPrompt>,
    ) -> Self {
        Self {
            opener,
            paths,
            focus,
            mode,
            whitelist,
            approver,
        }
    }

    pub fn definition() -> ToolDefinition {
        ToolDefinition {
            name: OPEN_TOOL.into(),
            description: "Open ONE thing: a web page (url — only a URL the user gave you or \
                          that appeared in a page/tool result; it opens in Third Eye's own \
                          browser tab), a file or folder (path — absolute), or an app (app — \
                          by name, same as focus_app). Pass exactly one of url, path, app."
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "url": { "type": "string", "description": "A web address to show in the browser." },
                    "path": { "type": "string", "description": "An absolute file or folder path to open with its default app." },
                    "app": { "type": "string", "description": "An application name to bring to the front (launched if needed)." }
                },
                "required": []
            }),
        }
    }

    /// Path and app opens are HID-class: mode + session grant + prompt.
    async fn approve(&self, kind: ActionKind, summary: String) -> Result<(), ToolOutcome> {
        let decision = {
            let wl = self.whitelist.lock().unwrap();
            resolve_approval(self.mode, kind, &wl)
        };
        match decision {
            ApprovalDecision::Refuse => Err(ToolOutcome::failure(
                "disabled",
                "input control is off — the user can enable it in Settings → Automation",
            )),
            ApprovalDecision::Perform => Ok(()),
            ApprovalDecision::Prompt => match self.approver.request(kind, summary.clone()).await {
                ApprovalVerdict::AllowOnce => Ok(()),
                ApprovalVerdict::AllowKind | ApprovalVerdict::AllowAlways => {
                    if let Ok(mut wl) = self.whitelist.lock() {
                        wl.allow(kind);
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
}

#[async_trait]
impl ToolExecutor for OpenTool {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![Self::definition()]
    }

    fn claims(&self, name: &str) -> bool {
        name == OPEN_TOOL
    }

    async fn execute(&self, call: &ToolCall) -> ToolOutcome {
        let args: OpenArgs = match serde_json::from_str(&call.arguments) {
            Ok(a) => a,
            Err(e) => {
                return ToolOutcome::failure(
                    "invalid-arguments",
                    format!("invalid {OPEN_TOOL} arguments: {e}"),
                )
            }
        };
        let given = [&args.url, &args.path, &args.app]
            .iter()
            .filter(|v| v.as_deref().is_some_and(|s| !s.trim().is_empty()))
            .count();
        if given != 1 {
            return ToolOutcome::failure("invalid-arguments", "pass exactly one of url, path, app");
        }
        if let Some(url) = args.url.as_deref().map(str::trim) {
            // Grounding (which URL is allowed) is the UrlGroundingExecutor's
            // job — it intercepts this tool the same way it does shell opens.
            return match self.opener.open(url).await {
                Ok(()) => ToolOutcome::success(
                    serde_json::json!({
                        "ok": true,
                        "opened": url,
                        "note": "showing in Third Eye's browser tab — focus_app the browser and screen_query/read_page it"
                    })
                    .to_string(),
                ),
                Err(e) => ToolOutcome::failure("open-failed", format!("could not open {url}: {e}")),
            };
        }
        if let Some(app) = args.app.as_deref().map(str::trim) {
            if let Err(refused) = self
                .approve(ActionKind::FocusApp, format!("Open app: {app}"))
                .await
            {
                return refused;
            }
            return match self.focus.focus(app).await {
                Ok(f) => ToolOutcome::success(
                    serde_json::json!({ "ok": true, "focused": f.app, "launched": f.launched, "frontWindow": f.front_window })
                        .to_string(),
                ),
                Err(e) => ToolOutcome::failure(e.kind(), e.to_string()),
            };
        }
        let path = PathBuf::from(args.path.as_deref().unwrap_or("").trim());
        if !path.is_absolute() {
            return ToolOutcome::failure(
                "invalid-arguments",
                "path must be absolute (e.g. /Users/you/Desktop/report.pdf)",
            );
        }
        if !path.exists() {
            return ToolOutcome::failure(
                "not-found",
                format!(
                    "{} does not exist — find_files or list_dir to locate it",
                    path.display()
                ),
            );
        }
        if let Err(refused) = self
            .approve(ActionKind::Open, format!("Open file: {}", path.display()))
            .await
        {
            return refused;
        }
        match self.paths.open_path(&path).await {
            Ok(()) => ToolOutcome::success(
                serde_json::json!({ "ok": true, "opened": path.display().to_string() }).to_string(),
            ),
            Err(e) => ToolOutcome::failure(
                "open-failed",
                format!("could not open {}: {e}", path.display()),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::appfocus::{AppFocusError, FocusedApp};

    struct QuietOpener(Mutex<Vec<String>>);
    #[async_trait]
    impl Opener for QuietOpener {
        async fn open(&self, url: &str) -> Result<(), String> {
            self.0.lock().unwrap().push(url.into());
            Ok(())
        }
    }
    struct QuietPaths(Mutex<Vec<PathBuf>>);
    #[async_trait]
    impl PathOpener for QuietPaths {
        async fn open_path(&self, path: &std::path::Path) -> Result<(), String> {
            self.0.lock().unwrap().push(path.to_path_buf());
            Ok(())
        }
    }
    struct AnyFocus;
    #[async_trait]
    impl AppFocus for AnyFocus {
        async fn focus(&self, app: &str) -> Result<FocusedApp, AppFocusError> {
            Ok(FocusedApp {
                app: app.into(),
                launched: false,
                visible_windows: Some(1),
                front_window: Some("w".into()),
            })
        }
        async fn running_apps(&self) -> Vec<String> {
            vec![]
        }
    }
    struct Scripted(ApprovalVerdict, Mutex<Vec<String>>);
    #[async_trait]
    impl ApprovalPrompt for Scripted {
        async fn request(&self, _k: ActionKind, s: String) -> ApprovalVerdict {
            self.1.lock().unwrap().push(s);
            self.0
        }
    }

    fn tool(
        mode: HidRunMode,
        verdict: ApprovalVerdict,
    ) -> (OpenTool, Arc<QuietOpener>, Arc<QuietPaths>, Arc<Scripted>) {
        let opener = Arc::new(QuietOpener(Mutex::new(vec![])));
        let paths = Arc::new(QuietPaths(Mutex::new(vec![])));
        let prompt = Arc::new(Scripted(verdict, Mutex::new(vec![])));
        let tool = OpenTool::new(
            opener.clone(),
            paths.clone(),
            Arc::new(AnyFocus),
            mode,
            Arc::new(Mutex::new(SessionWhitelist::new())),
            prompt.clone(),
        );
        (tool, opener, paths, prompt)
    }

    fn call(args: serde_json::Value) -> ToolCall {
        ToolCall {
            id: "c".into(),
            name: OPEN_TOOL.into(),
            arguments: args.to_string(),
        }
    }

    #[tokio::test]
    async fn exactly_one_target_is_required() {
        let (t, ..) = tool(HidRunMode::AutoRun, ApprovalVerdict::AllowOnce);
        for args in [
            serde_json::json!({}),
            serde_json::json!({"url":"https://a.example/","app":"Finder"}),
        ] {
            let out = t.execute(&call(args)).await;
            assert_eq!(out.failure.as_deref(), Some("invalid-arguments"));
        }
    }

    #[tokio::test]
    async fn urls_go_to_the_one_tab_opener_without_a_prompt() {
        let (t, opener, _, prompt) = tool(HidRunMode::Ask, ApprovalVerdict::Deny);
        let out = t
            .execute(&call(serde_json::json!({"url":"https://a.example/x"})))
            .await;
        assert!(out.ok, "{out:?}");
        assert_eq!(opener.0.lock().unwrap().as_slice(), ["https://a.example/x"]);
        assert!(
            prompt.1.lock().unwrap().is_empty(),
            "a page open never prompts"
        );
    }

    #[tokio::test]
    async fn paths_must_exist_and_ask_in_ask_mode() {
        let (t, _, paths, prompt) = tool(HidRunMode::Ask, ApprovalVerdict::AllowOnce);
        let missing = t
            .execute(&call(
                serde_json::json!({"path":"/definitely/not/here.txt"}),
            ))
            .await;
        assert_eq!(missing.failure.as_deref(), Some("not-found"));
        let relative = t
            .execute(&call(serde_json::json!({"path":"here.txt"})))
            .await;
        assert_eq!(relative.failure.as_deref(), Some("invalid-arguments"));
        let dir = std::env::temp_dir();
        let out = t
            .execute(&call(
                serde_json::json!({"path": dir.display().to_string()}),
            ))
            .await;
        assert!(out.ok, "{out:?}");
        assert_eq!(paths.0.lock().unwrap().len(), 1);
        assert!(prompt.1.lock().unwrap()[0].starts_with("Open file: "));
    }

    #[tokio::test]
    async fn off_refuses_paths_and_apps_typed_and_deny_is_typed() {
        let (t, _, paths, _) = tool(HidRunMode::Off, ApprovalVerdict::AllowOnce);
        let dir = std::env::temp_dir().display().to_string();
        assert_eq!(
            t.execute(&call(serde_json::json!({"path": dir})))
                .await
                .failure
                .as_deref(),
            Some("disabled")
        );
        assert_eq!(
            t.execute(&call(serde_json::json!({"app": "Finder"})))
                .await
                .failure
                .as_deref(),
            Some("disabled")
        );
        assert!(paths.0.lock().unwrap().is_empty());
        let (t, _, _, _) = tool(HidRunMode::Ask, ApprovalVerdict::Deny);
        let out = t.execute(&call(serde_json::json!({"app": "Finder"}))).await;
        assert_eq!(out.failure.as_deref(), Some("approval-denied"));
    }

    #[tokio::test]
    async fn app_opens_report_the_verified_front_window() {
        let (t, ..) = tool(HidRunMode::AutoRun, ApprovalVerdict::AllowOnce);
        let out = t.execute(&call(serde_json::json!({"app": "Finder"}))).await;
        assert!(out.ok);
        assert!(
            out.content.contains("\"frontWindow\":\"w\""),
            "{}",
            out.content
        );
    }
}
