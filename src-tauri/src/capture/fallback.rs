//! Non-macOS capture backend: every operation resolves to a typed value or
//! `unsupported` error, so Windows/Linux builds compile clean and the UI can
//! hide the attach affordance instead of failing silently (R007).
//!
//! Compiled on every platform (it has no platform dependencies) so its
//! contract is unit-tested even on macOS; it is only *bound* as the live
//! backend where no real one exists.

use async_trait::async_trait;

use super::{CaptureError, CapturePermission, CapturedFrame, ScreenCapture};

/// Trait binding for platforms without a capture backend.
pub struct FallbackCapture;

#[async_trait]
impl ScreenCapture for FallbackCapture {
    fn permission(&self) -> CapturePermission {
        CapturePermission {
            granted: false,
            supported: false,
        }
    }

    fn request_permission(&self) -> bool {
        false
    }

    async fn capture_primary(&self) -> Result<CapturedFrame, CaptureError> {
        let err = CaptureError::unsupported_here();
        log::error!("capture: {} ({err})", err.kind());
        Err(err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[tokio::test]
    async fn fallback_capture_returns_typed_unsupported() {
        let backend: Arc<dyn ScreenCapture> = Arc::new(FallbackCapture);
        let err = backend.capture_primary().await.unwrap_err();
        assert_eq!(err.kind(), "unsupported");
        let v = serde_json::to_value(&err).unwrap();
        assert_eq!(v["kind"], "unsupported");
        assert_eq!(v["platform"], std::env::consts::OS);
    }

    #[test]
    fn fallback_permission_is_unsupported_value_not_error() {
        let backend = FallbackCapture;
        assert_eq!(
            backend.permission(),
            CapturePermission {
                granted: false,
                supported: false
            }
        );
        assert!(!backend.request_permission());
    }
}
