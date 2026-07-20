//! Non-macOS app-focus backend: every focus resolves to a typed `unsupported`
//! error and the running-apps roster is empty, so Windows/Linux builds compile
//! clean (R020) and the tool surface names the failure class instead of failing
//! silently (R007).
//!
//! Compiled on every platform (it has no platform dependencies) so its contract
//! is unit-tested even on macOS; it is only *bound* as the live backend where no
//! real one exists. Mirrors [`crate::screenquery::fallback::FallbackScreenQuery`].

use async_trait::async_trait;

use super::{AppFocus, AppFocusError, FocusedApp};

/// Trait binding for platforms without an app-focus backend.
pub struct FallbackAppFocus;

#[async_trait]
impl AppFocus for FallbackAppFocus {
    async fn focus(&self, _app_name: &str) -> Result<FocusedApp, AppFocusError> {
        let err = AppFocusError::unsupported_here();
        log::error!("focus_app: {} ({err})", err.kind());
        Err(err)
    }

    async fn running_apps(&self) -> Vec<String> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[tokio::test]
    async fn fallback_app_focus_returns_typed_unsupported() {
        let backend: Arc<dyn AppFocus> = Arc::new(FallbackAppFocus);
        let err = backend.focus("Google Chrome").await.unwrap_err();
        assert_eq!(err.kind(), "unsupported");
        let v = serde_json::to_value(&err).unwrap();
        assert_eq!(v["kind"], "unsupported");
        assert_eq!(v["platform"], std::env::consts::OS);
    }

    #[tokio::test]
    async fn fallback_running_apps_is_empty() {
        let backend: Arc<dyn AppFocus> = Arc::new(FallbackAppFocus);
        assert!(backend.running_apps().await.is_empty());
    }
}
