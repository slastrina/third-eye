//! Tauri IPC surface for screen capture (R004/R007/R008): the
//! `capture_screen`, `capture_permission_status`, and `open_capture_settings`
//! commands over a managed [`CaptureState`].
//!
//! The webview never touches ScreenCaptureKit or CoreGraphics — this module
//! is the whole contract. `capture_screen` returns a [`CapturedFrame`] or a
//! typed kind-tagged [`CaptureError`] (never a bare string), so the T04
//! walkthrough can match on `kind == "permission-denied"`.
//! `capture_permission_status` is health-as-value: a [`CapturePermission`]
//! value at any time, never an error, never a prompt. `open_capture_settings`
//! deep-links to the macOS Screen Recording privacy pane for the walkthrough's
//! "Open System Settings" action.

use std::sync::Arc;

use tauri::{AppHandle, Emitter, Manager, State};

use super::{
    CaptureError, CapturePermission, CapturedFrame, PrivacyState, PrivacyStatus, ScreenCapture,
};

/// Privacy-state broadcast (S07): mutation responses only reach the calling
/// window, so every privacy toggle also emits the resulting
/// [`PrivacyStatus`] app-wide — the overlay's attach affordance stays
/// truthful when the settings window (or tray) flips privacy mode.
pub const PRIVACY_EVENT: &str = "capture://privacy";

/// Deep link to System Settings → Privacy & Security → Screen Recording —
/// the walkthrough's escape hatch after macOS suppresses the TCC prompt on
/// a repeat ask (it only ever shows once per app lifetime).
pub const SCREEN_RECORDING_SETTINGS_URL: &str =
    "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture";

/// Managed capture state: the platform backend behind the trait seam, so
/// commands (and tests) never name a concrete backend.
pub struct CaptureState {
    backend: Arc<dyn ScreenCapture>,
}

impl CaptureState {
    pub fn new(backend: Arc<dyn ScreenCapture>) -> Self {
        Self { backend }
    }

    /// State bound to this platform's live backend: ScreenCaptureKit on
    /// macOS, the typed-unsupported fallback everywhere else.
    pub fn with_platform_backend() -> Self {
        #[cfg(target_os = "macos")]
        {
            Self::new(Arc::new(super::macos::MacosCapture))
        }
        #[cfg(not(target_os = "macos"))]
        {
            Self::new(Arc::new(super::fallback::FallbackCapture))
        }
    }

    /// Testable core of `capture_screen`. Privacy mode blocks first — ahead
    /// of all permission logic, so an enabled toggle can never trigger the
    /// TCC prompt or touch the backend (S07); the block is a typed
    /// `privacy-mode` error, never silence (R007). Otherwise, if permission
    /// is missing on a supported platform, ask once first — the first ask
    /// shows the TCC prompt; after a prior denial macOS suppresses it and
    /// the request returns `false` immediately, yielding the typed
    /// `permission-denied` the walkthrough keys on. Unsupported platforms
    /// skip straight to the backend, which returns the typed `unsupported`
    /// error.
    pub async fn capture(&self, privacy_enabled: bool) -> Result<CapturedFrame, CaptureError> {
        if privacy_enabled {
            let err = CaptureError::privacy_mode();
            log::error!("capture: {} ({err})", err.kind());
            return Err(err);
        }
        let permission = self.backend.permission();
        if permission.supported && !permission.granted && !self.backend.request_permission() {
            let err = CaptureError::PermissionDenied {
                detail: "Screen Recording not granted; enable Third Eye in System Settings → \
                         Privacy & Security → Screen Recording"
                    .into(),
            };
            log::error!("capture: {} ({err})", err.kind());
            return Err(err);
        }
        self.backend.capture_primary().await
    }

    /// Testable core of `capture_permission_status` (health-as-value).
    pub fn permission(&self) -> CapturePermission {
        self.backend.permission()
    }

    /// Trigger the OS Screen Recording prompt through the backend and return
    /// the resulting permission value — the first-run onboarding entry point.
    /// Unlike [`Self::capture`] this never captures: it only spends the one-shot
    /// TCC prompt (macOS shows it once per app lifetime) and reports the outcome.
    /// On an unsupported platform the backend returns `false` and the value
    /// stays `supported: false`, so the UI can present it truthfully.
    pub fn request_permission(&self) -> CapturePermission {
        // Only ask where a prompt can appear; on an unsupported backend the
        // request is a logged no-op inside the backend.
        if self.backend.permission().supported {
            self.backend.request_permission();
        }
        self.backend.permission()
    }
}

/// Capture one frame of the primary display with every Third Eye window
/// excluded (R008), PNG-encoded and base64'd. Success and failure latency /
/// error-kind logging lives in the backend and [`CaptureState::capture`].
#[tauri::command]
pub async fn capture_screen(
    app: tauri::AppHandle,
    state: State<'_, CaptureState>,
) -> Result<CapturedFrame, CaptureError> {
    log::debug!("capture: capture_screen invoked");
    // Tray shows "watching" for the capture's duration (permission ask
    // included); the guard drops on both the success and error paths.
    #[cfg(desktop)]
    let _activity = crate::tray::begin_activity(&app, crate::tray::ActivityKind::Capture);
    let privacy_enabled = app
        .try_state::<PrivacyState>()
        .map(|p| p.enabled())
        .unwrap_or(false);
    state.capture(privacy_enabled).await
}

/// Current Screen Recording permission state — a value, never an error,
/// never a prompt (R007 health-as-value). Safe for the UI to poll while the
/// walkthrough waits for the user to flip the Settings toggle.
#[tauri::command]
pub fn capture_permission_status(state: State<'_, CaptureState>) -> CapturePermission {
    let status = state.permission();
    log::debug!(
        "capture: permission status granted={} supported={}",
        status.granted,
        status.supported
    );
    status
}

/// Open the macOS Screen Recording privacy pane — the walkthrough's "Open
/// System Settings" action. Typed `unsupported` off macOS.
#[tauri::command]
pub fn open_capture_settings() -> Result<(), CaptureError> {
    open_settings_impl()
}

fn open_settings_impl() -> Result<(), CaptureError> {
    #[cfg(target_os = "macos")]
    {
        log::info!("capture: opening Screen Recording settings pane");
        std::process::Command::new("open")
            .arg(SCREEN_RECORDING_SETTINGS_URL)
            .spawn()
            .map(|_| ())
            .map_err(|e| {
                let err = CaptureError::CaptureFailed {
                    detail: format!("failed to open System Settings: {e}"),
                };
                log::error!("capture: {} ({err})", err.kind());
                err
            })
    }
    #[cfg(not(target_os = "macos"))]
    {
        let err = CaptureError::unsupported_here();
        log::error!("capture: {} ({err})", err.kind());
        Err(err)
    }
}

/// The one shared privacy-mode applier (S07): both entry points — the tray
/// check item (`via = "tray"`) and the `set_privacy_mode` IPC
/// (`via = "ipc"`) — funnel through here, so they cannot drift (hotkey
/// precedent MEM044). Persists to settings.json; on persist failure the
/// in-memory toggle is rolled back (an unpersisted toggle must never
/// silently revert on restart) and the error naming the persist path is
/// kept queryable on the status. Always broadcasts the resulting status and
/// resyncs the tray check item and resting frame.
pub fn apply_privacy_mode(app: &AppHandle, desired: bool, via: &str) -> PrivacyStatus {
    let state = app.state::<PrivacyState>();
    let previous = state.enabled();
    state.set_enabled(desired);
    match crate::config::save_privacy_mode(app, desired) {
        Ok(()) => {
            state.record_error(None);
            log::info!(
                "capture: privacy mode {} (via {via})",
                if desired { "enabled" } else { "disabled" }
            );
        }
        Err(e) => {
            state.set_enabled(previous);
            log::error!("capture: {e}");
            state.record_error(Some(e));
        }
    }
    let status = state.status();
    if let Err(e) = app.emit(PRIVACY_EVENT, status.clone()) {
        log::warn!("capture: privacy broadcast failed: {e}");
    }
    #[cfg(desktop)]
    {
        crate::tray::sync_privacy_check(app, status.enabled);
        crate::tray::refresh_resting_frame(app);
    }
    status
}

/// Apply the persisted privacy toggle at startup (called from `setup()`
/// before the tray builds, so the check item and initial resting frame
/// reflect it). In-memory only: no re-save, no broadcast — nothing is
/// listening yet. An absent key keeps the default (off); load failures are
/// logged inside `config`, never fatal.
pub fn apply_persisted_privacy_mode(app: &AppHandle) {
    if let Some(enabled) = crate::config::load_privacy_mode(app) {
        app.state::<PrivacyState>().set_enabled(enabled);
        log::info!("capture: applied persisted privacy mode (enabled={enabled})");
    }
}

/// Set privacy mode from the UI (S07). Returns the resulting
/// [`PrivacyStatus`] instead of erroring — a persist failure is data the
/// caller can render, same contract as `set_autostart`.
#[tauri::command]
pub fn set_privacy_mode(app: AppHandle, enable: bool) -> PrivacyStatus {
    apply_privacy_mode(&app, enable, "ipc")
}

/// Current privacy-mode state — health-as-value beside `hotkey_status` and
/// `autostart_status` (R007): a value at any time, never an error.
#[tauri::command]
pub fn privacy_status(state: State<'_, PrivacyState>) -> PrivacyStatus {
    let status = state.status();
    log::debug!(
        "capture: privacy status enabled={} error={:?}",
        status.enabled,
        status.error
    );
    status
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// Scriptable backend recording whether the OS prompt was requested —
    /// the permission-flow contract the walkthrough depends on.
    struct ScriptedCapture {
        permission: CapturePermission,
        grant_on_request: bool,
        prompt_requested: AtomicBool,
        capture_result: Result<CapturedFrame, CaptureError>,
    }

    impl ScriptedCapture {
        fn frame() -> CapturedFrame {
            CapturedFrame {
                width: 4,
                height: 2,
                base64_png: "cGl4ZWxz".into(),
            }
        }
    }

    #[async_trait]
    impl ScreenCapture for ScriptedCapture {
        fn permission(&self) -> CapturePermission {
            self.permission
        }

        fn request_permission(&self) -> bool {
            self.prompt_requested.store(true, Ordering::SeqCst);
            self.grant_on_request
        }

        async fn capture_primary(&self) -> Result<CapturedFrame, CaptureError> {
            self.capture_result.clone()
        }
    }

    fn state_with(backend: ScriptedCapture) -> (CaptureState, Arc<ScriptedCapture>) {
        let backend = Arc::new(backend);
        (CaptureState::new(backend.clone()), backend)
    }

    #[tokio::test]
    async fn granted_permission_captures_without_prompting() {
        let (state, backend) = state_with(ScriptedCapture {
            permission: CapturePermission {
                granted: true,
                supported: true,
            },
            grant_on_request: false,
            prompt_requested: AtomicBool::new(false),
            capture_result: Ok(ScriptedCapture::frame()),
        });
        let frame = state.capture(false).await.unwrap();
        assert_eq!((frame.width, frame.height), (4, 2));
        assert!(
            !backend.prompt_requested.load(Ordering::SeqCst),
            "granted permission must never re-prompt"
        );
    }

    #[tokio::test]
    async fn missing_permission_prompts_once_then_captures_when_granted() {
        // First-run UX: TCC prompt appears, user grants, capture proceeds.
        let (state, backend) = state_with(ScriptedCapture {
            permission: CapturePermission {
                granted: false,
                supported: true,
            },
            grant_on_request: true,
            prompt_requested: AtomicBool::new(false),
            capture_result: Ok(ScriptedCapture::frame()),
        });
        assert!(state.capture(false).await.is_ok());
        assert!(backend.prompt_requested.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn denied_permission_is_typed_permission_denied_not_a_capture_attempt() {
        let (state, backend) = state_with(ScriptedCapture {
            permission: CapturePermission {
                granted: false,
                supported: true,
            },
            grant_on_request: false,
            prompt_requested: AtomicBool::new(false),
            capture_result: Ok(ScriptedCapture::frame()),
        });
        let err = state.capture(false).await.unwrap_err();
        assert_eq!(err.kind(), "permission-denied");
        // The walkthrough matches on the serialized kind tag over IPC.
        assert_eq!(
            serde_json::to_value(&err).unwrap()["kind"],
            "permission-denied"
        );
        assert!(backend.prompt_requested.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn unsupported_platform_skips_prompt_and_bubbles_typed_error() {
        let (state, backend) = state_with(ScriptedCapture {
            permission: CapturePermission {
                granted: false,
                supported: false,
            },
            grant_on_request: false,
            prompt_requested: AtomicBool::new(false),
            capture_result: Err(CaptureError::unsupported_here()),
        });
        let err = state.capture(false).await.unwrap_err();
        assert_eq!(err.kind(), "unsupported");
        assert!(
            !backend.prompt_requested.load(Ordering::SeqCst),
            "no prompt exists on unsupported platforms"
        );
    }

    #[tokio::test]
    async fn backend_capture_errors_bubble_untouched() {
        let (state, _) = state_with(ScriptedCapture {
            permission: CapturePermission {
                granted: true,
                supported: true,
            },
            grant_on_request: false,
            prompt_requested: AtomicBool::new(false),
            capture_result: Err(CaptureError::NoDisplay {
                detail: "asleep".into(),
            }),
        });
        let err = state.capture(false).await.unwrap_err();
        assert_eq!(
            err,
            CaptureError::NoDisplay {
                detail: "asleep".into()
            }
        );
    }

    #[tokio::test]
    async fn privacy_mode_blocks_capture_ahead_of_permission_logic() {
        // S07: even with permission missing (which would normally prompt),
        // privacy mode must return the typed error without touching the
        // backend — no TCC prompt, no capture attempt.
        let (state, backend) = state_with(ScriptedCapture {
            permission: CapturePermission {
                granted: false,
                supported: true,
            },
            grant_on_request: true,
            prompt_requested: AtomicBool::new(false),
            capture_result: Ok(ScriptedCapture::frame()),
        });
        let err = state.capture(true).await.unwrap_err();
        assert_eq!(err.kind(), "privacy-mode");
        // The UI matches on the serialized kind tag over IPC.
        assert_eq!(serde_json::to_value(&err).unwrap()["kind"], "privacy-mode");
        assert!(
            !backend.prompt_requested.load(Ordering::SeqCst),
            "privacy mode must block before any permission ask"
        );
    }

    #[tokio::test]
    async fn privacy_mode_blocks_even_granted_capture() {
        let (state, _) = state_with(ScriptedCapture {
            permission: CapturePermission {
                granted: true,
                supported: true,
            },
            grant_on_request: false,
            prompt_requested: AtomicBool::new(false),
            capture_result: Ok(ScriptedCapture::frame()),
        });
        assert_eq!(
            state.capture(true).await.unwrap_err().kind(),
            "privacy-mode"
        );
    }

    #[test]
    fn privacy_event_name_is_the_ipc_contract() {
        // src/App.tsx (T04) listens on this exact string.
        assert_eq!(PRIVACY_EVENT, "capture://privacy");
    }

    #[test]
    fn permission_status_is_a_backend_passthrough_value() {
        let (state, _) = state_with(ScriptedCapture {
            permission: CapturePermission {
                granted: false,
                supported: true,
            },
            grant_on_request: false,
            prompt_requested: AtomicBool::new(false),
            capture_result: Ok(ScriptedCapture::frame()),
        });
        assert_eq!(
            state.permission(),
            CapturePermission {
                granted: false,
                supported: true
            }
        );
    }

    #[test]
    fn platform_backend_binding_matches_this_os() {
        let state = CaptureState::with_platform_backend();
        assert_eq!(state.permission().supported, cfg!(target_os = "macos"));
    }

    #[test]
    fn request_permission_prompts_on_supported_and_reports_live_permission() {
        // First-run onboarding: a supported backend is prompted once, and the
        // returned value is the backend's LIVE permission read after the prompt
        // (on the real macOS backend that read reflects a fresh grant; the
        // scripted backend holds its permission fixed, so we assert the prompt
        // fired and the returned value equals that live read).
        let (state, backend) = state_with(ScriptedCapture {
            permission: CapturePermission {
                granted: true,
                supported: true,
            },
            grant_on_request: true,
            prompt_requested: AtomicBool::new(false),
            capture_result: Ok(ScriptedCapture::frame()),
        });
        let result = state.request_permission();
        assert!(
            backend.prompt_requested.load(Ordering::SeqCst),
            "a supported backend must be prompted"
        );
        assert_eq!(
            result,
            CapturePermission {
                granted: true,
                supported: true
            }
        );
    }

    #[test]
    fn request_permission_never_prompts_on_unsupported() {
        // Off macOS there is no prompt to spend — the request must be a no-op
        // that still reports the truthful unsupported value.
        let (state, backend) = state_with(ScriptedCapture {
            permission: CapturePermission {
                granted: false,
                supported: false,
            },
            grant_on_request: true,
            prompt_requested: AtomicBool::new(false),
            capture_result: Err(CaptureError::unsupported_here()),
        });
        let result = state.request_permission();
        assert!(
            !backend.prompt_requested.load(Ordering::SeqCst),
            "no prompt exists on unsupported platforms"
        );
        assert_eq!(
            result,
            CapturePermission {
                granted: false,
                supported: false
            }
        );
    }

    #[test]
    fn settings_deep_link_targets_the_screen_recording_pane() {
        // The walkthrough contract: this exact pane, not Settings generally.
        assert!(SCREEN_RECORDING_SETTINGS_URL.starts_with("x-apple.systempreferences:"));
        assert!(SCREEN_RECORDING_SETTINGS_URL.ends_with("Privacy_ScreenCapture"));
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn open_settings_off_macos_is_typed_unsupported() {
        assert_eq!(open_settings_impl().unwrap_err().kind(), "unsupported");
    }
}
