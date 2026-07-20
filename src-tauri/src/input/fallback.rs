//! Non-macOS HID input backend: every action resolves to a typed `unsupported`
//! error and permission is the `{ granted:false, supported:false }` value, so
//! Windows/Linux builds compile clean (R020) and the UI hides the arming
//! affordance instead of failing silently (R007).
//!
//! Compiled on every platform (it has no platform dependencies) so its contract
//! is unit-tested even on macOS; it is only *bound* as the live backend where no
//! real one exists. Mirrors [`crate::capture::fallback::FallbackCapture`].

use async_trait::async_trait;

use super::{InputAction, InputControl, InputError, InputPermission};

/// Trait binding for platforms without a HID input backend.
pub struct FallbackInput;

#[async_trait]
impl InputControl for FallbackInput {
    fn permission(&self) -> InputPermission {
        InputPermission { granted: false, supported: false }
    }

    fn request_permission(&self) -> bool {
        false
    }

    async fn perform(&self, _action: InputAction) -> Result<(), InputError> {
        let err = InputError::unsupported_here();
        log::error!("input: {} ({err})", err.kind());
        Err(err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::MouseButton;
    use std::sync::Arc;

    #[tokio::test]
    async fn fallback_input_returns_typed_unsupported_for_every_action() {
        let backend: Arc<dyn InputControl> = Arc::new(FallbackInput);
        let actions = [
            InputAction::MouseMove { x: 1, y: 2 },
            InputAction::click(MouseButton::Left),
            InputAction::click_at(MouseButton::Left, 5, 6),
            InputAction::TypeText { text: "hi".into() },
            InputAction::KeyPress { key: "return".into() },
        ];
        for action in actions {
            let err = backend.perform(action.clone()).await.unwrap_err();
            assert_eq!(err.kind(), "unsupported", "wrong kind for {action:?}");
            let v = serde_json::to_value(&err).unwrap();
            assert_eq!(v["kind"], "unsupported");
            assert_eq!(v["platform"], std::env::consts::OS);
        }
    }

    #[test]
    fn fallback_permission_is_unsupported_value_not_error() {
        let backend = FallbackInput;
        assert_eq!(backend.permission(), InputPermission { granted: false, supported: false });
        assert!(!backend.request_permission());
    }
}
