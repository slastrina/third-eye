//! Launch at login via tauri-plugin-autostart (R010).
//!
//! The OS owns the state — a macOS LaunchAgent, Windows registry run key, or
//! Linux XDG autostart entry — so it survives restarts with no app-side
//! persistence. Failures are never silent: every failed enable/disable or
//! state query is error-logged naming the operation and cause, and kept
//! queryable as a typed [`AutostartStatus`] via the `autostart_status`
//! command — the same health-as-value shape the hotkey surface uses.

use std::sync::Mutex;

use serde::Serialize;
use tauri::{AppHandle, Manager};
use tauri_plugin_autostart::ManagerExt;

/// Queryable launch-at-login health: the current OS-owned state plus the
/// most recent failure, if any.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AutostartStatus {
    pub enabled: bool,
    pub error: Option<String>,
}

/// Managed record of the last toggle failure, so `autostart_status` still
/// reports a failed toggle after the live OS query starts succeeding again.
/// A successful toggle clears it.
#[derive(Default)]
pub struct AutostartState {
    last_error: Mutex<Option<String>>,
}

/// Pure outcome mapping for one toggle attempt: success means the OS now
/// matches `desired`; failure keeps a typed error naming the failed
/// operation and reports the re-queried OS state (`os_enabled`), falling
/// back to the pre-toggle state when even that query failed (Q5/Q7).
pub fn status_after_toggle(
    desired: bool,
    result: Result<(), String>,
    os_enabled: Option<bool>,
) -> AutostartStatus {
    match result {
        Ok(()) => AutostartStatus { enabled: desired, error: None },
        Err(e) => AutostartStatus {
            enabled: os_enabled.unwrap_or(!desired),
            error: Some(format!(
                "autostart: {} launch at login failed: {e}",
                if desired { "enable" } else { "disable" }
            )),
        },
    }
}

/// Live OS state for menu construction: a failed query is logged and shown
/// as disabled rather than blocking the tray build (Q5).
pub fn is_enabled(app: &AppHandle) -> bool {
    app.autolaunch().is_enabled().unwrap_or_else(|e| {
        log::error!("autostart: launch-at-login state query failed (showing disabled): {e}");
        false
    })
}

/// Flip launch-at-login to the opposite of the current OS state and return
/// the resulting status. Drives the tray check item.
pub fn toggle(app: &AppHandle) -> AutostartStatus {
    apply(app, !is_enabled(app))
}

/// Drive the OS launcher entry to `desired`, log the outcome, and record it
/// on the managed [`AutostartState`].
pub fn apply(app: &AppHandle, desired: bool) -> AutostartStatus {
    let launcher = app.autolaunch();
    let result =
        if desired { launcher.enable() } else { launcher.disable() }.map_err(|e| e.to_string());
    // Re-query after the attempt: the OS owns the state, so the status must
    // report what the launcher entry actually is, not what was requested.
    let os_enabled = launcher.is_enabled().ok();
    let status = status_after_toggle(desired, result, os_enabled);
    match &status.error {
        None => log::info!(
            "autostart: launch at login {} (enabled={})",
            if desired { "enabled" } else { "disabled" },
            status.enabled
        ),
        Some(e) => log::error!("{e}"),
    }
    if let Some(state) = app.try_state::<AutostartState>() {
        *state.last_error.lock().unwrap() = status.error.clone();
    }
    status
}

/// Current status as health-as-value: a failed OS query is itself reported
/// in the value, never as an IPC error.
pub fn current_status(app: &AppHandle) -> AutostartStatus {
    let last_error =
        app.try_state::<AutostartState>().and_then(|s| s.last_error.lock().unwrap().clone());
    match app.autolaunch().is_enabled() {
        Ok(enabled) => AutostartStatus { enabled, error: last_error },
        Err(e) => AutostartStatus {
            enabled: false,
            error: Some(format!("autostart: launch-at-login state query failed: {e}")),
        },
    }
}

/// Set launch-at-login from the UI. Returns the resulting status instead of
/// erroring, so a failure is data the caller can render.
#[tauri::command]
pub fn set_autostart(app: AppHandle, enable: bool) -> AutostartStatus {
    apply(&app, enable)
}

/// Expose launch-at-login state to the UI: `{ enabled, error }`.
#[tauri::command]
pub fn autostart_status(app: AppHandle) -> AutostartStatus {
    current_status(&app)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn successful_toggle_reports_desired_state_and_no_error() {
        for desired in [true, false] {
            let s = status_after_toggle(desired, Ok(()), Some(desired));
            assert_eq!(s, AutostartStatus { enabled: desired, error: None });
        }
    }

    #[test]
    fn failed_toggle_reports_requeried_os_state_and_names_operation_and_cause() {
        // Q5: a failed registration must not lie about the OS state, and the
        // error must name what failed and why.
        let s = status_after_toggle(true, Err("plist write denied".into()), Some(false));
        assert!(!s.enabled);
        let err = s.error.expect("failure must carry an error");
        assert!(err.contains("enable"), "names the operation: {err}");
        assert!(err.contains("plist write denied"), "names the cause: {err}");

        let s = status_after_toggle(false, Err("agent busy".into()), Some(true));
        assert!(s.enabled, "failed disable leaves the entry registered");
        assert!(s.error.unwrap().contains("disable"));
    }

    #[test]
    fn failed_toggle_with_unknown_os_state_falls_back_to_pre_toggle_state() {
        // Q7 boundary: when even the re-query fails, report the last state
        // the OS was known to be in — the pre-toggle one, never the desired.
        assert!(!status_after_toggle(true, Err("boom".into()), None).enabled);
        assert!(status_after_toggle(false, Err("boom".into()), None).enabled);
    }

    #[test]
    fn status_serializes_enabled_and_error_fields() {
        let v = serde_json::to_value(AutostartStatus { enabled: true, error: None }).unwrap();
        assert_eq!(v, serde_json::json!({ "enabled": true, "error": null }));
        let v = serde_json::to_value(AutostartStatus {
            enabled: false,
            error: Some("autostart: enable launch at login failed: x".into()),
        })
        .unwrap();
        assert_eq!(v["enabled"], false);
        assert_eq!(v["error"], "autostart: enable launch at login failed: x");
    }
}
