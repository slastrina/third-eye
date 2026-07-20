//! Live macOS screen-query backend: capture the primary display, recognize its
//! on-screen text with bounding boxes via Apple Vision, and return the elements
//! in absolute top-left screen pixels.
//!
//! The whole capture→Vision→convert chain lives in
//! [`crate::ocr::macos::extract_elements_blocking`] (the R011 extract-and-drop
//! sibling of the text-only OCR path); this backend only lifts it onto the
//! async [`ScreenQuery`] seam via `spawn_blocking` and maps
//! [`crate::ocr::OcrError`] → [`ScreenQueryError`] and
//! [`crate::ocr::macos::TextElement`] → [`ScreenElement`]. No pixel type ever
//! crosses the seam.

use async_trait::async_trait;

use super::{ScreenElement, ScreenQuery, ScreenQueryError};
use crate::ocr::macos::{extract_elements_blocking, TextElement};

/// The live macOS backend: one capture of the primary display per `query`,
/// recognized with Apple Vision and returned with per-element pixel boxes.
pub struct MacosScreenQuery {
    /// Longest-edge pixel cap passed to the shared capture stage; constructed
    /// with [`crate::ocr::OCR_MAX_DIMENSION`].
    max_dimension: u32,
}

impl MacosScreenQuery {
    pub fn new(max_dimension: u32) -> Self {
        Self { max_dimension }
    }
}

impl From<TextElement> for ScreenElement {
    fn from(el: TextElement) -> Self {
        ScreenElement {
            text: el.text,
            x: el.x,
            y: el.y,
            width: el.width,
            height: el.height,
            app: el.app,
        }
    }
}

#[async_trait]
impl ScreenQuery for MacosScreenQuery {
    async fn query(&self) -> Result<Vec<ScreenElement>, ScreenQueryError> {
        let max_dimension = self.max_dimension;
        // Capture blocks on Swift completion handlers and Vision recognition is
        // CPU-heavy — both stay off the async runtime (OCR precedent).
        let elements = tokio::task::spawn_blocking(move || extract_elements_blocking(max_dimension))
            .await
            .map_err(|e| ScreenQueryError::RecognitionFailed {
                detail: format!("screen-query task panicked: {e}"),
            })??;
        Ok(elements.into_iter().map(ScreenElement::from).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn text_element_maps_to_screen_element_verbatim() {
        let te =
            TextElement { text: "hi".into(), x: 1, y: 2, width: 3, height: 4, app: Some("Zed".into()) };
        let se: ScreenElement = te.into();
        assert_eq!(
            se,
            ScreenElement { text: "hi".into(), x: 1, y: 2, width: 3, height: 4, app: Some("Zed".into()) }
        );
    }

    /// Live run of the full backend against the real screen (MEM038 precedent)
    /// — needs Screen Recording, ignored in the default suite. Without
    /// permission it must fail *typed*, never panic.
    #[tokio::test]
    #[ignore = "requires Screen Recording permission and a live display (slice UAT)"]
    async fn real_screen_query_smoke() {
        let backend: Arc<dyn ScreenQuery> =
            Arc::new(MacosScreenQuery::new(crate::ocr::OCR_MAX_DIMENSION));
        match backend.query().await {
            Ok(elements) => {
                println!("screen_query: {} element(s) from the live screen:", elements.len());
                for el in &elements {
                    println!("  {:?} @ ({},{}) {}x{}", el.text, el.x, el.y, el.width, el.height);
                }
            }
            Err(err) => {
                assert_eq!(err.kind(), "permission-denied", "unexpected: {err}");
            }
        }
    }
}
