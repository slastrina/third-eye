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
            log::error!("config: failed to open settings store at {}: {e}", store_path(app));
            return None;
        }
    };
    let value = store.get(HOTKEY_KEY)?;
    Some(value.as_str().map(str::to_owned).unwrap_or_else(|| value.to_string()))
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
            log::error!("config: failed to open settings store at {}: {e}", store_path(app));
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
            log::error!("config: failed to open settings store at {}: {e}", store_path(app));
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
            log::error!("config: failed to open settings store at {}: {e}", store_path(app));
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

/// Store key holding the nudges off-switch (M002 S05, D019). Unlike the
/// watcher/privacy toggles the default is ON — nudges only fire while the
/// watcher runs, so the off-switch is the feature, not the default.
pub const NUDGES_ENABLED_KEY: &str = "nudgesEnabled";

/// Default for [`NUDGES_ENABLED_KEY`] when the store has nothing usable.
pub const NUDGES_ENABLED_DEFAULT: bool = true;

/// Store key holding the nudge cooldown in seconds (D019's configurable
/// cooldown: a settings.json key read at startup, no UI). Read-only from
/// the app's perspective — there is deliberately no save fn.
pub const NUDGE_COOLDOWN_SECS_KEY: &str = "nudgeCooldownSecs";

/// Default for [`NUDGE_COOLDOWN_SECS_KEY`]: at most one nudge per 5 min.
pub const NUDGE_COOLDOWN_SECS_DEFAULT: u64 = 300;

/// Read the persisted nudges toggle. `None` means nothing usable is
/// persisted (no store, no key — both logged where relevant): the caller
/// keeps [`NUDGES_ENABLED_DEFAULT`].
pub fn load_nudges_enabled(app: &AppHandle) -> Option<bool> {
    let store = match app.store(SETTINGS_STORE) {
        Ok(store) => store,
        Err(e) => {
            log::error!("config: failed to open settings store at {}: {e}", store_path(app));
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
            log::error!("config: failed to open settings store at {}: {e}", store_path(app));
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::router::{HEAVY_LANE, THIN_LANE};

    #[test]
    fn lane_model_key_maps_canonical_lanes_and_rejects_others() {
        assert_eq!(lane_model_key(THIN_LANE), Some("thinModel"));
        assert_eq!(lane_model_key(HEAVY_LANE), Some("heavyModel"));
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
        assert_eq!(stored_model_pin(THIN_MODEL_KEY, &serde_json::Value::Null), None);
    }

    #[test]
    fn stored_blank_string_unpins_instead_of_pinning_a_nameless_model() {
        assert_eq!(stored_model_pin(THIN_MODEL_KEY, &serde_json::json!("")), None);
        assert_eq!(stored_model_pin(THIN_MODEL_KEY, &serde_json::json!("   ")), None);
    }

    #[test]
    fn stored_non_string_value_is_treated_as_unpinned() {
        assert_eq!(stored_model_pin(HEAVY_MODEL_KEY, &serde_json::json!(42)), None);
        assert_eq!(stored_model_pin(HEAVY_MODEL_KEY, &serde_json::json!({"id": "x"})), None);
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
    fn stored_non_boolean_nudges_value_falls_back_to_default_on() {
        // Q7: garbage must not flip the user-facing setting — the default
        // (on) is safe here because nudges are display-only and capture is
        // governed by the watcher gate.
        assert!(NUDGES_ENABLED_DEFAULT);
        assert!(stored_nudges_enabled(&serde_json::json!("false")));
        assert!(stored_nudges_enabled(&serde_json::json!(0)));
        assert!(stored_nudges_enabled(&serde_json::Value::Null));
        assert!(stored_nudges_enabled(&serde_json::json!({"enabled": false})));
    }

    #[test]
    fn stored_positive_integer_cooldown_is_trusted() {
        assert_eq!(stored_nudge_cooldown_secs(&serde_json::json!(60)), 60);
        assert_eq!(stored_nudge_cooldown_secs(&serde_json::json!(1)), 1);
        assert_eq!(stored_nudge_cooldown_secs(&serde_json::json!(86_400)), 86_400);
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
    fn stored_non_boolean_watcher_value_is_treated_as_off() {
        // Q7: garbage in the store must never silently start continuous
        // screen capture — off is the only safe fallback.
        assert!(!stored_watcher_enabled(&serde_json::json!("true")));
        assert!(!stored_watcher_enabled(&serde_json::json!(1)));
        assert!(!stored_watcher_enabled(&serde_json::Value::Null));
        assert!(!stored_watcher_enabled(&serde_json::json!({"enabled": true})));
    }
}
