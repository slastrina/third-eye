//! Managed screen-query state: the [`ScreenQueryState`] holder the composite
//! executor's `ScreenQueryTool` (S02/T03) draws its backend from.
//!
//! This is the screen-query twin of [`crate::input::commands::InputState`]:
//! it ships only the managed-state holder and the platform cfg-select
//! ([`ScreenQueryState::with_platform_backend`]) so T03 can mount the tool
//! against a real backend. No Tauri IPC commands live here — the tool reaches
//! the screen through the composite executor path, not a standalone command.

use std::sync::Arc;

use super::ScreenQuery;

/// Managed screen-query state: the platform backend behind the [`ScreenQuery`]
/// seam, so the composite executor (and tests) never name a concrete backend.
pub struct ScreenQueryState {
    backend: Arc<dyn ScreenQuery>,
}

impl ScreenQueryState {
    pub fn new(backend: Arc<dyn ScreenQuery>) -> Self {
        Self { backend }
    }

    /// State bound to this platform's live backend: the Vision-backed
    /// [`super::macos::MacosScreenQuery`] on macOS, the typed-unsupported
    /// [`super::fallback::FallbackScreenQuery`] everywhere else. Mirrors
    /// [`crate::input::commands::InputState::with_platform_backend`].
    pub fn with_platform_backend() -> Self {
        #[cfg(target_os = "macos")]
        {
            Self::new(Arc::new(super::macos::MacosScreenQuery::new(
                crate::ocr::OCR_MAX_DIMENSION,
            )))
        }
        #[cfg(not(target_os = "macos"))]
        {
            Self::new(Arc::new(super::fallback::FallbackScreenQuery))
        }
    }

    /// The backend handle for the composite executor's `ScreenQueryTool` (T03):
    /// a cheap `Arc` clone so the tool can query without holding a borrow on
    /// managed state across an `.await`.
    pub fn backend(&self) -> Arc<dyn ScreenQuery> {
        self.backend.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::screenquery::{ScreenElement, ScreenQueryError};
    use async_trait::async_trait;

    /// Minimal scriptable backend: returns a fixed element so delegation
    /// through the managed state can be asserted without touching the screen.
    struct ScriptedScreenQuery;

    #[async_trait]
    impl ScreenQuery for ScriptedScreenQuery {
        async fn query(&self) -> Result<Vec<ScreenElement>, ScreenQueryError> {
            Ok(vec![ScreenElement {
                text: "TARGET".into(),
                x: 5,
                y: 6,
                width: 7,
                height: 8,
                app: None,
            }])
        }
    }

    #[tokio::test]
    async fn backend_handle_reaches_the_same_backend() {
        let state = ScreenQueryState::new(Arc::new(ScriptedScreenQuery));
        // The Arc the executor will hold must dispatch to the state's backend.
        let elements = state.backend().query().await.unwrap();
        assert_eq!(elements.len(), 1);
        assert_eq!(elements[0].text, "TARGET");
        assert_eq!((elements[0].x, elements[0].y), (5, 6));
    }

    #[tokio::test]
    async fn platform_backend_binding_matches_this_os() {
        // On macOS the live backend queries the real screen (and without
        // permission fails typed); off macOS the fallback returns typed
        // unsupported. Assert the cfg-select wiring, not live permission.
        let state = ScreenQueryState::with_platform_backend();
        let result = state.backend().query().await;
        if cfg!(target_os = "macos") {
            // May succeed (permission granted) or fail typed — never unsupported.
            if let Err(err) = result {
                assert_ne!(err.kind(), "unsupported", "macOS bound the fallback: {err}");
            }
        } else {
            let err = result.expect_err("off-macOS must be unsupported");
            assert_eq!(err.kind(), "unsupported");
        }
    }
}
