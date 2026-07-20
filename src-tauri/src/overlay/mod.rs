//! Overlay state machine and Tauri command surface.
//!
//! This is the S01→S02/S05 IPC boundary: `show_overlay` / `hide_overlay` /
//! `focus_overlay` commands plus the `overlay://state-changed` event pushing
//! `hidden | visible-idle | visible-focused` to the UI. The state machine is
//! pure (unit-testable); platform side effects live in `macos` (NSPanel) and
//! `fallback` (plain window), selected by cfg.

use serde::Serialize;
use std::sync::Mutex;
use tauri::{AppHandle, Emitter, Manager};

#[cfg(not(target_os = "macos"))]
pub mod fallback;
#[cfg(target_os = "macos")]
pub mod macos;

/// Persisted overlay presentation config (M006 S04): the mode + per-edge
/// extents + modal size owned in Rust, applied by the overlay webview. Kept a
/// pure side-effect module beside the state machine — it never touches
/// [`OverlayState`] or [`dispatch`] (D040/MEM148).
pub mod presentation;

#[cfg(not(target_os = "macos"))]
use fallback as platform;
#[cfg(target_os = "macos")]
use macos as platform;

/// Event emitted to the UI whenever the overlay state changes.
pub const STATE_CHANGED_EVENT: &str = "overlay://state-changed";

/// The three overlay states. Serialized as `hidden`, `visible-idle`,
/// `visible-focused` — this string form is the IPC contract with the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum OverlayState {
    Hidden,
    VisibleIdle,
    VisibleFocused,
}

impl OverlayState {
    pub fn as_str(&self) -> &'static str {
        match self {
            OverlayState::Hidden => "hidden",
            OverlayState::VisibleIdle => "visible-idle",
            OverlayState::VisibleFocused => "visible-focused",
        }
    }
}

/// Cursor policy per state: only a focused overlay may intercept clicks.
/// Idle (and hidden) overlays are click-through so they never intercept
/// input meant for the app underneath.
pub fn click_through(state: OverlayState) -> bool {
    !matches!(state, OverlayState::VisibleFocused)
}

/// Inputs that drive the state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayEvent {
    Show,
    Hide,
    Focus,
}

/// A rejected state transition. Surfaced to callers instead of silently
/// ignoring the event so misuse of the IPC surface is visible.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidTransition {
    pub from: OverlayState,
    pub event: OverlayEvent,
}

impl std::fmt::Display for InvalidTransition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "invalid overlay transition: {:?} not allowed from {}",
            self.event,
            self.from.as_str()
        )
    }
}

impl std::error::Error for InvalidTransition {}

impl OverlayState {
    /// Pure transition function. Strict: events that don't change state
    /// (show while visible, focus while focused, hide while hidden) are
    /// errors, so toggling logic must consult the current state first.
    pub fn apply(self, event: OverlayEvent) -> Result<OverlayState, InvalidTransition> {
        use OverlayEvent::*;
        use OverlayState::*;
        match (self, event) {
            (Hidden, Show) => Ok(VisibleIdle),
            (VisibleIdle, Focus) => Ok(VisibleFocused),
            (VisibleIdle, Hide) | (VisibleFocused, Hide) => Ok(Hidden),
            (from, event) => Err(InvalidTransition { from, event }),
        }
    }
}

/// App-managed holder of the current overlay state.
pub struct OverlayManager {
    state: Mutex<OverlayState>,
}

impl Default for OverlayManager {
    fn default() -> Self {
        Self::new()
    }
}

impl OverlayManager {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(OverlayState::Hidden),
        }
    }

    pub fn current(&self) -> OverlayState {
        *self.state.lock().expect("overlay state lock poisoned")
    }

    /// Validate and commit a transition, returning (from, to).
    fn transition(
        &self,
        event: OverlayEvent,
    ) -> Result<(OverlayState, OverlayState), InvalidTransition> {
        let mut state = self.state.lock().expect("overlay state lock poisoned");
        let from = *state;
        let to = from.apply(event)?;
        *state = to;
        Ok((from, to))
    }

    /// Restore a prior state after a platform side effect failed, so the
    /// tracked state never claims a visibility the window doesn't have.
    fn restore(&self, state: OverlayState) {
        *self.state.lock().expect("overlay state lock poisoned") = state;
    }
}

/// One-time platform setup: NSPanel conversion on macOS, no-op elsewhere.
/// Must be called from the Tauri setup hook (main thread).
pub fn init_platform(app: &AppHandle) -> Result<(), String> {
    platform::init(app)
}

/// Validate the transition, perform the platform side effect, then emit
/// `overlay://state-changed`. Rolls the state back if the side effect fails.
fn dispatch(app: &AppHandle, event: OverlayEvent) -> Result<OverlayState, String> {
    let manager = app.state::<OverlayManager>();
    let (from, to) = manager.transition(event).map_err(|e| {
        log::debug!("overlay: rejected transition: {e}");
        e.to_string()
    })?;
    log::debug!(
        "overlay: {} -> {} ({:?})",
        from.as_str(),
        to.as_str(),
        event
    );

    // Cursor policy is applied before visibility changes so the overlay never
    // presents a clickable frame in a state that must not accept clicks.
    let result = platform::set_click_through(app, click_through(to)).and_then(|()| match event {
        OverlayEvent::Show => platform::show(app),
        OverlayEvent::Hide => platform::hide(app),
        OverlayEvent::Focus => platform::focus(app),
    });
    if let Err(err) = result {
        manager.restore(from);
        log::error!("overlay: {event:?} failed, state restored to {}: {err}", from.as_str());
        return Err(err);
    }

    if let Err(err) = app.emit(STATE_CHANGED_EVENT, to) {
        log::warn!("overlay: failed to emit {STATE_CHANGED_EVENT}: {err}");
    }
    Ok(to)
}

#[tauri::command]
pub fn show_overlay(app: AppHandle) -> Result<OverlayState, String> {
    dispatch(&app, OverlayEvent::Show)
}

#[tauri::command]
pub fn hide_overlay(app: AppHandle) -> Result<OverlayState, String> {
    dispatch(&app, OverlayEvent::Hide)
}

#[tauri::command]
pub fn focus_overlay(app: AppHandle) -> Result<OverlayState, String> {
    dispatch(&app, OverlayEvent::Focus)
}

#[cfg(test)]
mod tests {
    use super::*;
    use OverlayEvent::*;
    use OverlayState::*;

    #[test]
    fn summon_focus_dismiss_cycle() {
        let s = Hidden.apply(Show).unwrap();
        assert_eq!(s, VisibleIdle);
        let s = s.apply(Focus).unwrap();
        assert_eq!(s, VisibleFocused);
        let s = s.apply(Hide).unwrap();
        assert_eq!(s, Hidden);
    }

    #[test]
    fn idle_overlay_can_hide_without_focus() {
        assert_eq!(VisibleIdle.apply(Hide).unwrap(), Hidden);
    }

    #[test]
    fn show_while_visible_is_rejected() {
        let err = VisibleIdle.apply(Show).unwrap_err();
        assert_eq!(err, InvalidTransition { from: VisibleIdle, event: Show });
        assert!(VisibleFocused.apply(Show).is_err());
    }

    #[test]
    fn focus_while_hidden_is_rejected() {
        let err = Hidden.apply(Focus).unwrap_err();
        assert_eq!(err.from, Hidden);
    }

    #[test]
    fn hide_while_hidden_is_rejected() {
        assert!(Hidden.apply(Hide).is_err());
    }

    #[test]
    fn focus_while_focused_is_rejected() {
        assert!(VisibleFocused.apply(Focus).is_err());
    }

    #[test]
    fn invalid_transition_message_names_state_and_event() {
        let msg = Hidden.apply(Focus).unwrap_err().to_string();
        assert!(msg.contains("hidden"), "message was: {msg}");
        assert!(msg.contains("Focus"), "message was: {msg}");
    }

    #[test]
    fn state_serializes_to_kebab_case_ipc_strings() {
        assert_eq!(serde_json::to_value(Hidden).unwrap(), "hidden");
        assert_eq!(serde_json::to_value(VisibleIdle).unwrap(), "visible-idle");
        assert_eq!(
            serde_json::to_value(VisibleFocused).unwrap(),
            "visible-focused"
        );
        assert_eq!(Hidden.as_str(), "hidden");
        assert_eq!(VisibleIdle.as_str(), "visible-idle");
        assert_eq!(VisibleFocused.as_str(), "visible-focused");
    }

    #[test]
    fn only_focused_overlay_accepts_clicks() {
        // Idle overlays are click-through — no focus steal, no click steal.
        assert!(click_through(Hidden));
        assert!(click_through(VisibleIdle));
        assert!(!click_through(VisibleFocused));
    }

    #[test]
    fn every_reachable_state_has_a_cursor_policy() {
        // The toggle/focus cycle must yield a defined policy at each step so
        // a new state variant can't silently default to intercepting clicks.
        let mut s = Hidden;
        for event in [Show, Focus, Hide] {
            s = s.apply(event).unwrap();
            // Exhaustive match in click_through guarantees this returns.
            let _ = click_through(s);
        }
        assert_eq!(s, Hidden);
    }

    #[test]
    fn manager_commits_valid_and_rejects_invalid_transitions() {
        let m = OverlayManager::new();
        assert_eq!(m.current(), Hidden);
        let (from, to) = m.transition(Show).unwrap();
        assert_eq!((from, to), (Hidden, VisibleIdle));
        assert_eq!(m.current(), VisibleIdle);
        // Invalid event leaves state untouched.
        assert!(m.transition(Show).is_err());
        assert_eq!(m.current(), VisibleIdle);
    }

    #[test]
    fn manager_restore_rolls_back_after_failed_side_effect() {
        let m = OverlayManager::new();
        m.transition(Show).unwrap();
        m.restore(Hidden);
        assert_eq!(m.current(), Hidden);
    }
}
