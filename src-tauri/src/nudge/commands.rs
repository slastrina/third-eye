//! Tauri IPC surface and the single applier for the nudges toggle.
//!
//! Both entry points — the Settings switch (T03, `via = "ipc"`) and any
//! future surface — funnel through [`apply_nudges_enabled`], the
//! watcher/privacy applier pattern (MEM049/MEM053), so they cannot drift.
//! Persistence, rollback on persist failure, the immediate takedown of a
//! parked nudge on disable, and the `nudge://state` broadcast all live here
//! and nowhere else. [`dismiss_active`] is the one shared takedown path —
//! the hotkey's summon (`summoned`) and dismiss (`hidden`) arms and the
//! applier's `disabled` case all clear the active nudge through it.

use tauri::{AppHandle, Emitter, Manager, State};

use super::{DismissReason, NudgeState, NudgeStatus, DISMISS_EVENT, STATE_EVENT};
use crate::overlay::{self, OverlayManager, OverlayState};

/// Emit the current nudge status app-wide. Broadcast failure is cosmetic
/// (the truth stays queryable via `nudge_status`), so it is logged, never
/// bubbled.
pub fn emit_state(app: &AppHandle, status: NudgeStatus) {
    if let Err(e) = app.emit(STATE_EVENT, status) {
        log::warn!("nudge: state broadcast failed: {e}");
    }
}

/// The one shared nudges applier. Persists to settings.json; on persist
/// failure the in-memory toggle is rolled back (an unpersisted toggle must
/// never silently revert on restart) and the error naming the persist path
/// stays queryable on the status. Disabling takes a parked nudge down
/// immediately (`nudge://dismiss` with reason `disabled`); the detector
/// needs no wake — its gate re-reads the toggle every round. Always
/// broadcasts the resulting status.
pub fn apply_nudges_enabled(app: &AppHandle, desired: bool, via: &str) -> NudgeStatus {
    let state = app.state::<NudgeState>();
    let previous = state.set_enabled(desired);
    match crate::config::save_nudges_enabled(app, desired) {
        Ok(()) => {
            state.set_persist_error(None);
            log::info!("nudge: {} (via {via})", if desired { "enabled" } else { "disabled" });
        }
        Err(e) => {
            state.set_enabled(previous);
            log::error!("nudge: {e}");
            state.set_persist_error(Some(e));
        }
    }

    if !state.enabled() {
        dismiss_active(app, DismissReason::Disabled);
    }

    let status = state.status();
    emit_state(app, status.clone());
    status
}

/// Apply the persisted nudges toggle at startup (called from `setup()`
/// before the detector spawns). In-memory only: no re-save, no broadcast —
/// nothing is listening yet. An absent key keeps the default (on); load
/// failures are logged inside `config`, never fatal.
pub fn apply_persisted_nudges_enabled(app: &AppHandle) {
    if let Some(enabled) = crate::config::load_nudges_enabled(app) {
        app.state::<NudgeState>().set_enabled(enabled);
        log::info!("nudge: applied persisted nudges toggle (enabled={enabled})");
    }
}

/// Take the active nudge down, if there is one: clear it, emit
/// `nudge://dismiss` with `reason`, hide the overlay only when it is parked
/// idle (a focused chat is never hidden out from under the user), and
/// broadcast the resulting state. Returns whether a nudge was cleared —
/// callers with no active nudge get a silent no-op.
pub fn dismiss_active(app: &AppHandle, reason: DismissReason) -> bool {
    let state = app.state::<NudgeState>();
    if state.clear_active().is_none() {
        return false;
    }
    log::info!("nudge: dismissed ({})", reason.as_str());
    if let Err(e) = app.emit(DISMISS_EVENT, reason) {
        log::warn!("nudge: {DISMISS_EVENT} broadcast failed: {e}");
    }
    if app.state::<OverlayManager>().current() == OverlayState::VisibleIdle {
        if let Err(e) = overlay::hide_overlay(app.clone()) {
            log::error!("nudge: dismiss hide failed: {e}");
        }
    }
    emit_state(app, state.status());
    true
}

/// Set the nudges toggle from the UI. Returns the resulting [`NudgeStatus`]
/// instead of erroring — a persist failure is data the caller can render,
/// same contract as `set_watcher_enabled`.
#[tauri::command]
pub fn set_nudges_enabled(app: AppHandle, enable: bool) -> NudgeStatus {
    apply_nudges_enabled(&app, enable, "ipc")
}

/// Current nudge state — health-as-value beside `watcher_status` and
/// `memory_status` (R007): a value at any time, never an error.
#[tauri::command]
pub fn nudge_status(state: State<'_, NudgeState>) -> NudgeStatus {
    let status = state.status();
    log::debug!(
        "nudge: status enabled={} active={} lastError={:?} persistError={:?}",
        status.enabled,
        status.active,
        status.last_error.as_ref().map(|e| e.kind()),
        status.persist_error
    );
    status
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_names_are_the_ipc_contract() {
        // src/App.tsx (T03) and e2e/nudge.spec.ts (T04) listen on these
        // exact strings.
        assert_eq!(super::super::SHOW_EVENT, "nudge://show");
        assert_eq!(DISMISS_EVENT, "nudge://dismiss");
        assert_eq!(STATE_EVENT, "nudge://state");
    }
}
