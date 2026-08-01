//! Workspace roots (coding-agent S2, spec 2026-08-01): the ONLY places the
//! coding tools may touch the filesystem.
//!
//! The user designates roots in Settings; nothing is implicit. Containment
//! is checked on CANONICAL paths so `../` hops and symlinks cannot escape a
//! root — including write targets that do not exist yet (the deepest
//! existing ancestor is canonicalized and the remainder must be plain
//! descending components). With zero roots configured the file tools are
//! structurally inert (S3 offers no definitions), matching the D038
//! pattern everywhere else.

pub mod commands;
pub mod diff_tool;
pub mod exec_tool;
pub mod fs_tools;

use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;

use serde::Serialize;

/// The shared roots state: Settings mutates it, every tool call resolves
/// against it live.
#[derive(Default)]
pub struct WorkspaceState {
    roots: Mutex<Vec<PathBuf>>,
    persist_error: Mutex<Option<String>>,
    /// Per-workspace SESSION grants for `run_in_workspace` (S4): the user's
    /// "always this session" scoped to one canonical root, not the whole
    /// action kind. In-memory only — a restart prompts again (permanent
    /// grants ride the persisted approved-kinds path instead).
    exec_grants: Mutex<HashSet<PathBuf>>,
}

impl WorkspaceState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn roots(&self) -> Vec<PathBuf> {
        self.roots.lock().unwrap().clone()
    }

    pub fn has_roots(&self) -> bool {
        !self.roots.lock().unwrap().is_empty()
    }

    /// Replace the root set (applier/IPC). Entries are trimmed; relative
    /// paths are dropped with a warning — a root must be absolute.
    pub fn set_roots(&self, roots: Vec<String>) {
        let cleaned: Vec<PathBuf> = roots
            .iter()
            .map(|r| r.trim())
            .filter(|r| !r.is_empty())
            .filter_map(|r| {
                let p = PathBuf::from(r);
                if p.is_absolute() {
                    Some(p)
                } else {
                    log::warn!("workspace: dropping non-absolute root {r:?}");
                    None
                }
            })
            .collect();
        *self.roots.lock().unwrap() = cleaned;
    }

    pub fn set_persist_error(&self, error: Option<String>) {
        *self.persist_error.lock().unwrap() = error;
    }

    pub fn persist_error(&self) -> Option<String> {
        self.persist_error.lock().unwrap().clone()
    }

    /// Resolve `candidate` to a canonical absolute path PROVEN to live
    /// inside one of the roots — the single choke point every fs/exec tool
    /// must pass before any io. See [`resolve_contained`].
    pub fn resolve(&self, candidate: &str) -> Result<PathBuf, WorkspaceError> {
        self.resolve_with_root(candidate).map(|(path, _)| path)
    }

    /// [`Self::resolve`] plus the CANONICAL root that contains the result —
    /// the grant key for per-workspace approvals (S4).
    pub fn resolve_with_root(&self, candidate: &str) -> Result<(PathBuf, PathBuf), WorkspaceError> {
        let roots = self.roots();
        if roots.is_empty() {
            return Err(WorkspaceError::NoWorkspaces);
        }
        resolve_contained_with_root(Path::new(candidate), &roots)
    }

    /// Grant `run_in_workspace` for one canonical root, this session.
    pub fn grant_exec(&self, root: PathBuf) {
        self.exec_grants.lock().unwrap().insert(root);
    }

    /// Whether the root already holds a session exec grant.
    pub fn exec_granted(&self, root: &Path) -> bool {
        self.exec_grants.lock().unwrap().contains(root)
    }
}

/// Typed refusals for the workspace boundary (kind-tagged like every error
/// surface; the tool layer maps these onto ToolOutcome failures).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(
    tag = "kind",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase"
)]
pub enum WorkspaceError {
    /// No roots are configured — the tools should not even be offered.
    NoWorkspaces,
    /// The path (canonicalized) is not inside any designated root.
    OutsideWorkspace { path: String },
    /// The path could not be resolved (bad syntax, unreadable ancestor).
    Unresolvable { path: String, detail: String },
}

impl WorkspaceError {
    pub fn kind(&self) -> &'static str {
        match self {
            WorkspaceError::NoWorkspaces => "no-workspaces",
            WorkspaceError::OutsideWorkspace { .. } => "outside-workspace",
            WorkspaceError::Unresolvable { .. } => "unresolvable-path",
        }
    }
}

impl std::fmt::Display for WorkspaceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WorkspaceError::NoWorkspaces => write!(
                f,
                "no workspace folders are configured — the user adds them in Settings → Workspaces"
            ),
            WorkspaceError::OutsideWorkspace { path } => write!(
                f,
                "{path} is outside every designated workspace folder — file access is limited to \
                 the workspaces the user configured"
            ),
            WorkspaceError::Unresolvable { path, detail } => {
                write!(f, "cannot resolve {path}: {detail}")
            }
        }
    }
}

/// Canonical containment: resolve `candidate` (absolute, or relative to the
/// FIRST root) to a canonical path and prove it sits under a canonical
/// root. For not-yet-existing targets, the deepest existing ancestor is
/// canonicalized and the remaining components must be plain names (no `..`,
/// no `.`) — so a new file's path cannot smuggle an escape either.
pub fn resolve_contained(candidate: &Path, roots: &[PathBuf]) -> Result<PathBuf, WorkspaceError> {
    resolve_contained_with_root(candidate, roots).map(|(path, _)| path)
}

/// [`resolve_contained`] plus the canonical root that proved containment.
pub fn resolve_contained_with_root(
    candidate: &Path,
    roots: &[PathBuf],
) -> Result<(PathBuf, PathBuf), WorkspaceError> {
    let absolute = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        roots[0].join(candidate)
    };
    let (existing, remainder) = deepest_existing(&absolute);
    let canonical_base = existing
        .canonicalize()
        .map_err(|e| WorkspaceError::Unresolvable {
            path: absolute.display().to_string(),
            detail: e.to_string(),
        })?;
    // Remainder of a not-yet-existing path: only plain descending names.
    for component in remainder.components() {
        match component {
            Component::Normal(_) => {}
            _ => {
                return Err(WorkspaceError::OutsideWorkspace {
                    path: absolute.display().to_string(),
                })
            }
        }
    }
    // join("") would append a trailing slash (ENOTDIR on later io) — an
    // existing path has an empty remainder and IS the canonical base.
    let resolved = if remainder.as_os_str().is_empty() {
        canonical_base
    } else {
        canonical_base.join(&remainder)
    };
    for root in roots {
        if let Ok(canonical_root) = root.canonicalize() {
            if resolved.starts_with(&canonical_root) {
                return Ok((resolved, canonical_root));
            }
        }
    }
    Err(WorkspaceError::OutsideWorkspace {
        path: absolute.display().to_string(),
    })
}

/// Split into (deepest existing ancestor, remaining relative path).
fn deepest_existing(path: &Path) -> (PathBuf, PathBuf) {
    let mut existing = path.to_path_buf();
    let mut remainder = PathBuf::new();
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    while !existing.exists() {
        match (existing.file_name(), existing.parent()) {
            (Some(name), Some(parent)) => {
                tail.push(name.to_os_string());
                existing = parent.to_path_buf();
            }
            _ => break,
        }
    }
    for name in tail.into_iter().rev() {
        remainder.push(name);
    }
    (existing, remainder)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("te-ws-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("sub/inside.txt"), "x").unwrap();
        dir
    }

    #[test]
    fn contains_existing_and_new_paths_inside_the_root() {
        let root = scratch("in");
        let roots = vec![root.clone()];
        // Existing file.
        let resolved =
            resolve_contained(&root.join("sub/inside.txt"), &roots).expect("existing inside");
        assert!(resolved.ends_with("sub/inside.txt"));
        // The resolved path must be directly io-usable (no trailing slash).
        assert_eq!(std::fs::read_to_string(&resolved).unwrap(), "x");
        // New (not yet existing) file under an existing dir.
        assert!(resolve_contained(&root.join("sub/new.rs"), &roots).is_ok());
        // New nested dirs, still plain descending names.
        assert!(resolve_contained(&root.join("brand/new/tree.txt"), &roots).is_ok());
        // Relative resolves against the first root.
        assert!(resolve_contained(Path::new("sub/inside.txt"), &roots).is_ok());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn dotdot_and_absolute_escapes_are_refused() {
        let root = scratch("esc");
        let roots = vec![root.clone()];
        let escape = root.join("sub/../../../etc/passwd");
        assert!(matches!(
            resolve_contained(&escape, &roots),
            Err(WorkspaceError::OutsideWorkspace { .. }) | Err(WorkspaceError::Unresolvable { .. })
        ));
        assert!(matches!(
            resolve_contained(Path::new("/etc/passwd"), &roots),
            Err(WorkspaceError::OutsideWorkspace { .. })
        ));
        // A NEW path whose remainder smuggles `..` is refused before io.
        assert!(matches!(
            resolve_contained(&root.join("ghost/../..//x.txt"), &roots),
            Err(WorkspaceError::OutsideWorkspace { .. }) | Err(WorkspaceError::Unresolvable { .. })
        ));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_out_of_the_root_are_refused() {
        let root = scratch("sym");
        let outside = std::env::temp_dir().join(format!("te-ws-outside-{}", std::process::id()));
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.txt"), "s").unwrap();
        std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();
        let roots = vec![root.clone()];
        assert!(matches!(
            resolve_contained(&root.join("link/secret.txt"), &roots),
            Err(WorkspaceError::OutsideWorkspace { .. })
        ));
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[test]
    fn state_drops_relative_roots_and_resolves_against_the_set() {
        let root = scratch("state");
        let state = WorkspaceState::new();
        assert!(matches!(
            state.resolve("anything"),
            Err(WorkspaceError::NoWorkspaces)
        ));
        state.set_roots(vec![
            root.display().to_string(),
            "relative/never".into(),
            "  ".into(),
        ]);
        assert_eq!(state.roots().len(), 1);
        assert!(state
            .resolve(&root.join("sub/inside.txt").display().to_string())
            .is_ok());
        let _ = std::fs::remove_dir_all(&root);
    }
}
