//! macOS Screen Recording permission layer (TCC), via raw CoreGraphics FFI.
//!
//! Two calls, deliberately split so the health-as-value contract holds:
//! - [`has_permission`] wraps `CGPreflightScreenCaptureAccess` — reads TCC
//!   state only, never shows UI. Safe to call at any time, any thread.
//! - [`request_permission`] wraps `CGRequestScreenCaptureAccess` — shows the
//!   system prompt exactly once per app lifetime; after a denial it returns
//!   `false` without UI, which is why the T04 walkthrough deep-links to
//!   System Settings instead of re-prompting.
//!
//! The ScreenCaptureKit frame backend lives here too: [`MacosCapture`]
//! captures one frame of the primary display with every window owned by this
//! process excluded (R008), then hands tightly-packed RGBA pixels to
//! [`super::encode`] for the downscale → PNG → base64 pipeline.

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use screencapturekit::screenshot_manager::{CGImageExt, SCScreenshotManager};
use screencapturekit::shareable_content::{SCShareableContent, SCWindow};
use screencapturekit::stream::configuration::SCStreamConfiguration;
use screencapturekit::stream::content_filter::SCContentFilter;

use super::encode::{self, RawRgbaFrame};
use super::{CaptureError, CapturePermission, CapturedFrame, ScreenCapture};

// Raw FFI instead of a binding crate: these are stable, ABI-simple
// CoreGraphics functions, and the pinned dependency policy favors no new
// crates for them. `bool` is ABI-compatible with C `_Bool`.
#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGPreflightScreenCaptureAccess() -> bool;
    fn CGRequestScreenCaptureAccess() -> bool;
    fn CGMainDisplayID() -> u32;
}

/// Whether Screen Recording permission is currently granted. Read-only:
/// never triggers the system prompt.
pub fn has_permission() -> bool {
    // Safety: no arguments, no pointers; reads TCC state and returns a bool.
    unsafe { CGPreflightScreenCaptureAccess() }
}

/// Show the Screen Recording permission prompt (first ask only — macOS
/// suppresses it after a denial) and return the resulting granted state.
pub fn request_permission() -> bool {
    // Safety: no arguments, no pointers; may present system UI.
    unsafe { CGRequestScreenCaptureAccess() }
}

/// Current permission state as the IPC health-as-value shape.
pub fn permission_status() -> CapturePermission {
    CapturePermission { granted: has_permission(), supported: true }
}

/// The live macOS backend: one-shot ScreenCaptureKit capture of the primary
/// display via `SCScreenshotManager` (no stream setup), self-excluded by PID.
pub struct MacosCapture;

#[async_trait]
impl ScreenCapture for MacosCapture {
    fn permission(&self) -> CapturePermission {
        permission_status()
    }

    fn request_permission(&self) -> bool {
        super::request_permission()
    }

    async fn capture_primary(&self) -> Result<CapturedFrame, CaptureError> {
        let start = Instant::now();
        let result = capture_primary_inner().await;
        match &result {
            Ok(frame) => {
                // Mirrors the S01 summon-latency / S02 first-token log shape.
                log::info!(
                    "capture: frame {}x{} in {} ms",
                    frame.width,
                    frame.height,
                    start.elapsed().as_millis()
                );
            }
            Err(err) => log::error!("capture: {} ({err})", err.kind()),
        }
        result
    }
}

async fn capture_primary_inner() -> Result<CapturedFrame, CaptureError> {
    // Preflight is read-only and cheap; failing here yields the typed
    // permission-denied the walkthrough keys on, instead of an opaque
    // ScreenCaptureKit stream error (R007). Never prompts.
    if !has_permission() {
        return Err(CaptureError::PermissionDenied {
            detail: "CGPreflightScreenCaptureAccess returned false".into(),
        });
    }
    // SCShareableContent::get and SCScreenshotManager::capture_image both
    // block on Swift completion handlers; run them off the async runtime so
    // a capture can never stall the overlay's IPC thread.
    tokio::task::spawn_blocking(capture_frame_blocking)
        .await
        .map_err(|e| CaptureError::CaptureFailed { detail: format!("capture task panicked: {e}") })?
}

fn capture_frame_blocking() -> Result<CapturedFrame, CaptureError> {
    let content = SCShareableContent::get().map_err(|e| CaptureError::CaptureFailed {
        detail: format!("shareable content query failed: {e}"),
    })?;

    let displays = content.displays();
    if displays.is_empty() {
        return Err(CaptureError::NoDisplay { detail: "no shareable displays".into() });
    }
    // Prefer the primary display (the one hosting the overlay's summon
    // context); fall back to the first if CGMainDisplayID matches nothing.
    let main_id = unsafe { CGMainDisplayID() };
    let display = displays
        .iter()
        .find(|d| d.display_id() == main_id)
        .unwrap_or(&displays[0]);

    // R008: exclude every window owned by this process — the overlay panel
    // and any future Third Eye window — by PID, not by label, so nothing can
    // leak into its own capture.
    let own_pid = std::process::id() as i32;
    let windows = content.windows();
    let own_windows: Vec<&SCWindow> = windows
        .iter()
        .filter(|w| {
            w.owning_application()
                .map(|app| app.process_id() == own_pid)
                .unwrap_or(false)
        })
        .collect();
    log::debug!("capture: excluding {} own window(s)", own_windows.len());

    let filter = SCContentFilter::create()
        .with_display(display)
        .with_excluding_windows(&own_windows)
        .build();

    // Ask ScreenCaptureKit for the capped size directly (GPU scaling at
    // native pixel density); encode::fit_within re-caps as a safety net.
    let scale = f64::from(filter.point_pixel_scale().max(1.0));
    let px = |points: u32| (f64::from(points) * scale).round() as u32;
    let (target_w, target_h) =
        encode::fit_within(px(display.width()), px(display.height()), encode::MAX_DIMENSION);
    let config = SCStreamConfiguration::new()
        .with_width(target_w)
        .with_height(target_h)
        .with_shows_cursor(true);

    let image = SCScreenshotManager::capture_image(&filter, &config)
        .map_err(|e| CaptureError::CaptureFailed { detail: format!("screenshot failed: {e}") })?;

    let (width, height) = (image.width() as u32, image.height() as u32);
    let rgba = image.rgba_data().map_err(|e| CaptureError::CaptureFailed {
        detail: format!("pixel render failed: {e}"),
    })?;
    encode::encode_rgba_frame(RawRgbaFrame { width, height, rgba })
}

// Keeps the trait bound explicit: the T03 commands hold Arc<dyn ScreenCapture>.
#[allow(dead_code)]
fn _assert_backend_is_dyn_compatible() -> Arc<dyn ScreenCapture> {
    Arc::new(MacosCapture)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preflight_is_side_effect_free_and_stable() {
        // CGPreflightScreenCaptureAccess never prompts, so calling it twice
        // in a test is safe and must agree with the status shape.
        let first = has_permission();
        let second = has_permission();
        assert_eq!(first, second);
        let status = permission_status();
        assert!(status.supported);
        assert_eq!(status.granted, first);
    }

    /// Live one-frame capture through the full trait surface. Needs Screen
    /// Recording permission and a display, so it is ignored in the default
    /// suite (slice UAT runs it): `cargo test -- --ignored real_capture_smoke`.
    /// Without permission it must still fail *typed* — never a panic or hang.
    #[tokio::test]
    #[ignore = "requires Screen Recording permission and a live display (slice UAT)"]
    async fn real_capture_smoke() {
        let backend: Arc<dyn ScreenCapture> = Arc::new(MacosCapture);
        match backend.capture_primary().await {
            Ok(frame) => {
                assert!(frame.width > 0 && frame.width <= encode::MAX_DIMENSION);
                assert!(frame.height > 0 && frame.height <= encode::MAX_DIMENSION);
                use base64::Engine;
                let png = base64::engine::general_purpose::STANDARD
                    .decode(&frame.base64_png)
                    .expect("frame is valid base64");
                assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n", "frame is a real PNG");
            }
            Err(err) => {
                // In an unpermitted environment the only acceptable outcome
                // is the typed permission error the walkthrough keys on.
                assert_eq!(err.kind(), "permission-denied", "unexpected: {err}");
                assert!(!permission_status().granted);
            }
        }
    }
}
