//! Frame encode pipeline: raw RGBA pixels → downscale (max 2048px) → PNG →
//! base64, producing the [`CapturedFrame`] that crosses IPC and becomes the
//! `data:image/png;base64,...` vision URL in T03.
//!
//! Deliberately platform-free: the macOS backend hands in pixels, and every
//! branch (cap enforcement, aspect preservation, malformed buffers) is
//! unit-tested cross-platform with synthetic pixels — no ScreenCaptureKit
//! needed to prove the pipeline.

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use image::imageops::FilterType;
use image::RgbaImage;

use super::{CaptureError, CapturedFrame};

/// Longest edge the encoded frame may have. Caps vision-token cost and
/// request size; a 2048px-wide frame keeps on-screen text readable for the
/// model. The macOS backend also requests this size from ScreenCaptureKit so
/// the GPU does the scaling and this cap is normally a no-op safety net.
pub const MAX_DIMENSION: u32 = 2048;

/// One frame of tightly-packed RGBA pixels (no row padding), as produced by
/// the ScreenCaptureKit backend's CGImage render.
pub struct RawRgbaFrame {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// Target dimensions that fit `width`×`height` inside a `max`×`max` box,
/// preserving aspect ratio and never upscaling. Total function; both results
/// are ≥ 1 for non-zero input.
pub fn fit_within(width: u32, height: u32, max: u32) -> (u32, u32) {
    if width <= max && height <= max {
        return (width, height);
    }
    let scale = f64::from(max) / f64::from(width.max(height));
    let fit = |dim: u32| ((f64::from(dim) * scale).round() as u32).clamp(1, max);
    (fit(width), fit(height))
}

/// Encode one raw frame into the IPC [`CapturedFrame`], enforcing
/// [`MAX_DIMENSION`]. Every failure is a typed `capture-failed` error (R007).
pub fn encode_rgba_frame(frame: RawRgbaFrame) -> Result<CapturedFrame, CaptureError> {
    encode_rgba_frame_with_max(frame, MAX_DIMENSION)
}

/// [`encode_rgba_frame`] with an explicit cap so tests can exercise the
/// downscale path without megapixel fixtures.
fn encode_rgba_frame_with_max(
    frame: RawRgbaFrame,
    max: u32,
) -> Result<CapturedFrame, CaptureError> {
    let RawRgbaFrame { width, height, rgba } = frame;
    if width == 0 || height == 0 {
        return Err(CaptureError::CaptureFailed {
            detail: format!("empty frame: {width}x{height}"),
        });
    }
    let expected = (width as usize)
        .checked_mul(height as usize)
        .and_then(|n| n.checked_mul(4))
        .ok_or_else(|| CaptureError::CaptureFailed {
            detail: format!("frame dimensions overflow: {width}x{height}"),
        })?;
    if rgba.len() != expected {
        return Err(CaptureError::CaptureFailed {
            detail: format!(
                "pixel buffer size mismatch: {width}x{height} needs {expected} bytes, got {}",
                rgba.len()
            ),
        });
    }
    // Length was validated above, so from_raw cannot fail; keep the typed
    // error anyway rather than unwrap so no panic path exists (R007).
    let img = RgbaImage::from_raw(width, height, rgba).ok_or_else(|| {
        CaptureError::CaptureFailed { detail: "pixel buffer rejected by image layer".into() }
    })?;

    let (target_w, target_h) = fit_within(width, height, max);
    let img = if (target_w, target_h) == (width, height) {
        img
    } else {
        image::imageops::resize(&img, target_w, target_h, FilterType::Triangle)
    };

    let mut png: Vec<u8> = Vec::new();
    img.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .map_err(|e| CaptureError::CaptureFailed { detail: format!("png encode failed: {e}") })?;

    Ok(CapturedFrame {
        width: target_w,
        height: target_h,
        base64_png: BASE64.encode(&png),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic synthetic frame: each pixel encodes its own coordinates,
    /// so a roundtrip proves channel order and geometry survive the pipeline.
    fn synthetic_frame(width: u32, height: u32) -> RawRgbaFrame {
        let mut rgba = Vec::with_capacity((width * height * 4) as usize);
        for y in 0..height {
            for x in 0..width {
                rgba.extend_from_slice(&[(x % 256) as u8, (y % 256) as u8, 0x2a, 0xff]);
            }
        }
        RawRgbaFrame { width, height, rgba }
    }

    fn decode(frame: &CapturedFrame) -> RgbaImage {
        let png = BASE64.decode(&frame.base64_png).expect("valid base64");
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n", "PNG magic bytes");
        image::load_from_memory_with_format(&png, image::ImageFormat::Png)
            .expect("valid png")
            .to_rgba8()
    }

    #[test]
    fn roundtrip_preserves_pixels_below_cap() {
        let encoded = encode_rgba_frame(synthetic_frame(4, 2)).unwrap();
        assert_eq!((encoded.width, encoded.height), (4, 2));
        let img = decode(&encoded);
        assert_eq!(img.dimensions(), (4, 2));
        // PNG is lossless: pixel (3, 1) must carry its exact synthetic value.
        assert_eq!(img.get_pixel(3, 1).0, [3, 1, 0x2a, 0xff]);
        assert_eq!(img.get_pixel(0, 0).0, [0, 0, 0x2a, 0xff]);
    }

    #[test]
    fn oversized_frame_is_downscaled_preserving_aspect() {
        let frame = synthetic_frame(32, 16);
        let encoded = encode_rgba_frame_with_max(frame, 8).unwrap();
        assert_eq!((encoded.width, encoded.height), (8, 4));
        assert_eq!(decode(&encoded).dimensions(), (8, 4));
    }

    #[test]
    fn portrait_frame_caps_on_height() {
        let encoded = encode_rgba_frame_with_max(synthetic_frame(16, 32), 8).unwrap();
        assert_eq!((encoded.width, encoded.height), (4, 8));
    }

    #[test]
    fn fit_within_never_upscales_and_never_hits_zero() {
        assert_eq!(fit_within(1920, 1080, 2048), (1920, 1080));
        assert_eq!(fit_within(2048, 2048, 2048), (2048, 2048));
        assert_eq!(fit_within(5120, 2880, 2048), (2048, 1152));
        assert_eq!(fit_within(2880, 5120, 2048), (1152, 2048));
        // Extreme aspect ratio must clamp to 1, not round to 0.
        assert_eq!(fit_within(10_000, 1, 2048), (2048, 1));
    }

    #[test]
    fn zero_dimension_frame_is_typed_capture_failed() {
        let err = encode_rgba_frame(RawRgbaFrame { width: 0, height: 4, rgba: vec![] })
            .unwrap_err();
        assert_eq!(err.kind(), "capture-failed");
        assert!(err.to_string().contains("empty frame"), "{err}");
    }

    #[test]
    fn buffer_size_mismatch_is_typed_capture_failed() {
        // 4x2 needs 32 bytes; hand it 31 (torn buffer) and 33 (padded row).
        for len in [31usize, 33] {
            let err = encode_rgba_frame(RawRgbaFrame {
                width: 4,
                height: 2,
                rgba: vec![0; len],
            })
            .unwrap_err();
            assert_eq!(err.kind(), "capture-failed");
            assert!(err.to_string().contains("size mismatch"), "{err}");
        }
    }
}
