//! Tauri IPC surface and the single applier for the watcher toggle.
//!
//! Both entry points — the tray check item (T04, `via = "tray"`) and the
//! `set_watcher_enabled` IPC (`via = "ipc"`) — funnel through
//! [`apply_watcher_enabled`], the privacy-mode applier pattern
//! (MEM049/MEM053), so they cannot drift. Persistence, rollback on persist
//! failure, the one-time permission ask on enable, the loop wake, and the
//! `watcher://state` broadcast all live here and nowhere else.

use tauri::{AppHandle, Emitter, Manager, State};

use super::{decide_run_state, TextObservation, WatcherState, WatcherStatus};

/// Watcher-state broadcast: every toggle, run-state transition, and tick
/// error change emits the resulting [`WatcherStatus`] app-wide, so the
/// Settings diagnostics and the tray stay truthful whichever surface
/// flipped the toggle.
pub const WATCHER_STATE_EVENT: &str = "watcher://state";

/// Per-tick observation broadcast for the Settings diagnostics surface —
/// the same payload S02 will consume in-process via
/// [`WatcherState::subscribe`].
pub const WATCHER_OBSERVATION_EVENT: &str = "watcher://observation";

/// Emit the current watcher status app-wide. Broadcast failure is cosmetic
/// (the truth stays queryable via `watcher_status`), so it is logged, never
/// bubbled.
pub fn emit_state(app: &AppHandle, status: WatcherStatus) {
    if let Err(e) = app.emit(WATCHER_STATE_EVENT, status) {
        log::warn!("watcher: state broadcast failed: {e}");
    }
}

/// Emit one observation app-wide for the diagnostics surface. Same
/// cosmetic-failure posture as [`emit_state`].
pub fn emit_observation(app: &AppHandle, observation: TextObservation) {
    if let Err(e) = app.emit(WATCHER_OBSERVATION_EVENT, observation) {
        log::warn!("watcher: observation broadcast failed: {e}");
    }
}

/// The one shared watcher applier. Persists to settings.json; on persist
/// failure the in-memory toggle is rolled back (an unpersisted toggle must
/// never silently revert on restart) and the error naming the persist path
/// stays queryable on the status. On a successful off→on transition the
/// Screen Recording ask happens once, here — never per tick, so the loop
/// can never prompt-spam (macOS suppresses repeats anyway, MEM035). Always
/// reflects the decided run state immediately, wakes the loop, broadcasts
/// the resulting status, and resyncs the tray check item (T04).
pub fn apply_watcher_enabled(app: &AppHandle, desired: bool, via: &str) -> WatcherStatus {
    let state = app.state::<WatcherState>();
    let previous = state.enabled();
    state.set_enabled(desired);
    match crate::config::save_watcher_enabled(app, desired) {
        Ok(()) => {
            state.record_persist_error(None);
            log::info!(
                "watcher: {} (via {via})",
                if desired { "enabled" } else { "disabled" }
            );
        }
        Err(e) => {
            state.set_enabled(previous);
            log::error!("watcher: {e}");
            state.record_persist_error(Some(e));
        }
    }

    if state.enabled() && !previous {
        let permission = crate::capture::permission_status();
        if permission.supported && !permission.granted {
            let granted = crate::capture::request_permission();
            log::info!("watcher: Screen Recording requested on enable, granted={granted}");
        }
    }

    // The status returned to the toggling surface must already show the
    // decided run state — waiting for the loop's next tick would flash a
    // stale state in the UI. The loop converges on the same value because
    // both sides derive it from the same pure function.
    let privacy = app
        .try_state::<crate::capture::PrivacyState>()
        .map(|p| p.enabled())
        .unwrap_or(false);
    let next = decide_run_state(state.enabled(), privacy);
    if state.set_run_state(next) {
        log::info!("watcher: state -> {}", next.as_str());
    }
    state.wake();

    let status = state.status();
    emit_state(app, status.clone());
    // Resync the tray check item to the post-persist truth, whichever
    // surface flipped the toggle — a rolled-back persist visibly flips the
    // clicked item back (Q5), and an IPC toggle updates the tray too.
    crate::tray::sync_watcher_check(app, status.enabled);
    status
}

/// Apply the persisted watcher toggle at startup (called from `setup()`
/// after privacy, before the tray builds, so the T04 check item reflects it
/// across restarts). In-memory only: no re-save, no broadcast, no
/// permission ask — nothing is listening yet and the loop's first tick
/// surfaces any permission problem as a typed error. An absent key keeps
/// the default (off); load failures are logged inside `config`, never
/// fatal.
pub fn apply_persisted_watcher_enabled(app: &AppHandle) {
    if let Some(enabled) = crate::config::load_watcher_enabled(app) {
        app.state::<WatcherState>().set_enabled(enabled);
        log::info!("watcher: applied persisted watcher toggle (enabled={enabled})");
    }
}

/// Set the watcher toggle from the UI. Returns the resulting
/// [`WatcherStatus`] instead of erroring — a persist failure is data the
/// caller can render, same contract as `set_privacy_mode`.
#[tauri::command]
pub fn set_watcher_enabled(app: AppHandle, enable: bool) -> WatcherStatus {
    apply_watcher_enabled(&app, enable, "ipc")
}

/// Current watcher state — health-as-value beside `privacy_status` and
/// `hotkey_status` (R007): a value at any time, never an error.
#[tauri::command]
pub fn watcher_status(state: State<'_, WatcherState>) -> WatcherStatus {
    let status = state.status();
    log::debug!(
        "watcher: status enabled={} state={} lastTickError={:?} error={:?}",
        status.enabled,
        status.state.as_str(),
        status.last_tick_error.as_ref().map(|e| e.kind()),
        status.error
    );
    status
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_names_are_the_ipc_contract() {
        // src/watcher-state.ts (T05) and e2e/watcher.spec.ts (T06) listen
        // on these exact strings.
        assert_eq!(WATCHER_STATE_EVENT, "watcher://state");
        assert_eq!(WATCHER_OBSERVATION_EVENT, "watcher://observation");
    }
}
