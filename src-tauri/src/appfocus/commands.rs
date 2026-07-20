//! Managed app-focus state: the [`AppFocusState`] holder the composite
//! executor's `FocusAppTool` (M005) draws its backend from.
//!
//! This is the app-focus twin of [`crate::screenquery::commands::ScreenQueryState`]:
//! it ships only the managed-state holder and the platform cfg-select
//! ([`AppFocusState::with_platform_backend`]) so the tool can mount against a
//! real backend. No Tauri IPC commands live here — the tool reaches app
//! activation through the composite executor path, not a standalone command.

use std::sync::Arc;

use super::AppFocus;

/// Managed app-focus state: the platform backend behind the [`AppFocus`] seam,
/// so the composite executor (and tests) never name a concrete backend.
pub struct AppFocusState {
    backend: Arc<dyn AppFocus>,
}

impl AppFocusState {
    pub fn new(backend: Arc<dyn AppFocus>) -> Self {
        Self { backend }
    }

    /// State bound to this platform's live backend: the NSWorkspace-backed
    /// [`super::macos::MacosAppFocus`] on macOS, the typed-unsupported
    /// [`super::fallback::FallbackAppFocus`] everywhere else. Mirrors
    /// [`crate::screenquery::commands::ScreenQueryState::with_platform_backend`].
    pub fn with_platform_backend() -> Self {
        #[cfg(target_os = "macos")]
        {
            Self::new(Arc::new(super::macos::MacosAppFocus))
        }
        #[cfg(not(target_os = "macos"))]
        {
            Self::new(Arc::new(super::fallback::FallbackAppFocus))
        }
    }

    /// The backend handle for the composite executor's `FocusAppTool`: a cheap
    /// `Arc` clone so the tool can activate without holding a borrow on managed
    /// state across an `.await`.
    pub fn backend(&self) -> Arc<dyn AppFocus> {
        self.backend.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::appfocus::{AppFocusError, FocusedApp};
    use async_trait::async_trait;

    /// Minimal scriptable backend: returns a fixed match so delegation through
    /// the managed state can be asserted without touching any real app.
    struct ScriptedAppFocus;

    #[async_trait]
    impl AppFocus for ScriptedAppFocus {
        async fn focus(&self, _app_name: &str) -> Result<FocusedApp, AppFocusError> {
            Ok(FocusedApp { app: "TARGET".into() })
        }

        async fn running_apps(&self) -> Vec<String> {
            vec!["TARGET".into()]
        }
    }

    #[tokio::test]
    async fn backend_handle_reaches_the_same_backend() {
        let state = AppFocusState::new(Arc::new(ScriptedAppFocus));
        // The Arc the executor will hold must dispatch to the state's backend.
        let focused = state.backend().focus("target").await.unwrap();
        assert_eq!(focused.app, "TARGET");
        assert_eq!(state.backend().running_apps().await, vec!["TARGET"]);
    }

    #[tokio::test]
    async fn platform_backend_binding_matches_this_os() {
        // On macOS the live backend activates the real workspace (matching
        // against the actual running apps); off macOS the fallback returns typed
        // unsupported. Assert the cfg-select wiring, not a live activation.
        let state = AppFocusState::with_platform_backend();
        let result = state.backend().focus("no-such-app-xyz").await;
        if cfg!(target_os = "macos") {
            // No such app is running → not-found (never unsupported on macOS).
            if let Err(err) = result {
                assert_ne!(err.kind(), "unsupported", "macOS bound the fallback: {err}");
            }
        } else {
            let err = result.expect_err("off-macOS must be unsupported");
            assert_eq!(err.kind(), "unsupported");
        }
    }
}
