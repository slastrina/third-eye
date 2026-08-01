//! Persisted app settings: a tauri-plugin-store JSON file (settings.json)
//! in the app data dir. T04 stores the configurable hotkey here; S07 reuses
//! the same store for lane models and privacy mode. Fully local (R021).
//!
//! Failure policy: loading never blocks startup — an unreadable store or a
//! malformed value is logged and the caller falls back to defaults; saving
//! returns a typed error naming the persist path so `set_hotkey` can keep
//! the old binding and surface the failure (Q5).

use tauri::{AppHandle, Manager};
use tauri_plugin_store::StoreExt;

/// Store file name, resolved by the plugin relative to the app data dir.
pub const SETTINGS_STORE: &str = "settings.json";

/// Store key holding the global hotkey shortcut string.
pub const HOTKEY_KEY: &str = "hotkey";

/// Store keys holding the lane model pins (S07) — camelCase like every JSON
/// field of the IPC surface. Tri-state contract: key absent → fall back to
/// the THIRD_EYE_* env var; explicit JSON null → explicitly unpinned;
/// string → pinned to that model id.
pub const THIN_MODEL_KEY: &str = "thinModel";
pub const HEAVY_MODEL_KEY: &str = "heavyModel";
pub const CODER_MODEL_KEY: &str = "coderModel";

/// Store key for the routing mode: "auto" (default — requests pick their
/// lane) or "manual" (the chips pin one lane).
pub const ROUTER_MODE_KEY: &str = "routerMode";

/// Read the persisted routing mode; anything but "manual" is auto.
pub fn load_router_auto(app: &AppHandle) -> bool {
    let Ok(store) = app.store(SETTINGS_STORE) else {
        return true;
    };
    match store
        .get(ROUTER_MODE_KEY)
        .and_then(|v| v.as_str().map(String::from))
    {
        Some(mode) => mode != "manual",
        None => true,
    }
}

/// Persist the routing mode.
pub fn save_router_auto(app: &AppHandle, auto: bool) -> Result<(), String> {
    let path = store_path(app);
    let store = app
        .store(SETTINGS_STORE)
        .map_err(|e| format!("failed to open settings store at {path}: {e}"))?;
    store.set(
        ROUTER_MODE_KEY,
        serde_json::json!(if auto { "auto" } else { "manual" }),
    );
    store
        .save()
        .map_err(|e| format!("failed to persist {ROUTER_MODE_KEY} to {path}: {e}"))?;
    Ok(())
}

/// Store key holding the privacy-mode toggle (S07). Absent means off — the
/// default; there is no env fallback for privacy.
pub const PRIVACY_MODE_KEY: &str = "privacyMode";

/// Store key holding the continuous-watcher toggle (M002 S01). Absent means
/// off — the default; there is no env fallback.
pub const WATCHER_ENABLED_KEY: &str = "watcherEnabled";

/// The store key for a lane name, or `None` for a lane with no persistence
/// slot (the settings surface only knows thin/heavy).
pub fn lane_model_key(lane: &str) -> Option<&'static str> {
    match lane {
        crate::llm::router::THIN_LANE => Some(THIN_MODEL_KEY),
        crate::llm::router::CODER_LANE => Some(CODER_MODEL_KEY),
        crate::llm::router::HEAVY_LANE => Some(HEAVY_MODEL_KEY),
        _ => None,
    }
}

/// Full persist path for error messages — logs must name where persistence
/// failed, not just that it did.
fn store_path(app: &AppHandle) -> String {
    app.path()
        .app_data_dir()
        .map(|dir| dir.join(SETTINGS_STORE).display().to_string())
        .unwrap_or_else(|_| SETTINGS_STORE.into())
}

/// Read the persisted hotkey. `None` means "nothing usable persisted" (no
/// store, no key): the caller uses the default. A present but non-string
/// value is returned as its JSON text so the shortcut parser rejects it and
/// the startup fallback names the bad value in `HotkeyStatus.error`.
pub fn load_hotkey(app: &AppHandle) -> Option<String> {
    let store = match app.store(SETTINGS_STORE) {
        Ok(store) => store,
        Err(e) => {
            log::error!(
                "config: failed to open settings store at {}: {e}",
                store_path(app)
            );
            return None;
        }
    };
    let value = store.get(HOTKEY_KEY)?;
    Some(
        value
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| value.to_string()),
    )
}

/// Persist the hotkey. The error names the failed persist path; the caller
/// (hotkey::rebind) rolls the registration back so an unpersisted binding
/// can never silently revert on restart.
pub fn save_hotkey(app: &AppHandle, shortcut: &str) -> Result<(), String> {
    let path = store_path(app);
    let store = app
        .store(SETTINGS_STORE)
        .map_err(|e| format!("failed to open settings store at {path}: {e}"))?;
    store.set(HOTKEY_KEY, serde_json::json!(shortcut));
    store
        .save()
        .map_err(|e| format!("failed to persist {HOTKEY_KEY}='{shortcut}' to {path}: {e}"))?;
    log::info!("config: persisted {HOTKEY_KEY}='{shortcut}' to {path}");
    Ok(())
}

/// Read the persisted model pin for `key` ([`THIN_MODEL_KEY`] /
/// [`HEAVY_MODEL_KEY`]). Outer `None` means the key is absent (or the store
/// is unreadable, which is logged): the caller keeps its THIRD_EYE_* env
/// fallback. `Some(pin)` means the store has spoken and wins: `Some(None)`
/// is explicitly unpinned (JSON null), `Some(Some(id))` a pinned model.
pub fn load_lane_model(app: &AppHandle, key: &str) -> Option<Option<String>> {
    let store = match app.store(SETTINGS_STORE) {
        Ok(store) => store,
        Err(e) => {
            log::error!(
                "config: failed to open settings store at {}: {e}",
                store_path(app)
            );
            return None;
        }
    };
    let value = store.get(key)?;
    Some(stored_model_pin(key, &value))
}

/// Interpret one stored lane-model value. Strings pin (trimmed); null and
/// blank strings unpin; a non-string value is logged and treated as unpinned
/// rather than pinning a garbage model id.
fn stored_model_pin(key: &str, value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(s) => {
            let trimmed = s.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_string())
        }
        serde_json::Value::Null => None,
        other => {
            log::warn!("config: {key} holds non-string value {other}; treating as unpinned");
            None
        }
    }
}

/// Persist a lane's model pin under its store key (`None` writes an explicit
/// JSON null — "unpinned" is a decision, not an absence, so it must win over
/// the env fallback on the next start). The error names the failed persist
/// path; the caller rolls the in-memory re-pin back so an unpersisted pin
/// can never silently revert on restart.
pub fn save_lane_model(app: &AppHandle, lane: &str, model: Option<&str>) -> Result<(), String> {
    let key = lane_model_key(lane)
        .ok_or_else(|| format!("no settings key for lane \"{lane}\" (thin/heavy only)"))?;
    let path = store_path(app);
    let store = app
        .store(SETTINGS_STORE)
        .map_err(|e| format!("failed to open settings store at {path}: {e}"))?;
    store.set(key, serde_json::json!(model));
    store
        .save()
        .map_err(|e| format!("failed to persist {key}={model:?} to {path}: {e}"))?;
    log::info!("config: persisted {key}={model:?} to {path}");
    Ok(())
}

/// Read the persisted privacy-mode toggle. `None` means nothing usable is
/// persisted (no store, no key — both logged where relevant): the caller
/// keeps the default (off).
pub fn load_privacy_mode(app: &AppHandle) -> Option<bool> {
    let store = match app.store(SETTINGS_STORE) {
        Ok(store) => store,
        Err(e) => {
            log::error!(
                "config: failed to open settings store at {}: {e}",
                store_path(app)
            );
            return None;
        }
    };
    let value = store.get(PRIVACY_MODE_KEY)?;
    Some(stored_privacy_mode(&value))
}

/// Interpret one stored privacy-mode value. Only a JSON boolean is trusted;
/// anything else is logged and treated as off rather than silently blocking
/// every capture on garbage data.
fn stored_privacy_mode(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Bool(b) => *b,
        other => {
            log::warn!(
                "config: {PRIVACY_MODE_KEY} holds non-boolean value {other}; treating as off"
            );
            false
        }
    }
}

/// Persist the privacy-mode toggle. The error names the failed persist path;
/// the caller rolls the in-memory toggle back so an unpersisted privacy
/// state can never silently revert on restart (hotkey precedent).
pub fn save_privacy_mode(app: &AppHandle, enabled: bool) -> Result<(), String> {
    let path = store_path(app);
    let store = app
        .store(SETTINGS_STORE)
        .map_err(|e| format!("failed to open settings store at {path}: {e}"))?;
    store.set(PRIVACY_MODE_KEY, serde_json::json!(enabled));
    store
        .save()
        .map_err(|e| format!("failed to persist {PRIVACY_MODE_KEY}={enabled} to {path}: {e}"))?;
    log::info!("config: persisted {PRIVACY_MODE_KEY}={enabled} to {path}");
    Ok(())
}

/// Read the persisted watcher toggle. `None` means nothing usable is
/// persisted (no store, no key — both logged where relevant): the caller
/// keeps the default (off).
pub fn load_watcher_enabled(app: &AppHandle) -> Option<bool> {
    let store = match app.store(SETTINGS_STORE) {
        Ok(store) => store,
        Err(e) => {
            log::error!(
                "config: failed to open settings store at {}: {e}",
                store_path(app)
            );
            return None;
        }
    };
    let value = store.get(WATCHER_ENABLED_KEY)?;
    Some(stored_watcher_enabled(&value))
}

/// Interpret one stored watcher-toggle value. Only a JSON boolean is
/// trusted; anything else is logged and treated as off rather than silently
/// starting continuous capture on garbage data.
fn stored_watcher_enabled(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Bool(b) => *b,
        other => {
            log::warn!(
                "config: {WATCHER_ENABLED_KEY} holds non-boolean value {other}; treating as off"
            );
            false
        }
    }
}

/// Persist the watcher toggle. The error names the failed persist path; the
/// caller (the watcher applier) rolls the in-memory toggle back so an
/// unpersisted watcher state can never silently revert on restart.
pub fn save_watcher_enabled(app: &AppHandle, enabled: bool) -> Result<(), String> {
    let path = store_path(app);
    let store = app
        .store(SETTINGS_STORE)
        .map_err(|e| format!("failed to open settings store at {path}: {e}"))?;
    store.set(WATCHER_ENABLED_KEY, serde_json::json!(enabled));
    store
        .save()
        .map_err(|e| format!("failed to persist {WATCHER_ENABLED_KEY}={enabled} to {path}: {e}"))?;
    log::info!("config: persisted {WATCHER_ENABLED_KEY}={enabled} to {path}");
    Ok(())
}

/// Store key holding the cloud opt-in toggle (M004 S03). Absent means off —
/// the default; there is no env fallback. Opt-in is the single gate that lets
/// any remote-provider client be constructed, so garbage in the store must
/// fail safe to off (never silently enable cloud egress).
pub const CLOUD_OPTIN_KEY: &str = "cloudOptin";

/// Read the persisted cloud opt-in toggle. `None` means nothing usable is
/// persisted (no store, no key — both logged where relevant): the caller
/// keeps the default (off).
pub fn load_cloud_optin(app: &AppHandle) -> Option<bool> {
    let store = match app.store(SETTINGS_STORE) {
        Ok(store) => store,
        Err(e) => {
            log::error!(
                "config: failed to open settings store at {}: {e}",
                store_path(app)
            );
            return None;
        }
    };
    let value = store.get(CLOUD_OPTIN_KEY)?;
    Some(stored_cloud_optin(&value))
}

/// Interpret one stored opt-in value. Only a JSON boolean is trusted;
/// anything else is logged and treated as off rather than silently enabling
/// remote providers on garbage data — off is the only safe fallback for the
/// cloud-egress gate.
fn stored_cloud_optin(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Bool(b) => *b,
        other => {
            log::warn!(
                "config: {CLOUD_OPTIN_KEY} holds non-boolean value {other}; treating as off"
            );
            false
        }
    }
}

/// Persist the cloud opt-in toggle. The error names the failed persist path;
/// the caller (the opt-in applier) rolls the in-memory toggle back so an
/// unpersisted opt-in can never silently revert on restart (hotkey precedent).
pub fn save_cloud_optin(app: &AppHandle, enabled: bool) -> Result<(), String> {
    let path = store_path(app);
    let store = app
        .store(SETTINGS_STORE)
        .map_err(|e| format!("failed to open settings store at {path}: {e}"))?;
    store.set(CLOUD_OPTIN_KEY, serde_json::json!(enabled));
    store
        .save()
        .map_err(|e| format!("failed to persist {CLOUD_OPTIN_KEY}={enabled} to {path}: {e}"))?;
    log::info!("config: persisted {CLOUD_OPTIN_KEY}={enabled} to {path}");
    Ok(())
}

/// Store key holding the HID arming toggle (M005 S03, D038). Absent means off
/// — the default; there is no env fallback. Arming is the single gate that lets
/// the model be offered the input tool and lets any input action touch the
/// InputControl backend, so garbage in the store must fail safe to off (never
/// silently arm a capability that can click and type anywhere, R019).
pub const HID_ENABLED_KEY: &str = "hidEnabled";

/// Read the persisted HID arming toggle. `None` means nothing usable is
/// persisted (no store, no key — both logged where relevant): the caller
/// keeps the default (off, disarmed).
pub fn load_hid_enabled(app: &AppHandle) -> Option<bool> {
    let store = match app.store(SETTINGS_STORE) {
        Ok(store) => store,
        Err(e) => {
            log::error!(
                "config: failed to open settings store at {}: {e}",
                store_path(app)
            );
            return None;
        }
    };
    let value = store.get(HID_ENABLED_KEY)?;
    Some(stored_hid_enabled(&value))
}

/// Interpret one stored HID-arming value. Only a JSON boolean is trusted;
/// anything else is logged and treated as off (disarmed) rather than silently
/// arming a capability that can click and type anywhere on garbage data — off
/// is the only safe fallback for the HID gate (D038, R019).
fn stored_hid_enabled(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Bool(b) => *b,
        other => {
            log::warn!(
                "config: {HID_ENABLED_KEY} holds non-boolean value {other}; treating as off"
            );
            false
        }
    }
}

/// Persist the HID arming toggle. The error names the failed persist path; the
/// caller (the HID applier) rolls the in-memory toggle back so an unpersisted
/// arming state can never silently revert on restart (hotkey precedent).
pub fn save_hid_enabled(app: &AppHandle, enabled: bool) -> Result<(), String> {
    let path = store_path(app);
    let store = app
        .store(SETTINGS_STORE)
        .map_err(|e| format!("failed to open settings store at {path}: {e}"))?;
    store.set(HID_ENABLED_KEY, serde_json::json!(enabled));
    store
        .save()
        .map_err(|e| format!("failed to persist {HID_ENABLED_KEY}={enabled} to {path}: {e}"))?;
    log::info!("config: persisted {HID_ENABLED_KEY}={enabled} to {path}");
    Ok(())
}

/// Store key holding the HID run mode (M005 S04, D038) — the three-way
/// successor to [`HID_ENABLED_KEY`]. Absent means `off` — the default and the
/// structurally-inert state; there is no env fallback. The value is the
/// kebab-case wire name of [`crate::input::commands::HidRunMode`]
/// (`off`/`ask`/`auto-run`), so garbage in the store must fail safe to `off`
/// (never silently arm a capability that can click and type anywhere, R019).
pub const HID_RUN_MODE_KEY: &str = "hidRunMode";

/// Read the persisted HID run mode. `None` means nothing usable is persisted
/// (no store, no key — both logged where relevant): the caller keeps the default
/// (`off`, disarmed).
pub fn load_hid_run_mode(app: &AppHandle) -> Option<crate::input::commands::HidRunMode> {
    let store = match app.store(SETTINGS_STORE) {
        Ok(store) => store,
        Err(e) => {
            log::error!(
                "config: failed to open settings store at {}: {e}",
                store_path(app)
            );
            return None;
        }
    };
    let value = store.get(HID_RUN_MODE_KEY)?;
    Some(stored_hid_run_mode(&value))
}

/// Interpret one stored HID run-mode value. Only a recognized kebab-case mode
/// tag (`off`/`ask`/`auto-run`) is trusted; anything else — an unknown string,
/// a non-string, or null — is logged and treated as `off` (disarmed) rather than
/// silently arming a capability that can click and type anywhere on garbage
/// data. `off` is the only safe fallback for the HID gate (D038, R019).
fn stored_hid_run_mode(value: &serde_json::Value) -> crate::input::commands::HidRunMode {
    match serde_json::from_value::<crate::input::commands::HidRunMode>(value.clone()) {
        Ok(mode) => mode,
        Err(_) => {
            log::warn!(
                "config: {HID_RUN_MODE_KEY} holds unrecognized value {value}; treating as off"
            );
            crate::input::commands::HidRunMode::Off
        }
    }
}

/// Persist the HID run mode. The error names the failed persist path; the caller
/// (the HID applier) rolls the in-memory mode back so an unpersisted choice can
/// never silently revert on restart (hotkey precedent).
pub fn save_hid_run_mode(
    app: &AppHandle,
    mode: crate::input::commands::HidRunMode,
) -> Result<(), String> {
    let path = store_path(app);
    let store = app
        .store(SETTINGS_STORE)
        .map_err(|e| format!("failed to open settings store at {path}: {e}"))?;
    let wire = serde_json::json!(mode);
    store.set(HID_RUN_MODE_KEY, wire.clone());
    store
        .save()
        .map_err(|e| format!("failed to persist {HID_RUN_MODE_KEY}={wire} to {path}: {e}"))?;
    log::info!("config: persisted {HID_RUN_MODE_KEY}={wire} to {path}");
    Ok(())
}

/// Store key holding the MCP run mode (M007 S04, D038 twin) — the three-way
/// gate for external MCP tool actions, the MCP twin of [`HID_RUN_MODE_KEY`].
/// Absent means `off` — the default and the structurally-inert state; there is
/// no env fallback. The value is the kebab-case wire name of
/// [`crate::llm::mcp::McpRunMode`] (`off`/`ask`/`auto-run`), so garbage in the
/// store must fail safe to `off` (never silently let an external server's tools
/// run without approval, R016).
pub const MCP_RUN_MODE_KEY: &str = "mcpRunMode";

/// Read the persisted MCP run mode. `None` means nothing usable is persisted
/// (no store, no key — both logged where relevant): the caller keeps the default
/// (`off`, inert).
pub fn load_mcp_run_mode(app: &AppHandle) -> Option<crate::llm::mcp::McpRunMode> {
    let store = match app.store(SETTINGS_STORE) {
        Ok(store) => store,
        Err(e) => {
            log::error!(
                "config: failed to open settings store at {}: {e}",
                store_path(app)
            );
            return None;
        }
    };
    let value = store.get(MCP_RUN_MODE_KEY)?;
    Some(stored_mcp_run_mode(&value))
}

/// Interpret one stored MCP run-mode value. Only a recognized kebab-case mode
/// tag (`off`/`ask`/`auto-run`) is trusted; anything else — an unknown string, a
/// non-string, or null — is logged and treated as `off` (inert) rather than
/// silently letting an external server's tools run without approval on garbage
/// data. `off` is the only safe fallback for the MCP gate (R016).
fn stored_mcp_run_mode(value: &serde_json::Value) -> crate::llm::mcp::McpRunMode {
    match serde_json::from_value::<crate::llm::mcp::McpRunMode>(value.clone()) {
        Ok(mode) => mode,
        Err(_) => {
            log::warn!(
                "config: {MCP_RUN_MODE_KEY} holds unrecognized value {value}; treating as off"
            );
            crate::llm::mcp::McpRunMode::Off
        }
    }
}

/// Persist the MCP run mode. The error names the failed persist path; the caller
/// (the MCP applier, T02) rolls the in-memory mode back so an unpersisted choice
/// can never silently revert on restart (hotkey precedent).
pub fn save_mcp_run_mode(app: &AppHandle, mode: crate::llm::mcp::McpRunMode) -> Result<(), String> {
    let path = store_path(app);
    let store = app
        .store(SETTINGS_STORE)
        .map_err(|e| format!("failed to open settings store at {path}: {e}"))?;
    let wire = serde_json::json!(mode);
    store.set(MCP_RUN_MODE_KEY, wire.clone());
    store
        .save()
        .map_err(|e| format!("failed to persist {MCP_RUN_MODE_KEY}={wire} to {path}: {e}"))?;
    log::info!("config: persisted {MCP_RUN_MODE_KEY}={wire} to {path}");
    Ok(())
}

/// Store key holding the user-configured external MCP servers (M007 S04). Absent
/// means the empty list — no external server is spawned, the default. The value
/// is a JSON array of [`crate::llm::mcp::McpServerConfig`] records (camelCase),
/// repaired item-by-item on load: a single corrupt entry is dropped, never
/// nuking a good sibling, and an absent key yields an empty list.
pub const MCP_SERVERS_KEY: &str = "mcpServers";

/// Read the persisted MCP server list. `None` means nothing usable is persisted
/// (no store, no key — both logged where relevant): the caller keeps the default
/// (empty — no external server). A present-but-partly-garbage array is repaired
/// entry-by-entry inside [`stored_mcp_servers`], never rejected whole.
pub fn load_mcp_servers(app: &AppHandle) -> Option<Vec<crate::llm::mcp::McpServerConfig>> {
    let store = match app.store(SETTINGS_STORE) {
        Ok(store) => store,
        Err(e) => {
            log::error!(
                "config: failed to open settings store at {}: {e}",
                store_path(app)
            );
            return None;
        }
    };
    let value = store.get(MCP_SERVERS_KEY)?;
    Some(stored_mcp_servers(&value))
}

/// Interpret one stored server-list value with per-entry repair (modelled on
/// [`stored_overlay_presentation`]'s field-by-field contract): a non-array value
/// collapses to the empty list; each array element is parsed by
/// [`stored_mcp_server`] and a corrupt entry is dropped and logged rather than
/// taking the whole list down (a corrupt entry never nukes a good sibling).
fn stored_mcp_servers(value: &serde_json::Value) -> Vec<crate::llm::mcp::McpServerConfig> {
    let Some(items) = value.as_array() else {
        log::warn!("config: {MCP_SERVERS_KEY} is not a JSON array (got {value}); using empty list");
        return Vec::new();
    };
    items.iter().filter_map(stored_mcp_server).collect()
}

/// Interpret one stored server entry with field-level repair, transport-aware
/// (S05). A valid entry needs a non-blank `id` plus, per its `transport`, either
/// a non-blank `command` (stdio — nothing to spawn otherwise) or a non-blank,
/// http(s)-parseable `url` (http — nothing to connect to otherwise); the entry is
/// dropped and logged when that is missing/invalid. The `transport` discriminator
/// fails safe to `stdio` when absent or unknown, so an S04 entry with no
/// `transport` key stays the local stdio server it was. `args` is repaired to
/// only its string elements (a non-array or non-string element is skipped, not
/// fatal); `enabled` trusts only a JSON boolean and fails safe to `false`; an
/// http entry's `authRef` is a trimmed non-secret reference (`None` when absent).
/// This keeps a corrupt field from dropping an otherwise-usable entry while a
/// structurally unusable entry (no id, or no usable spawn/connect target) is
/// dropped — never nuking a good sibling.
fn stored_mcp_server(value: &serde_json::Value) -> Option<crate::llm::mcp::McpServerConfig> {
    use crate::llm::mcp::McpTransport;
    let obj = match value.as_object() {
        Some(obj) => obj,
        None => {
            log::warn!(
                "config: {MCP_SERVERS_KEY} entry is not an object (got {value}); dropping it"
            );
            return None;
        }
    };
    let trimmed_str = |key: &str| {
        obj.get(key)
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let Some(id) = trimmed_str("id") else {
        log::warn!(
            "config: {MCP_SERVERS_KEY} entry has no non-blank id (got {value}); dropping it"
        );
        return None;
    };
    // Transport discriminator: only "http" opts into the remote path; absent
    // (S04 entries), "stdio", or any unknown value fails safe to the local stdio
    // path so a garbled discriminator never routes an entry onto the network.
    let transport = match obj.get("transport").and_then(serde_json::Value::as_str) {
        Some("http") => McpTransport::Http,
        Some("stdio") | None => McpTransport::Stdio,
        Some(other) => {
            log::warn!(
                "config: {MCP_SERVERS_KEY} entry {id} has unknown transport {other:?}; treating as stdio"
            );
            McpTransport::Stdio
        }
    };
    let args = match obj.get("args") {
        Some(serde_json::Value::Array(items)) => items
            .iter()
            .filter_map(|a| match a.as_str() {
                Some(s) => Some(s.to_string()),
                None => {
                    log::warn!(
                        "config: {MCP_SERVERS_KEY} entry {id} has a non-string arg {a}; skipping it"
                    );
                    None
                }
            })
            .collect(),
        Some(other) => {
            log::warn!(
                "config: {MCP_SERVERS_KEY} entry {id} has a non-array args {other}; treating as empty"
            );
            Vec::new()
        }
        None => Vec::new(),
    };
    let enabled = match obj.get("enabled") {
        Some(serde_json::Value::Bool(b)) => *b,
        None => false,
        Some(other) => {
            log::warn!(
                "config: {MCP_SERVERS_KEY} entry {id} has non-boolean enabled {other}; treating as disabled"
            );
            false
        }
    };
    match transport {
        McpTransport::Stdio => {
            let Some(command) = trimmed_str("command") else {
                log::warn!(
                    "config: {MCP_SERVERS_KEY} entry {id} (stdio) has no non-blank command (got {value}); dropping it"
                );
                return None;
            };
            // url/auth_ref are meaningless for stdio — repaired away to the
            // canonical stdio shape rather than carried as dead data.
            Some(crate::llm::mcp::McpServerConfig {
                id,
                command,
                args,
                enabled,
                transport,
                url: None,
                auth_ref: None,
            })
        }
        McpTransport::Http => {
            let Some(url) = trimmed_str("url") else {
                log::warn!(
                    "config: {MCP_SERVERS_KEY} entry {id} (http) has no non-blank url (got {value}); dropping it"
                );
                return None;
            };
            if !is_http_url(&url) {
                log::warn!(
                    "config: {MCP_SERVERS_KEY} entry {id} (http) has an invalid url {url}; dropping it"
                );
                return None;
            }
            // command is unused for http; carry any trimmed value but never let
            // its absence drop the entry.
            let command = trimmed_str("command").unwrap_or_default();
            Some(crate::llm::mcp::McpServerConfig {
                id,
                command,
                args,
                enabled,
                transport,
                url: Some(url),
                auth_ref: trimmed_str("authRef"),
            })
        }
    }
}

/// Whether `candidate` is a usable remote MCP endpoint: a syntactically valid
/// URL with an `http`/`https` scheme and a host. Rejects blanks, relative refs,
/// and non-web schemes (`ws://`, `file://`) so a corrupt http entry is dropped
/// by [`stored_mcp_server`] rather than handed to the transport at connect time.
fn is_http_url(candidate: &str) -> bool {
    match url::Url::parse(candidate) {
        Ok(parsed) => matches!(parsed.scheme(), "http" | "https") && parsed.host().is_some(),
        Err(_) => false,
    }
}

/// Persist the MCP server list as one composite JSON array. The error names the
/// failed persist path; the caller (the settings command, T04) rolls the
/// in-memory list back and surfaces the failure as data so an unpersisted change
/// can never silently revert on restart (hotkey/opt-in precedent).
pub fn save_mcp_servers(
    app: &AppHandle,
    servers: &[crate::llm::mcp::McpServerConfig],
) -> Result<(), String> {
    let path = store_path(app);
    let store = app
        .store(SETTINGS_STORE)
        .map_err(|e| format!("failed to open settings store at {path}: {e}"))?;
    let wire = serde_json::to_value(servers)
        .map_err(|e| format!("failed to serialize {MCP_SERVERS_KEY}: {e}"))?;
    store.set(MCP_SERVERS_KEY, wire.clone());
    store
        .save()
        .map_err(|e| format!("failed to persist {MCP_SERVERS_KEY}={wire} to {path}: {e}"))?;
    log::info!(
        "config: persisted {MCP_SERVERS_KEY} ({} server(s)) to {path}",
        servers.len()
    );
    Ok(())
}

/// Store key holding the heavy-lane cloud provider selection (M004 S04).
/// Absent/null means no cloud provider is selected — the default. The value is
/// the provider's kebab-case wire name ("openai" / "anthropic"), kept
/// identical to [`crate::cloud::keystore::CloudProvider`]'s serde encoding.
pub const CLOUD_HEAVY_PROVIDER_KEY: &str = "cloudHeavyProvider";

/// Store key holding the coder-lane cloud provider selection (coding-agent
/// S6). Same encoding and semantics as [`CLOUD_HEAVY_PROVIDER_KEY`].
pub const CLOUD_CODER_PROVIDER_KEY: &str = "cloudCoderProvider";

/// Read the persisted heavy-lane provider. `None` means nothing usable is
/// persisted (no store, no key, null, or garbage — all logged where relevant):
/// the caller keeps the default (unselected). There is no env fallback, so a
/// flat `Option` is enough — "absent" and "explicitly none" are the same here.
pub fn load_cloud_heavy_provider(app: &AppHandle) -> Option<crate::cloud::keystore::CloudProvider> {
    load_cloud_lane_provider(app, CLOUD_HEAVY_PROVIDER_KEY)
}

/// Read the persisted coder-lane provider (coding-agent S6) — same contract
/// as [`load_cloud_heavy_provider`].
pub fn load_cloud_coder_provider(app: &AppHandle) -> Option<crate::cloud::keystore::CloudProvider> {
    load_cloud_lane_provider(app, CLOUD_CODER_PROVIDER_KEY)
}

fn load_cloud_lane_provider(
    app: &AppHandle,
    key: &str,
) -> Option<crate::cloud::keystore::CloudProvider> {
    let store = match app.store(SETTINGS_STORE) {
        Ok(store) => store,
        Err(e) => {
            log::error!(
                "config: failed to open settings store at {}: {e}",
                store_path(app)
            );
            return None;
        }
    };
    let value = store.get(key)?;
    stored_cloud_lane_provider(key, &value)
}

/// Interpret one stored heavy-provider value. Only a recognized provider wire
/// name is trusted; null, unknown strings, and non-strings are treated as
/// unselected rather than silently pinning a garbage provider — there is no
/// safe "default provider" to fall back to.
fn stored_cloud_lane_provider(
    key: &str,
    value: &serde_json::Value,
) -> Option<crate::cloud::keystore::CloudProvider> {
    match value {
        serde_json::Value::Null => None,
        other => {
            match serde_json::from_value::<crate::cloud::keystore::CloudProvider>(other.clone()) {
                Ok(provider) => Some(provider),
                Err(_) => {
                    log::warn!(
                        "config: {key} holds unrecognized value {other}; treating as unselected"
                    );
                    None
                }
            }
        }
    }
}

/// Persist the heavy-lane provider selection (`None` writes an explicit JSON
/// null — "unselected" is a decision, so it is stored, not just absent). The
/// error names the failed persist path; the caller rolls the in-memory
/// selection back so an unpersisted choice can never silently revert on
/// restart (opt-in / hotkey precedent).
pub fn save_cloud_heavy_provider(
    app: &AppHandle,
    provider: Option<crate::cloud::keystore::CloudProvider>,
) -> Result<(), String> {
    save_cloud_lane_provider(app, CLOUD_HEAVY_PROVIDER_KEY, provider)
}

/// Persist the coder-lane provider selection (coding-agent S6) — same
/// contract as [`save_cloud_heavy_provider`].
pub fn save_cloud_coder_provider(
    app: &AppHandle,
    provider: Option<crate::cloud::keystore::CloudProvider>,
) -> Result<(), String> {
    save_cloud_lane_provider(app, CLOUD_CODER_PROVIDER_KEY, provider)
}

fn save_cloud_lane_provider(
    app: &AppHandle,
    key: &str,
    provider: Option<crate::cloud::keystore::CloudProvider>,
) -> Result<(), String> {
    let path = store_path(app);
    let store = app
        .store(SETTINGS_STORE)
        .map_err(|e| format!("failed to open settings store at {path}: {e}"))?;
    let wire = provider.map(|p| p.account());
    store.set(key, serde_json::json!(wire));
    store
        .save()
        .map_err(|e| format!("failed to persist {key}={wire:?} to {path}: {e}"))?;
    log::info!("config: persisted {key}={wire:?} to {path}");
    Ok(())
}

/// Store key holding the first-run onboarding flag (M006). Absent/garbage means
/// the user has not been onboarded yet — the default — so the onboarding
/// explainer shows. Set to `true` once the user completes or skips onboarding so
/// it never shows again. This flag governs only whether the explainer is shown;
/// it grants nothing on its own (the OS TCC prompt does), so the safe default
/// (`false` = show again) can at worst re-show the panel, never leak a grant.
pub const FIRST_RUN_COMPLETE_KEY: &str = "firstRunComplete";

/// Read the persisted first-run onboarding flag. `false` (the default) means
/// nothing usable is persisted (no store, no key — both logged where relevant),
/// so onboarding should show; `true` means the user already completed or skipped
/// it. A missing store must not suppress onboarding, so failures fall to `false`.
pub fn load_first_run_complete(app: &AppHandle) -> bool {
    let store = match app.store(SETTINGS_STORE) {
        Ok(store) => store,
        Err(e) => {
            log::error!(
                "config: failed to open settings store at {}: {e}",
                store_path(app)
            );
            return false;
        }
    };
    match store.get(FIRST_RUN_COMPLETE_KEY) {
        Some(value) => stored_first_run_complete(&value),
        None => false,
    }
}

/// Interpret one stored first-run value. Only a JSON boolean is trusted;
/// anything else is logged and treated as `false` (not yet onboarded) so a
/// garbage value re-shows the harmless explainer rather than silently
/// suppressing it — the fail-safe direction here is "show again".
fn stored_first_run_complete(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Bool(b) => *b,
        other => {
            log::warn!(
                "config: {FIRST_RUN_COMPLETE_KEY} holds non-boolean value {other}; treating as not-complete"
            );
            false
        }
    }
}

/// Persist the first-run onboarding flag. The error names the failed persist
/// path; the caller decides whether an unpersisted flag is fatal (it is not —
/// re-showing the explainer once more is harmless), so this only surfaces the
/// error for logging, mirroring the other config savers.
pub fn save_first_run_complete(app: &AppHandle, complete: bool) -> Result<(), String> {
    let path = store_path(app);
    let store = app
        .store(SETTINGS_STORE)
        .map_err(|e| format!("failed to open settings store at {path}: {e}"))?;
    store.set(FIRST_RUN_COMPLETE_KEY, serde_json::json!(complete));
    store.save().map_err(|e| {
        format!("failed to persist {FIRST_RUN_COMPLETE_KEY}={complete} to {path}: {e}")
    })?;
    log::info!("config: persisted {FIRST_RUN_COMPLETE_KEY}={complete} to {path}");
    Ok(())
}

/// Store key holding the nudges off-switch (M002 S05, D019). Unlike the
/// watcher/privacy toggles the default is ON — nudges only fire while the
/// watcher runs, so the off-switch is the feature, not the default.
pub const NUDGES_ENABLED_KEY: &str = "nudgesEnabled";

/// Default for [`NUDGES_ENABLED_KEY`] when the store has nothing usable.
pub const NUDGES_ENABLED_DEFAULT: bool = true;

/// Store key holding the nudge cooldown in seconds (D019's configurable
/// cooldown; Settings chips since 2026-07-27).
pub const NUDGE_COOLDOWN_SECS_KEY: &str = "nudgeCooldownSecs";

/// Store key holding the nudge banner's auto-dismiss window in seconds.
pub const NUDGE_AUTO_DISMISS_SECS_KEY: &str = "nudgeAutoDismissSecs";

/// Default for [`NUDGE_AUTO_DISMISS_SECS_KEY`].
pub const NUDGE_AUTO_DISMISS_SECS_DEFAULT: u64 = 12;

/// Default for [`NUDGE_COOLDOWN_SECS_KEY`]: at most one nudge per 5 min.
pub const NUDGE_COOLDOWN_SECS_DEFAULT: u64 = 300;

/// Read the persisted nudges toggle. `None` means nothing usable is
/// persisted (no store, no key — both logged where relevant): the caller
/// keeps [`NUDGES_ENABLED_DEFAULT`].
pub fn load_nudges_enabled(app: &AppHandle) -> Option<bool> {
    let store = match app.store(SETTINGS_STORE) {
        Ok(store) => store,
        Err(e) => {
            log::error!(
                "config: failed to open settings store at {}: {e}",
                store_path(app)
            );
            return None;
        }
    };
    let value = store.get(NUDGES_ENABLED_KEY)?;
    Some(stored_nudges_enabled(&value))
}

/// Interpret one stored nudges-toggle value. Only a JSON boolean is
/// trusted; anything else is logged and yields the default (on) — garbage
/// in the store must not silently flip a user-facing setting, and unlike
/// the capture toggles there is no safety reason to force off (nudges are
/// display-only; the watcher gate governs capture).
fn stored_nudges_enabled(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Bool(b) => *b,
        other => {
            log::warn!(
                "config: {NUDGES_ENABLED_KEY} holds non-boolean value {other}; \
                 using default ({NUDGES_ENABLED_DEFAULT})"
            );
            NUDGES_ENABLED_DEFAULT
        }
    }
}

/// Persist the nudges toggle. The error names the failed persist path; the
/// caller (the nudge applier) rolls the in-memory toggle back so an
/// unpersisted nudge state can never silently revert on restart.
pub fn save_nudges_enabled(app: &AppHandle, enabled: bool) -> Result<(), String> {
    let path = store_path(app);
    let store = app
        .store(SETTINGS_STORE)
        .map_err(|e| format!("failed to open settings store at {path}: {e}"))?;
    store.set(NUDGES_ENABLED_KEY, serde_json::json!(enabled));
    store
        .save()
        .map_err(|e| format!("failed to persist {NUDGES_ENABLED_KEY}={enabled} to {path}: {e}"))?;
    log::info!("config: persisted {NUDGES_ENABLED_KEY}={enabled} to {path}");
    Ok(())
}

/// Read the persisted nudge cooldown in seconds, falling back to
/// [`NUDGE_COOLDOWN_SECS_DEFAULT`] when nothing usable is persisted.
pub fn load_nudge_cooldown_secs(app: &AppHandle) -> u64 {
    let store = match app.store(SETTINGS_STORE) {
        Ok(store) => store,
        Err(e) => {
            log::error!(
                "config: failed to open settings store at {}: {e}",
                store_path(app)
            );
            return NUDGE_COOLDOWN_SECS_DEFAULT;
        }
    };
    match store.get(NUDGE_COOLDOWN_SECS_KEY) {
        Some(value) => stored_nudge_cooldown_secs(&value),
        None => NUDGE_COOLDOWN_SECS_DEFAULT,
    }
}

/// Interpret one stored cooldown value. Only a positive JSON integer is
/// trusted; zero, negatives, fractions, and non-numbers are logged and
/// yield the default rather than letting garbage disable rate limiting.
fn stored_nudge_cooldown_secs(value: &serde_json::Value) -> u64 {
    match value.as_u64() {
        Some(secs) if secs > 0 => secs,
        _ => {
            log::warn!(
                "config: {NUDGE_COOLDOWN_SECS_KEY} holds non-positive-integer value {value}; \
                 using default ({NUDGE_COOLDOWN_SECS_DEFAULT}s)"
            );
            NUDGE_COOLDOWN_SECS_DEFAULT
        }
    }
}

/// Persist the nudge cooldown (Settings chips). Same rollback contract as
/// the nudges toggle: the caller reverts in-memory state on Err.
pub fn save_nudge_cooldown_secs(app: &AppHandle, secs: u64) -> Result<(), String> {
    let path = store_path(app);
    let store = app
        .store(SETTINGS_STORE)
        .map_err(|e| format!("failed to open settings store at {path}: {e}"))?;
    store.set(NUDGE_COOLDOWN_SECS_KEY, serde_json::json!(secs));
    store.save().map_err(|e| {
        format!("failed to persist {NUDGE_COOLDOWN_SECS_KEY}={secs} to {path}: {e}")
    })?;
    log::info!("config: persisted {NUDGE_COOLDOWN_SECS_KEY}={secs} to {path}");
    Ok(())
}

/// Read the persisted auto-dismiss window, falling back to the default for
/// anything non-positive or non-integer (same trust rule as the cooldown).
pub fn load_nudge_auto_dismiss_secs(app: &AppHandle) -> u64 {
    let Ok(store) = app.store(SETTINGS_STORE) else {
        log::error!(
            "config: failed to open settings store at {}",
            store_path(app)
        );
        return NUDGE_AUTO_DISMISS_SECS_DEFAULT;
    };
    match store
        .get(NUDGE_AUTO_DISMISS_SECS_KEY)
        .and_then(|v| v.as_u64())
    {
        Some(secs) if secs > 0 => secs,
        Some(_) | None => NUDGE_AUTO_DISMISS_SECS_DEFAULT,
    }
}

/// Persist the auto-dismiss window (Settings chips).
pub fn save_nudge_auto_dismiss_secs(app: &AppHandle, secs: u64) -> Result<(), String> {
    let path = store_path(app);
    let store = app
        .store(SETTINGS_STORE)
        .map_err(|e| format!("failed to open settings store at {path}: {e}"))?;
    store.set(NUDGE_AUTO_DISMISS_SECS_KEY, serde_json::json!(secs));
    store.save().map_err(|e| {
        format!("failed to persist {NUDGE_AUTO_DISMISS_SECS_KEY}={secs} to {path}: {e}")
    })?;
    log::info!("config: persisted {NUDGE_AUTO_DISMISS_SECS_KEY}={secs} to {path}");
    Ok(())
}

/// Store key holding the overlay presentation config (M006 S04). A single
/// composite object — `mode` + per-edge `edgeExtents` + `modalSize` — so the
/// corrupt-value fallback lives in one interpreter and a mode switch restores
/// that edge's preferred extent atomically (D040). Absent/garbage means the
/// safe default (modal at [`MODAL_DEFAULT_WIDTH`]×[`MODAL_DEFAULT_HEIGHT`]): a
/// corrupted geometry value must never yield an off-screen window, so every
/// field fails safe inside [`stored_overlay_presentation`].
pub const OVERLAY_PRESENTATION_KEY: &str = "overlayPresentation";

/// The smallest logical width/height the overlay may take — mirrors the
/// frontend `OVERLAY_MIN_WIDTH`/`OVERLAY_MIN_HEIGHT` (overlay-geometry.ts) so a
/// sub-min persisted extent is rejected here rather than clipping the chrome.
pub const OVERLAY_MIN_WIDTH: f64 = 360.0;
pub const OVERLAY_MIN_HEIGHT: f64 = 120.0;

/// Default drawer extents (logical px): variable width for left/right, variable
/// height for top/bottom. Both sit comfortably above the mins so the fallback
/// value is itself a valid, on-screen extent.
pub const DRAWER_WIDTH_DEFAULT: f64 = 420.0;
pub const DRAWER_HEIGHT_DEFAULT: f64 = 320.0;

/// Default modal size (logical px) — the free-floating (non-docked) shape.
pub const MODAL_DEFAULT_WIDTH: f64 = 720.0;
pub const MODAL_DEFAULT_HEIGHT: f64 = 480.0;

/// The overlay presentation mode: a free `modal` window or a drawer docked flush
/// against one display edge. The kebab-case wire tags mirror the frontend `Edge`
/// union plus a `modal` tag; the whole value is the store's presentation record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PresentationMode {
    Modal,
    Top,
    Bottom,
    Left,
    Right,
}

/// A logical-pixel size for the modal (free) presentation.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OverlaySizeConfig {
    pub width: f64,
    pub height: f64,
}

/// A logical-pixel point — the modal (free) presentation's remembered top-left
/// origin. Unlike [`OverlaySizeConfig`], a point carries NO minimum floor:
/// legal multi-monitor virtual desktops place monitors at negative origins, so
/// a negative x/y is a valid coordinate, not garbage (see the finite-only
/// interpreter [`stored_point`]).
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OverlayPointConfig {
    pub x: f64,
    pub y: f64,
}

/// The per-edge drawer extents (logical px): width for left/right, height for
/// top/bottom. Carrying all four lets a mode switch restore that edge's last
/// preferred extent without re-deriving it.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EdgeExtents {
    pub top: f64,
    pub bottom: f64,
    pub left: f64,
    pub right: f64,
}

/// The whole persisted overlay-presentation record: which mode is active plus
/// the remembered extent for every edge, the modal size, and the modal's
/// remembered position, so switching modes is lossless. [`Default`] is the safe
/// fallback the interpreter returns on garbage — modal at the default size with
/// no remembered position (`modal_position: None` → the frontend centers it),
/// never an off-screen shape. `modal_position` is `None` until the user first
/// drags the modal: absent means "never moved → center", preserving today's
/// behavior.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OverlayPresentation {
    pub mode: PresentationMode,
    pub edge_extents: EdgeExtents,
    pub modal_size: OverlaySizeConfig,
    pub modal_position: Option<OverlayPointConfig>,
}

impl Default for OverlayPresentation {
    fn default() -> Self {
        Self {
            mode: PresentationMode::Modal,
            edge_extents: EdgeExtents {
                top: DRAWER_HEIGHT_DEFAULT,
                bottom: DRAWER_HEIGHT_DEFAULT,
                left: DRAWER_WIDTH_DEFAULT,
                right: DRAWER_WIDTH_DEFAULT,
            },
            modal_size: OverlaySizeConfig {
                width: MODAL_DEFAULT_WIDTH,
                height: MODAL_DEFAULT_HEIGHT,
            },
            modal_position: None,
        }
    }
}

/// Read the persisted overlay presentation. `None` means nothing usable is
/// persisted (no store, no key — both logged where relevant): the caller keeps
/// [`OverlayPresentation::default`]. A present-but-garbage value is repaired
/// field-by-field inside [`stored_overlay_presentation`], never rejected whole.
pub fn load_overlay_presentation(app: &AppHandle) -> Option<OverlayPresentation> {
    let store = match app.store(SETTINGS_STORE) {
        Ok(store) => store,
        Err(e) => {
            log::error!(
                "config: failed to open settings store at {}: {e}",
                store_path(app)
            );
            return None;
        }
    };
    let value = store.get(OVERLAY_PRESENTATION_KEY)?;
    Some(stored_overlay_presentation(&value))
}

/// Interpret one stored dimension (an extent or a modal side). Only a finite
/// number at or above `min` is trusted; a non-number, NaN/∞, negative, or
/// sub-min value is logged and replaced with `default` — the acceptance-critical
/// seam that keeps a corrupted geometry value from producing an off-screen or
/// chrome-clipping window. `default` is always ≥ `min`, so the fallback is itself
/// a valid extent.
fn stored_dimension(value: Option<&serde_json::Value>, label: &str, min: f64, default: f64) -> f64 {
    match value.and_then(serde_json::Value::as_f64) {
        Some(n) if n.is_finite() && n >= min => n,
        _ => {
            log::warn!(
                "config: {OVERLAY_PRESENTATION_KEY}.{label} is not a finite number >= {min} \
                 (got {value:?}); using default {default}"
            );
            default
        }
    }
}

/// Interpret the stored modal position. Unlike [`stored_dimension`], a point
/// has NO minimum floor: legal multi-monitor virtual desktops place monitors at
/// negative origins, so a floor would corrupt a valid negative coordinate. Only
/// a JSON object with a finite `x` AND a finite `y` is trusted; an absent value,
/// an explicit null, a non-object, a missing coordinate, or a non-finite value
/// (NaN/∞) all yield `None` — "never moved / unusable → center". This repairs
/// the CORRUPT half of the off-screen guard (the frontend `isOnScreen` guard
/// repairs the off-screen-but-finite half). A present-but-garbage value is
/// logged; an absent/null value is the common no-op case and stays quiet.
fn stored_point(value: Option<&serde_json::Value>) -> Option<OverlayPointConfig> {
    match value {
        // Absent or explicit null: never moved (or explicitly cleared) — center.
        None | Some(serde_json::Value::Null) => None,
        Some(v) => {
            let parsed = v.as_object().and_then(|obj| {
                let x = obj.get("x").and_then(serde_json::Value::as_f64)?;
                let y = obj.get("y").and_then(serde_json::Value::as_f64)?;
                (x.is_finite() && y.is_finite()).then_some(OverlayPointConfig { x, y })
            });
            if parsed.is_none() {
                log::warn!(
                    "config: {OVERLAY_PRESENTATION_KEY}.modalPosition is not an object with \
                     finite x and y (got {v}); centering"
                );
            }
            parsed
        }
    }
}

/// Interpret the whole stored presentation object with per-field fallback: an
/// unknown/missing mode → `modal`; a non-number, negative, or sub-min extent or
/// modal side → its floored default. A non-object value collapses every field to
/// its default. The corrupt-value contract lives entirely here, so a single
/// garbage field can never take the window off-screen — the rest of the record
/// still applies and the bad field falls back to a sane on-screen value.
fn stored_overlay_presentation(value: &serde_json::Value) -> OverlayPresentation {
    let obj = value.as_object();
    if obj.is_none() {
        log::warn!(
            "config: {OVERLAY_PRESENTATION_KEY} is not a JSON object (got {value}); using all defaults"
        );
    }

    let mode = obj
        .and_then(|m| m.get("mode"))
        .and_then(|v| serde_json::from_value::<PresentationMode>(v.clone()).ok())
        .unwrap_or_else(|| {
            log::warn!(
                "config: {OVERLAY_PRESENTATION_KEY}.mode is missing or not a known mode; using modal"
            );
            PresentationMode::Modal
        });

    let edges = obj
        .and_then(|m| m.get("edgeExtents"))
        .and_then(serde_json::Value::as_object);
    let edge_extents = EdgeExtents {
        top: stored_dimension(
            edges.and_then(|m| m.get("top")),
            "edgeExtents.top",
            OVERLAY_MIN_HEIGHT,
            DRAWER_HEIGHT_DEFAULT,
        ),
        bottom: stored_dimension(
            edges.and_then(|m| m.get("bottom")),
            "edgeExtents.bottom",
            OVERLAY_MIN_HEIGHT,
            DRAWER_HEIGHT_DEFAULT,
        ),
        left: stored_dimension(
            edges.and_then(|m| m.get("left")),
            "edgeExtents.left",
            OVERLAY_MIN_WIDTH,
            DRAWER_WIDTH_DEFAULT,
        ),
        right: stored_dimension(
            edges.and_then(|m| m.get("right")),
            "edgeExtents.right",
            OVERLAY_MIN_WIDTH,
            DRAWER_WIDTH_DEFAULT,
        ),
    };

    let modal = obj
        .and_then(|m| m.get("modalSize"))
        .and_then(serde_json::Value::as_object);
    let modal_size = OverlaySizeConfig {
        width: stored_dimension(
            modal.and_then(|m| m.get("width")),
            "modalSize.width",
            OVERLAY_MIN_WIDTH,
            MODAL_DEFAULT_WIDTH,
        ),
        height: stored_dimension(
            modal.and_then(|m| m.get("height")),
            "modalSize.height",
            OVERLAY_MIN_HEIGHT,
            MODAL_DEFAULT_HEIGHT,
        ),
    };

    let modal_position = stored_point(obj.and_then(|m| m.get("modalPosition")));

    OverlayPresentation {
        mode,
        edge_extents,
        modal_size,
        modal_position,
    }
}

/// Persist the overlay presentation record as one composite JSON object. The
/// error names the failed persist path; the caller (the presentation applier,
/// T02) rolls the in-memory value back and surfaces the failure as data
/// (`persistError`, health-as-value) so an unpersisted shape can never silently
/// revert on restart (hotkey/opt-in precedent).
pub fn save_overlay_presentation(
    app: &AppHandle,
    presentation: &OverlayPresentation,
) -> Result<(), String> {
    let path = store_path(app);
    let store = app
        .store(SETTINGS_STORE)
        .map_err(|e| format!("failed to open settings store at {path}: {e}"))?;
    let wire = serde_json::to_value(presentation)
        .map_err(|e| format!("failed to serialize {OVERLAY_PRESENTATION_KEY}: {e}"))?;
    store.set(OVERLAY_PRESENTATION_KEY, wire.clone());
    store.save().map_err(|e| {
        format!("failed to persist {OVERLAY_PRESENTATION_KEY}={wire} to {path}: {e}")
    })?;
    log::info!("config: persisted {OVERLAY_PRESENTATION_KEY}={wire} to {path}");
    Ok(())
}

/// Store key holding the skills-discovery directory (M007 S06). Absent, blank,
/// or garbage means the default — `app_data_dir/skills` resolved by
/// [`default_skills_dir`]. The value is a filesystem path string; users drop
/// `<name>/SKILL.md` packs into this directory and the pure skills module (S06
/// T02) walks it, so a corrupt value must fail safe to the default discovery
/// directory rather than sending the walker at a garbage path.
pub const SKILLS_DIR_KEY: &str = "skillsDir";

/// The app-data-relative subdirectory used as the default skills-discovery
/// directory when nothing usable is persisted.
pub const SKILLS_DIR_DEFAULT_SUBDIR: &str = "skills";

/// Read the persisted skills-discovery directory. `None` means nothing usable
/// is persisted (no store, no key, or garbage — all logged where relevant): the
/// caller resolves the default via [`default_skills_dir`] / [`resolve_skills_dir`].
/// `Some(dir)` is a user-set, non-blank path.
pub fn load_skills_dir(app: &AppHandle) -> Option<String> {
    let store = match app.store(SETTINGS_STORE) {
        Ok(store) => store,
        Err(e) => {
            log::error!(
                "config: failed to open settings store at {}: {e}",
                store_path(app)
            );
            return None;
        }
    };
    let value = store.get(SKILLS_DIR_KEY)?;
    stored_skills_dir(&value)
}

/// Interpret one stored skills-directory value (the `stored_model_pin` template:
/// a trimmed string). A non-blank string is trusted (trimmed); a blank string,
/// null, or non-string is logged and treated as unset (`None`) so the caller
/// falls back to the default discovery directory rather than walking a garbage
/// path — the fail-soft direction here is "use the default skills dir" (R006/R007).
fn stored_skills_dir(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(s) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                log::warn!(
                    "config: {SKILLS_DIR_KEY} holds a blank string; using default skills dir"
                );
                None
            } else {
                Some(trimmed.to_string())
            }
        }
        serde_json::Value::Null => None,
        other => {
            log::warn!(
                "config: {SKILLS_DIR_KEY} holds non-string value {other}; using default skills dir"
            );
            None
        }
    }
}

/// Persist the skills-discovery directory. The error names the failed persist
/// path; the caller rolls its in-memory value back so an unpersisted change can
/// never silently revert on restart (hotkey precedent).
pub fn save_skills_dir(app: &AppHandle, dir: &str) -> Result<(), String> {
    let path = store_path(app);
    let store = app
        .store(SETTINGS_STORE)
        .map_err(|e| format!("failed to open settings store at {path}: {e}"))?;
    store.set(SKILLS_DIR_KEY, serde_json::json!(dir));
    store
        .save()
        .map_err(|e| format!("failed to persist {SKILLS_DIR_KEY}='{dir}' to {path}: {e}"))?;
    log::info!("config: persisted {SKILLS_DIR_KEY}='{dir}' to {path}");
    Ok(())
}

/// The default skills-discovery directory: `app_data_dir/skills`, falling back
/// to the bare `skills` relative path if the app data dir cannot be resolved
/// (mirrors [`store_path`]'s fallback — a resolution failure is never fatal).
pub fn default_skills_dir(app: &AppHandle) -> std::path::PathBuf {
    app.path()
        .app_data_dir()
        .map(|dir| dir.join(SKILLS_DIR_DEFAULT_SUBDIR))
        .unwrap_or_else(|_| std::path::PathBuf::from(SKILLS_DIR_DEFAULT_SUBDIR))
}

/// Resolve the effective skills-discovery directory the walker (S06 T02/T04)
/// should scan: the user-set path when one is persisted and usable, else the
/// default from [`default_skills_dir`]. This is the single seam that folds the
/// "unset or garbage → safe default" contract into one call for the caller.
pub fn resolve_skills_dir(app: &AppHandle) -> std::path::PathBuf {
    match load_skills_dir(app) {
        Some(dir) => std::path::PathBuf::from(dir),
        None => default_skills_dir(app),
    }
}

/// Store key holding the local LLM endpoint override (S05). Absent, null,
/// blank, or garbage means unset — the caller falls back to the
/// `THIRD_EYE_ENDPOINT` env var and then the project default
/// (`llm::openai::DEFAULT_ENDPOINT`). The value is an http(s) base URL of an
/// OpenAI-compatible server (LM Studio, Ollama, …); the endpoint is fixed per
/// run (router, probe, and embedder all cache it at construction), so a saved
/// change applies on the next launch.
pub const LLM_ENDPOINT_KEY: &str = "llmEndpoint";

/// Read the persisted LLM endpoint override. `None` means nothing usable is
/// persisted (no store, no key, null, or garbage — all logged where relevant):
/// the caller falls back to the env var / project default.
pub fn load_llm_endpoint(app: &AppHandle) -> Option<String> {
    let store = match app.store(SETTINGS_STORE) {
        Ok(store) => store,
        Err(e) => {
            log::error!(
                "config: failed to open settings store at {}: {e}",
                store_path(app)
            );
            return None;
        }
    };
    let value = store.get(LLM_ENDPOINT_KEY)?;
    stored_llm_endpoint(&value)
}

/// Interpret one stored endpoint value. Only a non-blank string that parses to
/// an http(s) URL with a host is trusted (trimmed, trailing slashes dropped —
/// the same normalization as `env_endpoint`); anything else is logged and
/// treated as unset so the router falls back to a reachable default rather
/// than booting against a garbage URL the guard would then classify External
/// and every request would fail on.
fn stored_llm_endpoint(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(s) => {
            let trimmed = s.trim().trim_end_matches('/');
            if trimmed.is_empty() {
                log::warn!(
                    "config: {LLM_ENDPOINT_KEY} holds a blank string; using default endpoint"
                );
                None
            } else if is_http_url(trimmed) {
                Some(trimmed.to_string())
            } else {
                log::warn!(
                    "config: {LLM_ENDPOINT_KEY} holds a non-http(s) value {trimmed:?}; using default endpoint"
                );
                None
            }
        }
        serde_json::Value::Null => None,
        other => {
            log::warn!(
                "config: {LLM_ENDPOINT_KEY} holds non-string value {other}; using default endpoint"
            );
            None
        }
    }
}

/// Persist the LLM endpoint override (`None` writes an explicit JSON null —
/// "reset to default" is a decision, stored like an unpinned lane model). The
/// error names the failed persist path; nothing in-memory changes on save (the
/// running router keeps its per-run endpoint), so there is no rollback to do.
pub fn save_llm_endpoint(app: &AppHandle, endpoint: Option<&str>) -> Result<(), String> {
    let path = store_path(app);
    let store = app
        .store(SETTINGS_STORE)
        .map_err(|e| format!("failed to open settings store at {path}: {e}"))?;
    store.set(LLM_ENDPOINT_KEY, serde_json::json!(endpoint));
    store
        .save()
        .map_err(|e| format!("failed to persist {LLM_ENDPOINT_KEY}={endpoint:?} to {path}: {e}"))?;
    log::info!("config: persisted {LLM_ENDPOINT_KEY}={endpoint:?} to {path}");
    Ok(())
}

/// Store key holding the chat-memory toggle (M008 S03, R032). Mirrors the
/// watcher trio with ONE deliberate inversion: the default is ON (opt-out) —
/// chat recall is the milestone's core promise, and the gate governs only
/// locally-stored chat distillation — so an absent key AND a non-boolean
/// (garbage) stored value both default to TRUE, warn-logging the garbage,
/// instead of the capture toggles' fail-safe-to-off.
pub const CHAT_MEMORY_ENABLED_KEY: &str = "chatMemoryEnabled";

/// Read the persisted chat-memory toggle. `None` means nothing usable is
/// persisted (no store, no key — both logged where relevant): the caller
/// keeps the default (ON — the deliberate opt-out inversion of the watcher
/// trio, see [`CHAT_MEMORY_ENABLED_KEY`]).
pub fn load_chat_memory_enabled(app: &AppHandle) -> Option<bool> {
    let store = match app.store(SETTINGS_STORE) {
        Ok(store) => store,
        Err(e) => {
            log::error!(
                "config: failed to open settings store at {}: {e}",
                store_path(app)
            );
            return None;
        }
    };
    let value = store.get(CHAT_MEMORY_ENABLED_KEY)?;
    Some(stored_chat_memory_enabled(&value))
}

/// Interpret one stored chat-memory value. Only a JSON boolean is trusted;
/// anything else is logged and treated as ON — the deliberate inversion of
/// the watcher/privacy interpreters: this toggle gates local-only chat
/// distillation (no capture, no egress), so garbage must not silently turn
/// off the milestone's core promise.
fn stored_chat_memory_enabled(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Bool(b) => *b,
        other => {
            log::warn!(
                "config: {CHAT_MEMORY_ENABLED_KEY} holds non-boolean value {other}; treating as on"
            );
            true
        }
    }
}

/// Persist the chat-memory toggle. The error names the failed persist path;
/// the caller (the chat-memory applier, T02) rolls the in-memory toggle back
/// so an unpersisted chat-memory state can never silently revert on restart.
pub fn save_chat_memory_enabled(app: &AppHandle, enabled: bool) -> Result<(), String> {
    let path = store_path(app);
    let store = app
        .store(SETTINGS_STORE)
        .map_err(|e| format!("failed to open settings store at {path}: {e}"))?;
    store.set(CHAT_MEMORY_ENABLED_KEY, serde_json::json!(enabled));
    store.save().map_err(|e| {
        format!("failed to persist {CHAT_MEMORY_ENABLED_KEY}={enabled} to {path}: {e}")
    })?;
    log::info!("config: persisted {CHAT_MEMORY_ENABLED_KEY}={enabled} to {path}");
    Ok(())
}

/// Store key holding how long observed memory is kept (2026-07 redesign,
/// tour step "Memory" + Settings). Wire contract shared with the TS side
/// (tour-state.ts RETENTION_OPTIONS): "7d" | "30d" | "90d" | "forever".
/// Display/persist only in this milestone — store pruning that honors the
/// value is a specced follow-up, so nothing here touches the memory store.
pub const MEMORY_RETENTION_KEY: &str = "memoryRetention";

/// The retention wire values, single source of truth for validation.
pub const MEMORY_RETENTION_VALUES: [&str; 4] = ["7d", "30d", "90d", "forever"];

/// Default when nothing (usable) is persisted.
pub const MEMORY_RETENTION_DEFAULT: &str = "30d";

/// Validate one candidate retention value to its canonical form. `None` for
/// anything outside the wire contract — callers decide between rejecting an
/// IPC write and falling back to the default for a stored value.
pub fn parse_memory_retention(value: &str) -> Option<&'static str> {
    MEMORY_RETENTION_VALUES
        .iter()
        .find(|v| **v == value)
        .copied()
}

/// Store key for the terminal-commands gate (computer-control I2). Default
/// OFF and fail-safe-off on garbage — run_command is the most powerful
/// actuator in the app, so it follows the capture toggles' posture, not the
/// chat-memory inversion.
pub const COMMANDS_ENABLED_KEY: &str = "commandsEnabled";

/// Read the persisted commands toggle. `None` = nothing usable persisted:
/// the caller keeps the default (OFF).
pub fn load_commands_enabled(app: &AppHandle) -> Option<bool> {
    let store = match app.store(SETTINGS_STORE) {
        Ok(store) => store,
        Err(e) => {
            log::error!(
                "config: failed to open settings store at {}: {e}",
                store_path(app)
            );
            return None;
        }
    };
    let value = store.get(COMMANDS_ENABLED_KEY)?;
    Some(match value {
        serde_json::Value::Bool(b) => b,
        other => {
            log::warn!(
                "config: {COMMANDS_ENABLED_KEY} holds non-boolean value {other}; treating as off"
            );
            false
        }
    })
}

/// Persist the commands toggle. The error names the failed persist path.
pub fn save_commands_enabled(app: &AppHandle, enabled: bool) -> Result<(), String> {
    let path = store_path(app);
    let store = app
        .store(SETTINGS_STORE)
        .map_err(|e| format!("failed to open settings store at {path}: {e}"))?;
    store.set(COMMANDS_ENABLED_KEY, serde_json::json!(enabled));
    store.save().map_err(|e| {
        format!("failed to persist {COMMANDS_ENABLED_KEY}={enabled} to {path}: {e}")
    })?;
    log::info!("config: persisted {COMMANDS_ENABLED_KEY}={enabled} to {path}");
    Ok(())
}

/// Store key for the persistent command allowlist (computer-control,
/// user request 2026-07-26): commands matching an entry run WITHOUT an
/// approval prompt while the commands gate is enabled. User-defined in
/// Settings; matching is exact-or-token-prefix (command_runner).
pub const COMMAND_ALLOWLIST_KEY: &str = "commandAllowlist";

/// Read the persisted allowlist. Garbage-tolerant: a non-array value or
/// non-string entries are dropped with a warning (never invent grants);
/// entries are trimmed and de-duplicated preserving order.
pub fn load_command_allowlist(app: &AppHandle) -> Vec<String> {
    let Ok(store) = app.store(SETTINGS_STORE) else {
        log::error!(
            "config: failed to open settings store at {}",
            store_path(app)
        );
        return Vec::new();
    };
    let Some(value) = store.get(COMMAND_ALLOWLIST_KEY) else {
        return Vec::new();
    };
    sanitize_command_allowlist(&value)
}

/// Interpret one stored allowlist value (pure, unit-tested).
pub fn sanitize_command_allowlist(value: &serde_json::Value) -> Vec<String> {
    let Some(items) = value.as_array() else {
        log::warn!("config: {COMMAND_ALLOWLIST_KEY} holds a non-array value; treating as empty");
        return Vec::new();
    };
    let mut seen = std::collections::HashSet::new();
    items
        .iter()
        .filter_map(|item| match item.as_str() {
            Some(s) => {
                let trimmed = s.trim();
                (!trimmed.is_empty() && seen.insert(trimmed.to_string()))
                    .then(|| trimmed.to_string())
            }
            None => {
                log::warn!("config: {COMMAND_ALLOWLIST_KEY} entry is not a string; dropped");
                None
            }
        })
        .collect()
}

/// Persist the allowlist (already sanitized by the caller).
pub fn save_command_allowlist(app: &AppHandle, entries: &[String]) -> Result<(), String> {
    let path = store_path(app);
    let store = app
        .store(SETTINGS_STORE)
        .map_err(|e| format!("failed to open settings store at {path}: {e}"))?;
    store.set(COMMAND_ALLOWLIST_KEY, serde_json::json!(entries));
    store
        .save()
        .map_err(|e| format!("failed to persist {COMMAND_ALLOWLIST_KEY} to {path}: {e}"))?;
    log::info!(
        "config: persisted {COMMAND_ALLOWLIST_KEY} ({} entries) to {path}",
        entries.len()
    );
    Ok(())
}

/// Store key for the per-tool switchboard (user request 2026-07-26): the
/// names of built-in tools the user turned OFF in Settings. Absent = all
/// tools on (the default).
pub const DISABLED_TOOLS_KEY: &str = "disabledTools";

/// Read the persisted disabled-tools set. Garbage-tolerant like the
/// command allowlist: non-array values and non-string entries drop with a
/// warning; unknown tool names are dropped later by the applier.
pub fn load_disabled_tools(app: &AppHandle) -> Vec<String> {
    let Ok(store) = app.store(SETTINGS_STORE) else {
        log::error!(
            "config: failed to open settings store at {}",
            store_path(app)
        );
        return Vec::new();
    };
    let Some(value) = store.get(DISABLED_TOOLS_KEY) else {
        return Vec::new();
    };
    let Some(items) = value.as_array() else {
        log::warn!("config: {DISABLED_TOOLS_KEY} holds a non-array value; treating as empty");
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| match item.as_str() {
            Some(s) if !s.trim().is_empty() => Some(s.trim().to_string()),
            _ => {
                log::warn!("config: {DISABLED_TOOLS_KEY} entry is not a usable string; dropped");
                None
            }
        })
        .collect()
}

/// Persist the disabled-tools set (already sorted by the caller).
pub fn save_disabled_tools(app: &AppHandle, names: &[String]) -> Result<(), String> {
    let path = store_path(app);
    let store = app
        .store(SETTINGS_STORE)
        .map_err(|e| format!("failed to open settings store at {path}: {e}"))?;
    store.set(DISABLED_TOOLS_KEY, serde_json::json!(names));
    store
        .save()
        .map_err(|e| format!("failed to persist {DISABLED_TOOLS_KEY} to {path}: {e}"))?;
    log::info!(
        "config: persisted {DISABLED_TOOLS_KEY} ({} entries) to {path}",
        names.len()
    );
    Ok(())
}

/// Store key for permanently approved HID action kinds (user request
/// 2026-07-27): kinds granted via the approval card's "Always" seed the
/// session whitelist at every boot. Kebab-case ActionKind strings.
pub const APPROVED_ACTION_KINDS_KEY: &str = "approvedActionKinds";

/// Read the persisted always-approved kinds. Garbage-tolerant like the
/// command allowlist: non-strings drop with a warning; unknown kind names
/// are dropped by the caller's parse.
pub fn load_approved_action_kinds(app: &AppHandle) -> Vec<String> {
    let Ok(store) = app.store(SETTINGS_STORE) else {
        log::error!(
            "config: failed to open settings store at {}",
            store_path(app)
        );
        return Vec::new();
    };
    let Some(value) = store.get(APPROVED_ACTION_KINDS_KEY) else {
        return Vec::new();
    };
    let Some(items) = value.as_array() else {
        log::warn!(
            "config: {APPROVED_ACTION_KINDS_KEY} holds a non-array value; treating as empty"
        );
        return Vec::new();
    };
    let mut seen = std::collections::HashSet::new();
    items
        .iter()
        .filter_map(|item| item.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty() && seen.insert(s.clone()))
        .collect()
}

/// Persist the always-approved kinds (already validated by the caller).
pub fn save_approved_action_kinds(app: &AppHandle, kinds: &[String]) -> Result<(), String> {
    let path = store_path(app);
    let store = app
        .store(SETTINGS_STORE)
        .map_err(|e| format!("failed to open settings store at {path}: {e}"))?;
    store.set(APPROVED_ACTION_KINDS_KEY, serde_json::json!(kinds));
    store
        .save()
        .map_err(|e| format!("failed to persist {APPROVED_ACTION_KINDS_KEY} to {path}: {e}"))?;
    log::info!(
        "config: persisted {APPROVED_ACTION_KINDS_KEY} ({} kinds) to {path}",
        kinds.len()
    );
    Ok(())
}

/// Store key for the designated workspace roots (coding-agent S2): the
/// ONLY directories the coding tools may touch. Absolute paths.
pub const WORKSPACE_ROOTS_KEY: &str = "workspaceRoots";

/// Read the persisted workspace roots (garbage-tolerant, order-preserving
/// dedupe like the command allowlist).
pub fn load_workspace_roots(app: &AppHandle) -> Vec<String> {
    let Ok(store) = app.store(SETTINGS_STORE) else {
        return Vec::new();
    };
    let Some(value) = store.get(WORKSPACE_ROOTS_KEY) else {
        return Vec::new();
    };
    let Some(items) = value.as_array() else {
        log::warn!("config: {WORKSPACE_ROOTS_KEY} holds a non-array value; treating as empty");
        return Vec::new();
    };
    let mut seen = std::collections::HashSet::new();
    items
        .iter()
        .filter_map(|item| item.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty() && seen.insert(s.clone()))
        .collect()
}

/// Persist the workspace roots (already cleaned by the caller).
pub fn save_workspace_roots(app: &AppHandle, roots: &[String]) -> Result<(), String> {
    let path = store_path(app);
    let store = app
        .store(SETTINGS_STORE)
        .map_err(|e| format!("failed to open settings store at {path}: {e}"))?;
    store.set(WORKSPACE_ROOTS_KEY, serde_json::json!(roots));
    store
        .save()
        .map_err(|e| format!("failed to persist {WORKSPACE_ROOTS_KEY} to {path}: {e}"))?;
    log::info!(
        "config: persisted {WORKSPACE_ROOTS_KEY} ({} roots) to {path}",
        roots.len()
    );
    Ok(())
}

/// The retention window in milliseconds; `None` for "forever" (never prune)
/// AND for out-of-contract values — garbage must never widen deletion.
pub fn memory_retention_window_ms(retention: &str) -> Option<i64> {
    const DAY_MS: i64 = 24 * 60 * 60 * 1000;
    match parse_memory_retention(retention) {
        Some("7d") => Some(7 * DAY_MS),
        Some("30d") => Some(30 * DAY_MS),
        Some("90d") => Some(90 * DAY_MS),
        _ => None, // "forever" or invalid: no pruning either way
    }
}

/// Interpret one stored retention value: a valid string is trusted, anything
/// else (wrong type, unknown string) is logged and treated as the default —
/// garbage in the store must not invent a retention policy.
fn stored_memory_retention(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::String(s) => parse_memory_retention(s).unwrap_or_else(|| {
            log::warn!(
                "config: {MEMORY_RETENTION_KEY} holds unknown value {s:?}; using {MEMORY_RETENTION_DEFAULT}"
            );
            MEMORY_RETENTION_DEFAULT
        }),
        other => {
            log::warn!(
                "config: {MEMORY_RETENTION_KEY} holds non-string value {other}; using {MEMORY_RETENTION_DEFAULT}"
            );
            MEMORY_RETENTION_DEFAULT
        }
    }
}

/// Read the persisted retention. `None` means nothing persisted (no store /
/// no key): the caller uses [`MEMORY_RETENTION_DEFAULT`].
pub fn load_memory_retention(app: &AppHandle) -> Option<&'static str> {
    let store = match app.store(SETTINGS_STORE) {
        Ok(store) => store,
        Err(e) => {
            log::error!(
                "config: failed to open settings store at {}: {e}",
                store_path(app)
            );
            return None;
        }
    };
    let value = store.get(MEMORY_RETENTION_KEY)?;
    Some(stored_memory_retention(&value))
}

/// Persist a retention value (already validated by the caller via
/// [`parse_memory_retention`]). The error names the failed persist path.
pub fn save_memory_retention(app: &AppHandle, retention: &str) -> Result<(), String> {
    let path = store_path(app);
    let store = app
        .store(SETTINGS_STORE)
        .map_err(|e| format!("failed to open settings store at {path}: {e}"))?;
    store.set(MEMORY_RETENTION_KEY, serde_json::json!(retention));
    store.save().map_err(|e| {
        format!("failed to persist {MEMORY_RETENTION_KEY}={retention} to {path}: {e}")
    })?;
    log::info!("config: persisted {MEMORY_RETENTION_KEY}={retention} to {path}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::router::{HEAVY_LANE, THIN_LANE};

    #[test]
    fn parse_memory_retention_accepts_only_wire_values() {
        for v in MEMORY_RETENTION_VALUES {
            assert_eq!(parse_memory_retention(v), Some(v));
        }
        assert_eq!(parse_memory_retention("60d"), None);
        assert_eq!(parse_memory_retention(""), None);
        assert_eq!(parse_memory_retention("Forever"), None); // case-sensitive wire contract
    }

    #[test]
    fn command_allowlist_sanitizer_drops_garbage_and_dedupes() {
        let value = serde_json::json!(["date", "  date  ", "", 42, "curl -s ifconfig.me"]);
        assert_eq!(
            sanitize_command_allowlist(&value),
            vec!["date".to_string(), "curl -s ifconfig.me".to_string()]
        );
        // Non-array garbage must never invent grants.
        assert!(sanitize_command_allowlist(&serde_json::json!("date")).is_empty());
        assert!(sanitize_command_allowlist(&serde_json::Value::Null).is_empty());
    }

    #[test]
    fn retention_window_maps_days_and_never_prunes_forever_or_garbage() {
        const DAY: i64 = 24 * 60 * 60 * 1000;
        assert_eq!(memory_retention_window_ms("7d"), Some(7 * DAY));
        assert_eq!(memory_retention_window_ms("30d"), Some(30 * DAY));
        assert_eq!(memory_retention_window_ms("90d"), Some(90 * DAY));
        assert_eq!(memory_retention_window_ms("forever"), None);
        // Garbage must never widen deletion.
        assert_eq!(memory_retention_window_ms("1d"), None);
        assert_eq!(memory_retention_window_ms(""), None);
    }

    #[test]
    fn stored_memory_retention_trusts_valid_strings() {
        assert_eq!(stored_memory_retention(&serde_json::json!("90d")), "90d");
        assert_eq!(
            stored_memory_retention(&serde_json::json!("forever")),
            "forever"
        );
    }

    #[test]
    fn stored_memory_retention_defaults_on_garbage() {
        // Unknown string, wrong type, null: all fall back instead of inventing policy.
        assert_eq!(
            stored_memory_retention(&serde_json::json!("6 months")),
            MEMORY_RETENTION_DEFAULT
        );
        assert_eq!(
            stored_memory_retention(&serde_json::json!(30)),
            MEMORY_RETENTION_DEFAULT
        );
        assert_eq!(
            stored_memory_retention(&serde_json::Value::Null),
            MEMORY_RETENTION_DEFAULT
        );
    }

    #[test]
    fn lane_model_key_maps_canonical_lanes_and_rejects_others() {
        assert_eq!(lane_model_key(THIN_LANE), Some("thinModel"));
        assert_eq!(lane_model_key(HEAVY_LANE), Some("heavyModel"));
        assert_eq!(
            lane_model_key(crate::llm::router::CODER_LANE),
            Some("coderModel")
        );
        assert_eq!(lane_model_key("turbo"), None);
    }

    #[test]
    fn stored_string_pins_the_trimmed_model_id() {
        let pin = stored_model_pin(THIN_MODEL_KEY, &serde_json::json!(" qwen2.5-7b "));
        assert_eq!(pin, Some("qwen2.5-7b".into()));
    }

    #[test]
    fn stored_null_means_explicitly_unpinned() {
        // The store-vs-env contract: null is a decision, not an absence.
        assert_eq!(
            stored_model_pin(THIN_MODEL_KEY, &serde_json::Value::Null),
            None
        );
    }

    #[test]
    fn stored_blank_string_unpins_instead_of_pinning_a_nameless_model() {
        assert_eq!(
            stored_model_pin(THIN_MODEL_KEY, &serde_json::json!("")),
            None
        );
        assert_eq!(
            stored_model_pin(THIN_MODEL_KEY, &serde_json::json!("   ")),
            None
        );
    }

    #[test]
    fn stored_non_string_value_is_treated_as_unpinned() {
        assert_eq!(
            stored_model_pin(HEAVY_MODEL_KEY, &serde_json::json!(42)),
            None
        );
        assert_eq!(
            stored_model_pin(HEAVY_MODEL_KEY, &serde_json::json!({"id": "x"})),
            None
        );
    }

    #[test]
    fn stored_privacy_booleans_round_trip() {
        assert!(stored_privacy_mode(&serde_json::json!(true)));
        assert!(!stored_privacy_mode(&serde_json::json!(false)));
    }

    #[test]
    fn stored_non_boolean_privacy_value_is_treated_as_off() {
        // Q7: garbage in the store must not lock the user out of capture.
        assert!(!stored_privacy_mode(&serde_json::json!("true")));
        assert!(!stored_privacy_mode(&serde_json::json!(1)));
        assert!(!stored_privacy_mode(&serde_json::Value::Null));
        assert!(!stored_privacy_mode(&serde_json::json!({"enabled": true})));
    }

    #[test]
    fn stored_cloud_optin_booleans_round_trip() {
        assert!(stored_cloud_optin(&serde_json::json!(true)));
        assert!(!stored_cloud_optin(&serde_json::json!(false)));
    }

    #[test]
    fn stored_non_boolean_cloud_optin_is_treated_as_off() {
        // Q5/Q7: garbage in the store must never silently enable cloud
        // egress — off is the only safe fallback for the remote-provider gate.
        assert!(!stored_cloud_optin(&serde_json::json!("true")));
        assert!(!stored_cloud_optin(&serde_json::json!(1)));
        assert!(!stored_cloud_optin(&serde_json::Value::Null));
        assert!(!stored_cloud_optin(&serde_json::json!({"enabled": true})));
    }

    #[test]
    fn stored_hid_armed_booleans_round_trip() {
        assert!(stored_hid_enabled(&serde_json::json!(true)));
        assert!(!stored_hid_enabled(&serde_json::json!(false)));
    }

    #[test]
    fn stored_non_boolean_hid_armed_is_treated_as_off() {
        // D038/R019: garbage in the store must never silently arm a capability
        // that can click and type anywhere — off (disarmed) is the only safe
        // fallback for the HID gate.
        assert!(!stored_hid_enabled(&serde_json::json!("true")));
        assert!(!stored_hid_enabled(&serde_json::json!(1)));
        assert!(!stored_hid_enabled(&serde_json::Value::Null));
        assert!(!stored_hid_enabled(&serde_json::json!({"enabled": true})));
    }

    #[test]
    fn stored_hid_run_mode_round_trips_each_mode() {
        // The persist round-trip proven at the interpreter level (the same level
        // every other config test proves at): the wire tag save_hid_run_mode
        // writes reads back to the same mode. Covers all three of Off/Ask/AutoRun.
        use crate::input::commands::HidRunMode;
        for mode in [HidRunMode::Off, HidRunMode::Ask, HidRunMode::AutoRun] {
            let stored = serde_json::json!(mode);
            assert_eq!(stored_hid_run_mode(&stored), mode, "wire form: {stored}");
        }
        // The exact kebab-case strings the store holds and src/chat.ts keys on.
        assert_eq!(
            stored_hid_run_mode(&serde_json::json!("off")),
            HidRunMode::Off
        );
        assert_eq!(
            stored_hid_run_mode(&serde_json::json!("ask")),
            HidRunMode::Ask
        );
        assert_eq!(
            stored_hid_run_mode(&serde_json::json!("auto-run")),
            HidRunMode::AutoRun
        );
    }

    #[test]
    fn stored_garbage_hid_run_mode_is_treated_as_off() {
        // D038/R019: garbage in the store must never silently arm a capability
        // that can click and type anywhere — off (disarmed) is the only safe
        // fallback for the HID gate. Unknown tags, wrong case, non-strings, and
        // null all collapse to Off.
        use crate::input::commands::HidRunMode;
        for bad in [
            serde_json::json!("on"),
            serde_json::json!("Ask"),
            serde_json::json!("auto_run"),
            serde_json::json!(true),
            serde_json::json!(1),
            serde_json::Value::Null,
            serde_json::json!({"mode": "ask"}),
            serde_json::json!(["ask"]),
        ] {
            assert_eq!(
                stored_hid_run_mode(&bad),
                HidRunMode::Off,
                "bad value: {bad}"
            );
        }
    }

    #[test]
    fn stored_mcp_run_mode_round_trips_each_mode() {
        // The persist round-trip proven at the interpreter level (the same level
        // every other config test proves at): the wire tag save_mcp_run_mode
        // writes reads back to the same mode. Covers all three of Off/Ask/AutoRun.
        use crate::llm::mcp::McpRunMode;
        for mode in [McpRunMode::Off, McpRunMode::Ask, McpRunMode::AutoRun] {
            let stored = serde_json::json!(mode);
            assert_eq!(stored_mcp_run_mode(&stored), mode, "wire form: {stored}");
        }
        // The exact kebab-case strings the store holds and src/chat.ts keys on.
        assert_eq!(
            stored_mcp_run_mode(&serde_json::json!("off")),
            McpRunMode::Off
        );
        assert_eq!(
            stored_mcp_run_mode(&serde_json::json!("ask")),
            McpRunMode::Ask
        );
        assert_eq!(
            stored_mcp_run_mode(&serde_json::json!("auto-run")),
            McpRunMode::AutoRun
        );
    }

    #[test]
    fn stored_garbage_mcp_run_mode_is_treated_as_off() {
        // R016: garbage in the store must never silently let an external
        // server's tools run without approval — off (inert) is the only safe
        // fallback for the MCP gate. Unknown tags, wrong case, non-strings, and
        // null all collapse to Off.
        use crate::llm::mcp::McpRunMode;
        for bad in [
            serde_json::json!("on"),
            serde_json::json!("Ask"),
            serde_json::json!("auto_run"),
            serde_json::json!(true),
            serde_json::json!(1),
            serde_json::Value::Null,
            serde_json::json!({"mode": "ask"}),
            serde_json::json!(["ask"]),
        ] {
            assert_eq!(
                stored_mcp_run_mode(&bad),
                McpRunMode::Off,
                "bad value: {bad}"
            );
        }
    }

    #[test]
    fn stored_mcp_servers_round_trips_a_serialized_list() {
        // The persist round-trip proven at the interpreter level: the JSON
        // save_mcp_servers writes reads back to the same list, args + enabled
        // preserved.
        use crate::llm::mcp::{McpServerConfig, McpTransport};
        let servers = vec![
            McpServerConfig {
                id: "everything".to_string(),
                command: "npx".to_string(),
                args: vec![
                    "-y".to_string(),
                    "@modelcontextprotocol/server-everything".to_string(),
                ],
                enabled: true,
                transport: McpTransport::Stdio,
                url: None,
                auth_ref: None,
            },
            McpServerConfig {
                id: "local".to_string(),
                command: "./server".to_string(),
                args: vec![],
                enabled: false,
                transport: McpTransport::Stdio,
                url: None,
                auth_ref: None,
            },
        ];
        let wire = serde_json::to_value(&servers).unwrap();
        assert_eq!(stored_mcp_servers(&wire), servers);
    }

    #[test]
    fn stored_mcp_servers_absent_or_non_array_is_empty() {
        // Absent key → empty list (the default: no external server). A
        // wholesale-garbage value (string, number, object, null) also collapses
        // to empty rather than erroring.
        for bad in [
            serde_json::json!("everything"),
            serde_json::json!(42),
            serde_json::json!(true),
            serde_json::Value::Null,
            serde_json::json!({ "id": "x", "command": "y" }),
        ] {
            assert!(stored_mcp_servers(&bad).is_empty(), "bad value: {bad}");
        }
    }

    #[test]
    fn stored_mcp_servers_drops_a_corrupt_entry_but_keeps_good_siblings() {
        // Per-item repair (the acceptance seam): a corrupt entry (missing
        // command, blank id, non-object) is dropped and logged, never nuking a
        // good sibling.
        use crate::llm::mcp::{McpServerConfig, McpTransport};
        let value = serde_json::json!([
            { "id": "good", "command": "npx", "args": ["-y", "pkg"], "enabled": true },
            { "id": "no-command" },                       // missing command → dropped
            { "id": "   ", "command": "run" },            // blank id → dropped
            "not-an-object",                               // non-object → dropped
            { "id": "second", "command": "./srv" }        // minimal, valid → kept
        ]);
        let got = stored_mcp_servers(&value);
        assert_eq!(
            got,
            vec![
                McpServerConfig {
                    id: "good".to_string(),
                    command: "npx".to_string(),
                    args: vec!["-y".to_string(), "pkg".to_string()],
                    enabled: true,
                    transport: McpTransport::Stdio,
                    url: None,
                    auth_ref: None,
                },
                McpServerConfig {
                    id: "second".to_string(),
                    command: "./srv".to_string(),
                    args: vec![],
                    enabled: false,
                    transport: McpTransport::Stdio,
                    url: None,
                    auth_ref: None,
                },
            ]
        );
    }

    #[test]
    fn stored_mcp_server_repairs_corrupt_fields_within_a_usable_entry() {
        // Field-level repair: a usable entry (valid id + command) survives even
        // when args is garbage or enabled is non-boolean — the bad field falls
        // back (args → only its string elements, enabled → false), the entry is
        // NOT dropped. id/command are trimmed.
        use crate::llm::mcp::{McpServerConfig, McpTransport};
        // Non-array args → empty; non-boolean enabled → false; fields trimmed.
        let entry = serde_json::json!({ "id": "  x  ", "command": " run ", "args": "nope", "enabled": "yes" });
        assert_eq!(
            stored_mcp_server(&entry),
            Some(McpServerConfig {
                id: "x".to_string(),
                command: "run".to_string(),
                args: vec![],
                enabled: false,
                transport: McpTransport::Stdio,
                url: None,
                auth_ref: None,
            })
        );
        // Array args with a non-string element → only the string args survive.
        let entry =
            serde_json::json!({ "id": "y", "command": "srv", "args": ["--flag", 42, "--other"] });
        assert_eq!(
            stored_mcp_server(&entry).unwrap().args,
            vec!["--flag".to_string(), "--other".to_string()]
        );
    }

    #[test]
    fn stored_mcp_server_without_transport_defaults_to_stdio() {
        // Back-compat: an S04 entry has no `transport` key, so repair must keep
        // treating it as a stdio server (non-blank command required, url/auth_ref
        // None) rather than mis-routing or dropping it.
        use crate::llm::mcp::{McpServerConfig, McpTransport};
        let entry = serde_json::json!({ "id": "everything", "command": "npx", "enabled": true });
        assert_eq!(
            stored_mcp_server(&entry),
            Some(McpServerConfig {
                id: "everything".to_string(),
                command: "npx".to_string(),
                args: vec![],
                enabled: true,
                transport: McpTransport::Stdio,
                url: None,
                auth_ref: None,
            })
        );
    }

    #[test]
    fn stored_mcp_server_keeps_a_valid_http_entry_with_auth_ref() {
        // An http entry with a valid https url + authRef is kept; command is
        // unused (empty ok). url/authRef are trimmed and carried through.
        use crate::llm::mcp::{McpServerConfig, McpTransport};
        let entry = serde_json::json!({
            "id": " weather ",
            "transport": "http",
            "url": "  https://mcp.example.com/sse  ",
            "authRef": " mcp:weather ",
            "enabled": true
        });
        assert_eq!(
            stored_mcp_server(&entry),
            Some(McpServerConfig {
                id: "weather".to_string(),
                command: String::new(),
                args: vec![],
                enabled: true,
                transport: McpTransport::Http,
                url: Some("https://mcp.example.com/sse".to_string()),
                auth_ref: Some("mcp:weather".to_string()),
            })
        );
        // authRef is optional — an unauthenticated http server round-trips with None.
        let entry = serde_json::json!({ "id": "open", "transport": "http", "url": "http://localhost:9000/mcp" });
        let got = stored_mcp_server(&entry).unwrap();
        assert_eq!(got.transport, McpTransport::Http);
        assert_eq!(got.auth_ref, None);
    }

    #[test]
    fn stored_mcp_servers_drops_a_corrupt_http_entry_but_keeps_good_siblings() {
        // Transport-aware per-entry repair: an http entry with a blank or
        // unparseable/non-web url is dropped (nothing to connect to), while a
        // valid http sibling and a valid stdio sibling both survive.
        use crate::llm::mcp::{McpServerConfig, McpTransport};
        let value = serde_json::json!([
            { "id": "good-http", "transport": "http", "url": "https://a.example/mcp", "enabled": true },
            { "id": "blank-url", "transport": "http", "url": "   " },          // blank url → dropped
            { "id": "no-url", "transport": "http" },                             // missing url → dropped
            { "id": "bad-scheme", "transport": "http", "url": "ftp://x/y" },     // non-web scheme → dropped
            { "id": "relative", "transport": "http", "url": "/just/a/path" },    // unparseable → dropped
            { "id": "good-stdio", "command": "./srv" }                           // stdio sibling → kept
        ]);
        let got = stored_mcp_servers(&value);
        assert_eq!(
            got,
            vec![
                McpServerConfig {
                    id: "good-http".to_string(),
                    command: String::new(),
                    args: vec![],
                    enabled: true,
                    transport: McpTransport::Http,
                    url: Some("https://a.example/mcp".to_string()),
                    auth_ref: None,
                },
                McpServerConfig {
                    id: "good-stdio".to_string(),
                    command: "./srv".to_string(),
                    args: vec![],
                    enabled: false,
                    transport: McpTransport::Stdio,
                    url: None,
                    auth_ref: None,
                },
            ]
        );
    }

    #[test]
    fn stored_mcp_server_unknown_transport_fails_safe_to_stdio() {
        // A garbled transport discriminator must never route an entry onto the
        // network: it falls back to stdio, so a non-blank command is then
        // required (present here → kept as stdio).
        use crate::llm::mcp::McpTransport;
        let entry = serde_json::json!({ "id": "x", "transport": "grpc", "command": "run", "url": "https://a/b" });
        let got = stored_mcp_server(&entry).unwrap();
        assert_eq!(got.transport, McpTransport::Stdio);
        assert_eq!(got.command, "run");
        assert_eq!(got.url, None);
    }

    #[test]
    fn stored_cloud_heavy_provider_round_trips_each_provider() {
        // The persist round-trip proven at the interpreter level (the same
        // level every other config test proves at): the wire name written by
        // save_cloud_heavy_provider reads back to the same provider.
        use crate::cloud::keystore::CloudProvider;
        for provider in CloudProvider::ALL {
            let stored = serde_json::json!(provider.account());
            assert_eq!(
                stored_cloud_lane_provider(CLOUD_HEAVY_PROVIDER_KEY, &stored),
                Some(provider)
            );
        }
    }

    #[test]
    fn stored_null_cloud_heavy_provider_is_unselected() {
        // null is the persisted "no provider selected" decision.
        assert_eq!(
            stored_cloud_lane_provider(CLOUD_CODER_PROVIDER_KEY, &serde_json::Value::Null),
            None
        );
    }

    #[test]
    fn stored_garbage_cloud_heavy_provider_is_unselected() {
        // Q5/Q7: an unknown or malformed value must never pin a garbage
        // provider — unselected is the only safe fallback (no default provider).
        for bad in [
            serde_json::json!("gemini"),
            serde_json::json!("OpenAI"),
            serde_json::json!(1),
            serde_json::json!(true),
            serde_json::json!({"provider": "openai"}),
            serde_json::json!(["openai"]),
        ] {
            assert_eq!(
                stored_cloud_lane_provider(CLOUD_HEAVY_PROVIDER_KEY, &bad),
                None,
                "bad value: {bad}"
            );
        }
    }

    #[test]
    fn stored_watcher_booleans_round_trip() {
        assert!(stored_watcher_enabled(&serde_json::json!(true)));
        assert!(!stored_watcher_enabled(&serde_json::json!(false)));
    }

    #[test]
    fn stored_nudges_booleans_round_trip() {
        assert!(stored_nudges_enabled(&serde_json::json!(true)));
        assert!(!stored_nudges_enabled(&serde_json::json!(false)));
    }

    #[test]
    #[allow(clippy::assertions_on_constants)] // the constant IS the documented claim
    fn stored_non_boolean_nudges_value_falls_back_to_default_on() {
        // Q7: garbage must not flip the user-facing setting — the default
        // (on) is safe here because nudges are display-only and capture is
        // governed by the watcher gate.
        assert!(NUDGES_ENABLED_DEFAULT);
        assert!(stored_nudges_enabled(&serde_json::json!("false")));
        assert!(stored_nudges_enabled(&serde_json::json!(0)));
        assert!(stored_nudges_enabled(&serde_json::Value::Null));
        assert!(stored_nudges_enabled(
            &serde_json::json!({"enabled": false})
        ));
    }

    #[test]
    fn stored_positive_integer_cooldown_is_trusted() {
        assert_eq!(stored_nudge_cooldown_secs(&serde_json::json!(60)), 60);
        assert_eq!(stored_nudge_cooldown_secs(&serde_json::json!(1)), 1);
        assert_eq!(
            stored_nudge_cooldown_secs(&serde_json::json!(86_400)),
            86_400
        );
    }

    #[test]
    fn stored_garbage_cooldown_falls_back_to_default() {
        // Q7: zero/negative/fractional/non-number values must not disable
        // or corrupt rate limiting — only a positive integer is trusted.
        for bad in [
            serde_json::json!(0),
            serde_json::json!(-30),
            serde_json::json!(2.5),
            serde_json::json!("300"),
            serde_json::json!(true),
            serde_json::Value::Null,
            serde_json::json!({"secs": 300}),
        ] {
            assert_eq!(
                stored_nudge_cooldown_secs(&bad),
                NUDGE_COOLDOWN_SECS_DEFAULT,
                "bad value: {bad}"
            );
        }
    }

    #[test]
    fn stored_first_run_booleans_round_trip() {
        assert!(stored_first_run_complete(&serde_json::json!(true)));
        assert!(!stored_first_run_complete(&serde_json::json!(false)));
    }

    #[test]
    fn stored_non_boolean_first_run_value_is_treated_as_not_complete() {
        // The fail-safe direction is "show the explainer again" — garbage must
        // never silently suppress onboarding, and the flag grants nothing on its
        // own, so re-showing is harmless while suppressing hides the walkthrough.
        assert!(!stored_first_run_complete(&serde_json::json!("true")));
        assert!(!stored_first_run_complete(&serde_json::json!(1)));
        assert!(!stored_first_run_complete(&serde_json::Value::Null));
        assert!(!stored_first_run_complete(
            &serde_json::json!({"complete": true})
        ));
    }

    #[test]
    fn stored_chat_memory_booleans_round_trip() {
        // false must survive the round trip — the user's opt-out is honored.
        assert!(stored_chat_memory_enabled(&serde_json::json!(true)));
        assert!(!stored_chat_memory_enabled(&serde_json::json!(false)));
    }

    #[test]
    fn stored_non_boolean_chat_memory_value_defaults_to_on() {
        // Q7: the deliberate inversion — a corrupted settings.json value loads
        // as TRUE (opt-out default), unlike the capture toggles. Chat memory is
        // local-only distillation, so "on" is the safe direction here.
        assert!(stored_chat_memory_enabled(&serde_json::json!("false")));
        assert!(stored_chat_memory_enabled(&serde_json::json!(0)));
        assert!(stored_chat_memory_enabled(&serde_json::Value::Null));
        assert!(stored_chat_memory_enabled(
            &serde_json::json!({"enabled": false})
        ));
    }

    #[test]
    fn stored_non_boolean_watcher_value_is_treated_as_off() {
        // Q7: garbage in the store must never silently start continuous
        // screen capture — off is the only safe fallback.
        assert!(!stored_watcher_enabled(&serde_json::json!("true")));
        assert!(!stored_watcher_enabled(&serde_json::json!(1)));
        assert!(!stored_watcher_enabled(&serde_json::Value::Null));
        assert!(!stored_watcher_enabled(
            &serde_json::json!({"enabled": true})
        ));
    }

    #[test]
    fn stored_overlay_presentation_round_trips_a_serialized_record() {
        // The persist round-trip proven at the interpreter level (the level every
        // config test proves at): the JSON save_overlay_presentation writes reads
        // back to the same record, for every mode.
        for mode in [
            PresentationMode::Modal,
            PresentationMode::Top,
            PresentationMode::Bottom,
            PresentationMode::Left,
            PresentationMode::Right,
        ] {
            let record = OverlayPresentation {
                mode,
                ..OverlayPresentation::default()
            };
            let wire = serde_json::to_value(record).unwrap();
            assert_eq!(stored_overlay_presentation(&wire), record, "mode: {mode:?}");
        }
    }

    #[test]
    fn stored_overlay_presentation_trusts_in_range_extents() {
        // A user-set extent at or above the axis min (even below the default) is
        // honoured — only sub-min/garbage falls back. 380 > 360 min for left/right;
        // 200 > 120 min for top/bottom; the modal size sits above both mins.
        let value = serde_json::json!({
            "mode": "left",
            "edgeExtents": { "top": 200, "bottom": 260, "left": 380, "right": 500 },
            "modalSize": { "width": 640, "height": 400 }
        });
        let got = stored_overlay_presentation(&value);
        assert_eq!(got.mode, PresentationMode::Left);
        assert_eq!(got.edge_extents.top, 200.0);
        assert_eq!(got.edge_extents.bottom, 260.0);
        assert_eq!(got.edge_extents.left, 380.0);
        assert_eq!(got.edge_extents.right, 500.0);
        assert_eq!(got.modal_size.width, 640.0);
        assert_eq!(got.modal_size.height, 400.0);
    }

    #[test]
    fn stored_overlay_presentation_unknown_mode_falls_back_to_modal() {
        // Slice acceptance: an unknown mode tag must never leave the mode in a
        // limbo state — modal is the safe default.
        for bad_mode in [
            serde_json::json!("center"),
            serde_json::json!("Modal"),
            serde_json::json!("top-left"),
            serde_json::json!(1),
            serde_json::json!(true),
            serde_json::Value::Null,
            serde_json::json!(["top"]),
        ] {
            let value = serde_json::json!({ "mode": bad_mode });
            assert_eq!(
                stored_overlay_presentation(&value).mode,
                PresentationMode::Modal,
                "bad mode: {bad_mode}"
            );
        }
    }

    #[test]
    fn stored_overlay_presentation_garbage_extents_fall_back_to_floored_defaults() {
        // The acceptance-critical seam: a corrupted geometry value must fall back
        // to a sane default, never an off-screen or chrome-clipping window.
        // Non-number, negative, NaN-ish, and sub-min values all collapse to the
        // per-axis default (which is itself above the min).
        let value = serde_json::json!({
            "mode": "top",
            "edgeExtents": {
                "top": -50,          // negative
                "bottom": "320",     // non-number (string)
                "left": 10,          // sub-min (< 360)
                "right": null        // null
            },
            "modalSize": {
                "width": 0,          // sub-min (< 360)
                "height": {}         // non-number (object)
            }
        });
        let got = stored_overlay_presentation(&value);
        assert_eq!(got.edge_extents.top, DRAWER_HEIGHT_DEFAULT);
        assert_eq!(got.edge_extents.bottom, DRAWER_HEIGHT_DEFAULT);
        assert_eq!(got.edge_extents.left, DRAWER_WIDTH_DEFAULT);
        assert_eq!(got.edge_extents.right, DRAWER_WIDTH_DEFAULT);
        assert_eq!(got.modal_size.width, MODAL_DEFAULT_WIDTH);
        assert_eq!(got.modal_size.height, MODAL_DEFAULT_HEIGHT);
        // Every fallback dimension is a valid, on-screen extent.
        assert!(got.edge_extents.left >= OVERLAY_MIN_WIDTH);
        assert!(got.edge_extents.top >= OVERLAY_MIN_HEIGHT);
        assert!(got.modal_size.width >= OVERLAY_MIN_WIDTH);
        assert!(got.modal_size.height >= OVERLAY_MIN_HEIGHT);
    }

    #[test]
    fn stored_overlay_presentation_missing_fields_use_defaults() {
        // A partially-written object (only mode present) fills every absent field
        // from the default record rather than erroring the whole value out.
        let value = serde_json::json!({ "mode": "bottom" });
        let got = stored_overlay_presentation(&value);
        assert_eq!(got.mode, PresentationMode::Bottom);
        assert_eq!(
            got.edge_extents,
            OverlayPresentation::default().edge_extents
        );
        assert_eq!(got.modal_size, OverlayPresentation::default().modal_size);
    }

    #[test]
    fn stored_overlay_presentation_round_trips_a_remembered_modal_position() {
        // SC4 foundation: a dragged-to position must survive the persist
        // round-trip. A record with a set modalPosition serializes and reads
        // back identically — the wire shape the frontend restore path reads.
        let record = OverlayPresentation {
            modal_position: Some(OverlayPointConfig { x: 512.0, y: 384.0 }),
            ..OverlayPresentation::default()
        };
        let wire = serde_json::to_value(record).unwrap();
        assert_eq!(stored_overlay_presentation(&wire), record);
        assert_eq!(
            stored_overlay_presentation(&wire).modal_position,
            Some(OverlayPointConfig { x: 512.0, y: 384.0 })
        );
    }

    #[test]
    fn stored_point_trusts_a_finite_point_with_no_floor() {
        // Position, unlike a floored dimension, has NO minimum: a legal
        // multi-monitor virtual desktop places monitors at negative origins, so
        // negative x/y is a valid coordinate that must survive untouched.
        assert_eq!(
            stored_point(Some(&serde_json::json!({ "x": 100.0, "y": 200.0 }))),
            Some(OverlayPointConfig { x: 100.0, y: 200.0 })
        );
        assert_eq!(
            stored_point(Some(&serde_json::json!({ "x": -1920.0, "y": -128.0 }))),
            Some(OverlayPointConfig {
                x: -1920.0,
                y: -128.0
            })
        );
        // Integers coerce to f64; zero is a legal origin.
        assert_eq!(
            stored_point(Some(&serde_json::json!({ "x": 0, "y": 0 }))),
            Some(OverlayPointConfig { x: 0.0, y: 0.0 })
        );
    }

    #[test]
    fn stored_point_absent_or_null_means_never_moved() {
        // The common no-op case: no stored position → None → the frontend
        // centers the modal. Absent and explicit null are equivalent.
        assert_eq!(stored_point(None), None);
        assert_eq!(stored_point(Some(&serde_json::Value::Null)), None);
    }

    #[test]
    fn stored_point_corrupt_value_falls_back_to_none() {
        // The CORRUPT half of the off-screen guard, repaired in Rust: a
        // non-object, a missing coordinate, or a non-finite value must yield
        // None (center), never a garbage point that could land off-screen.
        for bad in [
            serde_json::json!("512,384"),                // string
            serde_json::json!(512),                      // bare number
            serde_json::json!(true),                     // boolean
            serde_json::json!([512, 384]),               // array
            serde_json::json!({ "x": 512 }),             // missing y
            serde_json::json!({ "y": 384 }),             // missing x
            serde_json::json!({ "x": "512", "y": 384 }), // non-number x
            serde_json::json!({ "x": 512, "y": null }),  // null y
        ] {
            assert_eq!(stored_point(Some(&bad)), None, "bad value: {bad}");
        }
    }

    #[test]
    fn stored_overlay_presentation_picks_up_a_valid_modal_position() {
        // The interpreter threads modalPosition through: a full record with a
        // finite negative-origin point restores it alongside every other field.
        let value = serde_json::json!({
            "mode": "modal",
            "edgeExtents": { "top": 320, "bottom": 320, "left": 420, "right": 420 },
            "modalSize": { "width": 720, "height": 480 },
            "modalPosition": { "x": -256.0, "y": 40.0 }
        });
        assert_eq!(
            stored_overlay_presentation(&value).modal_position,
            Some(OverlayPointConfig { x: -256.0, y: 40.0 })
        );
    }

    #[test]
    fn stored_overlay_presentation_absent_modal_position_is_none() {
        // Absent modalPosition preserves today's behavior: None → center. A
        // corrupt modalPosition never takes the record down — only that field
        // falls back while the rest applies.
        let absent = serde_json::json!({ "mode": "modal" });
        assert_eq!(stored_overlay_presentation(&absent).modal_position, None);

        let corrupt = serde_json::json!({
            "mode": "left",
            "modalPosition": { "x": "nope", "y": 40.0 }
        });
        let got = stored_overlay_presentation(&corrupt);
        assert_eq!(got.mode, PresentationMode::Left);
        assert_eq!(got.modal_position, None);
    }

    #[test]
    fn stored_overlay_presentation_non_object_is_all_defaults() {
        // Q7: a wholesale-garbage value (string, number, array, null) yields the
        // full safe default — modal at the default size, never off-screen.
        for bad in [
            serde_json::json!("modal"),
            serde_json::json!(42),
            serde_json::json!(true),
            serde_json::Value::Null,
            serde_json::json!(["left", 420]),
        ] {
            assert_eq!(
                stored_overlay_presentation(&bad),
                OverlayPresentation::default(),
                "bad value: {bad}"
            );
        }
    }

    #[test]
    fn stored_skills_dir_trims_a_non_blank_path() {
        // A user-set discovery directory is trusted and trimmed, like a lane pin.
        assert_eq!(
            stored_skills_dir(&serde_json::json!("  /Users/me/skills  ")),
            Some("/Users/me/skills".into())
        );
        assert_eq!(
            stored_skills_dir(&serde_json::json!("skills")),
            Some("skills".into())
        );
    }

    #[test]
    fn stored_blank_or_garbage_skills_dir_is_unset() {
        // R006/R007: a blank string, null, or non-string value must never send
        // the walker at a garbage path — None means "fall back to the default
        // discovery directory" (resolve_skills_dir → default_skills_dir).
        for bad in [
            serde_json::json!(""),
            serde_json::json!("   "),
            serde_json::Value::Null,
            serde_json::json!(42),
            serde_json::json!(true),
            serde_json::json!(["/skills"]),
            serde_json::json!({ "dir": "/skills" }),
        ] {
            assert_eq!(stored_skills_dir(&bad), None, "bad value: {bad}");
        }
    }

    #[test]
    fn stored_llm_endpoint_trims_and_strips_trailing_slashes() {
        // Same normalization as env_endpoint: trim whitespace, drop trailing
        // slashes, keep the URL otherwise verbatim.
        assert_eq!(
            stored_llm_endpoint(&serde_json::json!("  http://127.0.0.1:11434/  ")),
            Some("http://127.0.0.1:11434".into())
        );
        assert_eq!(
            stored_llm_endpoint(&serde_json::json!("https://llm.lan:8080")),
            Some("https://llm.lan:8080".into())
        );
    }

    #[test]
    fn stored_blank_or_null_llm_endpoint_is_unset() {
        // null is the persisted "reset to default" decision; blank is repaired
        // to the same — the router must never boot against an empty URL.
        assert_eq!(stored_llm_endpoint(&serde_json::Value::Null), None);
        assert_eq!(stored_llm_endpoint(&serde_json::json!("")), None);
        assert_eq!(stored_llm_endpoint(&serde_json::json!("   ")), None);
    }

    #[test]
    fn stored_garbage_llm_endpoint_falls_back_to_unset() {
        // A non-http(s) or unparseable value must never reach the router: the
        // guard would classify it External and every request would die on it.
        for bad in [
            serde_json::json!("localhost:1234"),      // no scheme
            serde_json::json!("ws://127.0.0.1:1234"), // non-web scheme
            serde_json::json!("file:///tmp/x"),       // no host
            serde_json::json!("/just/a/path"),        // relative
            serde_json::json!(1234),
            serde_json::json!(true),
            serde_json::json!(["http://x:1"]),
            serde_json::json!({ "url": "http://x:1" }),
        ] {
            assert_eq!(stored_llm_endpoint(&bad), None, "bad value: {bad}");
        }
    }
}
