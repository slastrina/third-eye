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
use crate::capture::macos::capture_display_image_blocking;

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
                log::debug!("ocr: {} line(s) in {} ms", lines.len(), start.elapsed().as_millis());
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
            detail: format!("Vision performRequests failed: {}", e.localizedDescription()),
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
                size: CGSize { width: width as f64, height: height as f64 },
            },
        );

        // Safety: the CoreText calls only read the CF objects created here;
        // the attribute dictionary holds CFType keys/values with the
        // standard CFType callbacks.
        unsafe {
            let font = CTFont::with_name(&CFString::from_str("Helvetica"), font_size, null());
            let mut keys: [*const c_void; 1] =
                [(kCTFontAttributeName as *const CFString).cast()];
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
            let attributed = CFAttributedString::new(
                None,
                Some(&CFString::from_str(text)),
                Some(&attributes),
            )
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

    /// Live run of the full trait pipeline against the real screen (MEM038
    /// precedent) — demo-level evidence for the S01 loop, ignored in the
    /// default suite. Without permission it must fail *typed*.
    #[tokio::test]
    #[ignore = "requires Screen Recording permission and a live display (slice UAT)"]
    async fn real_screen_extract_smoke() {
        let engine: Arc<dyn OcrEngine> = Arc::new(VisionOcr::new(super::super::OCR_MAX_DIMENSION));
        match engine.extract().await {
            Ok(lines) => {
                println!("ocr: recognized {} line(s) from the live screen:", lines.len());
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
