//! Machine inventory (computer-control I1): what is installed on this
//! machine — GUI app bundles and PATH executables — cached in the memory
//! store's `inventory` table so the assistant can answer "is X installed"
//! instantly and ground `focus_app` / `run_command` targets in reality.
//!
//! Discovery is filesystem truth, no package managers: `*.app` bundles at
//! depth ≤ 2 under the standard application directories, plus executable
//! regular files in each `$PATH` directory (first-wins dedup in PATH
//! order, mirroring shell resolution). The cache refreshes atomically
//! (wipe+refill in one transaction) at startup, every 24 h, and on the
//! `refresh_inventory` IPC — a failed scan keeps the stale cache and logs.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Serialize;
use tauri::Manager;

use crate::llm::toolloop::{ToolExecutor, ToolOutcome};
use crate::llm::{ToolCall, ToolDefinition};
use crate::memory::store::{InventoryEntry, MemoryStore, NewInventoryEntry};
use crate::memory::MemoryState;

/// Name of the discovery tool the model calls.
pub const FIND_PROGRAMS_TOOL: &str = "find_programs";

/// Refresh cadence for the unattended loop.
const REFRESH_INTERVAL_SECS: u64 = 24 * 60 * 60;

/// Bound on one scan's PATH-tool count — a runaway PATH entry (a directory
/// with tens of thousands of files) must not balloon the cache.
const MAX_TOOLS: usize = 20_000;

/// GUI application roots scanned on macOS.
#[cfg(target_os = "macos")]
fn app_roots() -> Vec<PathBuf> {
    let mut roots = vec![
        PathBuf::from("/Applications"),
        PathBuf::from("/System/Applications"),
    ];
    if let Some(home) = dirs_home() {
        roots.push(home.join("Applications"));
    }
    roots
}

#[cfg(not(target_os = "macos"))]
fn app_roots() -> Vec<PathBuf> {
    Vec::new() // PATH scan only off macOS (typed absence, not a stub).
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Scan one root for `*.app` bundles, depth ≤ 2 (top level plus one
/// subfolder level — covers `/Applications/Utilities/*.app` without
/// descending into bundle internals).
fn scan_apps_under(root: &Path, depth: usize, out: &mut Vec<(String, PathBuf)>) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        if path.extension().is_some_and(|ext| ext == "app") {
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                out.push((stem.to_string(), path));
            }
        } else if depth > 0 {
            scan_apps_under(&path, depth - 1, out);
        }
    }
}

/// Whether a directory entry is an executable regular file (or a symlink to
/// one) — the PATH-tool predicate.
#[cfg(unix)]
fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path) // follows symlinks, like the shell does
        .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable_file(path: &Path) -> bool {
    path.is_file()
}

/// Pure: first-wins dedup by name, preserving order — mirrors shell PATH
/// resolution where the earliest directory shadows later ones.
pub fn dedup_first_wins(entries: Vec<(String, PathBuf)>) -> Vec<(String, PathBuf)> {
    let mut seen = HashSet::new();
    entries
        .into_iter()
        .filter(|(name, _)| seen.insert(name.clone()))
        .collect()
}

/// Scan every directory in a PATH-style string for executables, first-wins.
pub fn scan_path_tools(path_var: &str) -> Vec<(String, PathBuf)> {
    let mut found = Vec::new();
    for dir in std::env::split_paths(path_var) {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !is_executable_file(&path) {
                continue;
            }
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                found.push((name.to_string(), path));
            }
            if found.len() >= MAX_TOOLS {
                log::warn!("inventory: PATH scan hit the {MAX_TOOLS}-entry cap; truncating");
                return dedup_first_wins(found);
            }
        }
    }
    dedup_first_wins(found)
}

/// One full scan → the rows a refresh writes. GUI apps dedup first-wins
/// across roots too (a user-installed copy shadows the system one).
pub fn scan_all(now_ms: i64) -> Vec<NewInventoryEntry> {
    let mut apps = Vec::new();
    for root in app_roots() {
        scan_apps_under(&root, 1, &mut apps);
    }
    let apps = dedup_first_wins(apps);
    let tools = scan_path_tools(&std::env::var("PATH").unwrap_or_default());
    let entry = |(name, path): (String, PathBuf), kind: &str| NewInventoryEntry {
        name,
        path: path.to_string_lossy().into_owned(),
        kind: kind.into(),
        refreshed_at_ms: now_ms,
    };
    apps.into_iter()
        .map(|pair| entry(pair, "app"))
        .chain(tools.into_iter().map(|pair| entry(pair, "cli")))
        .collect()
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Run one refresh against the store. Non-fatal by contract: any failure
/// logs and keeps the previous cache.
fn refresh_once(store: &MemoryStore) {
    let entries = scan_all(now_ms());
    match store.inventory_replace(&entries) {
        Ok(count) => log::info!("inventory: refreshed ({count} programs)"),
        Err(e) => log::warn!("inventory: refresh failed (stale cache kept): {e:?}"),
    }
}

/// Spawn the unattended refresh loop: one scan at startup (off the main
/// thread — the scan walks thousands of dirents), then every 24 h. Skips
/// silently-but-logged when the store never opened.
pub fn spawn(app: &tauri::AppHandle) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        loop {
            let store = app.state::<MemoryState>().store();
            match store {
                Some(store) => {
                    let _ =
                        tauri::async_runtime::spawn_blocking(move || refresh_once(&store)).await;
                }
                None => log::debug!("inventory: store unavailable; refresh skipped"),
            }
            tokio::time::sleep(std::time::Duration::from_secs(REFRESH_INTERVAL_SECS)).await;
        }
    });
}

/// Inventory health snapshot (health-as-value, never rejects).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InventoryStatus {
    pub apps: usize,
    pub tools: usize,
    pub last_refresh_ms: Option<i64>,
    pub error: Option<String>,
}

#[tauri::command]
pub fn inventory_status(memory: tauri::State<'_, MemoryState>) -> InventoryStatus {
    let Some(store) = memory.store() else {
        return InventoryStatus {
            apps: 0,
            tools: 0,
            last_refresh_ms: None,
            error: Some("memory store unavailable".into()),
        };
    };
    match store.inventory_counts() {
        Ok((apps, tools, last_refresh_ms)) => InventoryStatus {
            apps,
            tools,
            last_refresh_ms,
            error: None,
        },
        Err(e) => InventoryStatus {
            apps: 0,
            tools: 0,
            last_refresh_ms: None,
            error: Some(format!("{e:?}")),
        },
    }
}

/// Search the cache (Settings/debug surface; the model uses find_programs).
#[tauri::command]
pub fn inventory_search(
    memory: tauri::State<'_, MemoryState>,
    query: String,
    limit: Option<usize>,
) -> Result<Vec<InventoryEntry>, String> {
    let store = memory.store().ok_or("memory store unavailable")?;
    store
        .inventory_search(&query, limit.unwrap_or(25).min(200))
        .map_err(|e| format!("{e:?}"))
}

/// Re-scan on demand; resolves with the resulting status.
#[tauri::command]
pub async fn refresh_inventory(app: tauri::AppHandle) -> InventoryStatus {
    let store = app.state::<MemoryState>().store();
    if let Some(store) = store {
        let _ = tauri::async_runtime::spawn_blocking(move || refresh_once(&store)).await;
    }
    inventory_status(app.state::<MemoryState>())
}

/// The model-facing discovery tool: name-substring search over the cache.
pub struct FindProgramsTool {
    store: Option<Arc<MemoryStore>>,
}

impl FindProgramsTool {
    pub fn new(store: Option<Arc<MemoryStore>>) -> Self {
        Self { store }
    }

    pub fn definition() -> ToolDefinition {
        ToolDefinition {
            name: FIND_PROGRAMS_TOOL.into(),
            description: "Search the cached inventory of programs installed on this machine — \
                          GUI applications and command-line tools. Call this BEFORE claiming \
                          something is or is not installed, before focus_app (to get the app's \
                          real name), and before run_command (to confirm a CLI tool exists). \
                          Returns matches as `kind\\tname\\tpath` lines; an empty result means \
                          nothing matching that name is installed."
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Program name or fragment, e.g. \"chrome\", \"ffmpeg\", \"docker\"."
                    }
                },
                "required": ["query"]
            }),
        }
    }
}

#[derive(serde::Deserialize)]
struct FindProgramsArgs {
    query: String,
}

#[async_trait::async_trait]
impl ToolExecutor for FindProgramsTool {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![Self::definition()]
    }

    async fn execute(&self, call: &ToolCall) -> ToolOutcome {
        if call.name != FIND_PROGRAMS_TOOL {
            return ToolOutcome::failure(
                "unknown-tool",
                format!(
                    "unknown tool: {} (available: {FIND_PROGRAMS_TOOL})",
                    call.name
                ),
            );
        }
        let args: FindProgramsArgs = match serde_json::from_str(&call.arguments) {
            Ok(args) => args,
            Err(e) => {
                return ToolOutcome::failure(
                    "invalid-arguments",
                    format!("invalid {FIND_PROGRAMS_TOOL} arguments: {e}"),
                )
            }
        };
        let Some(store) = &self.store else {
            return ToolOutcome::failure(
                "unavailable",
                "the program inventory is unavailable (memory store never opened)",
            );
        };
        match store.inventory_search(&args.query, 20) {
            Ok(matches) if matches.is_empty() => ToolOutcome::success(format!(
                "No installed program matches {:?}. The inventory covers GUI apps and PATH \
                 tools; the name may differ or it may not be installed.",
                args.query
            )),
            Ok(matches) => {
                let lines: Vec<String> = matches
                    .iter()
                    .map(|m| format!("{}\t{}\t{}", m.kind, m.name, m.path))
                    .collect();
                ToolOutcome::success(lines.join("\n"))
            }
            Err(e) => ToolOutcome::failure("db", format!("inventory search failed: {e:?}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dedup_first_wins_mirrors_shell_path_resolution() {
        let entries = vec![
            ("git".into(), PathBuf::from("/usr/local/bin/git")),
            ("git".into(), PathBuf::from("/usr/bin/git")),
            ("ls".into(), PathBuf::from("/bin/ls")),
        ];
        let deduped = dedup_first_wins(entries);
        assert_eq!(deduped.len(), 2);
        assert_eq!(deduped[0].1, PathBuf::from("/usr/local/bin/git"));
    }

    #[test]
    fn scan_path_tools_finds_executables_and_skips_the_rest() {
        let dir = std::env::temp_dir().join(format!("te-inv-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("mytool");
        let plain = dir.join("notes.txt");
        std::fs::write(&exe, "#!/bin/sh\n").unwrap();
        std::fs::write(&plain, "not a tool").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let tools = scan_path_tools(dir.to_str().unwrap());
        let names: Vec<&str> = tools.iter().map(|(n, _)| n.as_str()).collect();
        assert!(
            names.contains(&"mytool"),
            "executable must be found: {names:?}"
        );
        #[cfg(unix)]
        assert!(
            !names.contains(&"notes.txt"),
            "non-executable must be skipped"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_path_dirs_are_skipped_not_fatal() {
        assert!(scan_path_tools("/definitely/not/a/dir").is_empty());
        assert!(scan_path_tools("").is_empty());
    }
}
