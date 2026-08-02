//! Working directories (redesigned 2026-08-02 at user direction): the
//! coding tools may work ANYWHERE — the directories the user designates
//! (Settings / `thirdeye <path>` / Finder) are the STARTING points, not a
//! wall. The first entry is the ACTIVE directory: relative paths resolve
//! against it, and with none set a relative path refuses typed so the
//! model asks the user where to work instead of guessing.
//!
//! Safety moved from containment to CONSENT: every write or command in a
//! directory the user has not yet blessed prompts (the card names the
//! exact path; "this session" blesses that directory), EXCEPT tmp —
//! scratch space is always writable without a prompt. Paths still resolve
//! canonically (deepest existing ancestor + plain remainder) so symlinks
//! and `../` cannot mislabel where io actually lands.

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
    /// Per-DIRECTORY session grants (2026-08-02): "always this session"
    /// blesses one canonical directory (and everything under it) for
    /// writes and commands. In-memory only — a restart prompts again
    /// (permanent grants ride the persisted approved-kinds path).
    dir_grants: Mutex<HashSet<PathBuf>>,
    /// The pause-and-ask folder chooser, installed at boot (production
    /// only — absent in unit tests).
    chooser: std::sync::OnceLock<std::sync::Arc<dyn DirectoryChooser>>,
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

    /// Resolve `candidate` to a canonical absolute path — the single
    /// choke point every fs/exec tool passes before io. Absolute paths go
    /// anywhere; relative paths resolve against the ACTIVE working
    /// directory, and with none set they refuse typed (the model must ask
    /// the user where to work, never guess).
    pub fn resolve(&self, candidate: &str) -> Result<PathBuf, WorkspaceError> {
        let path = Path::new(candidate);
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            match self.roots().first() {
                Some(active) => active.join(path),
                None => return Err(WorkspaceError::NoWorkingDirectory),
            }
        };
        canonical_resolve(&absolute)
    }

    /// Bless one canonical directory (and its subtree) for this session.
    pub fn grant_dir(&self, dir: PathBuf) {
        self.dir_grants.lock().unwrap().insert(dir);
    }

    /// Whether `path` falls under any session-blessed directory.
    pub fn dir_granted(&self, path: &Path) -> bool {
        self.dir_grants
            .lock()
            .unwrap()
            .iter()
            .any(|dir| path.starts_with(dir))
    }

    /// Whether `path` is scratch space — ALWAYS writable, no prompt
    /// (user direction 2026-08-02: "writing to tmp is fine always").
    pub fn is_tmp(path: &Path) -> bool {
        let tmp = std::env::temp_dir();
        path.starts_with("/tmp")
            || path.starts_with("/private/tmp")
            || path.starts_with("/var/folders")
            || path.starts_with("/private/var/folders")
            || path.starts_with(&tmp)
    }
}

/// The pause-and-ask seam (user request 2026-08-02): when a tool needs a
/// working directory and none is set, the installed chooser shows the
/// native folder dialog, PROMOTES the pick to the active directory
/// (persisted + broadcast), and returns it. `None` = user declined.
#[async_trait::async_trait]
pub trait DirectoryChooser: Send + Sync {
    async fn choose(&self, purpose: &str) -> Option<String>;
}

impl WorkspaceState {
    /// Install the production chooser once at boot (tests leave it absent
    /// — resolve_or_ask then degrades to the typed refusal).
    pub fn install_chooser(&self, chooser: std::sync::Arc<dyn DirectoryChooser>) {
        let _ = self.chooser.set(chooser);
    }

    /// [`Self::resolve`], pausing to ASK THE USER for a folder when a
    /// relative path has no active working directory. The chooser promotes
    /// the pick to the active directory, so the retry resolves against it.
    pub async fn resolve_or_ask(
        &self,
        candidate: &str,
        purpose: &str,
    ) -> Result<std::path::PathBuf, WorkspaceError> {
        match self.resolve(candidate) {
            Err(WorkspaceError::NoWorkingDirectory) => {
                let Some(chooser) = self.chooser.get().cloned() else {
                    return Err(WorkspaceError::NoWorkingDirectory);
                };
                match chooser.choose(purpose).await {
                    Some(_) => self.resolve(candidate),
                    None => Err(WorkspaceError::NoWorkingDirectory),
                }
            }
            other => other,
        }
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
    /// A relative path with no active working directory — the model must
    /// ask the user where to work (or the chooser dialog was declined).
    NoWorkingDirectory,
    /// The path could not be resolved (bad syntax, unreadable ancestor,
    /// or a `..` remainder through a not-yet-existing segment).
    Unresolvable { path: String, detail: String },
}

impl WorkspaceError {
    pub fn kind(&self) -> &'static str {
        match self {
            WorkspaceError::NoWorkingDirectory => "no-working-directory",
            WorkspaceError::Unresolvable { .. } => "unresolvable-path",
        }
    }
}

impl std::fmt::Display for WorkspaceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WorkspaceError::NoWorkingDirectory => write!(
                f,
                "no working directory is set and the path was relative — ask the user which \
                 folder to work in (a chooser was shown if available), or use an absolute path"
            ),
            WorkspaceError::Unresolvable { path, detail } => {
                write!(f, "cannot resolve {path}: {detail}")
            }
        }
    }
}

/// Canonical resolution WITHOUT containment (2026-08-02 redesign): the
/// deepest existing ancestor is canonicalized (symlink-honest — io lands
/// where the result says it lands) and the not-yet-existing remainder must
/// be plain descending names, so `..` through a missing segment cannot
/// alias a different location.
pub fn canonical_resolve(absolute: &Path) -> Result<PathBuf, WorkspaceError> {
    let (existing, remainder) = deepest_existing(absolute);
    let canonical_base = existing
        .canonicalize()
        .map_err(|e| WorkspaceError::Unresolvable {
            path: absolute.display().to_string(),
            detail: e.to_string(),
        })?;
    for component in remainder.components() {
        match component {
            Component::Normal(_) => {}
            _ => {
                return Err(WorkspaceError::Unresolvable {
                    path: absolute.display().to_string(),
                    detail: "a not-yet-existing path segment may not contain '..' or '.'".into(),
                })
            }
        }
    }
    // join("") would append a trailing slash (ENOTDIR on later io) — an
    // existing path has an empty remainder and IS the canonical base.
    Ok(if remainder.as_os_str().is_empty() {
        canonical_base
    } else {
        canonical_base.join(&remainder)
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
    fn absolute_paths_resolve_anywhere_and_relative_needs_an_active_dir() {
        let root = scratch("any");
        let state = WorkspaceState::new();
        // No working directory: absolute is fine, relative refuses typed.
        let resolved = state
            .resolve(&root.join("sub/inside.txt").display().to_string())
            .expect("absolute resolves without any working directory");
        assert_eq!(std::fs::read_to_string(&resolved).unwrap(), "x");
        assert!(matches!(
            state.resolve("sub/inside.txt"),
            Err(WorkspaceError::NoWorkingDirectory)
        ));
        // With an active directory the same relative path resolves.
        state.set_roots(vec![root.display().to_string()]);
        assert!(state.resolve("sub/inside.txt").is_ok());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn canonical_resolution_is_symlink_honest_and_refuses_ghost_dotdot() {
        let root = scratch("canon");
        // A new file under an existing dir resolves; ghost `..` refuses.
        assert!(canonical_resolve(&root.join("sub/new.rs")).is_ok());
        assert!(matches!(
            canonical_resolve(&root.join("ghost/../x.txt")),
            Err(WorkspaceError::Unresolvable { .. })
        ));
        // Symlinks resolve to their REAL location — io lands where the
        // result says (macOS /tmp → /private/tmp is itself a symlink).
        let resolved = canonical_resolve(&root.join("sub/inside.txt")).unwrap();
        assert_eq!(std::fs::read_to_string(&resolved).unwrap(), "x");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn tmp_detection_and_dir_grants_cover_subtrees() {
        assert!(WorkspaceState::is_tmp(Path::new("/tmp/x/y.txt")));
        assert!(WorkspaceState::is_tmp(&std::env::temp_dir().join("z")));
        assert!(!WorkspaceState::is_tmp(Path::new("/Users/alex/code")));
        let state = WorkspaceState::new();
        assert!(!state.dir_granted(Path::new("/a/b/c.txt")));
        state.grant_dir(PathBuf::from("/a/b"));
        assert!(state.dir_granted(Path::new("/a/b/c.txt")));
        assert!(state.dir_granted(Path::new("/a/b/deep/d.txt")));
        assert!(!state.dir_granted(Path::new("/a/other.txt")));
    }

    #[tokio::test]
    async fn resolve_or_ask_uses_the_chooser_and_declining_refuses_typed() {
        struct PickScratch(PathBuf, std::sync::Arc<WorkspaceState>);
        #[async_trait::async_trait]
        impl DirectoryChooser for PickScratch {
            async fn choose(&self, _purpose: &str) -> Option<String> {
                // The production chooser promotes the pick; mirror that.
                self.1.set_roots(vec![self.0.display().to_string()]);
                Some(self.0.display().to_string())
            }
        }
        struct Decline;
        #[async_trait::async_trait]
        impl DirectoryChooser for Decline {
            async fn choose(&self, _purpose: &str) -> Option<String> {
                None
            }
        }
        let root = scratch("ask");
        let state = std::sync::Arc::new(WorkspaceState::new());
        state.install_chooser(std::sync::Arc::new(PickScratch(
            root.clone(),
            state.clone(),
        )));
        let resolved = state
            .resolve_or_ask("sub/inside.txt", "read a file")
            .await
            .expect("the chooser's pick resolves the relative path");
        assert_eq!(std::fs::read_to_string(&resolved).unwrap(), "x");

        let declined = std::sync::Arc::new(WorkspaceState::new());
        declined.install_chooser(std::sync::Arc::new(Decline));
        assert!(matches!(
            declined.resolve_or_ask("x.txt", "write").await,
            Err(WorkspaceError::NoWorkingDirectory)
        ));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn state_drops_relative_roots() {
        let state = WorkspaceState::new();
        state.set_roots(vec!["/tmp".into(), "relative/never".into(), "  ".into()]);
        assert_eq!(state.roots().len(), 1);
    }
}
