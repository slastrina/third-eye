//! First-run onboarding (M006): the Tauri IPC surface that lets the overlay's
//! first-launch explainer request the OS permissions the app needs, then mark
//! onboarding done so the panel never shows again.
//!
//! This module owns no state of its own — it composes the existing managed
//! [`crate::capture::commands::CaptureState`] and
//! [`crate::input::commands::InputState`] (the permission backends) and the
//! persisted `firstRunComplete` flag ([`crate::config`]). That composition IS
//! onboarding: showing the user, once, why the app wants Screen Recording and
//! Accessibility, and spending the one-shot macOS TCC prompts with context
//! rather than silently on a cold launch from an invisible Accessory app.
//!
//! Structural honesty (D038/R019): [`request_input_permission`] requests the
//! Accessibility *grant* only — it does not arm HID. HID stays `Off` until the
//! explicit, permission-gated Settings toggle; first-run onboarding merely
//! pre-grants the OS permission so a later arm is a one-click flip. Requesting
//! the grant here can never make the model able to click or type.
//!
//! Failure policy mirrors the rest of the app: the status commands are
//! health-as-value (never error), and `complete_first_run` returns the
//! resulting state rather than erroring — a persist failure is data the caller
//! can render, and an unpersisted flag at worst re-shows the harmless explainer.

use serde::Serialize;
use tauri::{AppHandle, Manager, State};

use crate::capture::{commands::CaptureState, CapturePermission};
use crate::input::{commands::InputState, InputPermission};

/// The first-run onboarding snapshot the overlay renders: whether onboarding is
/// still pending, plus the live permission state for both surfaces so the panel
/// can show what is already granted and what a persist failure left behind.
/// Health-as-value — never an error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FirstRunStatus {
    /// `true` while the user has not yet completed or skipped onboarding — the
    /// signal the overlay uses to show the explainer. Flips to `false` (and
    /// stays there) once [`complete_first_run`] persists the flag.
    pub pending: bool,
    /// Live Screen Recording permission (the watcher/capture loop's grant).
    pub capture: CapturePermission,
    /// Live Accessibility permission (HID's grant; requesting it does not arm HID).
    pub input: InputPermission,
    /// The last persist failure from [`complete_first_run`], if any — kept as a
    /// plain string (the flag is not typed like the arm/privacy errors) so the
    /// panel can surface that the "done" flag could not be saved.
    pub persist_error: Option<String>,
}

/// Whether the overlay should be summoned at launch for the first-run explainer:
/// onboarding is still pending AND at least one permission surface is supported
/// on this platform (there is nothing to onboard on a platform with neither
/// prompt). Mirrors the frontend show gate so the two never disagree — the
/// backend decides whether to *show* the overlay, the frontend whether to
/// *render* the panel, and both must agree. Reads managed state; safe from
/// `setup()`.
#[cfg(desktop)]
pub fn should_show_on_launch(app: &AppHandle) -> bool {
    if crate::config::load_first_run_complete(app) {
        return false;
    }
    let capture_supported = app.state::<CaptureState>().permission().supported;
    let input_supported = app.state::<InputState>().permission().supported;
    capture_supported || input_supported
}

/// Read the first-run onboarding snapshot — health-as-value, never an error,
/// safe for the overlay to query on mount. `pending` is the persisted flag
/// inverted; the permissions are read live so a grant made out of band (e.g. in
/// System Settings) is reflected without a restart.
#[tauri::command]
pub fn first_run_status(
    app: AppHandle,
    capture: State<'_, CaptureState>,
    input: State<'_, InputState>,
) -> FirstRunStatus {
    let pending = !crate::config::load_first_run_complete(&app);
    let status = FirstRunStatus {
        pending,
        capture: capture.permission(),
        input: input.permission(),
        persist_error: None,
    };
    log::debug!(
        "onboarding: first_run_status pending={pending} capture_granted={} input_granted={}",
        status.capture.granted,
        status.input.granted
    );
    status
}

/// Request the Screen Recording OS prompt (the watcher/capture grant) and return
/// the resulting permission value. Spends the one-shot macOS TCC prompt; after a
/// prior denial macOS suppresses it and the value comes back ungranted, at which
/// point the Settings deep-link (`open_capture_settings`) is the recourse.
#[tauri::command]
pub fn request_capture_permission(capture: State<'_, CaptureState>) -> CapturePermission {
    let result = capture.request_permission();
    log::info!(
        "onboarding: requested capture permission, granted={} supported={}",
        result.granted,
        result.supported
    );
    result
}

/// Request the Accessibility OS prompt (HID's grant) and return the resulting
/// permission value. Requesting the grant does NOT arm HID — arming stays the
/// explicit, permission-gated Settings choice (D038/R019); this only pre-grants
/// the OS permission so a later arm is a one-click flip. After a prior denial
/// macOS suppresses the prompt and the value comes back ungranted, at which
/// point the Settings deep-link (`open_input_settings`) is the recourse.
#[tauri::command]
pub fn request_input_permission(input: State<'_, InputState>) -> InputPermission {
    let result = input.request_permission();
    log::info!(
        "onboarding: requested input (Accessibility) permission, granted={} supported={} (HID NOT armed)",
        result.granted,
        result.supported
    );
    result
}

/// Mark first-run onboarding complete so the explainer never shows again —
/// called whether the user finished the permission steps or skipped them.
/// Returns the resulting [`FirstRunStatus`] rather than erroring: a persist
/// failure rides `persist_error` (the panel can surface it), and an unpersisted
/// flag at worst re-shows the harmless explainer next launch — never a grant leak.
#[tauri::command]
pub fn complete_first_run(
    app: AppHandle,
    capture: State<'_, CaptureState>,
    input: State<'_, InputState>,
) -> FirstRunStatus {
    let persist_error = match crate::config::save_first_run_complete(&app, true) {
        Ok(()) => {
            log::info!("onboarding: first-run marked complete");
            None
        }
        Err(e) => {
            log::error!("onboarding: {e}");
            Some(e)
        }
    };
    // `pending` reflects the persisted truth: on a persist failure the flag did
    // not stick, so onboarding is still pending and the panel must not claim
    // done — the error rides `persist_error` so the outcome is visible (R007).
    let pending = !crate::config::load_first_run_complete(&app);
    FirstRunStatus {
        pending,
        capture: capture.permission(),
        input: input.permission(),
        persist_error,
    }
}
