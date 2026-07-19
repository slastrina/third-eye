//! Screen capture boundary: the trait seam behind "Attach my screen".
//!
//! [`ScreenCapture`] is the abstraction the capture IPC commands
//! ([`commands`]) call — nothing outside this module may talk to
//! ScreenCaptureKit or CoreGraphics directly. R007 (failure visibility) is enforced structurally: every failure
//! a backend can hit maps to a typed [`CaptureError`] variant, serialized with
//! the same kind-tagged camelCase JSON contract as [`crate::llm::LlmError`],
//! so the UI can always show a guided walkthrough instead of silence.
//!
//! Permission state is health-as-value ([`CapturePermission`]): querying it
//! never errors and never triggers the OS prompt — only
//! [`request_permission`] does.
//!
//! Platform binding: macOS gets the real backend ([`macos::MacosCapture`] —
//! ScreenCaptureKit one-shot capture, self-excluded by PID, encoded via
//! [`encode`]); every other OS gets [`fallback::FallbackCapture`], which
//! returns typed `unsupported` errors so Windows/Linux builds stay clean.
//! The capture pipeline's first stage —
//! [`macos::capture_display_image_blocking`], producing a raw `CGImage` — is
//! exported separately so the S01 watcher's OCR path can consume pixels
//! in-memory without ever touching the PNG encode path.

pub mod commands;
pub mod encode;
pub mod fallback;
#[cfg(target_os = "macos")]
pub mod macos;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use async_trait::async_trait;
use serde::Serialize;

/// The full capture failure taxonomy (R007). Serialized with a `kind` tag
/// (`permission-denied` / `no-display` / `capture-failed` / `unsupported` /
/// `privacy-mode`) and camelCase fields — the same IPC error contract shape
/// as [`crate::llm::LlmError`]; the UI matches on `kind`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case", rename_all_fields = "camelCase")]
pub enum CaptureError {
    /// Screen Recording permission is not granted (TCC). The UI responds
    /// with the guided walkthrough, never a bare error string.
    PermissionDenied { detail: String },
    /// No display was available to capture (e.g. all displays asleep or
    /// disconnected mid-capture).
    NoDisplay { detail: String },
    /// The capture pipeline itself failed after permission checks passed
    /// (ScreenCaptureKit stream error, encode failure, timeout).
    CaptureFailed { detail: String },
    /// Screen capture is not implemented on this platform. `platform` names
    /// the running OS so logs and error surfaces are self-explanatory.
    Unsupported { platform: String, detail: String },
    /// Privacy mode is on (S07): every capture is blocked ahead of the
    /// permission logic with this typed error — never silence (R007). Chat
    /// streaming stays allowed; only capture is blocked.
    PrivacyMode { detail: String },
}

impl CaptureError {
    /// Stable machine-readable name, mirroring the serde `kind` tag. Used in
    /// error logs so grep for `permission-denied` / `no-display` /
    /// `capture-failed` / `unsupported` works.
    pub fn kind(&self) -> &'static str {
        match self {
            CaptureError::PermissionDenied { .. } => "permission-denied",
            CaptureError::NoDisplay { .. } => "no-display",
            CaptureError::CaptureFailed { .. } => "capture-failed",
            CaptureError::Unsupported { .. } => "unsupported",
            CaptureError::PrivacyMode { .. } => "privacy-mode",
        }
    }

    /// The `unsupported` error for the current platform — the one shape the
    /// fallback backend ever returns.
    pub fn unsupported_here() -> Self {
        CaptureError::Unsupported {
            platform: std::env::consts::OS.to_string(),
            detail: "screen capture is only implemented on macOS".to_string(),
        }
    }

    /// The one `privacy-mode` shape: capture blocked because the user turned
    /// privacy mode on. The detail names the way back out.
    pub fn privacy_mode() -> Self {
        CaptureError::PrivacyMode {
            detail: "Privacy Mode is on — screen capture is blocked; turn it off from the \
                     tray menu or settings to attach your screen"
                .to_string(),
        }
    }
}

impl std::fmt::Display for CaptureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CaptureError::PermissionDenied { detail } => {
                write!(f, "capture permission-denied: Screen Recording not granted ({detail})")
            }
            CaptureError::NoDisplay { detail } => {
                write!(f, "capture no-display: no display available ({detail})")
            }
            CaptureError::CaptureFailed { detail } => {
                write!(f, "capture capture-failed: {detail}")
            }
            CaptureError::Unsupported { platform, detail } => {
                write!(f, "capture unsupported on {platform}: {detail}")
            }
            CaptureError::PrivacyMode { detail } => {
                write!(f, "capture privacy-mode: capture blocked ({detail})")
            }
        }
    }
}

impl std::error::Error for CaptureError {}

/// Queryable privacy-mode state: `{ enabled, error }` — the same
/// health-as-value shape as `AutostartStatus`/`HotkeyStatus` (R007). `error`
/// carries the most recent persist failure so a toggle that could not be
/// saved stays visible after the fact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PrivacyStatus {
    pub enabled: bool,
    pub error: Option<String>,
}

/// The one shared privacy-mode core (S07). Both entry points — the tray
/// check item and the `set_privacy_mode` IPC command — mutate this managed
/// state through `commands::apply_privacy_mode`, so they cannot drift
/// (hotkey precedent MEM044). Pure in-memory state: persistence and
/// broadcasting live in the applier.
#[derive(Debug, Default)]
pub struct PrivacyState {
    enabled: AtomicBool,
    last_error: Mutex<Option<String>>,
}

impl PrivacyState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Privacy mode starts off; persisted state is applied in `setup()`.
    pub fn enabled(&self) -> bool {
        self.enabled.load(Ordering::SeqCst)
    }

    pub fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::SeqCst);
    }

    /// Record (or clear) the most recent persist failure.
    pub fn record_error(&self, error: Option<String>) {
        *self.last_error.lock().unwrap() = error;
    }

    /// Current status as health-as-value — never an error, safe to poll.
    pub fn status(&self) -> PrivacyStatus {
        PrivacyStatus { enabled: self.enabled(), error: self.last_error.lock().unwrap().clone() }
    }
}

/// One captured frame of the primary display, already PNG-encoded and
/// base64'd by [`encode::encode_rgba_frame`]. Crosses IPC as camelCase JSON;
/// T03 turns `base64_png` into the `data:image/png;base64,...` vision URL.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CapturedFrame {
    pub width: u32,
    pub height: u32,
    pub base64_png: String,
}

/// Queryable Screen Recording permission state: `{ granted, supported }`.
/// Health-as-value (R007): returned by the `capture_permission_status`
/// command (T03) and never an error. `supported: false` means this platform
/// has no capture backend at all, so the UI can hide the attach button
/// instead of walking the user through a prompt that will never appear.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CapturePermission {
    pub granted: bool,
    pub supported: bool,
}

/// The capture seam. Object-safe (`Arc<dyn ScreenCapture>`) so commands and
/// tests can hold any backend without knowing its transport.
#[async_trait]
pub trait ScreenCapture: Send + Sync {
    /// Current permission state — a value, never an error, and never
    /// triggers the OS prompt.
    fn permission(&self) -> CapturePermission;

    /// Trigger the OS permission prompt (or the Settings round-trip if the
    /// user previously denied). Returns the resulting granted state.
    fn request_permission(&self) -> bool;

    /// Capture one frame of the primary display with every window owned by
    /// this process excluded (R008). Never hangs silently: every failure
    /// path resolves to a [`CaptureError`].
    async fn capture_primary(&self) -> Result<CapturedFrame, CaptureError>;
}

/// Current Screen Recording permission state for this platform.
/// Health-as-value: total function, never errors, never prompts.
pub fn permission_status() -> CapturePermission {
    #[cfg(target_os = "macos")]
    {
        macos::permission_status()
    }
    #[cfg(not(target_os = "macos"))]
    {
        CapturePermission { granted: false, supported: false }
    }
}

/// Trigger the OS Screen Recording prompt where supported; `false` elsewhere.
/// Logged so permission-flow debugging never needs a debugger attached.
pub fn request_permission() -> bool {
    #[cfg(target_os = "macos")]
    {
        let granted = macos::request_permission();
        log::info!("capture: permission requested, granted={granted}");
        granted
    }
    #[cfg(not(target_os = "macos"))]
    {
        log::info!("capture: permission requested on unsupported platform");
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// Minimal in-memory backend proving the trait is implementable and
    /// object-safe — the same shape the T03 command tests will use.
    struct MockCapture {
        fail_with: Option<CaptureError>,
    }

    #[async_trait]
    impl ScreenCapture for MockCapture {
        fn permission(&self) -> CapturePermission {
            CapturePermission { granted: self.fail_with.is_none(), supported: true }
        }

        fn request_permission(&self) -> bool {
            self.fail_with.is_none()
        }

        async fn capture_primary(&self) -> Result<CapturedFrame, CaptureError> {
            if let Some(err) = &self.fail_with {
                return Err(err.clone());
            }
            Ok(CapturedFrame { width: 4, height: 2, base64_png: "cGl4ZWxz".into() })
        }
    }

    #[tokio::test]
    async fn trait_is_object_safe_and_captures_through_dyn() {
        let backend: Arc<dyn ScreenCapture> = Arc::new(MockCapture { fail_with: None });
        let frame = backend.capture_primary().await.unwrap();
        assert_eq!((frame.width, frame.height), (4, 2));
        assert_eq!(frame.base64_png, "cGl4ZWxz");
        assert!(backend.permission().granted);
    }

    #[tokio::test]
    async fn errors_propagate_through_dyn_with_kind() {
        let backend: Arc<dyn ScreenCapture> = Arc::new(MockCapture {
            fail_with: Some(CaptureError::PermissionDenied { detail: "TCC denied".into() }),
        });
        let err = backend.capture_primary().await.unwrap_err();
        assert_eq!(err.kind(), "permission-denied");
    }

    #[test]
    fn error_json_shape_is_the_ipc_contract() {
        // The UI matches on `kind` and reads camelCase fields; a change here
        // is a breaking IPC change and must be coordinated with src/chat.ts.
        let denied = CaptureError::PermissionDenied { detail: "TCC denied".into() };
        let v = serde_json::to_value(&denied).unwrap();
        assert_eq!(v["kind"], "permission-denied");
        assert_eq!(v["detail"], "TCC denied");

        let no_display = CaptureError::NoDisplay { detail: "asleep".into() };
        assert_eq!(serde_json::to_value(&no_display).unwrap()["kind"], "no-display");

        let failed = CaptureError::CaptureFailed { detail: "stream error".into() };
        assert_eq!(serde_json::to_value(&failed).unwrap()["kind"], "capture-failed");

        let unsupported =
            CaptureError::Unsupported { platform: "linux".into(), detail: "no backend".into() };
        let v = serde_json::to_value(&unsupported).unwrap();
        assert_eq!(v["kind"], "unsupported");
        assert_eq!(v["platform"], "linux");
        assert_eq!(v["detail"], "no backend");

        let privacy = CaptureError::PrivacyMode { detail: "privacy on".into() };
        let v = serde_json::to_value(&privacy).unwrap();
        assert_eq!(v["kind"], "privacy-mode");
        assert_eq!(v["detail"], "privacy on");
    }

    #[test]
    fn kind_matches_serde_tag_for_every_variant() {
        let all = [
            CaptureError::PermissionDenied { detail: String::new() },
            CaptureError::NoDisplay { detail: String::new() },
            CaptureError::CaptureFailed { detail: String::new() },
            CaptureError::Unsupported { platform: String::new(), detail: String::new() },
            CaptureError::PrivacyMode { detail: String::new() },
        ];
        for err in all {
            let v = serde_json::to_value(&err).unwrap();
            assert_eq!(v["kind"], err.kind(), "kind()/serde tag drift for {err:?}");
        }
    }

    #[test]
    fn privacy_mode_error_names_kind_and_the_way_out() {
        let err = CaptureError::privacy_mode();
        assert_eq!(err.kind(), "privacy-mode");
        let msg = err.to_string();
        assert!(msg.contains("privacy-mode"), "kind missing: {msg}");
        assert!(msg.contains("turn it off"), "recovery hint missing: {msg}");
    }

    #[test]
    fn privacy_state_defaults_off_and_toggles() {
        let s = PrivacyState::new();
        assert!(!s.enabled(), "privacy must default to off");
        s.set_enabled(true);
        assert!(s.enabled());
        s.set_enabled(false);
        assert!(!s.enabled());
    }

    #[test]
    fn privacy_status_carries_and_clears_the_last_persist_error() {
        let s = PrivacyState::new();
        assert_eq!(s.status(), PrivacyStatus { enabled: false, error: None });

        s.set_enabled(true);
        s.record_error(Some("failed to persist privacyMode=true to /tmp/settings.json".into()));
        let status = s.status();
        assert!(status.enabled);
        assert!(status.error.as_deref().unwrap().contains("privacyMode"));

        // A later successful persist clears the stale failure.
        s.record_error(None);
        assert_eq!(s.status().error, None);
    }

    #[test]
    fn privacy_status_serializes_camel_case() {
        let v = serde_json::to_value(PrivacyStatus { enabled: true, error: None }).unwrap();
        assert_eq!(v, serde_json::json!({ "enabled": true, "error": null }));
        let v = serde_json::to_value(PrivacyStatus {
            enabled: false,
            error: Some("persist failed".into()),
        })
        .unwrap();
        assert_eq!(v["enabled"], false);
        assert_eq!(v["error"], "persist failed");
    }

    #[test]
    fn error_display_names_kind_and_detail() {
        let err = CaptureError::PermissionDenied { detail: "TCC denied".into() };
        let msg = err.to_string();
        assert!(msg.contains("permission-denied"), "kind missing: {msg}");
        assert!(msg.contains("TCC denied"), "detail missing: {msg}");
    }

    #[test]
    fn frame_and_permission_serialize_camel_case() {
        let frame = CapturedFrame { width: 2048, height: 1152, base64_png: "QUJD".into() };
        let v = serde_json::to_value(&frame).unwrap();
        assert_eq!(v["width"], 2048);
        assert_eq!(v["height"], 1152);
        assert_eq!(v["base64Png"], "QUJD");

        let p = CapturePermission { granted: false, supported: true };
        let v = serde_json::to_value(&p).unwrap();
        assert_eq!(v["granted"], false);
        assert_eq!(v["supported"], true);
    }

    #[test]
    fn permission_status_is_total_and_consistent() {
        // Health-as-value: callable at any time, never errors, never prompts.
        // CGPreflightScreenCaptureAccess only reads TCC state.
        let status = permission_status();
        if cfg!(target_os = "macos") {
            assert!(status.supported);
        } else {
            assert_eq!(status, CapturePermission { granted: false, supported: false });
        }
    }
}
