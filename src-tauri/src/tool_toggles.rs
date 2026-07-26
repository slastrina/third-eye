//! Per-tool on/off switches (user request 2026-07-26): every built-in LLM
//! tool is listed in Settings and can be disabled individually.
//!
//! Enforcement is structural (the D038 pattern, same as the HID arm gate):
//! [`ToggleGatedExecutor`] wraps every built-in executor in the composite —
//! a disabled tool contributes NO definition (the model is never offered
//! it) and any execute that still names it is refused typed BEFORE the
//! inner tool is touched. This layer composes with the tools' own gates
//! (HID run mode, the commands toggle): a tool must pass both.
//!
//! The registry below is the single source of truth for what "every
//! built-in tool" means; unknown names (MCP tools ride the same composite)
//! are always enabled here — their lifecycle is the MCP server's, not this
//! switchboard's.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use serde::Serialize;

use crate::llm::toolloop::{ToolExecutor, ToolOutcome};
use crate::llm::{ToolCall, ToolDefinition};

/// Every built-in tool: (wire name, human label, one-line Settings blurb).
pub const BUILTIN_TOOLS: &[(&str, &str, &str)] = &[
    (
        crate::llm::toolloop::FOCUS_APP_TOOL,
        "Focus app",
        "Open an app or bring it to the front",
    ),
    (
        crate::llm::toolloop::SCREEN_QUERY_TOOL,
        "Read the screen",
        "Read on-screen text with click coordinates",
    ),
    (
        crate::llm::toolloop::INPUT_ACTION_TOOL,
        "Mouse & keyboard",
        "Click, type, scroll, drag (also gated by the HID mode)",
    ),
    (
        crate::capture::screenshot_tool::TAKE_SCREENSHOT_TOOL,
        "Screenshots",
        "Capture the screen to look at it; saves only when asked",
    ),
    (
        crate::llm::toolloop::MEMORY_SEARCH_TOOL,
        "Memory search",
        "Search stored activity and conversation memories",
    ),
    (
        crate::llm::toolloop::CHAT_HISTORY_SEARCH_TOOL,
        "Past chats",
        "Search verbatim transcripts of earlier chat sessions",
    ),
    (
        crate::inventory::FIND_PROGRAMS_TOOL,
        "Installed programs",
        "Search what is installed on this machine",
    ),
    (
        crate::command_runner::RUN_COMMAND_TOOL,
        "Terminal commands",
        "Run one shell command (also gated by the commands switch)",
    ),
    (
        crate::clipboard_tool::CLIPBOARD_TOOL,
        "Clipboard",
        "Read or write the clipboard (each read/write is approved)",
    ),
    (
        crate::clipboard_tool::WAIT_TOOL,
        "Wait",
        "Pause briefly for the screen to settle",
    ),
];

/// Whether `name` is one of the built-in tools this switchboard governs.
pub fn is_builtin_tool(name: &str) -> bool {
    BUILTIN_TOOLS.iter().any(|(n, _, _)| *n == name)
}

/// The shared switchboard: Settings mutates it, every chat run's composite
/// executor reads it live (an Arc — mid-run toggles apply to the next tool
/// call, matching the HID arm state's behavior).
#[derive(Default)]
pub struct ToolToggles {
    disabled: Mutex<HashSet<String>>,
    persist_error: Mutex<Option<String>>,
}

impl ToolToggles {
    pub fn new() -> Self {
        Self::default()
    }

    /// Enabled means "not explicitly disabled": unknown (MCP) names pass.
    pub fn is_enabled(&self, name: &str) -> bool {
        !self.disabled.lock().unwrap().contains(name)
    }

    /// Flip one KNOWN tool. Returns whether the name was known (an unknown
    /// name changes nothing — the registry is the contract).
    pub fn set_enabled(&self, name: &str, enabled: bool) -> bool {
        if !is_builtin_tool(name) {
            return false;
        }
        let mut disabled = self.disabled.lock().unwrap();
        if enabled {
            disabled.remove(name);
        } else {
            disabled.insert(name.to_string());
        }
        true
    }

    /// Replace the whole disabled set (startup applier). Unknown names are
    /// dropped — a stale persisted name from a removed tool cannot linger.
    pub fn apply_disabled(&self, names: &[String]) {
        let mut disabled = self.disabled.lock().unwrap();
        disabled.clear();
        disabled.extend(
            names
                .iter()
                .filter(|n| is_builtin_tool(n))
                .map(|n| n.to_string()),
        );
    }

    /// The disabled names, sorted for stable persistence.
    pub fn disabled_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.disabled.lock().unwrap().iter().cloned().collect();
        names.sort();
        names
    }

    pub fn set_persist_error(&self, error: Option<String>) {
        *self.persist_error.lock().unwrap() = error;
    }

    /// Settings snapshot — health-as-value, never an error.
    pub fn status(&self) -> ToolTogglesStatus {
        ToolTogglesStatus {
            tools: BUILTIN_TOOLS
                .iter()
                .map(|(name, label, description)| ToolToggleRow {
                    name: (*name).into(),
                    label: (*label).into(),
                    description: (*description).into(),
                    enabled: self.is_enabled(name),
                })
                .collect(),
            persist_error: self.persist_error.lock().unwrap().clone(),
        }
    }
}

/// One Settings row.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolToggleRow {
    pub name: String,
    pub label: String,
    pub description: String,
    pub enabled: bool,
}

/// The `tool_toggles_status` / `set_tool_enabled` IPC shape.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolTogglesStatus {
    pub tools: Vec<ToolToggleRow>,
    pub persist_error: Option<String>,
}

/// The structural gate: wraps one built-in executor; disabled tools vanish
/// from the advertised definitions AND refuse execution typed.
pub struct ToggleGatedExecutor {
    inner: Box<dyn ToolExecutor>,
    toggles: Arc<ToolToggles>,
}

impl ToggleGatedExecutor {
    pub fn new(inner: Box<dyn ToolExecutor>, toggles: Arc<ToolToggles>) -> Self {
        Self { inner, toggles }
    }
}

#[async_trait::async_trait]
impl ToolExecutor for ToggleGatedExecutor {
    fn definitions(&self) -> Vec<ToolDefinition> {
        self.inner
            .definitions()
            .into_iter()
            .filter(|d| self.toggles.is_enabled(&d.name))
            .collect()
    }

    async fn execute(&self, call: &ToolCall) -> ToolOutcome {
        if !self.toggles.is_enabled(&call.name) {
            return ToolOutcome::failure(
                "disabled",
                format!(
                    "the user turned the {} tool off in Third Eye's Settings — it did not run. \
                     Do not retry; work without it or tell the user it is disabled",
                    call.name
                ),
            );
        }
        self.inner.execute(call).await
    }
}

// ---------------------------------------------------------------------------
// IPC + applier (commands.rs-style, kept here — the module is small)
// ---------------------------------------------------------------------------

/// The one shared applier: flip, persist, roll back on persist failure,
/// return the authoritative status.
pub fn apply_tool_enabled(
    app: &tauri::AppHandle,
    toggles: &ToolToggles,
    name: &str,
    enable: bool,
) -> ToolTogglesStatus {
    if !toggles.set_enabled(name, enable) {
        log::warn!("tools: set_tool_enabled ignored unknown tool {name:?}");
        return toggles.status();
    }
    match crate::config::save_disabled_tools(app, &toggles.disabled_names()) {
        Ok(()) => {
            toggles.set_persist_error(None);
            log::info!(
                "tools: {name} {} (via ipc)",
                if enable { "enabled" } else { "disabled" }
            );
        }
        Err(e) => {
            // Roll back: an unpersisted toggle must not silently revert on
            // restart (the nudges/commands applier contract).
            toggles.set_enabled(name, !enable);
            log::error!("tools: {e}");
            toggles.set_persist_error(Some(e));
        }
    }
    toggles.status()
}

/// Apply the persisted disabled set at startup (in-memory only).
pub fn apply_persisted_tool_toggles(app: &tauri::AppHandle) {
    use tauri::Manager;
    let disabled = crate::config::load_disabled_tools(app);
    if !disabled.is_empty() {
        log::info!("tools: applied persisted disabled tools: {disabled:?}");
    }
    app.state::<Arc<ToolToggles>>().apply_disabled(&disabled);
}

#[tauri::command]
pub fn tool_toggles_status(toggles: tauri::State<'_, Arc<ToolToggles>>) -> ToolTogglesStatus {
    toggles.status()
}

#[tauri::command]
pub fn set_tool_enabled(
    app: tauri::AppHandle,
    toggles: tauri::State<'_, Arc<ToolToggles>>,
    name: String,
    enable: bool,
) -> ToolTogglesStatus {
    apply_tool_enabled(&app, &toggles, &name, enable)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeTool;

    #[async_trait::async_trait]
    impl ToolExecutor for FakeTool {
        fn definitions(&self) -> Vec<ToolDefinition> {
            vec![ToolDefinition {
                name: crate::clipboard_tool::WAIT_TOOL.into(),
                description: "test".into(),
                parameters: serde_json::json!({}),
            }]
        }

        async fn execute(&self, _call: &ToolCall) -> ToolOutcome {
            ToolOutcome::success("ran")
        }
    }

    fn wait_call() -> ToolCall {
        ToolCall {
            id: "c1".into(),
            name: crate::clipboard_tool::WAIT_TOOL.into(),
            arguments: "{}".into(),
        }
    }

    #[test]
    fn every_builtin_name_is_unique_and_known() {
        let mut seen = HashSet::new();
        for (name, label, blurb) in BUILTIN_TOOLS {
            assert!(seen.insert(*name), "duplicate registry entry {name}");
            assert!(!label.is_empty() && !blurb.is_empty());
            assert!(is_builtin_tool(name));
        }
        assert!(!is_builtin_tool("mcp_thing"));
    }

    #[test]
    fn toggles_only_govern_known_names_and_round_trip() {
        let t = ToolToggles::new();
        assert!(t.is_enabled("wait"));
        assert!(t.set_enabled("wait", false));
        assert!(!t.is_enabled("wait"));
        assert_eq!(t.disabled_names(), vec!["wait".to_string()]);
        // Unknown names: never disabled, never stored.
        assert!(!t.set_enabled("mcp_thing", false));
        assert!(t.is_enabled("mcp_thing"));
        // The applier path re-enables.
        assert!(t.set_enabled("wait", true));
        assert!(t.disabled_names().is_empty());
        // apply_disabled drops stale/unknown persisted names.
        t.apply_disabled(&["wait".into(), "gone_tool".into()]);
        assert_eq!(t.disabled_names(), vec!["wait".to_string()]);
    }

    #[tokio::test]
    async fn gate_hides_the_definition_and_refuses_execution_typed() {
        let toggles = Arc::new(ToolToggles::new());
        let gated = ToggleGatedExecutor::new(Box::new(FakeTool), toggles.clone());
        assert_eq!(gated.definitions().len(), 1);
        assert!(gated.execute(&wait_call()).await.ok);

        toggles.set_enabled("wait", false);
        // Structurally inert: no definition offered…
        assert!(gated.definitions().is_empty());
        // …and a call that still names it is refused before the inner tool.
        let outcome = gated.execute(&wait_call()).await;
        assert!(!outcome.ok);
        assert_eq!(outcome.failure.as_deref(), Some("disabled"));
        assert!(outcome.content.contains("Settings"));
    }

    #[test]
    fn status_lists_every_builtin_with_its_live_state() {
        let t = ToolToggles::new();
        t.set_enabled("clipboard", false);
        let status = t.status();
        assert_eq!(status.tools.len(), BUILTIN_TOOLS.len());
        let clipboard = status.tools.iter().find(|r| r.name == "clipboard").unwrap();
        assert!(!clipboard.enabled);
        assert!(status
            .tools
            .iter()
            .filter(|r| r.name != "clipboard")
            .all(|r| r.enabled));
    }
}
