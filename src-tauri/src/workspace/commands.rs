//! Workspace-roots IPC + persistence applier (coding-agent S2). The
//! nudges/commands applier contract: persist, roll back in-memory on a
//! persist failure, always return the authoritative snapshot.

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State};

/// Broadcast on every roots change so the overlay's workspace chip and the
/// Settings pane stay live without polling. Payload: [`WorkspaceStatus`].
pub const WORKSPACE_ROOTS_EVENT: &str = "workspace://roots";

fn broadcast_roots(app: &AppHandle, status: &WorkspaceStatus) {
    if let Err(e) = app.emit(WORKSPACE_ROOTS_EVENT, status.clone()) {
        log::warn!("workspace: {WORKSPACE_ROOTS_EVENT} emit failed: {e}");
    }
}

use super::WorkspaceState;

/// The `workspace_roots` / `set_workspace_roots` wire shape.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceStatus {
    pub roots: Vec<String>,
    pub persist_error: Option<String>,
}

fn status(state: &WorkspaceState) -> WorkspaceStatus {
    WorkspaceStatus {
        roots: state
            .roots()
            .iter()
            .map(|p| p.display().to_string())
            .collect(),
        persist_error: state.persist_error(),
    }
}

#[tauri::command]
pub fn workspace_roots(state: State<'_, std::sync::Arc<WorkspaceState>>) -> WorkspaceStatus {
    status(&state)
}

/// Replace the workspace roots. Relative entries are dropped (logged);
/// persist failure rolls the in-memory set back.
#[tauri::command]
pub fn set_workspace_roots(
    app: AppHandle,
    state: State<'_, std::sync::Arc<WorkspaceState>>,
    roots: Vec<String>,
) -> WorkspaceStatus {
    let previous = state.roots();
    state.set_roots(roots);
    let cleaned: Vec<String> = state
        .roots()
        .iter()
        .map(|p| p.display().to_string())
        .collect();
    match crate::config::save_workspace_roots(&app, &cleaned) {
        Ok(()) => {
            state.set_persist_error(None);
            log::info!("workspace: {} root(s) configured", cleaned.len());
        }
        Err(e) => {
            state.set_roots(previous.iter().map(|p| p.display().to_string()).collect());
            log::error!("workspace: {e}");
            state.set_persist_error(Some(e));
        }
    }
    let snapshot = status(&state);
    broadcast_roots(&app, &snapshot);
    snapshot
}

/// The pure "work here" ordering: `target` becomes the FIRST root — the
/// one every coding tool defaults to (list_dir with no path, relative
/// resolution, run_in_workspace cwd). An existing entry is MOVED to the
/// front, never duplicated.
pub fn promote_root(existing: &[String], target: &str) -> Vec<String> {
    let mut roots = vec![target.to_string()];
    roots.extend(existing.iter().filter(|r| *r != target).cloned());
    roots
}

/// Add ONE workspace root from an external entry point (the bridge's
/// `add-workspace` command — CLI / Finder, spec 2026-08-02 N3) and make it
/// the ACTIVE workspace: "work here" means the tools' default directory is
/// THIS one, so the new root goes first (an already-known root moves to
/// the front). Same canonical/absolute discipline as Settings; persisted
/// with rollback. Returns the canonical path.
pub fn add_workspace_root(app: &AppHandle, path: &str) -> Result<String, String> {
    let canonical = std::path::Path::new(path)
        .canonicalize()
        .map_err(|e| format!("cannot resolve {path}: {e}"))?;
    if !canonical.is_dir() {
        return Err(format!("{} is not a directory", canonical.display()));
    }
    let state = app.state::<std::sync::Arc<WorkspaceState>>();
    let display = canonical.display().to_string();
    let previous = state.roots();
    let previous_strings: Vec<String> = previous.iter().map(|p| p.display().to_string()).collect();
    let roots = promote_root(&previous_strings, &display);
    if roots == previous_strings {
        return Ok(display);
    }
    state.set_roots(roots.clone());
    match crate::config::save_workspace_roots(app, &roots) {
        Ok(()) => {
            log::info!("workspace: active root via bridge: {display}");
            broadcast_roots(app, &status(&state));
            Ok(display)
        }
        Err(e) => {
            state.set_roots(previous.iter().map(|p| p.display().to_string()).collect());
            log::error!("workspace: {e}");
            Err(e)
        }
    }
}

/// Production folder chooser (user request 2026-08-02): the native macOS
/// folder dialog via osascript — no plugin needed. A pick is PROMOTED to
/// the active working directory (persisted + broadcast, so the overlay
/// chip updates mid-run). Cancel/timeout returns None and the tool
/// refuses typed.
pub struct OsascriptChooser {
    pub app: AppHandle,
}

#[async_trait::async_trait]
impl super::DirectoryChooser for OsascriptChooser {
    async fn choose(&self, purpose: &str) -> Option<String> {
        let prompt = format!("Third Eye needs a folder to work in — {purpose}");
        let script = format!(
            "POSIX path of (choose folder with prompt {})",
            osa_quote(&prompt)
        );
        let output = tokio::time::timeout(
            std::time::Duration::from_secs(120),
            tokio::process::Command::new("/usr/bin/osascript")
                .args(["-e", &script])
                .output(),
        )
        .await
        .ok()?
        .ok()?;
        if !output.status.success() {
            log::info!("workspace: folder chooser declined/failed");
            return None;
        }
        let picked = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if picked.is_empty() {
            return None;
        }
        match add_workspace_root(&self.app, &picked) {
            Ok(canonical) => {
                log::info!("workspace: user chose working directory {canonical}");
                Some(canonical)
            }
            Err(e) => {
                log::error!("workspace: chooser pick rejected: {e}");
                None
            }
        }
    }
}

/// AppleScript string literal (quote + escape) — the prompt is app-built,
/// but quoting defensively costs nothing.
fn osa_quote(text: &str) -> String {
    format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Apply the persisted roots at boot (in-memory only).
pub fn apply_persisted_workspace_roots(app: &AppHandle) {
    let roots = crate::config::load_workspace_roots(app);
    if !roots.is_empty() {
        log::info!("workspace: applied {} persisted root(s)", roots.len());
    }
    app.state::<std::sync::Arc<WorkspaceState>>()
        .set_roots(roots);
}

#[cfg(test)]
mod tests {
    use super::promote_root;

    #[test]
    fn work_here_makes_the_target_the_first_root_without_duplicates() {
        let existing = vec!["/a".to_string(), "/b".to_string()];
        assert_eq!(promote_root(&existing, "/c"), vec!["/c", "/a", "/b"]);
        // An already-known root MOVES to the front (the pi-dir incident:
        // `thirdeye .` appended, tools kept defaulting to the old first).
        assert_eq!(promote_root(&existing, "/b"), vec!["/b", "/a"]);
        // Already first: unchanged.
        assert_eq!(promote_root(&existing, "/a"), vec!["/a", "/b"]);
        assert_eq!(promote_root(&[], "/x"), vec!["/x"]);
    }
}
