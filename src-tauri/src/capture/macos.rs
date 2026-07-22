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
//! The ScreenCaptureKit frame backend lives here too, split into two stages:
//! - [`capture_display_image_blocking`] — the reusable pixel-free-exit stage:
//!   one `CGImage` of the primary display with every window owned by this
//!   process excluded (R008). The S01 watcher's OCR path consumes this
//!   directly (Vision reads a `CGImage`), so watcher pixels never meet the
//!   PNG encoder.
//! - [`MacosCapture`] — the "Attach my screen" path: runs the stage, then
//!   hands tightly-packed RGBA pixels to [`super::encode`] for the
//!   downscale → PNG → base64 pipeline.

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use screencapturekit::screenshot_manager::{CGImage, CGImageExt, SCScreenshotManager};
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
    // SCShareableContent::get and SCScreenshotManager::capture_image both
    // block on Swift completion handlers; run them off the async runtime so
    // a capture can never stall the overlay's IPC thread.
    tokio::task::spawn_blocking(capture_frame_blocking)
        .await
        .map_err(|e| CaptureError::CaptureFailed { detail: format!("capture task panicked: {e}") })?
}

fn capture_frame_blocking() -> Result<CapturedFrame, CaptureError> {
    let image = capture_display_image_blocking(encode::MAX_DIMENSION)?;
    let (width, height) = (image.width() as u32, image.height() as u32);
    let rgba = image.rgba_data().map_err(|e| CaptureError::CaptureFailed {
        detail: format!("pixel render failed: {e}"),
    })?;
    encode::encode_rgba_frame(RawRgbaFrame { width, height, rgba })
}

/// One on-screen window's owning-app name and its bounding rect, converted into
/// the SAME absolute top-left screen-PIXEL space the OCR boxes land in — so the
/// screen_query path can attribute each recognized text element to the app whose
/// window covers it (M005). `layer` is the window's `SCWindow::window_layer`
/// (CGWindowLevel: HIGHER is closer to the viewer — normal app windows sit at 0,
/// the menu bar at 24/25, the Dock-owned wallpaper backstop deep negative) so a
/// caller can pick the TOPMOST window when rects overlap. Own-process windows
/// are excluded exactly like the capture
/// filter's PID exclusion (R008), so the overlay never attributes text to itself.
#[derive(Debug, Clone, PartialEq)]
pub struct WindowAppRect {
    pub app: String,
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    pub layer: i32,
}

/// The captured image's geometry, carried alongside the pixels so the
/// screen_query path can map captured-pixel boxes BACK to logical screen points
/// — the coordinate space the input backend (`enigo`, `Coordinate::Abs`) clicks
/// in. The capture is taken at native pixel density and then capped to
/// `max_dimension` on the longest edge, so the captured pixel space is neither
/// logical points nor native pixels; without this conversion a click at a
/// screen_query coordinate lands in the wrong physical spot (M005 targeting).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CaptureGeometry {
    /// Captured image width in pixels (the OCR normalization basis on x).
    pub pixel_w: u32,
    /// Captured image height in pixels (the OCR normalization basis on y).
    pub pixel_h: u32,
    /// Primary display width in logical points (the input backend's x space).
    pub point_w: f64,
    /// Primary display height in logical points (the input backend's y space).
    pub point_h: f64,
}

/// The reusable capture stage: one `CGImage` of the primary display, capped
/// to `max_dimension` on its longest edge (GPU scaling), with every window
/// owned by this process excluded by PID (R008).
///
/// Blocks on Swift completion handlers — call from `spawn_blocking` (or the
/// watcher's dedicated blocking tick), never on the async runtime. The
/// permission preflight lives here so every consumer — the PNG frame path
/// and the S01 OCR path — inherits the typed `permission-denied` instead of
/// an opaque ScreenCaptureKit error (R007). The returned image is a plain
/// in-memory bitmap: dropping it releases the pixels; nothing here encodes
/// or writes them.
pub fn capture_display_image_blocking(max_dimension: u32) -> Result<CGImage, CaptureError> {
    capture_display_image_with_windows_blocking(max_dimension).map(|(image, _windows)| image)
}

/// The windows-bearing capture that ALSO returns the [`CaptureGeometry`] the
/// screen_query path needs to convert captured-pixel boxes back to logical
/// screen points. [`capture_display_image_with_windows_blocking`] is the
/// backward-compatible view that drops the geometry.
pub fn capture_display_image_with_geometry_blocking(
    max_dimension: u32,
) -> Result<(CGImage, Vec<WindowAppRect>, CaptureGeometry), CaptureError> {
    capture_inner(max_dimension)
}

/// The capture stage plus the on-screen window→app rects in the captured image's
/// own pixel space (M005 app-labelling). Same capture as
/// [`capture_display_image_blocking`]; additionally returns every on-screen,
/// non-own-process window as a [`WindowAppRect`] converted with the *exact same*
/// point→pixel scale (`point_pixel_scale`) and `fit_within` cap the capture
/// itself uses, translated so the display's top-left is the pixel origin — the
/// coordinate space the OCR boxes are scaled into. The screen_query path calls
/// this so it can label each recognized element with its owning app; the PNG
/// frame path and the watcher use the image-only sibling and never pay for the
/// window walk.
pub fn capture_display_image_with_windows_blocking(
    max_dimension: u32,
) -> Result<(CGImage, Vec<WindowAppRect>), CaptureError> {
    capture_inner(max_dimension).map(|(image, windows, _geom)| (image, windows))
}

/// The full capture: the `CGImage`, the window→app rects, and the
/// [`CaptureGeometry`] mapping captured pixels ↔ logical points.
fn capture_inner(
    max_dimension: u32,
) -> Result<(CGImage, Vec<WindowAppRect>, CaptureGeometry), CaptureError> {
    // Preflight is read-only and cheap; never prompts.
    if !has_permission() {
        return Err(CaptureError::PermissionDenied {
            detail: "CGPreflightScreenCaptureAccess returned false".into(),
        });
    }

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
    // native pixel density); encode::fit_within re-caps as a safety net on
    // the frame path.
    let scale = f64::from(filter.point_pixel_scale().max(1.0));
    let px = |points: u32| (f64::from(points) * scale).round() as u32;
    let (target_w, target_h) =
        encode::fit_within(px(display.width()), px(display.height()), max_dimension);
    let config = SCStreamConfiguration::new()
        .with_width(target_w)
        .with_height(target_h)
        .with_shows_cursor(true);

    // Collect the app rects BEFORE the capture so the same shareable-content
    // snapshot backs both. Frames are in global points with the display's own
    // frame as the origin offset; scale by target/point so they land in the
    // captured pixel space, which is what the OCR boxes are normalized to
    // (extract_elements_blocking uses the image's own width()/height(), equal to
    // target_w/target_h). fit_within may cap the longest edge, so derive the
    // per-axis pixel scale from target vs. the display's point size — the exact
    // factor the capture applied — rather than assuming `scale`.
    let display_frame = display.frame();
    let (dpw, dph) = (display.width() as f64, display.height() as f64);
    let sx = if dpw > 0.0 { target_w as f64 / dpw } else { 0.0 };
    let sy = if dph > 0.0 { target_h as f64 / dph } else { 0.0 };
    let window_rects = window_app_rects(&windows, own_pid, display_frame.origin.x, display_frame.origin.y, sx, sy);

    let image = SCScreenshotManager::capture_image(&filter, &config)
        .map_err(|e| CaptureError::CaptureFailed { detail: format!("screenshot failed: {e}") })?;
    // The geometry the screen_query path uses to convert captured-pixel boxes
    // back to logical screen points: the captured image dims (pixel basis) and
    // the display's logical point dims (input-backend basis). The point→pixel
    // scale is target/point; the inverse maps a box back to points.
    let geometry = CaptureGeometry {
        pixel_w: target_w,
        pixel_h: target_h,
        point_w: dpw,
        point_h: dph,
    };
    Ok((image, window_rects, geometry))
}

/// Pure geometry: convert each on-screen, non-own-process window's frame (global
/// points) into a [`WindowAppRect`] in the captured pixel space. `origin_x/_y`
/// are the display's own frame origin (global points) subtracted so the rect is
/// relative to the display's top-left; `sx/sy` are the per-axis point→pixel
/// scales. Kept as a free function over the collected `(app, frame, layer,
/// on_screen, pid)` tuples so the scaling and filtering are testable without a
/// live capture. Sorted topmost-first (DESCENDING layer — higher CGWindowLevel
/// is closer to the viewer) so the first covering window wins attribution.
fn window_app_rects(
    windows: &[SCWindow],
    own_pid: i32,
    origin_x: f64,
    origin_y: f64,
    sx: f64,
    sy: f64,
) -> Vec<WindowAppRect> {
    let mut rects: Vec<WindowAppRect> = windows
        .iter()
        .filter(|w| w.is_on_screen())
        .filter_map(|w| {
            let app = w.owning_application()?;
            // Skip our own windows (R008 parity with the capture filter).
            if app.process_id() == own_pid {
                return None;
            }
            // Desktop-class windows live at deep-negative CGWindowLevels: the
            // Dock-owned wallpaper backstop and Finder's desktop-icon layer both
            // cover the whole display. Attributing text to them would hand the
            // model a "clickable" app that is really the wallpaper — the exact
            // reveal-desktop hazard M005 exists to prevent — so those regions
            // must read as unattributed (app=None) instead.
            if w.window_layer() < 0 {
                return None;
            }
            // WindowServer/menu-bar owners and some system windows report an
            // empty application_name(). A blank-named rect would attribute
            // elements to app=Some("") — indistinguishable from a real target
            // yet unfocusable and unusable. Drop it so those regions read as
            // unattributed (app=None), the honest "no clickable app here" the
            // targeting filter and the model both key on (M005).
            let name = app.application_name();
            if name.trim().is_empty() {
                return None;
            }
            let frame = w.frame();
            Some(WindowAppRect {
                app: name,
                x: ((frame.origin.x - origin_x) * sx).round() as i32,
                y: ((frame.origin.y - origin_y) * sy).round() as i32,
                w: (frame.size.width * sx).round() as i32,
                h: (frame.size.height * sy).round() as i32,
                layer: w.window_layer(),
            })
        })
        .collect();
    // Topmost first: CGWindowLevel ascends TOWARD the viewer (menu bar 24/25
    // above normal windows at 0), so the highest layer is frontmost and wins
    // when rects overlap. The sort is stable, so windows sharing a layer keep
    // ScreenCaptureKit's front-to-back enumeration order.
    rects.sort_by_key(|r| std::cmp::Reverse(r.layer));
    rects
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

    /// Live run of the reusable `CGImage` stage the S01 watcher OCR path
    /// consumes. Needs Screen Recording permission and a display, so it is
    /// ignored in the default suite (slice UAT runs it). Without permission
    /// it must still fail *typed* — never a panic or hang.
    #[test]
    #[ignore = "requires Screen Recording permission and a live display (slice UAT)"]
    fn real_cgimage_stage_smoke() {
        let cap = 640;
        match capture_display_image_blocking(cap) {
            Ok(image) => {
                let (w, h) = (image.width() as u32, image.height() as u32);
                assert!(w > 0 && h > 0, "empty image: {w}x{h}");
                assert!(w <= cap && h <= cap, "cap not honored: {w}x{h} > {cap}");
            }
            Err(err) => {
                assert_eq!(err.kind(), "permission-denied", "unexpected: {err}");
                assert!(!permission_status().granted);
            }
        }
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
