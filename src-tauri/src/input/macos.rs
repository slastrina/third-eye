//! macOS HID input backend: Accessibility permission (TCC) via raw
//! `AXIsProcessTrusted` FFI, event synthesis via enigo.
//!
//! Two deliberate constraints shape this file, both mirroring
//! [`crate::capture::macos`]:
//!
//! - **Health-as-value permission.** enigo does not expose a read-only
//!   Accessibility check — `Enigo::new` *prompts* when permission is missing,
//!   which would break the invariant that querying permission never triggers
//!   the OS prompt ([`crate::input`] module docs). So [`has_permission`] wraps
//!   `AXIsProcessTrusted()` (read-only, never prompts, any thread), exactly as
//!   capture wraps `CGPreflightScreenCaptureAccess`. Only [`request_permission`]
//!   constructs enigo with a prompting `Settings`.
//!
//! - **enigo is `!Send`/`!Sync` on macOS** (enigo#96: it holds a
//!   `CGEventSource`, and its keyboard path wants TIS/TSM off a real thread).
//!   [`InputControl`] requires `Send + Sync` so it can live in `Arc<dyn>`
//!   managed state. Resolution: [`MacosInput`] is a trivial ZST; a live `Enigo`
//!   is *never* stored — it is constructed fresh per action inside
//!   `tokio::task::spawn_blocking` and dropped before the closure returns, so
//!   the `!Send` handle never crosses an `.await`. This is the same escape
//!   hatch capture uses for blocking ScreenCaptureKit calls.

use async_trait::async_trait;
use enigo::{
    Button, Coordinate, Direction, Enigo, Key, Keyboard, Mouse, Settings,
};

use super::{InputAction, InputControl, InputError, InputPermission, MouseButton};

// Raw FFI instead of a binding crate: `AXIsProcessTrusted` is a stable,
// ABI-simple Accessibility (HIServices) call, and the pinned-dependency policy
// favors no new crates for it — the same call the capture layer makes for
// CoreGraphics (see capture/macos.rs:33-35). It lives in the ApplicationServices
// umbrella framework. `bool` is ABI-compatible with C `_Bool`.
#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXIsProcessTrusted() -> bool;
}

/// Whether Accessibility (TCC) permission is currently granted. Read-only:
/// never triggers the system prompt, safe on any thread.
pub fn has_permission() -> bool {
    // Safety: no arguments, no pointers; reads TCC state and returns a bool.
    unsafe { AXIsProcessTrusted() }
}

/// Show the Accessibility permission prompt (or the Settings round-trip macOS
/// falls back to after a denial) and return the resulting granted state.
///
/// enigo has no standalone "prompt" call, so this constructs a throwaway
/// `Enigo` with `open_prompt_to_get_permissions: true` — the only place in the
/// crate that is allowed to prompt — then reads the granted state back via the
/// read-only FFI. The `Enigo` is dropped immediately; nothing is stored.
pub fn request_permission() -> bool {
    let settings = Settings { open_prompt_to_get_permissions: true, ..Settings::default() };
    // Constructing Enigo is what surfaces the TCC prompt; we don't need the
    // handle afterwards. If construction fails we still report the real
    // Accessibility state rather than assuming denial.
    match Enigo::new(&settings) {
        Ok(_enigo) => {}
        Err(e) => log::warn!("input: permission prompt construction failed: {e}"),
    }
    let granted = has_permission();
    log::info!("input: permission requested, granted={granted}");
    granted
}

/// Current permission state as the IPC health-as-value shape.
pub fn permission_status() -> InputPermission {
    InputPermission { granted: has_permission(), supported: true }
}

/// The live macOS backend: synthesizes mouse/keyboard events via enigo. A ZST
/// (`Send + Sync`) — every action builds its own transient `Enigo` inside
/// `spawn_blocking`, so no `!Send` state is ever held.
pub struct MacosInput;

#[async_trait]
impl InputControl for MacosInput {
    fn permission(&self) -> InputPermission {
        permission_status()
    }

    fn request_permission(&self) -> bool {
        request_permission()
    }

    async fn perform(&self, action: InputAction) -> Result<(), InputError> {
        // Read-only preflight (never prompts): give the typed permission error
        // the walkthrough keys on instead of enigo prompting mid-action (R007).
        if !has_permission() {
            let err = InputError::PermissionDenied {
                detail: "AXIsProcessTrusted returned false".into(),
            };
            log::error!("input: {} ({err})", err.kind());
            return Err(err);
        }

        // enigo is !Send: construct it fresh inside the blocking closure so the
        // live handle never crosses this await. spawn_blocking runs off the
        // async runtime, so a hung event post can never stall the IPC thread.
        let result = tokio::task::spawn_blocking(move || perform_blocking(action))
            .await
            .map_err(|e| InputError::InputFailed {
                detail: format!("input task panicked: {e}"),
            })?;

        if let Err(err) = &result {
            log::error!("input: {} ({err})", err.kind());
        }
        result
    }
}

/// The per-action blocking stage: build a throwaway `Enigo`, synthesize the one
/// action, drop the handle. Runs on a `spawn_blocking` thread. Every enigo
/// failure (construction or event post) collapses onto `InputFailed` — the
/// permission-denied case is already handled by the caller's preflight, and a
/// construction failure here after a passing preflight is a genuine synthesis
/// fault, not a permission one.
fn perform_blocking(action: InputAction) -> Result<(), InputError> {
    // Don't prompt on the action path — permission was already verified, and a
    // prompt here would violate health-as-value.
    let settings = Settings { open_prompt_to_get_permissions: false, ..Settings::default() };
    let mut enigo = Enigo::new(&settings)
        .map_err(|e| InputError::InputFailed { detail: format!("enigo init failed: {e}") })?;

    match action {
        InputAction::MouseMove { x, y } => enigo
            .move_mouse(x, y, Coordinate::Abs)
            .map_err(|e| InputError::InputFailed { detail: format!("move_mouse failed: {e}") }),
        InputAction::MouseClick { button } => enigo
            .button(map_button(button), Direction::Click)
            .map_err(|e| InputError::InputFailed { detail: format!("button click failed: {e}") }),
        InputAction::TypeText { text } => enigo
            .text(&text)
            .map_err(|e| InputError::InputFailed { detail: format!("text entry failed: {e}") }),
        InputAction::KeyPress { key } => {
            let k = key_from_str(&key)?;
            enigo
                .key(k, Direction::Click)
                .map_err(|e| InputError::InputFailed { detail: format!("key press failed: {e}") })
        }
    }
}

/// Map the crate's wire button onto enigo's.
fn map_button(button: MouseButton) -> Button {
    match button {
        MouseButton::Left => Button::Left,
        MouseButton::Right => Button::Right,
        MouseButton::Middle => Button::Middle,
    }
}

/// Resolve a `key` string from the wire contract to an enigo [`Key`]. Named keys
/// (case-insensitive) map to the corresponding special key; a single-character
/// string maps to `Key::Unicode`. Anything else is a typed `input-failed` so the
/// model gets an actionable error instead of a silent no-op (R007).
fn key_from_str(key: &str) -> Result<Key, InputError> {
    let named = match key.to_ascii_lowercase().as_str() {
        "return" | "enter" => Some(Key::Return),
        "tab" => Some(Key::Tab),
        "space" => Some(Key::Space),
        "escape" | "esc" => Some(Key::Escape),
        "backspace" => Some(Key::Backspace),
        "delete" | "del" => Some(Key::Delete),
        "up" | "uparrow" => Some(Key::UpArrow),
        "down" | "downarrow" => Some(Key::DownArrow),
        "left" | "leftarrow" => Some(Key::LeftArrow),
        "right" | "rightarrow" => Some(Key::RightArrow),
        _ => None,
    };
    if let Some(k) = named {
        return Ok(k);
    }

    let mut chars = key.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) => Ok(Key::Unicode(c)),
        _ => Err(InputError::InputFailed {
            detail: format!("unrecognized key: {key:?} (expected a named key or single character)"),
        }),
    }
}

// Keeps the trait bound explicit: managed state and the composite executor hold
// Arc<dyn InputControl>, so the backend must stay object-safe + Send + Sync.
#[allow(dead_code)]
fn _assert_backend_is_dyn_compatible() -> std::sync::Arc<dyn InputControl> {
    std::sync::Arc::new(MacosInput)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn preflight_is_side_effect_free_and_stable() {
        // AXIsProcessTrusted never prompts, so calling it twice in a test is
        // safe and must agree with the status shape.
        let first = has_permission();
        let second = has_permission();
        assert_eq!(first, second);
        let status = permission_status();
        assert!(status.supported);
        assert_eq!(status.granted, first);
    }

    #[test]
    fn permission_through_dyn_matches_free_function() {
        let backend: Arc<dyn InputControl> = Arc::new(MacosInput);
        assert_eq!(backend.permission(), permission_status());
        // supported is unconditionally true on macOS — the backend exists.
        assert!(backend.permission().supported);
    }

    #[test]
    fn named_keys_resolve_case_insensitively() {
        assert_eq!(key_from_str("return").unwrap(), Key::Return);
        assert_eq!(key_from_str("ENTER").unwrap(), Key::Return);
        assert_eq!(key_from_str("Tab").unwrap(), Key::Tab);
        assert_eq!(key_from_str("escape").unwrap(), Key::Escape);
        assert_eq!(key_from_str("esc").unwrap(), Key::Escape);
        assert_eq!(key_from_str("space").unwrap(), Key::Space);
        assert_eq!(key_from_str("backspace").unwrap(), Key::Backspace);
        assert_eq!(key_from_str("delete").unwrap(), Key::Delete);
        assert_eq!(key_from_str("up").unwrap(), Key::UpArrow);
        assert_eq!(key_from_str("Down").unwrap(), Key::DownArrow);
        assert_eq!(key_from_str("LEFT").unwrap(), Key::LeftArrow);
        assert_eq!(key_from_str("right").unwrap(), Key::RightArrow);
    }

    #[test]
    fn single_character_maps_to_unicode() {
        assert_eq!(key_from_str("a").unwrap(), Key::Unicode('a'));
        assert_eq!(key_from_str("Z").unwrap(), Key::Unicode('Z'));
        assert_eq!(key_from_str("é").unwrap(), Key::Unicode('é'));
    }

    #[test]
    fn empty_or_unknown_multichar_key_is_typed_input_failed() {
        for bad in ["", "notarealkey", "f13x"] {
            let err = key_from_str(bad).unwrap_err();
            assert_eq!(err.kind(), "input-failed", "wrong kind for {bad:?}");
        }
    }

    #[test]
    fn buttons_map_one_to_one() {
        assert_eq!(map_button(MouseButton::Left), Button::Left);
        assert_eq!(map_button(MouseButton::Right), Button::Right);
        assert_eq!(map_button(MouseButton::Middle), Button::Middle);
    }

    /// Live event synthesis through the full trait surface. Needs Accessibility
    /// permission (and moves the real cursor), so it is ignored in the default
    /// suite (slice UAT runs it): `cargo test -- --ignored real_input_smoke`.
    /// Without permission it must still fail *typed* — never a panic or hang.
    #[tokio::test]
    #[ignore = "requires Accessibility permission and synthesizes real input (slice UAT)"]
    async fn real_input_smoke() {
        let backend: Arc<dyn InputControl> = Arc::new(MacosInput);
        // A mouse move is the least disruptive proof that the spawn_blocking →
        // enigo path round-trips without a !Send compile error or a runtime hang.
        match backend.perform(InputAction::MouseMove { x: 200, y: 200 }).await {
            Ok(()) => {
                assert!(backend.permission().granted, "input succeeded but permission reads false");
            }
            Err(err) => {
                // In an unpermitted environment the only acceptable outcome is
                // the typed permission error the walkthrough keys on.
                assert_eq!(err.kind(), "permission-denied", "unexpected: {err}");
                assert!(!backend.permission().granted);
            }
        }
    }
}
