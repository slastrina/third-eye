//! Overlay presentation config (M006 S04). The persisted overlay shape —
//! presentation `mode` (Modal or a Drawer edge) plus the per-edge extents and
//! modal size — owned in Rust and surfaced to the webviews through one command
//! trio and one broadcast. This mirrors [`crate::cloud::optin`] mechanically:
//! a pure in-memory core, an applier that mutates → persists → rolls back on a
//! persist failure → broadcasts, and a startup applier that restores the last
//! shape on launch.
//!
//! The ACL split is deliberate: this module NEVER moves the window. The
//! settings webview holds no window-geometry ACLs (`capabilities/settings.json`
//! is `core:default`), so the applier only persists and broadcasts the new
//! shape; the overlay webview — the sole holder of `setSize`/`setPosition`/
//! `currentMonitor` — listens for [`PRESENTATION_EVENT`] and applies geometry
//! itself (T03). Geometry therefore stays a platform side effect (D040/MEM148):
//! there is no `OverlayState` field and no `dispatch()` change for presentation.

use std::sync::Mutex;

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State};

use crate::config::{
    self, EdgeExtents, OverlayPointConfig, OverlayPresentation, OverlaySizeConfig, PresentationMode,
    OVERLAY_MIN_HEIGHT, OVERLAY_MIN_WIDTH,
};

/// Presentation broadcast: every presentation mutation emits the resulting
/// [`PresentationStatus`] app-wide so the overlay webview applies the new
/// geometry and any other window (Settings) stays truthful whichever surface
/// changed it — the `cloud://optin` / `overlay://state-changed` precedent.
/// `src/overlay-presentation-state.ts` (T03) and `src/App.tsx` listen on this
/// exact string.
pub const PRESENTATION_EVENT: &str = "overlay://presentation";

/// Queryable presentation state: the whole [`OverlayPresentation`] record
/// (flattened: `mode` + `edgeExtents` + `modalSize`) plus a `persistError` —
/// the same health-as-value shape as `CloudOptInStatus` (R007). `persistError`
/// carries the most recent persist failure so a change that could not be saved
/// stays visible after the fact (never an IPC rejection). The camelCase wire
/// mirror is `src/overlay-presentation-state.ts`'s `PresentationStatus`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PresentationStatus {
    #[serde(flatten)]
    pub presentation: OverlayPresentation,
    pub persist_error: Option<String>,
}

/// Floor one UI-supplied dimension to its minimum. Defense in depth beside the
/// config interpreter (`stored_dimension`): a non-finite/negative/sub-min value
/// coming in over IPC is floored to `min` rather than sizing the window below
/// its chrome — the same on-screen invariant the persisted-value fallback
/// guarantees, applied to the live command path.
fn clamp_dim(value: f64, min: f64) -> f64 {
    if value.is_finite() && value >= min {
        value
    } else {
        min
    }
}

/// Pure: the record obtained by switching `mode`, keeping every stored extent
/// so a mode switch losslessly restores that edge's remembered size (D040).
fn with_mode(mut presentation: OverlayPresentation, mode: PresentationMode) -> OverlayPresentation {
    presentation.mode = mode;
    presentation
}

/// Pure: the record obtained by writing the active mode's extent from a live
/// `(width, height)` size. A drawer edge stores only its relevant axis (height
/// for top/bottom, width for left/right); modal stores both. Every axis is
/// floored to its minimum so a live resize can never persist an off-screen or
/// chrome-clipping shape.
fn with_extent(
    mut presentation: OverlayPresentation,
    mode: PresentationMode,
    width: f64,
    height: f64,
) -> OverlayPresentation {
    match mode {
        PresentationMode::Top => {
            presentation.edge_extents.top = clamp_dim(height, OVERLAY_MIN_HEIGHT)
        }
        PresentationMode::Bottom => {
            presentation.edge_extents.bottom = clamp_dim(height, OVERLAY_MIN_HEIGHT)
        }
        PresentationMode::Left => {
            presentation.edge_extents.left = clamp_dim(width, OVERLAY_MIN_WIDTH)
        }
        PresentationMode::Right => {
            presentation.edge_extents.right = clamp_dim(width, OVERLAY_MIN_WIDTH)
        }
        PresentationMode::Modal => {
            presentation.modal_size = OverlaySizeConfig {
                width: clamp_dim(width, OVERLAY_MIN_WIDTH),
                height: clamp_dim(height, OVERLAY_MIN_HEIGHT),
            }
        }
    }
    presentation
}

/// Pure: the record obtained by writing the modal's remembered position from a
/// live `(x, y)` top-left origin. Unlike [`with_extent`], the coordinates carry
/// NO floor — a legal multi-monitor virtual desktop places monitors at negative
/// origins, so a negative x/y is a valid point, not garbage. The finite guard
/// still lives in the config interpreter ([`config::stored_point`]); a live IPC
/// value is trusted as-is here, and the OFF-SCREEN-BUT-FINITE case (a point on a
/// since-removed monitor) is repaired frontend-side against live monitors. The
/// mode is irrelevant: only the modal presentation reads `modal_position`, and
/// carrying it across a mode switch keeps the switch lossless (D040).
fn with_position(mut presentation: OverlayPresentation, x: f64, y: f64) -> OverlayPresentation {
    presentation.modal_position = Some(OverlayPointConfig { x, y });
    presentation
}

/// The one shared presentation core. Pure in-memory state — persistence and the
/// broadcast live in the applier — so the swap/rollback invariants are
/// unit-testable without a Tauri runtime. Defaults to [`OverlayPresentation::default`]
/// (modal at the default size); the persisted shape is applied in `setup()`.
pub struct OverlayPresentationState {
    presentation: Mutex<OverlayPresentation>,
    /// Most recent persist failure (kept until a save succeeds), the surface
    /// the status renders — same shape as the opt-in/watcher cores.
    persist_error: Mutex<Option<String>>,
}

impl Default for OverlayPresentationState {
    fn default() -> Self {
        Self {
            presentation: Mutex::new(OverlayPresentation::default()),
            persist_error: Mutex::new(None),
        }
    }
}

impl OverlayPresentationState {
    pub fn new() -> Self {
        Self::default()
    }

    /// The current presentation record.
    pub fn current(&self) -> OverlayPresentation {
        *self.presentation.lock().unwrap()
    }

    /// Replace the record, returning the previous value so the applier can roll
    /// back on a persist failure.
    pub fn set(&self, presentation: OverlayPresentation) -> OverlayPresentation {
        std::mem::replace(&mut self.presentation.lock().unwrap(), presentation)
    }

    /// Record (or clear) the most recent persist failure.
    pub fn record_persist_error(&self, error: Option<String>) {
        *self.persist_error.lock().unwrap() = error;
    }

    /// The most recent persist failure, if any.
    pub fn persist_error(&self) -> Option<String> {
        self.persist_error.lock().unwrap().clone()
    }

    /// Current presentation as health-as-value — never an error, safe to poll.
    pub fn status(&self) -> PresentationStatus {
        PresentationStatus {
            presentation: self.current(),
            persist_error: self.persist_error(),
        }
    }
}

/// The one shared presentation applier. Persists to settings.json; on persist
/// failure the in-memory record is rolled back (an unpersisted shape must never
/// silently take or revert across a restart) and the error naming the persist
/// path stays queryable. Broadcasts the resulting [`PresentationStatus`]
/// app-wide ([`PRESENTATION_EVENT`]) so the overlay webview applies the geometry
/// and every window stays truthful, then returns that same status — the value
/// the calling window renders without a second query. It does NOT move the
/// window: geometry is the overlay webview's job (the ACL split).
pub fn apply_overlay_presentation(
    app: &AppHandle,
    desired: OverlayPresentation,
    via: &str,
) -> PresentationStatus {
    let state = app.state::<OverlayPresentationState>();
    let previous = state.set(desired);
    match config::save_overlay_presentation(app, &desired) {
        Ok(()) => {
            state.record_persist_error(None);
            log::info!("overlay: presentation set to {:?} (via {via})", desired.mode);
        }
        Err(e) => {
            state.set(previous);
            log::error!("overlay: {e}");
            state.record_persist_error(Some(e));
        }
    }
    let status = state.status();
    // Broadcast failure is cosmetic (the truth stays queryable via
    // `overlay_presentation`), so it is logged, never bubbled.
    if let Err(e) = app.emit(PRESENTATION_EVENT, status.clone()) {
        log::warn!("overlay: presentation broadcast failed: {e}");
    }
    status
}

/// Set the presentation mode from the UI, keeping every stored extent so the
/// switch restores that edge's remembered size (D040). Returns the resulting
/// [`PresentationStatus`] instead of erroring — a persist failure is data the
/// caller can render (R007).
#[tauri::command]
pub fn set_overlay_presentation(app: AppHandle, mode: PresentationMode) -> PresentationStatus {
    let current = app.state::<OverlayPresentationState>().current();
    apply_overlay_presentation(&app, with_mode(current, mode), "ipc")
}

/// Set the active mode's extent from a live `(width, height)` size — the
/// resize-end / Settings surface. A drawer edge persists only its relevant axis
/// (the caller passes the full inner size and the backend floors + selects it);
/// modal persists both. Returns the resulting [`PresentationStatus`]; a persist
/// failure is data, never a reject.
#[tauri::command]
pub fn set_overlay_extent(
    app: AppHandle,
    mode: PresentationMode,
    width: f64,
    height: f64,
) -> PresentationStatus {
    let current = app.state::<OverlayPresentationState>().current();
    apply_overlay_presentation(&app, with_extent(current, mode, width, height), "ipc")
}

/// Persist the modal's remembered position from a live `(x, y)` top-left origin
/// — the drag-end surface, mirroring [`set_overlay_extent`]. The caller passes
/// LOGICAL px (the overlay webview converts `outerPosition()`'s physical px via
/// `scaleFactor`); the coordinates are stored as-is (no floor: negative
/// multi-monitor origins are legal). Persists+broadcasts via
/// [`apply_overlay_presentation`] and NEVER moves the window — geometry is the
/// overlay webview's job (the ACL split). Returns the resulting
/// [`PresentationStatus`]; a persist failure is data, never a reject (R007).
#[tauri::command]
pub fn set_overlay_position(app: AppHandle, x: f64, y: f64) -> PresentationStatus {
    let current = app.state::<OverlayPresentationState>().current();
    apply_overlay_presentation(&app, with_position(current, x, y), "ipc")
}

/// Current presentation — health-as-value beside `cloud_optin_status` (R007): a
/// value at any time, never an error. The overlay webview reads this on mount
/// to restore the persisted shape (the "restores after relaunch" path).
#[tauri::command]
pub fn overlay_presentation(state: State<'_, OverlayPresentationState>) -> PresentationStatus {
    state.status()
}

/// Apply the persisted presentation at startup (called from `setup()`). In
/// memory only: no re-save, no broadcast — nothing is listening yet, and the
/// overlay webview reads the shape via `overlay_presentation` on mount. An
/// absent key keeps the default (modal); a garbage value is repaired
/// field-by-field inside `config::stored_overlay_presentation`, so this can
/// never adopt an off-screen shape.
pub fn apply_persisted_overlay_presentation(app: &AppHandle) {
    if let Some(presentation) = config::load_overlay_presentation(app) {
        app.state::<OverlayPresentationState>().set(presentation);
        log::info!("overlay: applied persisted presentation ({:?})", presentation.mode);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn defaults() -> OverlayPresentation {
        OverlayPresentation::default()
    }

    #[test]
    fn presentation_defaults_to_modal_with_no_persist_error() {
        let state = OverlayPresentationState::new();
        assert_eq!(state.current(), defaults());
        assert_eq!(state.current().mode, PresentationMode::Modal);
        assert_eq!(state.persist_error(), None);
    }

    #[test]
    fn set_replaces_and_returns_previous_for_rollback() {
        let state = OverlayPresentationState::new();
        let next = with_mode(defaults(), PresentationMode::Left);
        let previous = state.set(next);
        assert_eq!(previous, defaults(), "set returns the prior record for rollback");
        assert_eq!(state.current(), next);
        // Rolling back restores the exact prior record.
        state.set(previous);
        assert_eq!(state.current(), defaults());
    }

    #[test]
    fn persist_errors_are_queryable_and_clearable() {
        let state = OverlayPresentationState::new();
        state.record_persist_error(Some("failed to persist overlayPresentation".into()));
        assert!(state.persist_error().unwrap().contains("overlayPresentation"));
        state.record_persist_error(None);
        assert_eq!(state.persist_error(), None);
    }

    #[test]
    fn event_name_is_the_ipc_contract() {
        // src/overlay-presentation-state.ts (T03) and e2e listen on this exact
        // string — the cloud://optin / overlay://state-changed precedent.
        assert_eq!(PRESENTATION_EVENT, "overlay://presentation");
    }

    #[test]
    fn with_mode_preserves_every_extent() {
        // A mode switch must be lossless (D040): only `mode` changes; the
        // per-edge extents and modal size are carried forward untouched.
        let base = defaults();
        let switched = with_mode(base, PresentationMode::Right);
        assert_eq!(switched.mode, PresentationMode::Right);
        assert_eq!(switched.edge_extents, base.edge_extents);
        assert_eq!(switched.modal_size, base.modal_size);
    }

    #[test]
    fn with_extent_writes_only_the_active_edge_axis() {
        let base = defaults();
        // Top/bottom store height; left/right store width — the other axis and
        // the other three edges are untouched.
        let top = with_extent(base, PresentationMode::Top, 999.0, 260.0);
        assert_eq!(top.edge_extents.top, 260.0);
        assert_eq!(top.edge_extents.bottom, base.edge_extents.bottom);
        assert_eq!(top.edge_extents.left, base.edge_extents.left);

        let left = with_extent(base, PresentationMode::Left, 500.0, 999.0);
        assert_eq!(left.edge_extents.left, 500.0);
        assert_eq!(left.edge_extents.right, base.edge_extents.right);
        assert_eq!(left.modal_size, base.modal_size);
    }

    #[test]
    fn with_extent_modal_writes_both_axes() {
        let modal = with_extent(defaults(), PresentationMode::Modal, 800.0, 600.0);
        assert_eq!(modal.modal_size, OverlaySizeConfig { width: 800.0, height: 600.0 });
        assert_eq!(modal.edge_extents, defaults().edge_extents, "edges untouched by a modal resize");
    }

    #[test]
    fn with_extent_floors_sub_min_and_non_finite_values() {
        // The live-command mirror of the persisted-value fallback: a UI-sent
        // sub-min, negative, or non-finite size floors to the minimum rather
        // than sizing the window below its chrome.
        let tiny = with_extent(defaults(), PresentationMode::Left, 10.0, 10.0);
        assert_eq!(tiny.edge_extents.left, OVERLAY_MIN_WIDTH);

        let nan = with_extent(defaults(), PresentationMode::Modal, f64::NAN, f64::NEG_INFINITY);
        assert_eq!(nan.modal_size.width, OVERLAY_MIN_WIDTH);
        assert_eq!(nan.modal_size.height, OVERLAY_MIN_HEIGHT);

        let negative = with_extent(defaults(), PresentationMode::Bottom, 400.0, -5.0);
        assert_eq!(negative.edge_extents.bottom, OVERLAY_MIN_HEIGHT);
    }

    #[test]
    fn with_position_stores_the_point_verbatim_with_no_floor() {
        // Position, unlike an extent, carries NO floor: legal multi-monitor
        // virtual desktops place monitors at negative origins, so a negative
        // coordinate is a valid point stored as-is. Only `modal_position`
        // changes; the mode and every extent are carried forward untouched.
        let base = defaults();
        let moved = with_position(base, -1920.0, -128.0);
        assert_eq!(moved.modal_position, Some(OverlayPointConfig { x: -1920.0, y: -128.0 }));
        assert_eq!(moved.mode, base.mode);
        assert_eq!(moved.edge_extents, base.edge_extents);
        assert_eq!(moved.modal_size, base.modal_size);

        // A finite positive point round-trips identically (no clamp/select).
        let moved = with_position(base, 512.0, 384.0);
        assert_eq!(moved.modal_position, Some(OverlayPointConfig { x: 512.0, y: 384.0 }));
    }

    #[test]
    fn status_is_health_as_value_camelcase() {
        // The broadcast/command payload contract: { mode, edgeExtents,
        // modalSize, modalPosition, persistError } — a flattened record plus
        // the error. modalPosition serializes as an object when the modal has a
        // remembered point.
        let state = OverlayPresentationState::new();
        state.set(with_position(with_mode(defaults(), PresentationMode::Top), 256.0, 128.0));
        state.record_persist_error(Some("failed to persist overlayPresentation".into()));
        let v = serde_json::to_value(state.status()).unwrap();
        let obj = v.as_object().unwrap();
        assert_eq!(obj.len(), 5, "presentation status shape drifted: {obj:?}");
        assert_eq!(obj["mode"], "top");
        assert!(obj["edgeExtents"].is_object());
        assert!(obj["modalSize"].is_object());
        assert_eq!(obj["modalPosition"], serde_json::json!({ "x": 256.0, "y": 128.0 }));
        assert!(obj["persistError"].as_str().unwrap().contains("overlayPresentation"));
    }

    #[test]
    fn status_serializes_no_error_as_null() {
        // The default (never-moved) record: no persist error AND no remembered
        // position — both flattened optionals must serialize as JSON null, not
        // vanish, so the frontend can read `modalPosition === null` → center.
        let v = serde_json::to_value(OverlayPresentationState::new().status()).unwrap();
        assert_eq!(v["mode"], "modal");
        assert!(v["persistError"].is_null(), "absent persist error must be JSON null");
        assert!(v["modalPosition"].is_null(), "never-moved modal position must be JSON null");
    }

    #[test]
    fn edge_extents_default_is_a_valid_on_screen_shape() {
        // The fallback record the applier can never make off-screen: every
        // default extent sits at or above the mins.
        let EdgeExtents { top, bottom, left, right } = defaults().edge_extents;
        assert!(top >= OVERLAY_MIN_HEIGHT && bottom >= OVERLAY_MIN_HEIGHT);
        assert!(left >= OVERLAY_MIN_WIDTH && right >= OVERLAY_MIN_WIDTH);
    }
}
