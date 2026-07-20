//! Non-macOS screen-query backend: every query resolves to a typed
//! `unsupported` error, so Windows/Linux builds compile clean (R020) and the
//! tool surface names the failure class instead of failing silently (R007).
//!
//! Compiled on every platform (it has no platform dependencies) so its contract
//! is unit-tested even on macOS; it is only *bound* as the live backend where
//! no real one exists. Mirrors [`crate::input::fallback::FallbackInput`].

use async_trait::async_trait;

use super::{ScreenElement, ScreenQuery, ScreenQueryError};

/// Trait binding for platforms without a screen-query backend.
pub struct FallbackScreenQuery;

#[async_trait]
impl ScreenQuery for FallbackScreenQuery {
    async fn query(&self) -> Result<Vec<ScreenElement>, ScreenQueryError> {
        let err = ScreenQueryError::unsupported_here();
        log::error!("screen_query: {} ({err})", err.kind());
        Err(err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[tokio::test]
    async fn fallback_screen_query_returns_typed_unsupported() {
        let backend: Arc<dyn ScreenQuery> = Arc::new(FallbackScreenQuery);
        let err = backend.query().await.unwrap_err();
        assert_eq!(err.kind(), "unsupported");
        let v = serde_json::to_value(&err).unwrap();
        assert_eq!(v["kind"], "unsupported");
        assert_eq!(v["platform"], std::env::consts::OS);
    }
}
