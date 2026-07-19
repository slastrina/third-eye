//! Non-macOS OCR backend: every extract resolves to a typed `unsupported`
//! error, so Windows/Linux builds compile clean and the watcher can show a
//! self-explanatory status instead of failing silently (R007/R020).
//!
//! Compiled on every platform (it has no platform dependencies) so its
//! contract is unit-tested even on macOS; it is only *bound* as the live
//! backend where no real one exists.

use async_trait::async_trait;

use super::{OcrEngine, OcrError};

/// Trait binding for platforms without an OCR backend.
pub struct FallbackOcr;

#[async_trait]
impl OcrEngine for FallbackOcr {
    async fn extract(&self) -> Result<Vec<String>, OcrError> {
        let err = OcrError::unsupported_here();
        log::error!("ocr: {} ({err})", err.kind());
        Err(err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[tokio::test]
    async fn fallback_extract_returns_typed_unsupported() {
        let engine: Arc<dyn OcrEngine> = Arc::new(FallbackOcr);
        let err = engine.extract().await.unwrap_err();
        assert_eq!(err.kind(), "unsupported");
        let v = serde_json::to_value(&err).unwrap();
        assert_eq!(v["kind"], "unsupported");
        assert_eq!(v["platform"], std::env::consts::OS);
    }
}
