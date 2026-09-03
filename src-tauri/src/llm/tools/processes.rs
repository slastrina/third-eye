//! `processes` (S5): what is running (pid, cpu, memory) and a way to end
//! one — structured, instead of the model composing ps/pkill. `kill`
//! ALWAYS asks, in every run mode (dangerous-verb parity), and "always"
//! never sticks; SIGTERM by default, `force` for SIGKILL.

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::input::ActionKind;
use crate::llm::toolloop::{ApprovalPrompt, ApprovalVerdict, ToolExecutor, ToolOutcome};
use crate::llm::{ToolCall, ToolDefinition};

pub const PROCESSES_TOOL: &str = "processes";
pub const DEFAULT_LIMIT: usize = 15;
pub const MAX_LIMIT: usize = 50;

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcInfo {
    pub pid: i32,
    pub name: String,
    pub cpu_percent: f32,
    pub memory_mb: f32,
    pub user: String,
}

#[async_trait]
pub trait ProcessBackend: Send + Sync {
    async fn list(&self) -> Result<Vec<ProcInfo>, String>;
    async fn kill(&self, pid: i32, force: bool) -> Result<(), String>;
}

/// Parse `ps -axo pid=,pcpu=,rss=,user=,comm=` lines. Pure.
pub fn parse_ps(output: &str) -> Vec<ProcInfo> {
    output
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let pid = parts.next()?.parse().ok()?;
            let cpu_percent: f32 = parts.next()?.parse().ok()?;
            let rss_kb: f32 = parts.next()?.parse().ok()?;
            let user = parts.next()?.to_string();
            let comm: Vec<&str> = parts.collect();
            if comm.is_empty() {
                return None;
            }
            let full = comm.join(" ");
            let name = full.rsplit('/').next().unwrap_or(&full).to_string();
            Some(ProcInfo {
                pid,
                name,
                cpu_percent,
                memory_mb: rss_kb / 1024.0,
                user,
            })
        })
        .collect()
}

pub struct PsBackend;

#[async_trait]
impl ProcessBackend for PsBackend {
    async fn list(&self) -> Result<Vec<ProcInfo>, String> {
        let output = tokio::process::Command::new("/bin/ps")
            .args(["-axo", "pid=,pcpu=,rss=,user=,comm="])
            .output()
            .await
            .map_err(|e| format!("could not run ps: {e}"))?;
        Ok(parse_ps(&String::from_utf8_lossy(&output.stdout)))
    }
    async fn kill(&self, pid: i32, force: bool) -> Result<(), String> {
        let status = tokio::process::Command::new("/bin/kill")
            .arg(if force { "-9" } else { "-15" })
            .arg(pid.to_string())
            .status()
            .await
            .map_err(|e| format!("could not run kill: {e}"))?;
        if status.success() {
            Ok(())
        } else {
            Err(format!(
                "kill exited {status} (no such process, or not yours)"
            ))
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Args {
    action: String,
    #[serde(default)]
    sort: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    pid: Option<i32>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    force: Option<bool>,
}

pub struct ProcessesTool {
    backend: Arc<dyn ProcessBackend>,
    approver: Arc<dyn ApprovalPrompt>,
}

impl ProcessesTool {
    pub fn new(backend: Arc<dyn ProcessBackend>, approver: Arc<dyn ApprovalPrompt>) -> Self {
        Self { backend, approver }
    }

    pub fn definition() -> ToolDefinition {
        ToolDefinition {
            name: PROCESSES_TOOL.into(),
            description: "Running processes: list {sort: cpu|memory|name, limit} shows pid, name, \
                          CPU %, memory MB and user; kill {pid | name, force?} ends one (the user \
                          is always asked first; force sends SIGKILL). Use instead of ps/top/kill \
                          commands."
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["list", "kill"] },
                    "sort": { "type": "string", "enum": ["cpu", "memory", "name"], "description": "list: order (default cpu)." },
                    "limit": { "type": "integer", "description": "list: how many (default 15, max 50)." },
                    "pid": { "type": "integer", "description": "kill: the process id." },
                    "name": { "type": "string", "description": "kill: the process name (must match exactly one running process)." },
                    "force": { "type": "boolean", "description": "kill: SIGKILL instead of SIGTERM." }
                },
                "required": ["action"]
            }),
        }
    }

    /// Resolve pid or name to exactly one process. Pure over the list.
    pub fn resolve(
        list: &[ProcInfo],
        pid: Option<i32>,
        name: Option<&str>,
    ) -> Result<ProcInfo, ToolOutcome> {
        if let Some(pid) = pid {
            return list.iter().find(|p| p.pid == pid).cloned().ok_or_else(|| {
                ToolOutcome::failure("not-found", format!("no process with pid {pid}"))
            });
        }
        let Some(name) = name.map(str::trim).filter(|n| !n.is_empty()) else {
            return Err(ToolOutcome::failure(
                "invalid-arguments",
                "kill needs pid or name",
            ));
        };
        let matches: Vec<&ProcInfo> = list
            .iter()
            .filter(|p| p.name.eq_ignore_ascii_case(name))
            .collect();
        match matches.len() {
            1 => Ok(matches[0].clone()),
            0 => Err(ToolOutcome::failure(
                "not-found",
                format!("no running process named {name:?} — processes list to see names"),
            )),
            _ => Err(ToolOutcome::failure(
                "ambiguous",
                format!(
                    "{} processes are named {name:?} — kill by pid: {}",
                    matches.len(),
                    matches
                        .iter()
                        .map(|p| p.pid.to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            )),
        }
    }
}

#[async_trait]
impl ToolExecutor for ProcessesTool {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![Self::definition()]
    }

    fn claims(&self, name: &str) -> bool {
        name == PROCESSES_TOOL
    }

    async fn execute(&self, call: &ToolCall) -> ToolOutcome {
        let args: Args = match serde_json::from_str(&call.arguments) {
            Ok(a) => a,
            Err(e) => {
                return ToolOutcome::failure(
                    "invalid-arguments",
                    format!("invalid {PROCESSES_TOOL} arguments: {e}"),
                )
            }
        };
        match args.action.as_str() {
            "list" => {
                let mut list = match self.backend.list().await {
                    Ok(l) => l,
                    Err(e) => return ToolOutcome::failure("list-failed", e),
                };
                match args.sort.as_deref().unwrap_or("cpu") {
                    "memory" => list.sort_by(|a, b| b.memory_mb.total_cmp(&a.memory_mb)),
                    "name" => list.sort_by_key(|a| a.name.to_lowercase()),
                    _ => list.sort_by(|a, b| b.cpu_percent.total_cmp(&a.cpu_percent)),
                }
                list.truncate(args.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT));
                ToolOutcome {
                    result_count: Some(list.len()),
                    ..ToolOutcome::success(
                        serde_json::json!({ "ok": true, "processes": list }).to_string(),
                    )
                }
            }
            "kill" => {
                let list = match self.backend.list().await {
                    Ok(l) => l,
                    Err(e) => return ToolOutcome::failure("list-failed", e),
                };
                let target = match Self::resolve(&list, args.pid, args.name.as_deref()) {
                    Ok(t) => t,
                    Err(o) => return o,
                };
                let force = args.force.unwrap_or(false);
                // ALWAYS asks (⚠: persist_always_grant refuses to make it stick).
                let summary = format!(
                    "⚠ `kill` can end a running app: {} (pid {}){}",
                    target.name,
                    target.pid,
                    if force { " — force (SIGKILL)" } else { "" }
                );
                if self
                    .approver
                    .request(ActionKind::RunCommand, summary.clone())
                    .await
                    == ApprovalVerdict::Deny
                {
                    return ToolOutcome::failure(
                        "approval-denied",
                        format!("the user declined: {summary}"),
                    );
                }
                match self.backend.kill(target.pid, force).await {
                    Ok(()) => ToolOutcome::success(
                        serde_json::json!({ "ok": true, "killed": { "pid": target.pid, "name": target.name }, "signal": if force { "SIGKILL" } else { "SIGTERM" } }).to_string(),
                    ),
                    Err(e) => ToolOutcome::failure("kill-failed", e),
                }
            }
            other => ToolOutcome::failure(
                "invalid-arguments",
                format!("unknown action {other:?} (list | kill)"),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn ps_lines_parse_with_paths_and_spaces_in_names() {
        let out = "  123  12.5 204800 alex /Applications/Google Chrome.app/Contents/MacOS/Google Chrome\n  456   0.0   1024 root /usr/sbin/cupsd\nbad line\n";
        let list = parse_ps(out);
        assert_eq!(list.len(), 2);
        assert_eq!(
            list[0],
            ProcInfo {
                pid: 123,
                name: "Google Chrome".into(),
                cpu_percent: 12.5,
                memory_mb: 200.0,
                user: "alex".into()
            }
        );
        assert_eq!(list[1].name, "cupsd");
    }

    struct Fake(Vec<ProcInfo>, Mutex<Vec<(i32, bool)>>);
    #[async_trait]
    impl ProcessBackend for Fake {
        async fn list(&self) -> Result<Vec<ProcInfo>, String> {
            Ok(self.0.clone())
        }
        async fn kill(&self, pid: i32, force: bool) -> Result<(), String> {
            self.1.lock().unwrap().push((pid, force));
            Ok(())
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
    fn p(pid: i32, name: &str, cpu: f32, mem: f32) -> ProcInfo {
        ProcInfo {
            pid,
            name: name.into(),
            cpu_percent: cpu,
            memory_mb: mem,
            user: "alex".into(),
        }
    }
    fn tool(verdict: ApprovalVerdict) -> (ProcessesTool, Arc<Fake>, Arc<Prompt>) {
        let fake = Arc::new(Fake(
            vec![
                p(1, "node", 50.0, 100.0),
                p(2, "node", 1.0, 10.0),
                p(3, "Finder", 0.5, 300.0),
            ],
            Mutex::new(vec![]),
        ));
        let prompt = Arc::new(Prompt(verdict, Mutex::new(vec![])));
        (
            ProcessesTool::new(fake.clone(), prompt.clone()),
            fake,
            prompt,
        )
    }
    fn call(args: serde_json::Value) -> ToolCall {
        ToolCall {
            id: "c".into(),
            name: PROCESSES_TOOL.into(),
            arguments: args.to_string(),
        }
    }

    #[tokio::test]
    async fn list_sorts_and_bounds() {
        let (t, ..) = tool(ApprovalVerdict::AllowOnce);
        let out = t
            .execute(&call(
                serde_json::json!({"action":"list","sort":"memory","limit":1}),
            ))
            .await;
        assert!(
            out.ok && out.content.contains("\"name\":\"Finder\"") && !out.content.contains("node"),
            "{}",
            out.content
        );
        let out = t.execute(&call(serde_json::json!({"action":"list"}))).await;
        assert!(
            out.content.find("\"pid\":1").unwrap() < out.content.find("\"pid\":3").unwrap(),
            "cpu order by default"
        );
    }

    #[tokio::test]
    async fn kill_always_asks_with_a_warning_and_resolves_names_exactly() {
        let (t, fake, prompt) = tool(ApprovalVerdict::AllowAlways);
        let out = t
            .execute(&call(serde_json::json!({"action":"kill","name":"finder"})))
            .await;
        assert!(out.ok, "{out:?}");
        assert_eq!(fake.1.lock().unwrap().as_slice(), [(3, false)]);
        assert!(
            prompt.1.lock().unwrap()[0].starts_with("⚠ `kill`"),
            "dangerous summary → never persisted"
        );
        let amb = t
            .execute(&call(serde_json::json!({"action":"kill","name":"node"})))
            .await;
        assert_eq!(amb.failure.as_deref(), Some("ambiguous"));
        assert!(amb.content.contains("1, 2"));
        let missing = t
            .execute(&call(serde_json::json!({"action":"kill","pid":99})))
            .await;
        assert_eq!(missing.failure.as_deref(), Some("not-found"));
        let (t, fake, _) = tool(ApprovalVerdict::Deny);
        let out = t
            .execute(&call(
                serde_json::json!({"action":"kill","pid":1,"force":true}),
            ))
            .await;
        assert_eq!(out.failure.as_deref(), Some("approval-denied"));
        assert!(fake.1.lock().unwrap().is_empty());
    }
}
