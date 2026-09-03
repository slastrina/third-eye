//! Workspace exec (coding-agent S4): `run_in_workspace`, the coder's
//! build/test actuator.
//!
//! Same structural posture as the S3 file tools — inert without roots,
//! canonical containment on the working directory before anything spawns —
//! plus the exec-specific hardening the spec demands:
//!
//! - approval on its own `ActionKind::RunInWorkspace`, with SESSION grants
//!   scoped per workspace root (a grant for repo A never covers repo B);
//! - a wall-clock budget up to 10 minutes, default 2 (builds, not `date`);
//! - the whole PROCESS GROUP dies on timeout or user Stop/Esc — a stuck
//!   `cargo build` must not outlive the run it belongs to;
//! - stdout/stderr are streamed to a [`TerminalSink`] chunk-by-chunk as the
//!   command runs (the transcript's terminal block shows a live build), and
//!   capped/truncated in the final report exactly like `run_command`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde::Deserialize;
use tokio::io::AsyncReadExt;

use super::{WorkspaceError, WorkspaceState};
use crate::command_runner::truncate_stream;
use crate::input::commands::SessionWhitelist;
use crate::input::ActionKind;
use crate::llm::toolloop::{ApprovalPrompt, ApprovalVerdict, ToolExecutor, ToolOutcome};
use crate::llm::{ToolCall, ToolDefinition};

pub const RUN_IN_WORKSPACE_TOOL: &str = "run_in_workspace";

/// Wall-clock band: builds and test suites, not interactive daemons.
pub const DEFAULT_TIMEOUT_SECS: u64 = 120;
pub const MAX_TIMEOUT_SECS: u64 = 600;

/// How often the runner polls the Stop flag / deadline while streaming.
const WATCH_TICK_MS: u64 = 200;

/// Live output receiver: each chunk of the running command's stdout/stderr,
/// tagged with the call id so the transcript appends it to the right
/// terminal block. Production broadcasts a Tauri event; tests record.
pub trait TerminalSink: Send + Sync {
    fn chunk(&self, call_id: &str, text: &str);
}

/// No-op sink for contexts with no transcript (unit tests, headless).
pub struct NoopTerminalSink;

impl TerminalSink for NoopTerminalSink {
    fn chunk(&self, _call_id: &str, _text: &str) {}
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RunInWorkspaceArgs {
    command: String,
    /// Working directory — absolute inside a workspace, or relative to the
    /// first root. Defaults to the first workspace root.
    cwd: Option<String>,
    timeout_secs: Option<u64>,
}

/// Whether `command` is ONLY a `cd` — which cannot work: every command
/// runs in a fresh shell, so a bare `cd` changes nothing and the model
/// then reasons from a directory it never actually entered (the
/// Desktop-listing incident). Compound commands (`cd x && make`) are
/// legitimate and pass.
pub fn bare_cd(command: &str) -> bool {
    let trimmed = command.trim();
    (trimmed == "cd" || trimmed.starts_with("cd "))
        && !trimmed.contains("&&")
        && !trimmed.contains(';')
        && !trimmed.contains('|')
        && !trimmed.contains('\n')
}

/// The typed refusal both exec tools return for a bare `cd`.
pub const CD_DOES_NOT_PERSIST_KIND: &str = "cd-does-not-persist";

pub fn cd_refusal(active: Option<&std::path::Path>) -> ToolOutcome {
    let active = active
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "the active workspace".into());
    ToolOutcome::failure(
        CD_DOES_NOT_PERSIST_KIND,
        format!(
            "cd does NOT persist — every command runs in a FRESH shell starting in {active}. \
             To run something in a different folder INSIDE a workspace, pass it as the cwd \
             argument (or `cd there && command` in one call). Folders OUTSIDE the workspaces \
             are unreachable by design: say so and tell the user to add the folder in \
             Settings → Workspaces (or `thirdeye <path>` in a terminal). Never claim the \
             directory changed."
        ),
    )
}

/// Clamp a requested timeout into the exec band.
pub fn clamp_timeout(requested: Option<u64>) -> u64 {
    requested
        .unwrap_or(DEFAULT_TIMEOUT_SECS)
        .clamp(1, MAX_TIMEOUT_SECS)
}

pub struct RunInWorkspaceTool {
    workspace: Arc<WorkspaceState>,
    whitelist: Arc<Mutex<SessionWhitelist>>,
    approver: Arc<dyn ApprovalPrompt>,
    /// The run's cooperative Stop flag (Esc / Stop button) — polled while
    /// the child runs so a stop kills the build mid-flight, not at the next
    /// tool boundary.
    stop: Arc<AtomicBool>,
    sink: Arc<dyn TerminalSink>,
}

impl RunInWorkspaceTool {
    pub fn new(
        workspace: Arc<WorkspaceState>,
        whitelist: Arc<Mutex<SessionWhitelist>>,
        approver: Arc<dyn ApprovalPrompt>,
        stop: Arc<AtomicBool>,
        sink: Arc<dyn TerminalSink>,
    ) -> Self {
        Self {
            workspace,
            whitelist,
            approver,
            stop,
            sink,
        }
    }

    pub fn definition() -> ToolDefinition {
        ToolDefinition {
            name: RUN_IN_WORKSPACE_TOOL.into(),
            description: "Run one build/test/tooling command with its working directory locked \
                          inside a designated workspace folder (e.g. `cargo build`, `npm test`, \
                          `python3 main.py`). The user approves each command (or grants the \
                          workspace for the session). Output streams live and is truncated past \
                          16KB; long builds get up to 10 minutes via timeoutSecs. Use THIS — \
                          never run_command — for anything that compiles, tests, or runs \
                          workspace code."
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "The shell command line, e.g. \"cargo test\" or \"npm run build\"."
                    },
                    "cwd": {
                        "type": "string",
                        "description": "Working directory inside a workspace (optional; defaults to the first workspace root)."
                    },
                    "timeoutSecs": {
                        "type": "integer",
                        "description": "Wall-clock limit in seconds (default 120, max 600 — raise it for long builds)."
                    }
                },
                "required": ["command"]
            }),
        }
    }

    /// Spawn + stream + watch: the already-approved command, in its own
    /// process group, killed as a GROUP on timeout or Stop.
    async fn run(
        &self,
        call_id: &str,
        command: &str,
        cwd: &std::path::Path,
        timeout_secs: u64,
    ) -> ToolOutcome {
        let started = Instant::now();
        let deadline = started + Duration::from_secs(timeout_secs);
        let mut builder = tokio::process::Command::new("/bin/sh");
        builder
            .arg("-lc")
            .arg(command)
            .current_dir(cwd)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        // Own process group: the group id is the child's pid, so a stuck
        // build's whole tree (sh → cargo → rustc…) dies together.
        #[cfg(unix)]
        builder.process_group(0);
        let mut child = match builder.spawn() {
            Ok(child) => child,
            Err(e) => {
                return ToolOutcome::failure("spawn-failed", format!("could not run /bin/sh: {e}"))
            }
        };
        let pid = child.id();
        let mut stdout_pipe = child.stdout.take();
        let mut stderr_pipe = child.stderr.take();
        let mut stdout: Vec<u8> = Vec::new();
        let mut stderr: Vec<u8> = Vec::new();
        let mut out_buf = [0u8; 4096];
        let mut err_buf = [0u8; 4096];
        let mut tick = tokio::time::interval(Duration::from_millis(WATCH_TICK_MS));
        let mut exit_status: Option<std::process::ExitStatus> = None;
        let killed: Option<&'static str> = loop {
            tokio::select! {
                // Streams: forward each chunk live, accumulate for the report.
                read = read_some(&mut stdout_pipe, &mut out_buf) => {
                    match read {
                        Some(n) if n > 0 => {
                            self.sink.chunk(call_id, &String::from_utf8_lossy(&out_buf[..n]));
                            stdout.extend_from_slice(&out_buf[..n]);
                        }
                        _ => stdout_pipe = None,
                    }
                }
                read = read_some(&mut stderr_pipe, &mut err_buf) => {
                    match read {
                        Some(n) if n > 0 => {
                            self.sink.chunk(call_id, &String::from_utf8_lossy(&err_buf[..n]));
                            stderr.extend_from_slice(&err_buf[..n]);
                        }
                        _ => stderr_pipe = None,
                    }
                }
                status = child.wait(), if stdout_pipe.is_none() && stderr_pipe.is_none() => {
                    match status {
                        Ok(status) => { exit_status = Some(status); break None; }
                        Err(e) => {
                            return ToolOutcome::failure(
                                "spawn-failed",
                                format!("waiting for the command failed: {e}"),
                            );
                        }
                    }
                }
                _ = tick.tick() => {
                    if self.stop.load(Ordering::SeqCst) {
                        break Some("stopped");
                    }
                    if Instant::now() >= deadline {
                        break Some("timeout");
                    }
                }
            }
        };
        if let Some(kind) = killed {
            kill_process_group(pid);
            let _ = child.wait().await;
            let detail = match kind {
                "stopped" => format!("the user stopped the run; killed: {command}"),
                _ => {
                    format!("command exceeded its {timeout_secs}s limit and was killed: {command}")
                }
            };
            self.sink
                .chunk(call_id, &format!("\n[{kind} — process group killed]\n"));
            return ToolOutcome::failure(kind, detail);
        }
        let status = exit_status.expect("loop breaks None only after wait");
        let secs = started.elapsed().as_secs_f64();
        let code = status
            .code()
            .map(|c| c.to_string())
            .unwrap_or_else(|| "killed by signal".into());
        let stdout = truncate_stream(&stdout);
        let stderr = truncate_stream(&stderr);
        let mut report = format!("exit code: {code} (in {secs:.2}s, cwd {})\n", cwd.display());
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
        if status.success() {
            ToolOutcome::success(report)
        } else {
            ToolOutcome::failure("command-failed", report)
        }
    }
}

/// Read from an optional pipe; `None` pends forever (the select arm is
/// effectively disabled once a stream closes).
async fn read_some<R: tokio::io::AsyncRead + Unpin>(
    pipe: &mut Option<R>,
    buf: &mut [u8],
) -> Option<usize> {
    match pipe {
        Some(p) => p.read(buf).await.ok(),
        None => std::future::pending().await,
    }
}

/// Kill the child's whole process group (`kill -9 -- -pgid`). The group id
/// equals the child pid because it was spawned with `process_group(0)`.
/// `/bin/kill` instead of libc keeps this dependency-free.
fn kill_process_group(pid: Option<u32>) {
    let Some(pid) = pid else { return };
    let result = std::process::Command::new("/bin/kill")
        .args(["-9", "--", &format!("-{pid}")])
        .status();
    match result {
        Ok(status) if status.success() => log::info!("workspace: killed process group {pid}"),
        Ok(status) => log::warn!("workspace: kill -9 -{pid} exited {status}"),
        Err(e) => log::error!("workspace: kill -9 -{pid} failed: {e}"),
    }
}

fn workspace_failure(err: WorkspaceError) -> ToolOutcome {
    ToolOutcome::failure(err.kind(), err.to_string())
}

#[async_trait]
impl ToolExecutor for RunInWorkspaceTool {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![Self::definition()]
    }

    fn claims(&self, name: &str) -> bool {
        name == RUN_IN_WORKSPACE_TOOL
    }

    async fn execute(&self, call: &ToolCall) -> ToolOutcome {
        let args: RunInWorkspaceArgs = match serde_json::from_str(&call.arguments) {
            Ok(args) => args,
            Err(e) => {
                return ToolOutcome::failure(
                    "invalid-arguments",
                    format!("invalid {RUN_IN_WORKSPACE_TOOL} arguments: {e}"),
                )
            }
        };
        let command = args.command.trim().to_string();
        if command.is_empty() {
            return ToolOutcome::failure("invalid-arguments", "command must not be empty");
        }
        if bare_cd(&command) {
            return cd_refusal(self.workspace.roots().first().map(|p| p.as_path()));
        }
        // Resolve the cwd (2026-08-02 anywhere semantics): the active
        // working directory by default; any absolute path otherwise; with
        // neither, the chooser pauses to ask the user for a folder.
        let cwd_arg = args.cwd.unwrap_or_default();
        let candidate = if cwd_arg.trim().is_empty() {
            ".".to_string()
        } else {
            cwd_arg
        };
        let cwd = match self
            .workspace
            .resolve_or_ask(&candidate, &format!("run: {command}"))
            .await
        {
            Ok(cwd) => cwd,
            Err(e) => return workspace_failure(e),
        };
        if !cwd.is_dir() {
            return ToolOutcome::failure(
                "not-a-directory",
                format!("cwd {} is not an existing directory", cwd.display()),
            );
        }
        // Consent (not containment): tmp is always fine; a kind-wide grant
        // (persisted Always) or a session-blessed directory skips the
        // prompt; anything else asks, naming the exact directory.
        let kind_granted = self
            .whitelist
            .lock()
            .map(|w| w.contains(ActionKind::RunInWorkspace))
            .unwrap_or(false);
        // Dangerous verbs ALWAYS ask (review item 6): tmp, the kind grant and
        // a blessed directory are all ignored, and "always" is allow-once.
        let danger = crate::command_runner::dangerous_command(&command);
        let promptless = danger.is_none()
            && (WorkspaceState::is_tmp(&cwd) || kind_granted || self.workspace.dir_granted(&cwd));
        if !promptless {
            let summary = match &danger {
                Some(reason) => format!("⚠ {reason} — in {}: {command}", cwd.display()),
                None => format!("Run in {}: {command}", cwd.display()),
            };
            match self
                .approver
                .request(ActionKind::RunInWorkspace, summary)
                .await
            {
                ApprovalVerdict::AllowOnce => {}
                ApprovalVerdict::AllowKind | ApprovalVerdict::AllowAlways => {
                    // Session grant for THIS directory (and its subtree) —
                    // never earned by a dangerous command.
                    if danger.is_none() {
                        self.workspace.grant_dir(cwd.clone());
                    }
                }
                ApprovalVerdict::Deny => {
                    return ToolOutcome::failure(
                        "approval-denied",
                        format!("the user declined to run in {}: {command}", cwd.display()),
                    );
                }
            }
        }
        self.run(&call.id, &command, &cwd, clamp_timeout(args.timeout_secs))
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    struct ScriptedPrompt(ApprovalVerdict, std::sync::atomic::AtomicUsize);

    impl ScriptedPrompt {
        fn new(verdict: ApprovalVerdict) -> Self {
            Self(verdict, std::sync::atomic::AtomicUsize::new(0))
        }
    }

    #[async_trait]
    impl ApprovalPrompt for ScriptedPrompt {
        async fn request(&self, _kind: ActionKind, _summary: String) -> ApprovalVerdict {
            self.1.fetch_add(1, Ordering::SeqCst);
            self.0
        }
    }

    struct RecordingSink(Mutex<String>);

    impl TerminalSink for RecordingSink {
        fn chunk(&self, _call_id: &str, text: &str) {
            self.0.lock().unwrap().push_str(text);
        }
    }

    fn scratch_ws(tag: &str) -> (Arc<WorkspaceState>, PathBuf) {
        let dir = std::env::temp_dir().join(format!("te-exec-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let ws = Arc::new(WorkspaceState::new());
        ws.set_roots(vec![dir.display().to_string()]);
        (ws, dir)
    }

    fn tool(
        ws: Arc<WorkspaceState>,
        verdict: ApprovalVerdict,
    ) -> (RunInWorkspaceTool, Arc<ScriptedPrompt>, Arc<RecordingSink>) {
        let prompt = Arc::new(ScriptedPrompt::new(verdict));
        let sink = Arc::new(RecordingSink(Mutex::new(String::new())));
        let tool = RunInWorkspaceTool::new(
            ws,
            Arc::new(Mutex::new(SessionWhitelist::new())),
            prompt.clone(),
            Arc::new(AtomicBool::new(false)),
            sink.clone(),
        );
        (tool, prompt, sink)
    }

    /// Writable NON-tmp scratch (tmp bypasses approval by design).
    fn non_tmp_scratch(tag: &str) -> PathBuf {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!("te-exec-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn tool_with(
        ws: Arc<WorkspaceState>,
        verdict: ApprovalVerdict,
    ) -> (RunInWorkspaceTool, Arc<ScriptedPrompt>, Arc<RecordingSink>) {
        let prompt = Arc::new(ScriptedPrompt::new(verdict));
        let sink = Arc::new(RecordingSink(Mutex::new(String::new())));
        let tool = RunInWorkspaceTool::new(
            ws,
            Arc::new(Mutex::new(SessionWhitelist::new())),
            prompt.clone(),
            Arc::new(AtomicBool::new(false)),
            sink.clone(),
        );
        (tool, prompt, sink)
    }

    fn call(args: serde_json::Value) -> ToolCall {
        ToolCall {
            id: "c1".into(),
            name: RUN_IN_WORKSPACE_TOOL.into(),
            arguments: args.to_string(),
        }
    }

    #[tokio::test]
    async fn always_offered_and_no_working_dir_refuses_typed() {
        let ws = Arc::new(WorkspaceState::new());
        let (tool, _, _) = tool(ws, ApprovalVerdict::AllowOnce);
        assert_eq!(tool.definitions().len(), 1, "offered without roots");
        let outcome = tool
            .execute(&call(serde_json::json!({"command": "true"})))
            .await;
        assert_eq!(outcome.failure.as_deref(), Some("no-working-directory"));
    }

    #[tokio::test]
    async fn tmp_cwd_never_prompts_and_elsewhere_asks_with_the_directory() {
        struct PanicPrompt;
        #[async_trait::async_trait]
        impl ApprovalPrompt for PanicPrompt {
            async fn request(&self, _k: ActionKind, _s: String) -> ApprovalVerdict {
                panic!("tmp commands must never prompt");
            }
        }
        // tmp cwd (scratch_ws lives in temp_dir): runs with no prompt.
        let (ws, dir) = scratch_ws("tmpfree");
        let sink = Arc::new(RecordingSink(Mutex::new(String::new())));
        let tool = RunInWorkspaceTool::new(
            ws,
            Arc::new(Mutex::new(SessionWhitelist::new())),
            Arc::new(PanicPrompt),
            Arc::new(AtomicBool::new(false)),
            sink,
        );
        let outcome = tool
            .execute(&call(serde_json::json!({"command": "echo tmp-free"})))
            .await;
        assert!(outcome.ok, "{outcome:?}");
        let _ = std::fs::remove_dir_all(&dir);
        // A non-tmp cwd prompts, naming the exact directory.
        let non_tmp = non_tmp_scratch("ask");
        let ws = Arc::new(WorkspaceState::new());
        ws.set_roots(vec![non_tmp.display().to_string()]);
        let (tool, prompt, _) = tool_with(ws, ApprovalVerdict::AllowOnce);
        let outcome = tool
            .execute(&call(serde_json::json!({"command": "true"})))
            .await;
        assert!(outcome.ok, "{outcome:?}");
        assert_eq!(prompt.1.load(Ordering::SeqCst), 1, "non-tmp prompts");
        let _ = std::fs::remove_dir_all(&non_tmp);
    }

    /// Review item 6: even in tmp, with the kind granted, a dangerous verb
    /// asks — and its "always" answer blesses nothing.
    #[tokio::test]
    async fn dangerous_verbs_ask_even_in_tmp_with_the_kind_granted() {
        let (ws, dir) = scratch_ws("danger");
        let whitelist = Arc::new(Mutex::new(SessionWhitelist::new()));
        whitelist.lock().unwrap().allow(ActionKind::RunInWorkspace);
        let prompt = Arc::new(ScriptedPrompt::new(ApprovalVerdict::AllowAlways));
        let sink = Arc::new(RecordingSink(Mutex::new(String::new())));
        let tool = RunInWorkspaceTool::new(
            ws.clone(),
            whitelist,
            prompt.clone(),
            Arc::new(AtomicBool::new(false)),
            sink,
        );
        let outcome = tool
            .execute(&call(serde_json::json!({"command": "kill -0 $$"})))
            .await;
        assert!(outcome.ok, "{outcome:?}");
        assert_eq!(
            prompt.1.load(Ordering::SeqCst),
            1,
            "tmp + kind grant still asks"
        );
        assert!(
            !ws.dir_granted(&dir),
            "always on a dangerous command blesses nothing"
        );
        let _ = tool
            .execute(&call(serde_json::json!({"command": "kill -0 $$"})))
            .await;
        assert_eq!(prompt.1.load(Ordering::SeqCst), 2, "asks every time");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn deny_never_executes() {
        let non_tmp = non_tmp_scratch("deny");
        let ws = Arc::new(WorkspaceState::new());
        ws.set_roots(vec![non_tmp.display().to_string()]);
        let (tool, _, _) = tool_with(ws, ApprovalVerdict::Deny);
        let outcome = tool
            .execute(&call(serde_json::json!({"command": "touch never.txt"})))
            .await;
        assert_eq!(outcome.failure.as_deref(), Some("approval-denied"));
        assert!(!non_tmp.join("never.txt").exists());
        let _ = std::fs::remove_dir_all(&non_tmp);
    }

    #[tokio::test]
    async fn runs_in_the_workspace_and_streams_output() {
        let (ws, dir) = scratch_ws("run");
        let (tool, _, sink) = tool(ws, ApprovalVerdict::AllowOnce);
        let outcome = tool
            .execute(&call(
                serde_json::json!({"command": "pwd; echo streamed-marker"}),
            ))
            .await;
        assert!(outcome.ok, "{outcome:?}");
        assert!(outcome.content.contains("exit code: 0"));
        // cwd really is the workspace root (canonical form).
        assert!(
            outcome
                .content
                .contains(dir.canonicalize().unwrap().to_str().unwrap()),
            "{}",
            outcome.content
        );
        // The sink saw the output live, chunk-by-chunk.
        assert!(sink.0.lock().unwrap().contains("streamed-marker"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn session_grant_blesses_the_directory_not_the_kind() {
        let dir_a = non_tmp_scratch("ga");
        let dir_b = non_tmp_scratch("gb");
        let ws = Arc::new(WorkspaceState::new());
        ws.set_roots(vec![dir_a.display().to_string()]);
        let (tool, prompt, _) = tool_with(ws, ApprovalVerdict::AllowKind);
        // First run in A prompts and blesses A; the second is silent.
        for _ in 0..2 {
            let outcome = tool
                .execute(&call(serde_json::json!({"command": "true"})))
                .await;
            assert!(outcome.ok, "{outcome:?}");
        }
        assert_eq!(prompt.1.load(Ordering::SeqCst), 1, "directory blessed");
        // Directory B prompts AGAIN — the blessing never crossed dirs.
        let outcome = tool
            .execute(&call(serde_json::json!({
                "command": "true",
                "cwd": dir_b.display().to_string(),
            })))
            .await;
        assert!(outcome.ok);
        assert_eq!(prompt.1.load(Ordering::SeqCst), 2, "dir B re-prompts");
        let _ = std::fs::remove_dir_all(&dir_a);
        let _ = std::fs::remove_dir_all(&dir_b);
    }

    #[tokio::test]
    async fn timeout_kills_the_process_group_fast_and_types_the_failure() {
        let (ws, dir) = scratch_ws("timeout");
        let (tool, _, sink) = tool(ws, ApprovalVerdict::AllowOnce);
        let started = Instant::now();
        let outcome = tool
            .execute(&call(serde_json::json!({
                "command": "sleep 30 & sleep 30",
                "timeoutSecs": 1,
            })))
            .await;
        assert_eq!(outcome.failure.as_deref(), Some("timeout"), "{outcome:?}");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "kill must not wait for the sleeps"
        );
        assert!(sink.0.lock().unwrap().contains("process group killed"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn stop_flag_kills_mid_run_with_the_stopped_kind() {
        let (ws, dir) = scratch_ws("stop");
        let stop = Arc::new(AtomicBool::new(false));
        let tool = RunInWorkspaceTool::new(
            ws,
            Arc::new(Mutex::new(SessionWhitelist::new())),
            Arc::new(ScriptedPrompt::new(ApprovalVerdict::AllowOnce)),
            stop.clone(),
            Arc::new(NoopTerminalSink),
        );
        let flip = stop.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(400)).await;
            flip.store(true, Ordering::SeqCst);
        });
        let started = Instant::now();
        let outcome = tool
            .execute(&call(serde_json::json!({"command": "sleep 30"})))
            .await;
        assert_eq!(outcome.failure.as_deref(), Some("stopped"), "{outcome:?}");
        assert!(started.elapsed() < Duration::from_secs(5));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn nonzero_exit_types_command_failed_with_stderr() {
        let (ws, dir) = scratch_ws("fail");
        let (tool, _, _) = tool(ws, ApprovalVerdict::AllowOnce);
        let outcome = tool
            .execute(&call(
                serde_json::json!({"command": "echo oops >&2; exit 3"}),
            ))
            .await;
        assert_eq!(outcome.failure.as_deref(), Some("command-failed"));
        assert!(outcome.content.contains("oops"));
        assert!(outcome.content.contains("exit code: 3"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn bare_cd_refuses_typed_and_compound_cd_passes() {
        assert!(bare_cd("cd /tmp"));
        assert!(bare_cd("  cd .."));
        assert!(bare_cd("cd"));
        assert!(!bare_cd("cd sub && cargo test"));
        assert!(!bare_cd("cd x; make"));
        assert!(!bare_cd("echo cd"));
        let (ws, dir) = scratch_ws("cd");
        let (tool, prompt, _) = tool(ws, ApprovalVerdict::AllowOnce);
        let outcome = tool
            .execute(&call(serde_json::json!({"command": "cd /tmp"})))
            .await;
        assert_eq!(outcome.failure.as_deref(), Some(CD_DOES_NOT_PERSIST_KIND));
        assert!(outcome.content.contains("FRESH"), "{}", outcome.content);
        assert_eq!(
            prompt.1.load(Ordering::SeqCst),
            0,
            "refused before approval"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn timeout_clamps_into_the_exec_band() {
        assert_eq!(clamp_timeout(None), DEFAULT_TIMEOUT_SECS);
        assert_eq!(clamp_timeout(Some(0)), 1);
        assert_eq!(clamp_timeout(Some(4000)), MAX_TIMEOUT_SECS);
        assert_eq!(clamp_timeout(Some(300)), 300);
    }

    #[test]
    fn tool_name_matches_the_toolloop_preview_literal() {
        // toolloop.rs's result_preview compares against the literal
        // "run_in_workspace" (this module is cfg(desktop), llm is not).
        assert_eq!(RUN_IN_WORKSPACE_TOOL, "run_in_workspace");
    }
}
