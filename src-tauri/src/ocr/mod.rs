//! OCR boundary: the screen-text-source seam behind the S01 watcher.
//!
//! [`OcrEngine`] is deliberately shaped as a *text source*, not an image
//! processor: the macOS backend owns the whole capture→Vision chain
//! internally and only text crosses the trait. No pixel type appears in any
//! signature here — that is the structural form of the extract-and-discard
//! promise (R011): the captured image lives and dies inside one blocking
//! call in [`macos`], is never encoded, and never touches disk.
//!
//! Failure taxonomy mirrors [`crate::capture::CaptureError`]: every failure
//! is a typed [`OcrError`] variant, serialized kind-tagged with camelCase
//! fields (R007), so watcher status surfaces and logs always name the
//! failure class (`permission-denied` / `capture-failed` /
//! `recognition-failed` / `unsupported`).
//!
//! Platform binding: macOS gets [`macos::VisionOcr`] (Apple Vision via
//! objc2-vision, on-device only); every other OS gets
//! [`fallback::FallbackOcr`], which returns typed `unsupported` errors so
//! Windows/Linux builds stay clean (R020).

pub mod fallback;
#[cfg(target_os = "macos")]
pub mod macos;

use std::sync::Arc;

use async_trait::async_trait;
use serde::Serialize;

use crate::capture::CaptureError;

/// Longest-edge pixel cap for OCR captures. Higher than a thumbnail (Vision
/// accuracy degrades on small glyphs) but below native retina so per-tick
/// recognition cost stays bounded; the watcher passes this to the shared
/// capture stage (`capture_display_image_blocking`).
pub const OCR_MAX_DIMENSION: u32 = 2048;

/// The full OCR failure taxonomy (R007). Serialized with a `kind` tag
/// (`permission-denied` / `capture-failed` / `recognition-failed` /
/// `unsupported`) and camelCase fields — the same IPC error contract shape
/// as [`CaptureError`] and [`crate::llm::LlmError`]; consumers match on
/// `kind`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(
    tag = "kind",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase"
)]
pub enum OcrError {
    /// Screen Recording permission is not granted (TCC) — the capture stage
    /// refused before any pixel was read. The watcher surfaces the same
    /// guided walkthrough as "Attach my screen".
    PermissionDenied { detail: String },
    /// The screen capture stage failed after the permission check
    /// (no display, ScreenCaptureKit error). Recognition never ran.
    CaptureFailed { detail: String },
    /// Capture succeeded but Vision text recognition failed on the
    /// in-memory image.
    RecognitionFailed { detail: String },
    /// OCR is not implemented on this platform. `platform` names the
    /// running OS so logs and status surfaces are self-explanatory.
    Unsupported { platform: String, detail: String },
}

impl OcrError {
    /// Stable machine-readable name, mirroring the serde `kind` tag. Used in
    /// watcher tick error logs so grep for `permission-denied` /
    /// `capture-failed` / `recognition-failed` / `unsupported` works.
    pub fn kind(&self) -> &'static str {
        match self {
            OcrError::PermissionDenied { .. } => "permission-denied",
            OcrError::CaptureFailed { .. } => "capture-failed",
            OcrError::RecognitionFailed { .. } => "recognition-failed",
            OcrError::Unsupported { .. } => "unsupported",
        }
    }

    /// The `unsupported` error for the current platform — the one shape the
    /// fallback backend ever returns.
    pub fn unsupported_here() -> Self {
        OcrError::Unsupported {
            platform: std::env::consts::OS.to_string(),
            detail: "on-screen text extraction is only implemented on macOS".to_string(),
        }
    }
}

/// Total mapping from the shared capture stage's failures: the permission
/// class survives (the watcher keys its walkthrough on it, exactly like the
/// attach flow); everything else — no display, pipeline failure, and the
/// never-expected privacy/unsupported shapes — collapses into
/// `capture-failed` with the original kind preserved in the detail. (The
/// watcher checks privacy *before* capturing, so a `privacy-mode` arriving
/// here is a bug worth seeing in the detail, not a variant worth its own
/// OCR kind.)
impl From<CaptureError> for OcrError {
    fn from(err: CaptureError) -> Self {
        match err {
            CaptureError::PermissionDenied { detail } => OcrError::PermissionDenied { detail },
            other => OcrError::CaptureFailed {
                detail: format!("{}: {}", other.kind(), other),
            },
        }
    }
}

impl std::fmt::Display for OcrError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OcrError::PermissionDenied { detail } => {
                write!(
                    f,
                    "ocr permission-denied: Screen Recording not granted ({detail})"
                )
            }
            OcrError::CaptureFailed { detail } => {
                write!(f, "ocr capture-failed: {detail}")
            }
            OcrError::RecognitionFailed { detail } => {
                write!(f, "ocr recognition-failed: {detail}")
            }
            OcrError::Unsupported { platform, detail } => {
                write!(f, "ocr unsupported on {platform}: {detail}")
            }
        }
    }
}

impl std::error::Error for OcrError {}

/// The OCR seam. Object-safe (`Arc<dyn OcrEngine>`) so the watcher loop and
/// tests can hold any backend without knowing its transport.
///
/// `extract` is the whole pipeline: capture the primary display (with this
/// process's windows excluded, R008) and recognize its text on-device,
/// returning recognized lines in Vision's reading order. Pixels are an
/// internal detail of the backend — they are dropped before this future
/// resolves and can never cross this seam (R011).
#[async_trait]
pub trait OcrEngine: Send + Sync {
    async fn extract(&self) -> Result<Vec<String>, OcrError>;
}

/// The live backend for this platform: Apple Vision on macOS, typed
/// `unsupported` fallback everywhere else. `max_dimension` caps the capture's
/// longest edge (watcher passes [`OCR_MAX_DIMENSION`]).
pub fn platform_engine(max_dimension: u32) -> Arc<dyn OcrEngine> {
    #[cfg(target_os = "macos")]
    {
        Arc::new(macos::VisionOcr::new(max_dimension))
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = max_dimension;
        Arc::new(fallback::FallbackOcr)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal in-memory engine proving the trait is implementable and
    /// object-safe — the same shape the T03 watcher tests will use.
    struct MockOcr {
        fail_with: Option<OcrError>,
    }

    #[async_trait]
    impl OcrEngine for MockOcr {
        async fn extract(&self) -> Result<Vec<String>, OcrError> {
            if let Some(err) = &self.fail_with {
                return Err(err.clone());
            }
            Ok(vec!["hello".to_string(), "world".to_string()])
        }
    }

    #[tokio::test]
    async fn trait_is_object_safe_and_extracts_through_dyn() {
        let engine: Arc<dyn OcrEngine> = Arc::new(MockOcr { fail_with: None });
        let lines = engine.extract().await.unwrap();
        assert_eq!(lines, vec!["hello", "world"]);
    }

    #[tokio::test]
    async fn errors_propagate_through_dyn_with_kind() {
        let engine: Arc<dyn OcrEngine> = Arc::new(MockOcr {
            fail_with: Some(OcrError::RecognitionFailed {
                detail: "vision failed".into(),
            }),
        });
        let err = engine.extract().await.unwrap_err();
        assert_eq!(err.kind(), "recognition-failed");
    }

    #[test]
    fn error_json_shape_is_the_ipc_contract() {
        // Watcher status and diagnostics match on `kind` and read camelCase
        // fields; a change here is a breaking IPC change.
        let denied = OcrError::PermissionDenied {
            detail: "TCC denied".into(),
        };
        let v = serde_json::to_value(&denied).unwrap();
        assert_eq!(v["kind"], "permission-denied");
        assert_eq!(v["detail"], "TCC denied");

        let capture = OcrError::CaptureFailed {
            detail: "no-display: asleep".into(),
        };
        let v = serde_json::to_value(&capture).unwrap();
        assert_eq!(v["kind"], "capture-failed");
        assert_eq!(v["detail"], "no-display: asleep");

        let recognition = OcrError::RecognitionFailed {
            detail: "vision error".into(),
        };
        let v = serde_json::to_value(&recognition).unwrap();
        assert_eq!(v["kind"], "recognition-failed");
        assert_eq!(v["detail"], "vision error");

        let unsupported = OcrError::Unsupported {
            platform: "linux".into(),
            detail: "no backend".into(),
        };
        let v = serde_json::to_value(&unsupported).unwrap();
        assert_eq!(v["kind"], "unsupported");
        assert_eq!(v["platform"], "linux");
        assert_eq!(v["detail"], "no backend");
    }

    #[test]
    fn kind_matches_serde_tag_for_every_variant() {
        let all = [
            OcrError::PermissionDenied {
                detail: String::new(),
            },
            OcrError::CaptureFailed {
                detail: String::new(),
            },
            OcrError::RecognitionFailed {
                detail: String::new(),
            },
            OcrError::Unsupported {
                platform: String::new(),
                detail: String::new(),
            },
        ];
        for err in all {
            let v = serde_json::to_value(&err).unwrap();
            assert_eq!(v["kind"], err.kind(), "kind()/serde tag drift for {err:?}");
        }
    }

    #[test]
    fn capture_permission_error_survives_the_mapping() {
        let err: OcrError = CaptureError::PermissionDenied {
            detail: "TCC denied".into(),
        }
        .into();
        assert_eq!(err.kind(), "permission-denied");
        assert_eq!(
            err,
            OcrError::PermissionDenied {
                detail: "TCC denied".into()
            }
        );
    }

    #[test]
    fn other_capture_errors_collapse_to_capture_failed_with_kind_in_detail() {
        let cases: Vec<(CaptureError, &str)> = vec![
            (
                CaptureError::NoDisplay {
                    detail: "asleep".into(),
                },
                "no-display",
            ),
            (
                CaptureError::CaptureFailed {
                    detail: "stream error".into(),
                },
                "capture-failed",
            ),
            (CaptureError::privacy_mode(), "privacy-mode"),
            (CaptureError::unsupported_here(), "unsupported"),
        ];
        for (capture_err, original_kind) in cases {
            let err: OcrError = capture_err.into();
            assert_eq!(err.kind(), "capture-failed");
            match err {
                OcrError::CaptureFailed { detail } => {
                    assert!(
                        detail.contains(original_kind),
                        "original kind {original_kind} lost in detail: {detail}"
                    );
                }
                other => panic!("expected capture-failed, got {other:?}"),
            }
        }
    }

    #[test]
    fn error_display_names_kind_and_detail() {
        let err = OcrError::RecognitionFailed {
            detail: "vision gave up".into(),
        };
        let msg = err.to_string();
        assert!(msg.contains("recognition-failed"), "kind missing: {msg}");
        assert!(msg.contains("vision gave up"), "detail missing: {msg}");
    }

    #[test]
    fn unsupported_here_names_this_platform() {
        let err = OcrError::unsupported_here();
        assert_eq!(err.kind(), "unsupported");
        match err {
            OcrError::Unsupported { platform, .. } => {
                assert_eq!(platform, std::env::consts::OS);
            }
            other => panic!("expected unsupported, got {other:?}"),
        }
    }

    #[test]
    fn platform_engine_binds_without_panicking() {
        // Smoke: the binding compiles and instantiates on every platform.
        let _engine: Arc<dyn OcrEngine> = platform_engine(OCR_MAX_DIMENSION);
    }
}
