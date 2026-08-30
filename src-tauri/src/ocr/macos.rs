//! Apple Vision OCR backend: capture → recognize → drop, all inside one
//! blocking call.
//!
//! The pipeline reuses the shared capture stage
//! ([`crate::capture::macos::capture_display_image_blocking`] — permission
//! preflight, primary-display pick, PID self-exclusion per R008) and hands
//! the raw in-memory `CGImage` straight to `VNRecognizeTextRequest`.
//! Recognition is fully on-device; the image is dropped before the future
//! resolves, is never PNG-encoded, and never touches disk (R011).
//!
//! objc2-vision is generated `unsafe` bindings — every message send is
//! encapsulated here; nothing unsafe leaks past the [`OcrEngine`] trait.
//! Recognition on an in-memory image needs no TCC permission, which is why
//! the synthetic-image test below runs non-ignored in the default suite.

use std::time::Instant;

use async_trait::async_trait;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::AllocAnyThread;
use objc2_foundation::{NSArray, NSDictionary};
use objc2_vision::{
    VNImageRequestHandler, VNRecognizeTextRequest, VNRequest, VNRequestTextRecognitionLevel,
};
use screencapturekit::screenshot_manager::CGImage as ScCGImage;

use super::{OcrEngine, OcrError};
use crate::capture::macos::{
    capture_display_image_blocking, capture_display_image_with_geometry_blocking, CaptureGeometry,
    WindowAppRect,
};

/// The live macOS backend: one capture of the primary display per
/// `extract`, recognized with Apple Vision at accurate level.
pub struct VisionOcr {
    /// Longest-edge pixel cap passed to the shared capture stage; the
    /// watcher constructs with [`super::OCR_MAX_DIMENSION`].
    max_dimension: u32,
}

impl VisionOcr {
    pub fn new(max_dimension: u32) -> Self {
        Self { max_dimension }
    }
}

#[async_trait]
impl OcrEngine for VisionOcr {
    async fn extract(&self) -> Result<Vec<String>, OcrError> {
        let max_dimension = self.max_dimension;
        let start = Instant::now();
        // Capture blocks on Swift completion handlers and Vision recognition
        // is CPU-heavy — both stay off the async runtime (capture precedent).
        let result = tokio::task::spawn_blocking(move || extract_blocking(max_dimension))
            .await
            .map_err(|e| OcrError::RecognitionFailed {
                detail: format!("ocr task panicked: {e}"),
            })?;
        match &result {
            Ok(lines) => {
                // Mirrors the capture frame log shape: outcome + latency.
                log::debug!(
                    "ocr: {} line(s) in {} ms",
                    lines.len(),
                    start.elapsed().as_millis()
                );
            }
            Err(err) => log::error!("ocr: {} ({err})", err.kind()),
        }
        result
    }
}

/// The whole extract-and-discard pipeline in one frame: the `CGImage` is a
/// local that dies at the end of this function — recognition output is the
/// only thing that escapes.
fn extract_blocking(max_dimension: u32) -> Result<Vec<String>, OcrError> {
    let image = capture_display_image_blocking(max_dimension)?;
    recognize_text(as_vision_cgimage(&image))
}

/// The one contained cast between two wrappers of the same CF object:
/// screencapturekit's `CGImage` (apple-cf) exposes the raw retained
/// `CGImageRef` via `as_ptr`, and objc2-core-graphics' `CGImage` is a
/// repr-transparent view of that same ref. Borrow only — no ownership
/// transfer, and `VNImageRequestHandler` retains internally.
fn as_vision_cgimage(image: &ScCGImage) -> &objc2_core_graphics::CGImage {
    unsafe { &*image.as_ptr().cast::<objc2_core_graphics::CGImage>() }
}

/// Run accurate-level Vision text recognition on an in-memory image and
/// return the best candidate per observation, in Vision's reading order.
/// Needs no TCC permission — pure on-device compute.
fn recognize_text(image: &objc2_core_graphics::CGImage) -> Result<Vec<String>, OcrError> {
    let request = VNRecognizeTextRequest::new();
    request.setRecognitionLevel(VNRequestTextRecognitionLevel::Accurate);
    request.setUsesLanguageCorrection(true);

    // Safety: `image` is a valid CGImage borrow for the whole call and the
    // empty options dictionary matches the expected generic shape.
    let handler = unsafe {
        VNImageRequestHandler::initWithCGImage_options(
            VNImageRequestHandler::alloc(),
            image,
            &NSDictionary::<_, AnyObject>::new(),
        )
    };

    // Upcast VNRecognizeTextRequest → VNImageBasedRequest → VNRequest for
    // the homogeneous request array performRequests expects.
    let base: Retained<VNRequest> = Retained::into_super(Retained::into_super(request.clone()));
    handler
        .performRequests_error(&NSArray::from_retained_slice(&[base]))
        .map_err(|e| OcrError::RecognitionFailed {
            detail: format!(
                "Vision performRequests failed: {}",
                e.localizedDescription()
            ),
        })?;

    // No results (nil) and zero results both mean "no text on screen" — a
    // valid empty extraction, not an error.
    let mut lines = Vec::new();
    if let Some(observations) = request.results() {
        for observation in observations.iter() {
            if let Some(best) = observation.topCandidates(1).iter().next() {
                let text = best.string().to_string();
                if !text.is_empty() {
                    lines.push(text);
                }
            }
        }
    }
    Ok(lines)
}

/// One recognized on-screen text run with its bounding box already
/// converted from Vision's normalized, lower-left-origin space to absolute
/// top-left screen pixels — the shape the screen_query tool hands the model
/// so it can aim an input_action click. Coordinates are transient: they are
/// produced per query and never persisted (R011).
#[derive(Debug, Clone, PartialEq)]
pub struct TextElement {
    pub text: String,
    /// Left edge in top-left-origin screen pixels.
    pub x: i32,
    /// Top edge in top-left-origin screen pixels.
    pub y: i32,
    pub width: i32,
    pub height: i32,
    /// The box center, computed server-side at FULL precision during the
    /// pixel→point mapping (never re-derived from the rounded x/width —
    /// double rounding costs a point, and small models doing x+w/2 cost
    /// more). This is the coordinate a click should aim at.
    pub cx: i32,
    pub cy: i32,
    /// The localized name of the app whose on-screen window covers this
    /// element's center, or `None` when no window is attributable (M005). Set by
    /// [`attribute_app`] against the capture's [`WindowAppRect`] list; lets the
    /// model distinguish same-labelled elements across apps and pair with the
    /// `focus_app` tool.
    pub app: Option<String>,
}

/// The topmost app owning a window that covers `(center_x, center_y)`, or `None`
/// when no window does (the desktop, an unattributed menu-bar region). Pure:
/// `windows` is expected already sorted topmost-first (descending `layer` —
/// higher CGWindowLevel is closer to the viewer — as
/// [`crate::capture::macos::WindowAppRect`]s arrive), so the first covering rect
/// wins — the frontmost window at that point. Point-in-rect is half-open on the
/// far edges so adjacent windows never both claim a shared boundary pixel.
pub fn attribute_app(center_x: i32, center_y: i32, windows: &[WindowAppRect]) -> Option<String> {
    windows
        .iter()
        .find(|w| {
            center_x >= w.x && center_x < w.x + w.w && center_y >= w.y && center_y < w.y + w.h
        })
        .map(|w| w.app.clone())
}

/// Run accurate-level Vision text recognition and return each observation's
/// best candidate *with* its bounding box, converted to top-left-origin
/// screen pixels. `cw`/`ch` are the source image's pixel width/height — the
/// normalization basis. Vision boxes are normalized 0..1 with the origin at
/// the image's LOWER-left corner; we scale by the pixel dimensions and flip
/// Y so callers get the same top-left screen space the input backend uses.
///
/// Each element is attributed to the app owning the topmost window covering its
/// center via [`attribute_app`] against `windows` (M005) — the same pixel space
/// the boxes land in. Passing an empty `windows` slice leaves every `app` as
/// `None`, which is what the synthetic-image tests do.
///
/// Mirrors [`recognize_text`] but keeps geometry; the text-only path stays
/// the R011 proof (no coordinate ever crosses [`OcrEngine::extract`]).
fn recognize_text_with_bounds(
    image: &objc2_core_graphics::CGImage,
    cw: u32,
    ch: u32,
    windows: &[WindowAppRect],
) -> Result<Vec<TextElement>, OcrError> {
    let request = VNRecognizeTextRequest::new();
    request.setRecognitionLevel(VNRequestTextRecognitionLevel::Accurate);
    request.setUsesLanguageCorrection(true);

    // Safety: `image` is a valid CGImage borrow for the whole call and the
    // empty options dictionary matches the expected generic shape.
    let handler = unsafe {
        VNImageRequestHandler::initWithCGImage_options(
            VNImageRequestHandler::alloc(),
            image,
            &NSDictionary::<_, AnyObject>::new(),
        )
    };

    let base: Retained<VNRequest> = Retained::into_super(Retained::into_super(request.clone()));
    handler
        .performRequests_error(&NSArray::from_retained_slice(&[base]))
        .map_err(|e| OcrError::RecognitionFailed {
            detail: format!(
                "Vision performRequests failed: {}",
                e.localizedDescription()
            ),
        })?;

    let cwf = cw as f64;
    let chf = ch as f64;
    let mut elements = Vec::new();
    if let Some(observations) = request.results() {
        for observation in observations.iter() {
            if let Some(best) = observation.topCandidates(1).iter().next() {
                let text = best.string().to_string();
                if text.is_empty() {
                    continue;
                }
                // Safety: `boundingBox` is inherited from
                // VNDetectedObjectObservation; normalized 0..1, lower-left
                // origin (objc2-vision VNObservation.rs).
                let bbox: objc2_core_foundation::CGRect = unsafe { observation.boundingBox() };
                let x = (bbox.origin.x * cwf).round() as i32;
                let width = (bbox.size.width * cwf).round() as i32;
                let height = (bbox.size.height * chf).round() as i32;
                // Flip Y: Vision's origin.y is the box's bottom in lower-left
                // space; the top edge in top-left space is
                // (1 - origin.y - height) scaled to pixels.
                let y = ((1.0 - bbox.origin.y - bbox.size.height) * chf).round() as i32;
                // Attribute to the app whose topmost window covers the box's
                // center — the same pixel space the box lives in (M005).
                let app = attribute_app(x + width / 2, y + height / 2, windows);
                elements.push(TextElement {
                    text,
                    x,
                    y,
                    width,
                    height,
                    // Pixel-space seed; to_screen_points recomputes at full
                    // precision during the point mapping.
                    cx: x + width / 2,
                    cy: y + height / 2,
                    app,
                });
            }
        }
    }
    Ok(elements)
}

/// The coordinate-bearing sibling of [`extract_blocking`]: capture the
/// display, recognize its text with bounding boxes, and return the elements
/// in logical screen points. Whole-screen scope — see
/// [`extract_elements_scoped_blocking`] for the window-cropped fast path.
pub fn extract_elements_blocking(max_dimension: u32) -> Result<Vec<TextElement>, OcrError> {
    extract_elements_scoped_blocking(max_dimension, None).map(|(elements, _)| elements)
}

/// Which pixels a screen query actually recognized.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryScope {
    /// Only the scoped app's front window was OCR'd (the fast path).
    Window,
    /// The whole display was OCR'd — no scope, no matching window, or the
    /// window read nothing (fallback).
    Screen,
}

/// A crop of the captured frame, in captured-pixel space (top-left origin).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CropRect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

/// A window covering at least this fraction of the frame is not worth
/// cropping — the OCR saving would not pay for the extra pass.
const CROP_MIN_SAVING: f64 = 0.85;
/// Crops narrower than this (pixels) are chrome slivers, not content.
const CROP_MIN_EDGE: i32 = 16;

/// The frame region to OCR for `app`: its FRONT window (the window list is
/// topmost-first), clamped to the frame. `None` when the app has no usable
/// window on screen or its window already covers nearly the whole frame —
/// the caller then reads the whole screen as before. Pure.
pub fn crop_rect_for(app: &str, windows: &[WindowAppRect], cw: u32, ch: u32) -> Option<CropRect> {
    let win = windows
        .iter()
        .find(|w| w.w > 0 && w.h > 0 && crate::appfocus::macos::names_match(&w.app, app))?;
    let x0 = win.x.max(0);
    let y0 = win.y.max(0);
    let x1 = (win.x + win.w).min(cw as i32);
    let y1 = (win.y + win.h).min(ch as i32);
    if x1 - x0 < CROP_MIN_EDGE || y1 - y0 < CROP_MIN_EDGE {
        return None;
    }
    let full = cw as f64 * ch as f64;
    if full > 0.0 && ((x1 - x0) as f64 * (y1 - y0) as f64) / full >= CROP_MIN_SAVING {
        return None;
    }
    Some(CropRect {
        x: x0,
        y: y0,
        w: x1 - x0,
        h: y1 - y0,
    })
}

/// Translate a crop-space element back into frame-pixel space. Pure.
fn shift_element(el: TextElement, dx: i32, dy: i32) -> TextElement {
    TextElement {
        x: el.x + dx,
        y: el.y + dy,
        cx: el.cx + dx,
        cy: el.cy + dy,
        ..el
    }
}

/// Capture the display and recognize text — only inside `window_of`'s front
/// window when one is on screen (2026-08-30: OCR cost scales with pixels,
/// and a screen_query already filters its answer to the focused app, so
/// reading the Dock, the wallpaper and every other window was paid for and
/// thrown away). Boxes come back in logical screen points either way: the
/// crop is OCR'd in its own pixel space, shifted back into the frame, then
/// mapped through the frame geometry exactly as the whole-screen path is —
/// click targets are identical to a full read. An empty crop falls back to
/// the whole screen (a dialog from another app may be what is showing).
/// The source image's own pixel `width()`/`height()` are the normalization
/// basis (NOT `max_dimension`, which only caps the capture). The `CGImage`
/// dies at the end of this function — only the elements escape (R011).
pub fn extract_elements_scoped_blocking(
    max_dimension: u32,
    window_of: Option<&str>,
) -> Result<(Vec<TextElement>, QueryScope), OcrError> {
    let start = Instant::now();
    // The window→app rects come back in the SAME pixel space the captured image
    // is normalized to, so each element can be labelled with its owning app.
    let (image, windows, geometry) = capture_display_image_with_geometry_blocking(max_dimension)?;
    let (cw, ch) = (image.width() as u32, image.height() as u32);
    let frame = as_vision_cgimage(&image);
    let mut scope = QueryScope::Screen;
    let mut recognized: Option<Vec<TextElement>> = None;
    if let Some((app, rect)) =
        window_of.and_then(|app| crop_rect_for(app, &windows, cw, ch).map(|r| (app, r)))
    {
        let cropped = objc2_core_graphics::CGImage::with_image_in_rect(
            Some(frame),
            objc2_core_foundation::CGRect::new(
                objc2_core_foundation::CGPoint::new(rect.x as f64, rect.y as f64),
                objc2_core_foundation::CGSize::new(rect.w as f64, rect.h as f64),
            ),
        );
        match cropped {
            Some(cropped) => {
                // Attribution runs in crop space: the same windows, shifted.
                let shifted: Vec<WindowAppRect> = windows
                    .iter()
                    .map(|w| WindowAppRect {
                        app: w.app.clone(),
                        x: w.x - rect.x,
                        y: w.y - rect.y,
                        w: w.w,
                        h: w.h,
                        layer: w.layer,
                    })
                    .collect();
                let elements =
                    recognize_text_with_bounds(&cropped, rect.w as u32, rect.h as u32, &shifted)?;
                if elements.is_empty() {
                    log::debug!(
                        "screen_query: {app:?} window crop read nothing — reading the whole screen"
                    );
                } else {
                    scope = QueryScope::Window;
                    recognized = Some(
                        elements
                            .into_iter()
                            .map(|el| shift_element(el, rect.x, rect.y))
                            .collect(),
                    );
                }
            }
            None => log::warn!("screen_query: crop {rect:?} failed — reading the whole screen"),
        }
    }
    let pixels = match (
        scope,
        window_of.and_then(|a| crop_rect_for(a, &windows, cw, ch)),
    ) {
        (QueryScope::Window, Some(r)) => format!("{}×{} of {cw}×{ch}", r.w, r.h),
        _ => format!("{cw}×{ch}"),
    };
    let result = match recognized {
        Some(elements) => Ok(elements),
        None => recognize_text_with_bounds(frame, cw, ch, &windows),
    }
    // Attribution ran in captured-pixel space (windows are in that space);
    // NOW map every box back to logical screen points so the model aims a
    // click in the coordinate space the input backend actually clicks in.
    .map(|elements| -> Vec<TextElement> {
        elements
            .into_iter()
            .map(|el| to_screen_points(el, geometry))
            .collect()
    });
    match &result {
        Ok(elements) => log::debug!(
            "screen_query: {} element(s) in {} ms (scope={scope:?}, {pixels})",
            elements.len(),
            start.elapsed().as_millis()
        ),
        Err(err) => log::error!("screen_query: {} ({err})", err.kind()),
    }
    result.map(|elements| (elements, scope))
}

/// Convert one element's bounding box from captured-pixel space to logical
/// screen points. The capture is taken at native density then capped to
/// `max_dimension`, so the pixel→point scale is `point / pixel` on each axis;
/// this is the exact inverse of the `point→pixel` scale the capture applied.
/// A degenerate geometry (zero pixels) leaves the box unchanged rather than
/// dividing by zero — the model then aims in pixel space, which is no worse than
/// before and never panics.
fn to_screen_points(el: TextElement, geom: CaptureGeometry) -> TextElement {
    if geom.pixel_w == 0 || geom.pixel_h == 0 {
        return el;
    }
    let sx = geom.point_w / geom.pixel_w as f64;
    let sy = geom.point_h / geom.pixel_h as f64;
    // Center at FULL precision before any rounding: x, width round
    // independently for display, but the click target must not inherit
    // their combined error (up to a point each way).
    // The display's global origin puts secondary-monitor boxes in global
    // point space — the same space enigo clicks in (multi-display).
    let fx = geom.origin_x + el.x as f64 * sx;
    let fy = geom.origin_y + el.y as f64 * sy;
    let fw = el.width as f64 * sx;
    let fh = el.height as f64 * sy;
    TextElement {
        text: el.text,
        x: fx.round() as i32,
        y: fy.round() as i32,
        width: fw.round() as i32,
        height: fh.round() as i32,
        cx: (fx + fw / 2.0).round() as i32,
        cy: (fy + fh / 2.0).round() as i32,
        app: el.app,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::c_void;
    use std::ptr::{null, null_mut};
    use std::sync::Arc;

    use objc2_core_foundation::{
        kCFTypeDictionaryKeyCallBacks, kCFTypeDictionaryValueCallBacks, CFAttributedString,
        CFDictionary, CFRetained, CFString, CGPoint, CGRect, CGSize,
    };
    use objc2_core_graphics::{
        CGBitmapContextCreate, CGBitmapContextCreateImage, CGColorSpace, CGContext,
        CGImageAlphaInfo,
    };
    use objc2_core_text::{kCTFontAttributeName, CTFont, CTLine};

    /// Render `text` in black Helvetica on a white bitmap and return the
    /// resulting in-memory CGImage — the synthetic input that lets Vision
    /// run in the default suite with no TCC permission and no live display.
    fn render_text_image(
        text: &str,
        width: usize,
        height: usize,
        font_size: f64,
    ) -> CFRetained<objc2_core_graphics::CGImage> {
        let color_space = CGColorSpace::new_device_rgb().expect("device RGB color space");
        // Safety: null data lets CG own the buffer; RGBA premultiplied-last
        // at 8 bits/component is a supported bitmap layout.
        let ctx = unsafe {
            CGBitmapContextCreate(
                null_mut(),
                width,
                height,
                8,
                0,
                Some(&color_space),
                CGImageAlphaInfo::PremultipliedLast.0,
            )
        }
        .expect("bitmap context");

        CGContext::set_rgb_fill_color(Some(&ctx), 1.0, 1.0, 1.0, 1.0);
        CGContext::fill_rect(
            Some(&ctx),
            CGRect {
                origin: CGPoint { x: 0.0, y: 0.0 },
                size: CGSize {
                    width: width as f64,
                    height: height as f64,
                },
            },
        );

        // Safety: the CoreText calls only read the CF objects created here;
        // the attribute dictionary holds CFType keys/values with the
        // standard CFType callbacks.
        unsafe {
            let font = CTFont::with_name(&CFString::from_str("Helvetica"), font_size, null());
            let mut keys: [*const c_void; 1] = [(kCTFontAttributeName as *const CFString).cast()];
            let mut values: [*const c_void; 1] = [(&*font as *const CTFont).cast()];
            let attributes = CFDictionary::new(
                None,
                keys.as_mut_ptr(),
                values.as_mut_ptr(),
                1,
                &kCFTypeDictionaryKeyCallBacks,
                &kCFTypeDictionaryValueCallBacks,
            )
            .expect("attribute dictionary");
            let attributed =
                CFAttributedString::new(None, Some(&CFString::from_str(text)), Some(&attributes))
                    .expect("attributed string");
            let line = CTLine::with_attributed_string(&attributed);
            CGContext::set_text_position(Some(&ctx), 24.0, height as f64 / 2.0 - font_size / 2.0);
            line.draw(&ctx);
        }

        CGBitmapContextCreateImage(Some(&ctx)).expect("bitmap image")
    }

    /// The non-ignored Vision proof (slice must-have 1): accurate-level
    /// recognition on a synthetic in-memory image — no Screen Recording
    /// permission, no live display, runs headless in CI-like environments.
    #[test]
    fn vision_recognizes_synthetic_in_memory_image() {
        let image = render_text_image("THIRD EYE WATCHER 42", 900, 200, 64.0);
        let lines = recognize_text(&image).expect("recognition succeeds on in-memory image");
        let joined = lines.join(" ").to_uppercase();
        assert!(joined.contains("THIRD"), "missing THIRD in {lines:?}");
        assert!(joined.contains("WATCHER"), "missing WATCHER in {lines:?}");
        assert!(joined.contains("42"), "missing 42 in {lines:?}");
    }

    /// A blank image must resolve to an empty extraction — "no text on
    /// screen" is a valid result, never an error (watcher ticks on empty
    /// desktops rely on this).
    #[test]
    fn vision_returns_empty_for_blank_image() {
        let image = render_text_image("", 400, 200, 64.0);
        let lines = recognize_text(&image).expect("blank image still recognizes cleanly");
        assert!(lines.is_empty(), "expected no text, got {lines:?}");
    }

    /// The non-ignored bounding-box proof (slice must-have 1): accurate-level
    /// recognition on a synthetic in-memory image returns the word WITH a
    /// pixel box, and the lower-left→top-left flip lands inside the image.
    #[test]
    fn recognize_text_with_bounds_returns_flipped_pixel_box() {
        let (w, h) = (900usize, 200usize);
        let image = render_text_image("TARGET", w, h, 64.0);
        // No window rects for a synthetic in-memory image → every app is None.
        let elements = recognize_text_with_bounds(&image, w as u32, h as u32, &[])
            .expect("recognition succeeds on in-memory image");
        assert!(!elements.is_empty(), "no elements recognized");
        assert!(
            elements.iter().all(|e| e.app.is_none()),
            "empty windows must leave every app unattributed"
        );
        let joined = elements
            .iter()
            .map(|e| e.text.as_str())
            .collect::<Vec<_>>()
            .join(" ")
            .to_uppercase();
        assert!(joined.contains("TARGET"), "missing TARGET in {elements:?}");
        // Every box must be a positive-area region inside the source image,
        // with a flipped-Y top edge in [0, height).
        for el in &elements {
            assert!(el.x > 0, "x not positive: {el:?}");
            assert!(el.width > 0, "width not positive: {el:?}");
            assert!(el.height > 0, "height not positive: {el:?}");
            assert!(el.x + el.width <= w as i32, "box exceeds width: {el:?}");
            assert!(
                el.y >= 0 && el.y < h as i32,
                "flipped y out of [0,h): {el:?}"
            );
            assert!(el.y + el.height <= h as i32, "box exceeds height: {el:?}");
        }
    }

    fn win(app: &str, x: i32, y: i32, w: i32, h: i32, layer: i32) -> WindowAppRect {
        WindowAppRect {
            app: app.into(),
            x,
            y,
            w,
            h,
            layer,
        }
    }

    #[test]
    fn to_screen_points_maps_captured_pixels_back_to_logical_points() {
        // A 3840x2160 captured image (Retina 1920x1080 @ 2x, under the 2048 cap
        // it would actually be capped, but exercise the pure scale here): a box
        // at pixel (1680, 480) 800x80 maps to point (840, 240) 400x40 — exactly
        // the space the input backend clicks in. This is the fix: without it the
        // model clicks pixel coords as if they were points and lands off-target.
        let geom = CaptureGeometry {
            pixel_w: 3840,
            pixel_h: 2160,
            point_w: 1920.0,
            point_h: 1080.0,
            origin_x: 0.0,
            origin_y: 0.0,
        };
        let el = TextElement {
            text: "Search Google or type a URL".into(),
            x: 1680,
            y: 480,
            width: 800,
            height: 80,
            cx: 0,
            cy: 0,
            app: Some("Google Chrome".into()),
        };
        let pt = to_screen_points(el, geom);
        assert_eq!((pt.x, pt.y, pt.width, pt.height), (840, 240, 400, 40));
        // The click target is the box centre, computed server-side.
        assert_eq!((pt.cx, pt.cy), (1040, 260));
        assert_eq!(pt.app.as_deref(), Some("Google Chrome"));
    }

    #[test]
    fn to_screen_points_is_identity_when_pixels_equal_points() {
        // Non-Retina 1:1 display: captured pixels already ARE points, so the box
        // is unchanged.
        let geom = CaptureGeometry {
            pixel_w: 1440,
            pixel_h: 900,
            point_w: 1440.0,
            point_h: 900.0,
            origin_x: 0.0,
            origin_y: 0.0,
        };
        let el = TextElement {
            text: "x".into(),
            x: 100,
            y: 200,
            width: 60,
            height: 24,
            cx: 130,
            cy: 212,
            app: None,
        };
        let pt = to_screen_points(el.clone(), geom);
        assert_eq!(pt, el);
    }

    #[test]
    fn to_screen_points_leaves_box_unchanged_on_degenerate_geometry() {
        // Zero-pixel geometry must not divide by zero — return the box as-is.
        let geom = CaptureGeometry {
            pixel_w: 0,
            pixel_h: 0,
            point_w: 1920.0,
            point_h: 1080.0,
            origin_x: 0.0,
            origin_y: 0.0,
        };
        let el = TextElement {
            text: "x".into(),
            x: 5,
            y: 6,
            width: 7,
            height: 8,
            cx: 0,
            cy: 0,
            app: None,
        };
        assert_eq!(to_screen_points(el.clone(), geom), el);
    }

    #[test]
    fn to_screen_points_adds_the_display_origin_for_secondary_monitors() {
        // A 2x display sitting to the RIGHT of the primary (origin 1512, 0):
        // a captured-pixel box maps into GLOBAL points, so the click lands on
        // that monitor, not at the same offset on the primary.
        let geom = CaptureGeometry {
            pixel_w: 3840,
            pixel_h: 2160,
            point_w: 1920.0,
            point_h: 1080.0,
            origin_x: 1512.0,
            origin_y: 0.0,
        };
        let el = TextElement {
            text: "Submit".into(),
            x: 200,
            y: 400,
            width: 100,
            height: 40,
            cx: 0,
            cy: 0,
            app: None,
        };
        let pt = to_screen_points(el, geom);
        assert_eq!((pt.x, pt.y), (1612, 200));
        assert_eq!((pt.cx, pt.cy), (1637, 210));
    }

    #[test]
    fn to_screen_points_center_is_computed_before_rounding() {
        // The reason cx/cy exist: rounding x and width independently and THEN
        // taking x + width/2 (what the model used to do) accumulates both
        // errors. At scale 0.75, a 2x2 box at pixel (2, 2) maps to 1.5..2.25 —
        // true centre 2.25 → 2, while the rounded-box arithmetic gives
        // 2 + 2/2 = 3, a full point off.
        let geom = CaptureGeometry {
            pixel_w: 2048,
            pixel_h: 2048,
            point_w: 1536.0,
            point_h: 1536.0,
            origin_x: 0.0,
            origin_y: 0.0,
        };
        let el = TextElement {
            text: "x".into(),
            x: 2,
            y: 2,
            width: 2,
            height: 2,
            cx: 0,
            cy: 0,
            app: None,
        };
        let pt = to_screen_points(el, geom);
        assert_eq!((pt.x, pt.width), (2, 2));
        assert_eq!((pt.cx, pt.cy), (2, 2));
        assert_ne!(
            pt.cx,
            pt.x + pt.width / 2 + 1,
            "naive arithmetic lands at 3"
        );
    }

    /// The window-scoped fast path (2026-08-30): crop to the app's FRONT
    /// window, clamped to the frame; skip when it would not save anything.
    #[test]
    fn crop_rect_picks_the_front_window_and_clamps_to_the_frame() {
        let windows = vec![
            win("Terminal", 100, 80, 900, 600, 3), // front Terminal window
            win("Terminal", 40, 40, 1200, 900, 2), // one behind it
            win("Google Chrome", -50, -20, 1500, 1100, 1),
        ];
        assert_eq!(
            crop_rect_for("Terminal", &windows, 2048, 1330),
            Some(CropRect {
                x: 100,
                y: 80,
                w: 900,
                h: 600
            }),
            "topmost-first: the first matching window is the front one"
        );
        assert_eq!(
            crop_rect_for("chrome", &windows, 2048, 1330),
            Some(CropRect {
                x: 0,
                y: 0,
                w: 1450,
                h: 1080
            }),
            "fuzzy name match; a window hanging off the top-left is clamped"
        );
        assert_eq!(crop_rect_for("Finder", &windows, 2048, 1330), None);
    }

    #[test]
    fn crop_rect_is_none_when_nothing_would_be_saved() {
        let full = vec![win("Zed", 0, 0, 2000, 1300, 0)];
        assert_eq!(
            crop_rect_for("Zed", &full, 2048, 1330),
            None,
            "a near-full-screen window reads the whole frame"
        );
        let sliver = vec![win("Zed", 10, 10, 8, 500, 0)];
        assert_eq!(crop_rect_for("Zed", &sliver, 2048, 1330), None);
        let zero = vec![win("Zed", 10, 10, 0, 0, 0)];
        assert_eq!(crop_rect_for("Zed", &zero, 2048, 1330), None);
    }

    #[test]
    fn shifted_elements_land_back_in_frame_space() {
        let el = TextElement {
            text: "ok".into(),
            x: 5,
            y: 6,
            width: 10,
            height: 4,
            cx: 10,
            cy: 8,
            app: None,
        };
        let back = shift_element(el, 100, 80);
        assert_eq!((back.x, back.y, back.cx, back.cy), (105, 86, 110, 88));
        assert_eq!((back.width, back.height), (10, 4));
    }

    #[test]
    fn attribute_app_center_inside_one_rect() {
        let windows = vec![win("Google Chrome", 100, 100, 400, 300, 0)];
        // A point well inside the single window is attributed to it.
        assert_eq!(
            attribute_app(200, 200, &windows).as_deref(),
            Some("Google Chrome")
        );
    }

    #[test]
    fn attribute_app_outside_all_rects_is_none() {
        let windows = vec![win("Zed", 0, 0, 100, 100, 0)];
        assert_eq!(
            attribute_app(200, 200, &windows),
            None,
            "the desktop is unattributed"
        );
        // No windows at all → None.
        assert_eq!(attribute_app(10, 10, &[]), None);
    }

    #[test]
    fn attribute_app_overlapping_rects_picks_topmost() {
        // Two overlapping windows; the slice is topmost-first (descending layer
        // — higher CGWindowLevel is closer to the viewer), exactly as
        // WindowAppRects arrive, so the first covering rect wins.
        let windows = vec![
            win("Google Chrome", 0, 0, 500, 500, 5), // frontmost
            win("Zed", 0, 0, 500, 500, 0),           // behind
        ];
        assert_eq!(
            attribute_app(250, 250, &windows).as_deref(),
            Some("Google Chrome"),
            "the topmost (highest layer) window must win the overlap"
        );
    }

    #[test]
    fn attribute_app_edges_are_half_open() {
        // Half-open on the far edges so adjacent windows never both claim a
        // boundary pixel: the left/top edge is inside, the right/bottom is not.
        let windows = vec![win("App", 10, 10, 20, 20, 0)]; // covers x∈[10,30), y∈[10,30)
        assert_eq!(
            attribute_app(10, 10, &windows).as_deref(),
            Some("App"),
            "top-left is inside"
        );
        assert_eq!(
            attribute_app(29, 29, &windows).as_deref(),
            Some("App"),
            "last inside pixel"
        );
        assert_eq!(
            attribute_app(30, 20, &windows),
            None,
            "right edge is exclusive"
        );
        assert_eq!(
            attribute_app(20, 30, &windows),
            None,
            "bottom edge is exclusive"
        );
    }

    /// Live probe of the coordinate-space fix (M005 targeting): capture the real
    /// screen, print the geometry, and assert every returned box falls within the
    /// display's LOGICAL POINT bounds — the space the input backend clicks in.
    /// Before the fix, boxes were in captured-pixel space (capped at 2048), so on
    /// a Retina panel they exceeded the point bounds and the click landed
    /// off-target. Ignored (needs Screen Recording + a live display).
    #[test]
    #[ignore = "requires Screen Recording permission and a live display (targeting UAT)"]
    fn live_screen_query_coordinates_are_in_logical_points() {
        use crate::capture::macos::capture_display_image_with_geometry_blocking;
        let (_image, _windows, geom) =
            match capture_display_image_with_geometry_blocking(super::super::OCR_MAX_DIMENSION) {
                Ok(t) => t,
                Err(err) => {
                    assert_eq!(err.kind(), "permission-denied", "unexpected: {err}");
                    return;
                }
            };
        eprintln!(
            "geometry: pixels {}x{}  points {}x{}  (scale {:.3}x {:.3}y)",
            geom.pixel_w,
            geom.pixel_h,
            geom.point_w,
            geom.point_h,
            geom.pixel_w as f64 / geom.point_w,
            geom.pixel_h as f64 / geom.point_h,
        );
        let elements = extract_elements_blocking(super::super::OCR_MAX_DIMENSION)
            .expect("screen query succeeds with permission");
        eprintln!(
            "{} element(s); first few in LOGICAL POINTS:",
            elements.len()
        );
        for el in elements.iter().take(6) {
            eprintln!(
                "  {:?} @ ({},{}) {}x{} app={:?}",
                el.text, el.x, el.y, el.width, el.height, el.app
            );
        }
        // Every box must lie within the display's logical-point bounds (with a
        // small margin for rounding) — proof the coordinates are clickable.
        let (pw, ph) = (geom.point_w as i32, geom.point_h as i32);
        for el in &elements {
            assert!(
                el.x >= 0 && el.x <= pw + 2 && el.y >= 0 && el.y <= ph + 2,
                "box outside logical point bounds {pw}x{ph}: {el:?}",
            );
        }
    }

    /// Live run of the full trait pipeline against the real screen (MEM038
    /// precedent) — demo-level evidence for the S01 loop, ignored in the
    /// default suite. Without permission it must fail *typed*.
    #[tokio::test]
    #[ignore = "requires Screen Recording permission and a live display (slice UAT)"]
    async fn real_screen_extract_smoke() {
        let engine: Arc<dyn OcrEngine> = Arc::new(VisionOcr::new(super::super::OCR_MAX_DIMENSION));
        match engine.extract().await {
            Ok(lines) => {
                println!(
                    "ocr: recognized {} line(s) from the live screen:",
                    lines.len()
                );
                for line in &lines {
                    println!("  {line}");
                }
            }
            Err(err) => {
                assert_eq!(err.kind(), "permission-denied", "unexpected: {err}");
            }
        }
    }
}
