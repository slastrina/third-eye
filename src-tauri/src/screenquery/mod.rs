//! Screen-query boundary: the on-screen-text-with-coordinates seam behind the
//! `screen_query` tool (S02).
//!
//! [`ScreenQuery`] is the object-safe abstraction the composite executor's
//! `ScreenQueryTool` (T03) holds as `Arc<dyn ScreenQuery>`, mirroring the S01
//! [`crate::input::commands::InputState`] pattern. It is deliberately *not* the
//! watcher's [`crate::ocr::OcrEngine`]: that seam is text-only and private (the
//! R011 proof — no coordinate ever crosses it), so the tool needs its own
//! coordinate-bearing seam rather than an extended `OcrEngine`.
//!
//! [`ScreenElement`] is the platform-neutral element type the tool serializes
//! to the model: on-screen text with an absolute top-left-origin screen-pixel
//! box (sourced from Apple Vision on macOS, converted in
//! [`crate::ocr::macos::TextElement`]). Coordinates are transient — produced
//! per query, handed to the model to aim an `input_action`, and never persisted
//! (R011/R023).
//!
//! Failure taxonomy mirrors [`crate::ocr::OcrError`] and
//! [`crate::input::InputError`]: every failure is a typed [`ScreenQueryError`]
//! variant, serialized kind-tagged with camelCase fields (R007), so the tool
//! surfaces name the failure class (`permission-denied` / `recognition-failed`
//! / `unsupported`).
//!
//! Platform binding: macOS gets [`macos::MacosScreenQuery`] (Apple Vision via
//! the shared OCR capture chain); every other OS gets
//! [`fallback::FallbackScreenQuery`], which returns typed `unsupported` errors
//! so Windows/Linux builds stay clean (R020).

#[cfg(target_os = "macos")]
pub mod ax;
pub mod commands;
pub mod fallback;
#[cfg(target_os = "macos")]
pub mod macos;

use async_trait::async_trait;
use serde::Serialize;

use crate::ocr::OcrError;

/// One on-screen text element with its bounding box in absolute top-left-origin
/// screen pixels — the platform-neutral shape the `screen_query` tool hands the
/// model so it can aim an `input_action` click. camelCase in JSON to ride the
/// tool-call contract. Coordinates are transient and never persisted (R011).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScreenElement {
    pub text: String,
    /// Left edge in top-left-origin screen pixels.
    pub x: i32,
    /// Top edge in top-left-origin screen pixels.
    pub y: i32,
    pub width: i32,
    pub height: i32,
    /// Server-computed click target: the box center at full precision. The
    /// model clicks THIS — no arithmetic on its side.
    pub cx: i32,
    pub cy: i32,
    /// The localized name of the app owning the on-screen window this element's
    /// center falls inside — `None` when no attributable window covers it (the
    /// desktop, a menu bar extra, or the primary display's own chrome). Lets the
    /// model tell "the Submit button in Chrome" from an identically-labelled one
    /// elsewhere, and pair with `focus_app` to operate the right app (M005).
    pub app: Option<String>,
    /// The accessibility role for elements sourced from the AX tree
    /// ("AXButton", "AXLink", …) — the REAL interactive controls, with exact
    /// frames. `None` for OCR-sourced text (accuracy v2, 2026-07-27).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
}

/// The full screen-query failure taxonomy (R007). Serialized with a `kind` tag
/// (`permission-denied` / `recognition-failed` / `unsupported`) and camelCase
/// fields — the same IPC error contract shape as [`OcrError`] and
/// [`crate::input::InputError`]; consumers match on `kind`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(
    tag = "kind",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase"
)]
pub enum ScreenQueryError {
    /// Screen Recording permission is not granted (TCC) — capture refused
    /// before any pixel was read. The UI responds with the guided walkthrough.
    PermissionDenied { detail: String },
    /// Capture or Vision recognition failed after the permission check. The
    /// underlying [`OcrError`] kind is preserved in `detail`.
    RecognitionFailed { detail: String },
    /// Screen query is not implemented on this platform. `platform` names the
    /// running OS so logs and status surfaces are self-explanatory.
    Unsupported { platform: String, detail: String },
}

impl ScreenQueryError {
    /// Stable machine-readable name, mirroring the serde `kind` tag. Used in
    /// error logs so grep for `permission-denied` / `recognition-failed` /
    /// `unsupported` works.
    pub fn kind(&self) -> &'static str {
        match self {
            ScreenQueryError::PermissionDenied { .. } => "permission-denied",
            ScreenQueryError::RecognitionFailed { .. } => "recognition-failed",
            ScreenQueryError::Unsupported { .. } => "unsupported",
        }
    }

    /// The `unsupported` error for the current platform — the one shape the
    /// fallback backend ever returns.
    pub fn unsupported_here() -> Self {
        ScreenQueryError::Unsupported {
            platform: std::env::consts::OS.to_string(),
            detail: "screen query is only implemented on macOS".to_string(),
        }
    }
}

/// Mapping from the OCR seam's failures: the permission class survives (the UI
/// keys its walkthrough on it, exactly like the capture flow); everything else
/// — capture failure, recognition failure, and the never-expected unsupported
/// shape — collapses into `recognition-failed` with the original kind preserved
/// in the detail.
impl From<OcrError> for ScreenQueryError {
    fn from(err: OcrError) -> Self {
        match err {
            OcrError::PermissionDenied { detail } => ScreenQueryError::PermissionDenied { detail },
            other => ScreenQueryError::RecognitionFailed {
                detail: format!("{}: {}", other.kind(), other),
            },
        }
    }
}

impl std::fmt::Display for ScreenQueryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ScreenQueryError::PermissionDenied { detail } => {
                write!(
                    f,
                    "screen-query permission-denied: Screen Recording not granted ({detail})"
                )
            }
            ScreenQueryError::RecognitionFailed { detail } => {
                write!(f, "screen-query recognition-failed: {detail}")
            }
            ScreenQueryError::Unsupported { platform, detail } => {
                write!(f, "screen-query unsupported on {platform}: {detail}")
            }
        }
    }
}

impl std::error::Error for ScreenQueryError {}

/// The screen-query seam. Object-safe (`Arc<dyn ScreenQuery>`) so managed state,
/// the composite executor, and tests can hold any backend without knowing its
/// transport. `Send + Sync` so it can live in Tauri managed state like
/// [`crate::input::commands::InputState`].
///
/// `query` is the whole pipeline: capture the primary display, recognize its
/// on-screen text with bounding boxes on-device, and return the elements in
/// absolute top-left screen pixels. Pixels are an internal detail of the
/// backend — they are dropped before this future resolves and can never cross
/// this seam (R011).
#[async_trait]
pub trait ScreenQuery: Send + Sync {
    async fn query(&self) -> Result<Vec<ScreenElement>, ScreenQueryError>;

    /// The focused app's interactive elements from its accessibility tree —
    /// real buttons/links/fields with exact point frames. Best-effort and
    /// additive: backends without an AX walk (fallback, mocks) return empty
    /// and the query result stands on OCR alone.
    async fn interactive(&self, app_name: &str) -> Vec<ScreenElement> {
        let _ = app_name;
        Vec::new()
    }

    /// The readable text of the app's frontmost content in tree order —
    /// the `read_page` tool's substance ("what does this page say").
    /// `None` when the backend has no text harvest (fallback, mocks) or
    /// the app exposes nothing.
    async fn page_text(&self, app_name: &str) -> Option<String> {
        let _ = app_name;
        None
    }
}

/// Merge AX-sourced interactive elements with OCR text: AX first (they are
/// the authoritative frames), then OCR elements whose center does NOT fall
/// inside any AX element's box — a text line centered inside a button IS
/// that button's label, and keeping both would offer the model two targets
/// for one control with slightly different coordinates.
pub fn merge_ax_and_ocr(ax: Vec<ScreenElement>, ocr: Vec<ScreenElement>) -> Vec<ScreenElement> {
    let mut merged = ax;
    let kept: Vec<ScreenElement> = ocr
        .into_iter()
        .filter(|el| {
            !merged.iter().any(|a| {
                el.cx >= a.x && el.cx < a.x + a.width && el.cy >= a.y && el.cy < a.y + a.height
            })
        })
        .collect();
    merged.extend(kept);
    merged
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn el(text: &str, x: i32, y: i32, w: i32, h: i32, role: Option<&str>) -> ScreenElement {
        ScreenElement {
            text: text.into(),
            x,
            y,
            width: w,
            height: h,
            cx: x + w / 2,
            cy: y + h / 2,
            app: Some("Chrome".into()),
            role: role.map(Into::into),
        }
    }

    #[test]
    fn merge_puts_ax_first_and_drops_ocr_labels_inside_ax_boxes() {
        let ax = vec![el("Search", 100, 100, 120, 40, Some("AXButton"))];
        let ocr = vec![
            // OCR read the button's own label — a duplicate target with
            // slightly different coordinates; it must drop.
            el("Search", 110, 112, 100, 16, None),
            // Unrelated text elsewhere survives.
            el("Results for carbonara", 100, 300, 300, 20, None),
        ];
        let merged = merge_ax_and_ocr(ax, ocr);
        assert_eq!(
            merged
                .iter()
                .map(|e| (e.text.as_str(), e.role.is_some()))
                .collect::<Vec<_>>(),
            vec![("Search", true), ("Results for carbonara", false)]
        );
    }

    #[test]
    fn merge_with_no_ax_elements_is_the_ocr_result_unchanged() {
        let ocr = vec![el("plain text", 0, 0, 50, 10, None)];
        assert_eq!(merge_ax_and_ocr(Vec::new(), ocr.clone()), ocr);
    }

    #[test]
    fn role_serializes_only_when_present() {
        let with = serde_json::to_value(el("Go", 0, 0, 10, 10, Some("AXLink"))).unwrap();
        assert_eq!(with["role"], "AXLink");
        let without = serde_json::to_value(el("Go", 0, 0, 10, 10, None)).unwrap();
        assert!(
            without.get("role").is_none(),
            "None role must not appear on the wire"
        );
    }

    /// Minimal in-memory backend proving the trait is implementable and
    /// object-safe — the same shape the T03 tool tests will use.
    struct MockScreenQuery {
        fail_with: Option<ScreenQueryError>,
    }

    #[async_trait]
    impl ScreenQuery for MockScreenQuery {
        async fn query(&self) -> Result<Vec<ScreenElement>, ScreenQueryError> {
            if let Some(err) = &self.fail_with {
                return Err(err.clone());
            }
            Ok(vec![ScreenElement {
                text: "OK".to_string(),
                x: 10,
                y: 20,
                width: 30,
                height: 40,
                cx: 0,
                cy: 0,
                app: Some("Finder".to_string()),
                role: None,
            }])
        }
    }

    #[tokio::test]
    async fn trait_is_object_safe_and_queries_through_dyn() {
        let backend: Arc<dyn ScreenQuery> = Arc::new(MockScreenQuery { fail_with: None });
        let elements = backend.query().await.unwrap();
        assert_eq!(elements.len(), 1);
        assert_eq!(elements[0].x, 10);
        assert_eq!(elements[0].y, 20);
    }

    #[tokio::test]
    async fn errors_propagate_through_dyn_with_kind() {
        let backend: Arc<dyn ScreenQuery> = Arc::new(MockScreenQuery {
            fail_with: Some(ScreenQueryError::RecognitionFailed {
                detail: "vision failed".into(),
            }),
        });
        let err = backend.query().await.unwrap_err();
        assert_eq!(err.kind(), "recognition-failed");
    }

    #[test]
    fn element_json_shape_is_camel_case() {
        let el = ScreenElement {
            text: "hi".into(),
            x: 1,
            y: 2,
            width: 3,
            height: 4,
            cx: 0,
            cy: 0,
            app: Some("Zed".into()),
            role: None,
        };
        let v = serde_json::to_value(&el).unwrap();
        assert_eq!(v["text"], "hi");
        assert_eq!(v["x"], 1);
        assert_eq!(v["y"], 2);
        assert_eq!(v["width"], 3);
        assert_eq!(v["height"], 4);
        assert_eq!(v["app"], "Zed");

        // A None app serializes as JSON null — the model reads "unattributed".
        let bare = ScreenElement {
            text: "x".into(),
            x: 0,
            y: 0,
            width: 1,
            height: 1,
            cx: 0,
            cy: 0,
            app: None,
            role: None,
        };
        assert!(serde_json::to_value(&bare).unwrap()["app"].is_null());
    }

    #[test]
    fn error_json_shape_is_the_ipc_contract() {
        // The tool surfaces match on `kind` and read camelCase fields; a change
        // here is a breaking IPC change.
        let denied = ScreenQueryError::PermissionDenied {
            detail: "TCC denied".into(),
        };
        let v = serde_json::to_value(&denied).unwrap();
        assert_eq!(v["kind"], "permission-denied");
        assert_eq!(v["detail"], "TCC denied");

        let recognition = ScreenQueryError::RecognitionFailed {
            detail: "vision error".into(),
        };
        let v = serde_json::to_value(&recognition).unwrap();
        assert_eq!(v["kind"], "recognition-failed");
        assert_eq!(v["detail"], "vision error");

        let unsupported = ScreenQueryError::Unsupported {
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
            ScreenQueryError::PermissionDenied {
                detail: String::new(),
            },
            ScreenQueryError::RecognitionFailed {
                detail: String::new(),
            },
            ScreenQueryError::Unsupported {
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
    fn ocr_permission_error_survives_the_mapping() {
        let err: ScreenQueryError = OcrError::PermissionDenied {
            detail: "TCC denied".into(),
        }
        .into();
        assert_eq!(err.kind(), "permission-denied");
        assert_eq!(
            err,
            ScreenQueryError::PermissionDenied {
                detail: "TCC denied".into()
            }
        );
    }

    #[test]
    fn other_ocr_errors_collapse_to_recognition_failed_with_kind_in_detail() {
        let cases: Vec<(OcrError, &str)> = vec![
            (
                OcrError::CaptureFailed {
                    detail: "no display".into(),
                },
                "capture-failed",
            ),
            (
                OcrError::RecognitionFailed {
                    detail: "vision".into(),
                },
                "recognition-failed",
            ),
            (OcrError::unsupported_here(), "unsupported"),
        ];
        for (ocr_err, original_kind) in cases {
            let err: ScreenQueryError = ocr_err.into();
            assert_eq!(err.kind(), "recognition-failed");
            match err {
                ScreenQueryError::RecognitionFailed { detail } => {
                    assert!(
                        detail.contains(original_kind),
                        "original kind {original_kind} lost in detail: {detail}"
                    );
                }
                other => panic!("expected recognition-failed, got {other:?}"),
            }
        }
    }

    #[test]
    fn error_display_names_kind_and_detail() {
        let err = ScreenQueryError::RecognitionFailed {
            detail: "vision gave up".into(),
        };
        let msg = err.to_string();
        assert!(msg.contains("recognition-failed"), "kind missing: {msg}");
        assert!(msg.contains("vision gave up"), "detail missing: {msg}");
    }

    #[test]
    fn unsupported_here_names_this_platform() {
        let err = ScreenQueryError::unsupported_here();
        assert_eq!(err.kind(), "unsupported");
        match err {
            ScreenQueryError::Unsupported { platform, .. } => {
                assert_eq!(platform, std::env::consts::OS);
            }
            other => panic!("expected unsupported, got {other:?}"),
        }
    }
}
