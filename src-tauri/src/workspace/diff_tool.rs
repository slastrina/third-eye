//! Workspace diff (coding-agent S5): `workspace_diff`, the coder's mirror.
//!
//! Read-only and ungated beyond toggles (like `read_file`): it runs
//! `git status --porcelain` + `git diff HEAD` in a workspace root and
//! reports the UNCOMMITTED state of the repo — the model is prompted to
//! review this before declaring an edit task done, and the transcript
//! renders the same output as a collapsible colored diff block. Git is the
//! diff mechanism per the spec; a workspace that is not a repo refuses
//! typed (`not-a-repo`) so the model says so instead of inventing a diff.
//! This tool NEVER writes: no commit, no stage, no stash.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use super::{WorkspaceError, WorkspaceState};
use crate::command_runner::truncate_stream;
use crate::llm::toolloop::{ToolExecutor, ToolOutcome};
use crate::llm::{ToolCall, ToolDefinition};

pub const WORKSPACE_DIFF_TOOL: &str = "workspace_diff";

/// Diffs are read-only queries; a repo that takes longer than this to
/// answer `git diff` is wedged, not busy.
const GIT_TIMEOUT_SECS: u64 = 10;

pub struct WorkspaceDiffTool {
    workspace: Arc<WorkspaceState>,
}

impl WorkspaceDiffTool {
    pub fn new(workspace: Arc<WorkspaceState>) -> Self {
        Self { workspace }
    }

    pub fn definition() -> ToolDefinition {
        ToolDefinition {
            name: WORKSPACE_DIFF_TOOL.into(),
            description: "Show the UNCOMMITTED changes in a workspace (git status + git diff). \
                          After editing files, call this and REVIEW the diff before declaring \
                          the task done — confirm it contains exactly the intended changes and \
                          summarize them for the user. Read-only: it never commits, stages, or \
                          reverts anything."
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "cwd": {
                        "type": "string",
                        "description": "Workspace directory to diff (optional; defaults to the first workspace root)."
                    }
                },
                "required": []
            }),
        }
    }
}

/// One bounded `git` invocation in `dir`. Returns (exit-ok, combined text).
async fn git(dir: &Path, args: &[&str]) -> Result<(bool, String), String> {
    let output = tokio::time::timeout(
        Duration::from_secs(GIT_TIMEOUT_SECS),
        tokio::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| format!("git {} timed out after {GIT_TIMEOUT_SECS}s", args.join(" ")))?
    .map_err(|e| format!("could not run git: {e}"))?;
    let mut text = truncate_stream(&output.stdout);
    let stderr = truncate_stream(&output.stderr);
    if !output.status.success() && !stderr.is_empty() {
        text.push_str(&stderr);
    }
    Ok((output.status.success(), text))
}

fn workspace_failure(err: WorkspaceError) -> ToolOutcome {
    ToolOutcome::failure(err.kind(), err.to_string())
}

#[async_trait]
impl ToolExecutor for WorkspaceDiffTool {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![Self::definition()]
    }

    fn claims(&self, name: &str) -> bool {
        name == WORKSPACE_DIFF_TOOL
    }

    async fn execute(&self, call: &ToolCall) -> ToolOutcome {
        let cwd_arg = serde_json::from_str::<serde_json::Value>(&call.arguments)
            .ok()
            .and_then(|v| v.get("cwd").and_then(|c| c.as_str().map(String::from)))
            .unwrap_or_default();
        let candidate = if cwd_arg.trim().is_empty() {
            ".".to_string()
        } else {
            cwd_arg
        };
        let dir = match self
            .workspace
            .resolve_or_ask(&candidate, "review the git diff")
            .await
        {
            Ok(dir) => dir,
            Err(e) => return workspace_failure(e),
        };
        if !dir.is_dir() {
            return ToolOutcome::failure(
                "not-a-directory",
                format!("cwd {} is not an existing directory", dir.display()),
            );
        }
        // A workspace that is not a git repo has no diff to show — typed,
        // so the model reports that honestly instead of inventing one.
        match git(&dir, &["rev-parse", "--is-inside-work-tree"]).await {
            Ok((true, _)) => {}
            Ok((false, detail)) => {
                return ToolOutcome::failure(
                    "not-a-repo",
                    format!(
                        "{} is not a git repository — there is no diff to review ({})",
                        dir.display(),
                        detail.trim()
                    ),
                );
            }
            Err(e) => return ToolOutcome::failure("git-failed", e),
        }
        let status = match git(&dir, &["status", "--porcelain"]).await {
            Ok((_, text)) => text,
            Err(e) => return ToolOutcome::failure("git-failed", e),
        };
        // HEAD may not exist yet (fresh repo, no commits): fall back to a
        // plain worktree diff so brand-new repos still show their changes.
        let diff = match git(&dir, &["diff", "HEAD"]).await {
            Ok((true, text)) => text,
            Ok((false, _)) => match git(&dir, &["diff"]).await {
                Ok((_, text)) => text,
                Err(e) => return ToolOutcome::failure("git-failed", e),
            },
            Err(e) => return ToolOutcome::failure("git-failed", e),
        };
        if status.trim().is_empty() && diff.trim().is_empty() {
            return ToolOutcome::success(format!(
                "[{}]\nworking tree clean — no uncommitted changes",
                dir.display()
            ));
        }
        let mut report = format!("[{}]\nstatus:\n{}", dir.display(), status);
        if !diff.trim().is_empty() {
            report.push_str(&format!("\ndiff:\n{diff}"));
        } else {
            // Porcelain shows entries but the diff is empty: untracked files.
            report.push_str("\n(untracked files only — no content diff against HEAD)");
        }
        ToolOutcome::success(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn scratch_repo(tag: &str) -> (Arc<WorkspaceState>, PathBuf) {
        let dir = std::env::temp_dir().join(format!("te-diff-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let git = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .arg("-C")
                .arg(&dir)
                .args(args)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .output()
                .unwrap();
            assert!(status.status.success(), "git {args:?} failed");
        };
        git(&["init", "-q"]);
        std::fs::write(dir.join("main.rs"), "fn main() {}\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-qm", "init"]);
        let ws = Arc::new(WorkspaceState::new());
        ws.set_roots(vec![dir.display().to_string()]);
        (ws, dir)
    }

    fn call(args: serde_json::Value) -> ToolCall {
        ToolCall {
            id: "c1".into(),
            name: WORKSPACE_DIFF_TOOL.into(),
            arguments: args.to_string(),
        }
    }

    #[tokio::test]
    async fn always_offered_and_no_working_dir_refuses_typed() {
        let tool = WorkspaceDiffTool::new(Arc::new(WorkspaceState::new()));
        assert_eq!(tool.definitions().len(), 1);
        let outcome = tool.execute(&call(serde_json::json!({}))).await;
        assert_eq!(outcome.failure.as_deref(), Some("no-working-directory"));
    }

    #[tokio::test]
    async fn non_repo_refuses_typed_not_a_repo() {
        let dir = std::env::temp_dir().join(format!("te-diff-norepo-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let ws = Arc::new(WorkspaceState::new());
        ws.set_roots(vec![dir.display().to_string()]);
        let outcome = WorkspaceDiffTool::new(ws)
            .execute(&call(serde_json::json!({})))
            .await;
        assert_eq!(
            outcome.failure.as_deref(),
            Some("not-a-repo"),
            "{outcome:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn clean_tree_says_so_and_edits_show_in_the_diff() {
        let (ws, dir) = scratch_repo("diff");
        let tool = WorkspaceDiffTool::new(ws);
        let clean = tool.execute(&call(serde_json::json!({}))).await;
        assert!(clean.ok, "{clean:?}");
        assert!(clean.content.contains("working tree clean"));
        // An edit shows up as a real diff hunk.
        std::fs::write(dir.join("main.rs"), "fn main() { changed(); }\n").unwrap();
        let dirty = tool.execute(&call(serde_json::json!({}))).await;
        assert!(dirty.ok, "{dirty:?}");
        assert!(dirty.content.contains("main.rs"), "{}", dirty.content);
        assert!(dirty.content.contains("+fn main() { changed(); }"));
        assert!(dirty.content.contains("-fn main() {}"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn anywhere_cwd_reads_fine_and_non_repos_stay_typed() {
        // Anywhere semantics: an absolute cwd outside any working
        // directory is fine for a read-only diff — /etc just isn't a repo.
        let (ws, dir) = scratch_repo("any");
        let outcome = WorkspaceDiffTool::new(ws)
            .execute(&call(serde_json::json!({"cwd": "/etc"})))
            .await;
        assert_eq!(
            outcome.failure.as_deref(),
            Some("not-a-repo"),
            "{outcome:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tool_name_matches_the_toolloop_preview_literal() {
        // toolloop.rs's result_preview compares against the literal
        // "workspace_diff" (this module is cfg(desktop), llm is not).
        assert_eq!(WORKSPACE_DIFF_TOOL, "workspace_diff");
    }
}
