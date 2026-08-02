//! Terminal command execution (computer-control I2): the `run_command` LLM
//! tool. The most powerful actuator in the app, so its posture is layered:
//!
//! 1. **Structural gate** — the persisted `commandsEnabled` setting,
//!    default OFF. Disabled ⇒ the tool refuses with the typed `disabled`
//!    kind before anything else runs (D038's structural-inertness posture).
//! 2. **Per-command approval** — every call flows through the SAME
//!    prompt/whitelist plumbing as HID actions (`ActionKind::RunCommand`):
//!    the overlay shows the EXACT command line and the user answers
//!    allow-once / always-this-session / deny. There is no auto-run mode
//!    for commands; only an explicit session grant skips the prompt.
//! 3. **Bounded execution** — `/bin/sh -lc`, cwd = home, hard timeout
//!    (15 s default, 60 s cap), stdout/stderr captured and truncated with
//!    the truncation marked. No stdin, no PTY.
//!
//! Visibility is structural: the tool's call and result ride the existing
//! `llm://tool-call` / `llm://tool-result` broadcasts, so the chat
//! transcript and the HUD trail show every command and outcome — nothing
//! executes silently.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::input::commands::SessionWhitelist;
use crate::input::ActionKind;
use crate::llm::toolloop::{ApprovalPrompt, ApprovalVerdict, ToolExecutor, ToolOutcome};
use crate::llm::{ToolCall, ToolDefinition};

/// Name of the terminal tool the model calls.
pub const RUN_COMMAND_TOOL: &str = "run_command";

/// Default / maximum wall-clock a command may run.
pub const DEFAULT_TIMEOUT_SECS: u64 = 15;
pub const MAX_TIMEOUT_SECS: u64 = 60;

/// Per-stream capture cap; beyond it the output is cut and marked.
pub const MAX_STREAM_BYTES: usize = 16 * 1024;

/// App-shared commands gate: the live `commandsEnabled` value. Managed once;
/// the Settings IPC flips it (persist + rollback) and every chat run's tool
/// reads it at execute time — a mid-run disable stops the NEXT command.
pub struct CommandState {
    enabled: AtomicBool,
    /// User-defined persistent allowlist: commands matching an entry
    /// (exact, or entry + a space-separated tail) run without a prompt
    /// while the gate is enabled. Settings-editable, persisted.
    allowlist: Mutex<Vec<String>>,
}

impl CommandState {
    pub fn new() -> Self {
        Self {
            enabled: AtomicBool::new(false),
            allowlist: Mutex::new(Vec::new()),
        }
    }

    pub fn enabled(&self) -> bool {
        self.enabled.load(Ordering::SeqCst)
    }

    pub fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::SeqCst);
    }

    pub fn allowlist(&self) -> Vec<String> {
        self.allowlist.lock().map(|l| l.clone()).unwrap_or_default()
    }

    pub fn set_allowlist(&self, entries: Vec<String>) {
        if let Ok(mut list) = self.allowlist.lock() {
            *list = entries;
        }
    }

    /// Whether `command` is covered by the persistent allowlist.
    pub fn is_allowlisted(&self, command: &str) -> bool {
        self.allowlist
            .lock()
            .map(|list| command_allowlisted(&list, command))
            .unwrap_or(false)
    }
}

/// Pure matching contract: an entry covers the EXACT command, or the
/// command starting with the entry followed by a space (token boundary —
/// "ls" covers "ls -la" but never "lsof"). User-defined entries are the
/// user's own risk assessment; the boundary just prevents accidents.
pub fn command_allowlisted(allowlist: &[String], command: &str) -> bool {
    let command = command.trim();
    allowlist.iter().any(|entry| {
        command == entry
            || (command.len() > entry.len()
                && command.starts_with(entry.as_str())
                && command.as_bytes()[entry.len()] == b' ')
    })
}

impl Default for CommandState {
    fn default() -> Self {
        Self::new()
    }
}

/// Commands-gate snapshot (health-as-value): the effective toggle plus any
/// persist failure as data (watcher/chat-memory contract).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandsStatus {
    pub enabled: bool,
    /// The persistent user-defined allowlist (Settings-editable).
    pub allowlist: Vec<String>,
    pub error: Option<String>,
}

/// Restore the persisted toggle at setup, before any chat can run.
pub fn apply_persisted(app: &tauri::AppHandle) {
    use tauri::Manager;
    let enabled = crate::config::load_commands_enabled(app).unwrap_or(false);
    let allowlist = crate::config::load_command_allowlist(app);
    let state = app.state::<Arc<CommandState>>();
    state.set_enabled(enabled);
    log::info!(
        "commands: enabled={enabled}, allowlist={} entries (persisted)",
        allowlist.len()
    );
    state.set_allowlist(allowlist);
}

#[tauri::command]
pub fn commands_status(state: tauri::State<'_, Arc<CommandState>>) -> CommandsStatus {
    CommandsStatus {
        enabled: state.enabled(),
        allowlist: state.allowlist(),
        error: None,
    }
}

/// Replace the persistent command allowlist (Settings editor). Sanitized
/// server-side; a persist failure rolls back and returns as data.
#[tauri::command]
pub fn set_commands_allowlist(
    app: tauri::AppHandle,
    state: tauri::State<'_, Arc<CommandState>>,
    entries: Vec<String>,
) -> CommandsStatus {
    let sanitized = crate::config::sanitize_command_allowlist(&serde_json::json!(entries));
    let previous = state.allowlist();
    state.set_allowlist(sanitized.clone());
    match crate::config::save_command_allowlist(&app, &sanitized) {
        Ok(()) => {
            log::info!("commands: allowlist set ({} entries)", sanitized.len());
            CommandsStatus {
                enabled: state.enabled(),
                allowlist: sanitized,
                error: None,
            }
        }
        Err(e) => {
            state.set_allowlist(previous.clone());
            log::error!("commands: {e}");
            CommandsStatus {
                enabled: state.enabled(),
                allowlist: previous,
                error: Some(e),
            }
        }
    }
}

/// Flip the commands gate. Never rejects: a persist failure rolls the
/// in-memory value back and returns as data (an unpersisted flip must never
/// silently revert on restart).
#[tauri::command]
pub fn set_commands_enabled(
    app: tauri::AppHandle,
    state: tauri::State<'_, Arc<CommandState>>,
    enable: bool,
) -> CommandsStatus {
    let previous = state.enabled();
    state.set_enabled(enable);
    match crate::config::save_commands_enabled(&app, enable) {
        Ok(()) => {
            log::info!("commands: enabled={enable} via=ipc");
            CommandsStatus {
                enabled: enable,
                allowlist: state.allowlist(),
                error: None,
            }
        }
        Err(e) => {
            state.set_enabled(previous);
            log::error!("commands: {e}");
            CommandsStatus {
                enabled: previous,
                allowlist: state.allowlist(),
                error: Some(e),
            }
        }
    }
}

/// Truncate one captured stream to [`MAX_STREAM_BYTES`], marking the cut —
/// silent truncation would let the model believe it saw everything.
pub fn truncate_stream(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    if text.len() <= MAX_STREAM_BYTES {
        return text.into_owned();
    }
    // Cut on a char boundary at or below the cap.
    let mut cut = MAX_STREAM_BYTES;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    format!(
        "{}\n… [truncated: {} of {} bytes shown]",
        &text[..cut],
        cut,
        text.len()
    )
}

/// Clamp a requested timeout into the allowed band.
pub fn clamp_timeout(requested: Option<u64>) -> u64 {
    requested
        .unwrap_or(DEFAULT_TIMEOUT_SECS)
        .clamp(1, MAX_TIMEOUT_SECS)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RunCommandArgs {
    command: String,
    timeout_secs: Option<u64>,
}

/// The gated terminal tool. Holds the structural gate, the session
/// whitelist, and the approval prompt seam — all injected so tests script
/// every path without a Tauri runtime.
pub struct RunCommandTool {
    state: Arc<CommandState>,
    whitelist: Arc<Mutex<SessionWhitelist>>,
    approver: Arc<dyn ApprovalPrompt>,
}

impl RunCommandTool {
    pub fn new(
        state: Arc<CommandState>,
        whitelist: Arc<Mutex<SessionWhitelist>>,
        approver: Arc<dyn ApprovalPrompt>,
    ) -> Self {
        Self {
            state,
            whitelist,
            approver,
        }
    }

    pub fn definition() -> ToolDefinition {
        ToolDefinition {
            name: RUN_COMMAND_TOOL.into(),
            description: "Run one shell command on this machine (/bin/sh -lc, home directory, \
                          bounded timeout) and get its exit code and output. The user sees and \
                          approves every command before it runs. PREFER this over screen-driving \
                          for simple machine facts — e.g. `date` (what time is it), \
                          `curl -s ifconfig.me` (public IP), `hostname`, `df -h` (disk), \
                          `pmset -g batt` (battery). Use find_programs first when unsure a CLI \
                          tool exists. Keep commands short and read-only unless the user asked \
                          for a change; output is truncated past 16KB."
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "The shell command line to execute, e.g. \"date\" or \"curl -s ifconfig.me\"."
                    },
                    "timeoutSecs": {
                        "type": "integer",
                        "description": "Optional wall-clock limit in seconds (default 15, max 60)."
                    }
                },
                "required": ["command"]
            }),
        }
    }

    /// Execute the (already approved) command. Separate so tests exercise
    /// the runner without the gate.
    async fn run(command: &str, timeout_secs: u64) -> ToolOutcome {
        let started = std::time::Instant::now();
        let mut builder = tokio::process::Command::new("/bin/sh");
        builder
            .arg("-lc")
            .arg(command)
            .stdin(std::process::Stdio::null());
        if let Some(home) = std::env::var_os("HOME") {
            builder.current_dir(home);
        }
        builder.kill_on_drop(true);
        let child = builder.output();
        match tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), child).await {
            Ok(Ok(output)) => {
                let secs = started.elapsed().as_secs_f64();
                let code = output
                    .status
                    .code()
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "killed by signal".into());
                let stdout = truncate_stream(&output.stdout);
                let stderr = truncate_stream(&output.stderr);
                let mut report = format!("exit code: {code} (in {secs:.2}s)\n");
                report.push_str(&format!(
                    "stdout:\n{}\n",
                    if stdout.is_empty() {
                        "(empty)"
                    } else {
                        &stdout
                    }
                ));
                if !stderr.is_empty() {
                    report.push_str(&format!("stderr:\n{stderr}\n"));
                }
                if output.status.success() {
                    ToolOutcome::success(report)
                } else {
                    // Non-zero exit is a tool-level failure the model should
                    // see typed, with the full report as the detail.
                    ToolOutcome::failure("command-failed", report)
                }
            }
            Ok(Err(e)) => {
                ToolOutcome::failure("spawn-failed", format!("could not run /bin/sh: {e}"))
            }
            Err(_) => ToolOutcome::failure(
                "timeout",
                format!("command exceeded its {timeout_secs}s limit and was killed: {command}"),
            ),
        }
    }
}

#[async_trait::async_trait]
impl ToolExecutor for RunCommandTool {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![Self::definition()]
    }

    async fn execute(&self, call: &ToolCall) -> ToolOutcome {
        if call.name != RUN_COMMAND_TOOL {
            return ToolOutcome::failure(
                "unknown-tool",
                format!(
                    "unknown tool: {} (available: {RUN_COMMAND_TOOL})",
                    call.name
                ),
            );
        }
        let args: RunCommandArgs = match serde_json::from_str(&call.arguments) {
            Ok(args) => args,
            Err(e) => {
                return ToolOutcome::failure(
                    "invalid-arguments",
                    format!("invalid {RUN_COMMAND_TOOL} arguments: {e}"),
                )
            }
        };
        let command = args.command.trim();
        if command.is_empty() {
            return ToolOutcome::failure("invalid-arguments", "command must not be empty");
        }
        // A bare `cd` cannot work (fresh shell per command) — refuse typed
        // so the model never builds on a directory change that never
        // happened (workspace::exec_tool has the shared rationale).
        #[cfg(desktop)]
        if crate::workspace::exec_tool::bare_cd(command) {
            return crate::workspace::exec_tool::cd_refusal(None);
        }
        // 1. Structural gate (D038 posture): disabled means inert, typed.
        if !self.state.enabled() {
            return ToolOutcome::failure(
                "disabled",
                "terminal commands are disabled — the user can enable them in \
                 Settings → Automation → Terminal commands",
            );
        }
        // 2a. Persistent user allowlist: a Settings-defined entry covering
        //     this exact command (or its token-prefix) runs without a
        //     prompt — logged, and still fully visible in chat + HUD.
        if self.state.is_allowlisted(command) {
            log::info!("commands: allowlisted, running without prompt: {command}");
            return Self::run(command, clamp_timeout(args.timeout_secs)).await;
        }
        // 2b. Approval: an explicit session grant skips the prompt; otherwise
        //    the user sees the exact command line. No auto-run for commands.
        let granted = self
            .whitelist
            .lock()
            .map(|w| w.contains(ActionKind::RunCommand))
            .unwrap_or(false);
        if !granted {
            let verdict = self
                .approver
                .request(ActionKind::RunCommand, format!("Run command: {command}"))
                .await;
            match verdict {
                ApprovalVerdict::AllowOnce => {}
                // AllowAlways is downgraded by the production prompt;
                // treat a raw one like the session grant defensively.
                ApprovalVerdict::AllowKind | ApprovalVerdict::AllowAlways => {
                    if let Ok(mut whitelist) = self.whitelist.lock() {
                        whitelist.allow(ActionKind::RunCommand);
                    }
                }
                ApprovalVerdict::Deny => {
                    return ToolOutcome::failure(
                        "approval-denied",
                        format!("the user declined to run: {command}"),
                    );
                }
            }
        }
        // 3. Bounded execution.
        Self::run(command, clamp_timeout(args.timeout_secs)).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;

    struct ScriptedPrompt(ApprovalVerdict);

    #[async_trait]
    impl ApprovalPrompt for ScriptedPrompt {
        async fn request(&self, _kind: ActionKind, _summary: String) -> ApprovalVerdict {
            self.0
        }
    }

    fn tool(enabled: bool, verdict: ApprovalVerdict) -> RunCommandTool {
        let state = Arc::new(CommandState::new());
        state.set_enabled(enabled);
        RunCommandTool::new(
            state,
            Arc::new(Mutex::new(SessionWhitelist::new())),
            Arc::new(ScriptedPrompt(verdict)),
        )
    }

    fn call(args: serde_json::Value) -> ToolCall {
        ToolCall {
            id: "c1".into(),
            name: RUN_COMMAND_TOOL.into(),
            arguments: args.to_string(),
        }
    }

    #[tokio::test]
    async fn disabled_refuses_typed_before_anything_runs() {
        let outcome = tool(false, ApprovalVerdict::AllowOnce)
            .execute(&call(serde_json::json!({"command": "echo hi"})))
            .await;
        assert!(!outcome.ok);
        assert_eq!(outcome.failure.as_deref(), Some("disabled"));
    }

    #[tokio::test]
    async fn deny_verdict_never_executes() {
        let outcome = tool(true, ApprovalVerdict::Deny)
            .execute(&call(serde_json::json!({"command": "echo hi"})))
            .await;
        assert!(!outcome.ok);
        assert_eq!(outcome.failure.as_deref(), Some("approval-denied"));
    }

    #[tokio::test]
    async fn approved_command_runs_and_reports_exit_and_output() {
        let outcome = tool(true, ApprovalVerdict::AllowOnce)
            .execute(&call(
                serde_json::json!({"command": "echo hello-third-eye"}),
            ))
            .await;
        assert!(outcome.ok, "{:?}", outcome);
        assert!(outcome.content.contains("exit code: 0"));
        assert!(outcome.content.contains("hello-third-eye"));
    }

    #[tokio::test]
    async fn allow_kind_grants_the_session_so_the_next_call_skips_the_prompt() {
        struct CountingPrompt(std::sync::atomic::AtomicUsize);
        #[async_trait]
        impl ApprovalPrompt for CountingPrompt {
            async fn request(&self, _k: ActionKind, _s: String) -> ApprovalVerdict {
                self.0.fetch_add(1, Ordering::SeqCst);
                ApprovalVerdict::AllowKind
            }
        }
        let state = Arc::new(CommandState::new());
        state.set_enabled(true);
        let prompt = Arc::new(CountingPrompt(std::sync::atomic::AtomicUsize::new(0)));
        let tool = RunCommandTool::new(
            state,
            Arc::new(Mutex::new(SessionWhitelist::new())),
            prompt.clone(),
        );
        for _ in 0..2 {
            let outcome = tool
                .execute(&call(serde_json::json!({"command": "true"})))
                .await;
            assert!(outcome.ok, "{outcome:?}");
        }
        assert_eq!(
            prompt.0.load(Ordering::SeqCst),
            1,
            "second call must skip the prompt"
        );
    }

    #[tokio::test]
    async fn nonzero_exit_is_typed_command_failed_with_stderr() {
        let outcome = tool(true, ApprovalVerdict::AllowOnce)
            .execute(&call(
                serde_json::json!({"command": "echo oops >&2; exit 3"}),
            ))
            .await;
        assert!(!outcome.ok);
        assert_eq!(outcome.failure.as_deref(), Some("command-failed"));
        assert!(outcome.content.contains("oops"));
    }

    #[tokio::test]
    async fn timeout_kills_and_reports_typed() {
        let outcome = tool(true, ApprovalVerdict::AllowOnce)
            .execute(&call(
                serde_json::json!({"command": "sleep 30", "timeoutSecs": 1}),
            ))
            .await;
        assert!(!outcome.ok);
        assert_eq!(outcome.failure.as_deref(), Some("timeout"));
    }

    #[test]
    fn truncation_is_marked_never_silent() {
        let big = vec![b'x'; MAX_STREAM_BYTES + 100];
        let cut = truncate_stream(&big);
        assert!(cut.contains("[truncated:"));
        let small = truncate_stream(b"tiny");
        assert_eq!(small, "tiny");
    }

    #[test]
    fn allowlist_matching_is_exact_or_token_prefix() {
        let list = vec!["ls".to_string(), "curl -s ifconfig.me".to_string()];
        assert!(command_allowlisted(&list, "ls"));
        assert!(command_allowlisted(&list, "ls -la"));
        assert!(command_allowlisted(&list, "  curl -s ifconfig.me  "));
        // Token boundary: never a bare string prefix.
        assert!(!command_allowlisted(&list, "lsof"));
        assert!(!command_allowlisted(&list, "curl -s ifconfig.methis"));
        assert!(!command_allowlisted(&[], "ls"));
    }

    #[tokio::test]
    async fn allowlisted_command_runs_without_any_prompt() {
        struct PanicPrompt;
        #[async_trait]
        impl ApprovalPrompt for PanicPrompt {
            async fn request(&self, _k: ActionKind, _s: String) -> ApprovalVerdict {
                panic!("allowlisted command must never prompt");
            }
        }
        let state = Arc::new(CommandState::new());
        state.set_enabled(true);
        state.set_allowlist(vec!["echo".into()]);
        let tool = RunCommandTool::new(
            state.clone(),
            Arc::new(Mutex::new(SessionWhitelist::new())),
            Arc::new(PanicPrompt),
        );
        let outcome = tool
            .execute(&call(serde_json::json!({"command": "echo allowlisted"})))
            .await;
        assert!(outcome.ok, "{outcome:?}");
        assert!(outcome.content.contains("allowlisted"));
        // Disabled still wins over the allowlist (structural gate first).
        state.set_enabled(false);
        let refused = tool
            .execute(&call(serde_json::json!({"command": "echo allowlisted"})))
            .await;
        assert_eq!(refused.failure.as_deref(), Some("disabled"));
    }

    #[test]
    fn tool_name_matches_the_toolloop_preview_literal() {
        // toolloop.rs's result_preview compares against the literal
        // "run_command" (this module is cfg(desktop), the llm module is not).
        assert_eq!(RUN_COMMAND_TOOL, "run_command");
    }

    #[test]
    fn timeout_clamps_into_the_band() {
        assert_eq!(clamp_timeout(None), DEFAULT_TIMEOUT_SECS);
        assert_eq!(clamp_timeout(Some(0)), 1);
        assert_eq!(clamp_timeout(Some(300)), MAX_TIMEOUT_SECS);
        assert_eq!(clamp_timeout(Some(30)), 30);
    }
}
