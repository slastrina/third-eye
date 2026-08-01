//! Workspace-roots IPC + persistence applier (coding-agent S2). The
//! nudges/commands applier contract: persist, roll back in-memory on a
//! persist failure, always return the authoritative snapshot.

use serde::Serialize;
use tauri::{AppHandle, Manager, State};

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
    status(&state)
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
