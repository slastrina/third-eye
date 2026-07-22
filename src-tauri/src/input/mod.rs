//! HID input boundary: the trait seam behind "let the model drive the machine".
//!
//! [`InputControl`] is the abstraction the input tool ([`crate::llm`]) and the
//! S01 composite executor call — nothing outside this module may talk to enigo
//! or the Accessibility APIs directly. R007 (failure visibility) is enforced
//! structurally: every failure a backend can hit maps to a typed [`InputError`]
//! variant, serialized with the same kind-tagged camelCase JSON contract as
//! [`crate::capture::CaptureError`] and [`crate::llm::LlmError`], so the UI can
//! always show a guided walkthrough instead of a silent no-op.
//!
//! Permission state is health-as-value ([`InputPermission`]): querying it never
//! errors and never triggers the OS Accessibility prompt — only
//! `request_permission` does. Accessibility is a *separate* TCC entitlement from
//! Screen Recording; a machine granted capture is not automatically granted HID.
//!
//! Platform binding mirrors [`crate::capture`]: macOS gets the real backend
//! (enigo synthesized events, AXIsProcessTrusted permission read); every other
//! OS gets a fallback that returns typed `unsupported` errors so Windows/Linux
//! builds stay clean (R020). Backends are added in later S01 tasks; this module
//! ships the object-safe seam and the error taxonomy everything downstream
//! depends on.

pub mod commands;
pub mod fallback;
#[cfg(target_os = "macos")]
pub mod macos;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// A single HID action the model can request. Serialized with an `action` tag
/// (kebab-case) and camelCase fields so it can ride the tool-call JSON contract
/// directly (S01's `input_action` tool discriminates on this tag). One tagged
/// enum keeps the composite executor's dispatch-by-name simple and the model's
/// tool list short.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "kebab-case", rename_all_fields = "camelCase")]
pub enum InputAction {
    /// Move the mouse cursor to an absolute screen coordinate.
    MouseMove { x: i32, y: i32 },
    /// Synthesize a mouse button click. When `x`/`y` are present the cursor is
    /// moved to that absolute screen coordinate *then* clicked (the model's
    /// natural "click at (x,y)" — one action, grounded on a `screen_query`
    /// coordinate); when absent the click fires at the current cursor position.
    /// Both must be present together; one without the other is a malformed
    /// action the tool rejects.
    MouseClick {
        button: MouseButton,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        x: Option<i32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        y: Option<i32>,
    },
    /// Type a run of Unicode text as keystrokes.
    TypeText { text: String },
    /// Press (and release) a single named key — e.g. `return`, `tab`, `escape`,
    /// or a one-character string for a literal key.
    KeyPress { key: String },
}

impl InputAction {
    /// The [`ActionKind`] this action belongs to — the granularity the S04
    /// approval whitelist grants by ("Always allow this kind"). Mirrors the
    /// serde `action` tag: a `TypeText` with different text is the same kind, so
    /// one grant covers every keystroke run without re-prompting.
    pub fn kind(&self) -> ActionKind {
        match self {
            InputAction::MouseMove { .. } => ActionKind::MouseMove,
            InputAction::MouseClick { .. } => ActionKind::MouseClick,
            InputAction::TypeText { .. } => ActionKind::TypeText,
            InputAction::KeyPress { .. } => ActionKind::KeyPress,
        }
    }

    /// The kebab-case action name for logs — the same string as the serde
    /// `action` tag (`mouse-move` / `mouse-click` / `type-text` / `key-press`).
    pub fn kind_str(&self) -> &'static str {
        match self {
            InputAction::MouseMove { .. } => "mouse-move",
            InputAction::MouseClick { .. } => "mouse-click",
            InputAction::TypeText { .. } => "type-text",
            InputAction::KeyPress { .. } => "key-press",
        }
    }

    /// A bare click at the current cursor position — the coordless
    /// [`InputAction::MouseClick`]. Keeps construction terse at call sites that
    /// don't aim (tests, the coordless fallback path).
    pub fn click(button: MouseButton) -> Self {
        InputAction::MouseClick { button, x: None, y: None }
    }

    /// A click that moves to `(x, y)` first — the coordinate-bearing
    /// [`InputAction::MouseClick`], the shape the model emits when it aims a
    /// click at a `screen_query` coordinate.
    pub fn click_at(button: MouseButton, x: i32, y: i32) -> Self {
        InputAction::MouseClick { button, x: Some(x), y: Some(y) }
    }

    /// The target coordinate a `mouse-move` or coordinate-bearing `mouse-click`
    /// aims at, if any. `None` for a bare click / type / key-press. The gate
    /// reads this to decide whether an action needs a prior `screen_query` to be
    /// grounded (the coordinate must have come from the screen, never a guess).
    pub fn aim_target(&self) -> Option<(i32, i32)> {
        match self {
            InputAction::MouseMove { x, y } => Some((*x, *y)),
            InputAction::MouseClick { x: Some(x), y: Some(y), .. } => Some((*x, *y)),
            _ => None,
        }
    }

    /// A coordinate-bearing click must carry BOTH x and y or neither — one
    /// without the other is a malformed action the tool rejects before it
    /// reaches the backend (so a half-specified aim never silently clicks at the
    /// cursor). Returns the offending field name for the error detail.
    pub fn validate(&self) -> Result<(), &'static str> {
        if let InputAction::MouseClick { x, y, .. } = self {
            match (x, y) {
                (Some(_), None) => return Err("mouse-click has x but no y"),
                (None, Some(_)) => return Err("mouse-click has y but no x"),
                _ => {}
            }
        }
        Ok(())
    }
}

/// The kind of a HID [`InputAction`], stripped of its payload — the unit the
/// S04 session whitelist grants by. `Copy + Eq + Hash` so it can live in the
/// whitelist's [`std::collections::HashSet`]. The kebab-case serde names match
/// [`InputAction`]'s `action` tag exactly, so a kind serializes the same string
/// the action would.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ActionKind {
    MouseMove,
    MouseClick,
    TypeText,
    KeyPress,
    /// Bring a running app to the front — the `focus_app` tool's action kind
    /// (M005). It has no [`InputAction`] payload variant (activation is not an
    /// enigo event), but it is HID-class: gated through the same `ApprovalGate`
    /// and grantable ("Always allow this kind") via the session whitelist.
    FocusApp,
}

/// Which mouse button an action targets. Kebab-case in JSON to match the rest
/// of the input contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MouseButton {
    Left,
    Right,
    Middle,
}

/// The full HID input failure taxonomy (R007). Serialized with a `kind` tag
/// (`disabled` / `permission-denied` / `unsupported` / `input-failed`) and
/// camelCase fields — the same IPC error contract shape as
/// [`crate::capture::CaptureError`]; the UI matches on `kind`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case", rename_all_fields = "camelCase")]
pub enum InputError {
    /// HID is disarmed: the Settings arming toggle is off, so no input is
    /// synthesized. This is the structural-inertness refusal (D038): a
    /// disarmed input action is rejected with this typed error BEFORE the
    /// [`InputControl`] backend is touched, so a disabled attempt surfaces as a
    /// visible tool result rather than a silent no-op (R007).
    Disabled { detail: String },
    /// Accessibility permission is not granted (TCC). The UI responds with the
    /// guided walkthrough, never a bare error string.
    PermissionDenied { detail: String },
    /// HID input is not implemented on this platform. `platform` names the
    /// running OS so logs and error surfaces are self-explanatory.
    Unsupported { platform: String, detail: String },
    /// The input synthesis itself failed after permission checks passed (enigo
    /// construction or event-post error).
    InputFailed { detail: String },
}

impl InputError {
    /// Stable machine-readable name, mirroring the serde `kind` tag. Used in
    /// error logs so grep for `permission-denied` / `unsupported` /
    /// `input-failed` works.
    pub fn kind(&self) -> &'static str {
        match self {
            InputError::Disabled { .. } => "disabled",
            InputError::PermissionDenied { .. } => "permission-denied",
            InputError::Unsupported { .. } => "unsupported",
            InputError::InputFailed { .. } => "input-failed",
        }
    }

    /// The `disabled` error the structural gate returns when a disarmed
    /// [`InputControl`] is asked to act — the one shape [`crate::llm`]'s
    /// `InputTool` refuses with before touching the backend (D038).
    pub fn disabled() -> Self {
        InputError::Disabled {
            detail: "HID is off; arm it in Settings to let the model drive input".to_string(),
        }
    }

    /// The `unsupported` error for the current platform — the one shape the
    /// fallback backend ever returns.
    pub fn unsupported_here() -> Self {
        InputError::Unsupported {
            platform: std::env::consts::OS.to_string(),
            detail: "HID input is only implemented on macOS".to_string(),
        }
    }
}

impl std::fmt::Display for InputError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InputError::Disabled { detail } => {
                write!(f, "input disabled: {detail}")
            }
            InputError::PermissionDenied { detail } => {
                write!(f, "input permission-denied: Accessibility not granted ({detail})")
            }
            InputError::Unsupported { platform, detail } => {
                write!(f, "input unsupported on {platform}: {detail}")
            }
            InputError::InputFailed { detail } => {
                write!(f, "input input-failed: {detail}")
            }
        }
    }
}

impl std::error::Error for InputError {}

/// Queryable Accessibility permission state: `{ granted, supported }`.
/// Health-as-value (R007): returned by the backend's `permission()` and never
/// an error, never a prompt. `supported: false` means this platform has no HID
/// backend at all, so the UI can hide the arming affordance instead of walking
/// the user through a prompt that will never appear.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InputPermission {
    pub granted: bool,
    pub supported: bool,
}

/// Post-action evidence a backend reads back from the OS AFTER the synthesized
/// event was posted — the model's proof that the action actually did what was
/// commanded, not just that the event-post call returned. Every prior HID bug
/// in this project (silently-dropped posts, clicks at a stale cursor,
/// keystrokes swallowed by the overlay's own key window) reported `ok` while
/// doing the wrong thing; this report is how the tool loop detects that class
/// of wrongness instead of claiming success blind (R007).
///
/// All fields are best-effort observations: a failed readback leaves its field
/// `None` and never fails the action itself. Serialized camelCase into the
/// `input_action` tool result's `verified` block, so its contents reach the
/// model's context — values are truncated and secure fields skipped at the
/// observation site.
#[derive(Debug, Clone, PartialEq, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ActionReport {
    /// Where the system cursor ACTUALLY is after the action (read back from the
    /// OS, not echoed from the command) — present for mouse actions.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<CursorPosition>,
    /// The UI element holding keyboard focus after the action — present for
    /// clicks (a click on a field should focus it) and keyboard actions (the
    /// element the keystrokes went into).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub focus: Option<FocusReport>,
    /// `type-text` only: whether the focused element's value was observed to
    /// contain the typed text afterwards. `Some(false)` is not proof of failure
    /// (some targets never echo text), but `Some(true)` is proof of success.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text_entered: Option<bool>,
}

/// An absolute cursor position in logical screen points (top-left origin) —
/// the same space `screen_query` coordinates and aimed actions use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct CursorPosition {
    pub x: i32,
    pub y: i32,
}

/// The system-wide keyboard-focused UI element, as the OS reports it: which
/// app owns it, what kind of element it is, and (for text-bearing elements)
/// an excerpt of its current value. Secure fields never carry a value.
#[derive(Debug, Clone, PartialEq, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FocusReport {
    /// Localized name of the app owning the focused element — the same
    /// namespace `focus_app` and `screen_query` attribution use.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub app: Option<String>,
    /// The element's accessibility role (e.g. `AXTextField`, `AXButton`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// The element's title or description, when it has one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Excerpt (tail) of the element's current value, for text elements.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
}

/// The HID input seam. Object-safe (`Arc<dyn InputControl>`) so managed state,
/// the composite executor, and tests can hold any backend without knowing its
/// transport. `Send + Sync` so it can live in Tauri managed state like
/// [`crate::capture::ScreenCapture`] — backends that wrap a `!Send` handle
/// (enigo on macOS) must construct it transiently inside the action, never
/// store it.
#[async_trait]
pub trait InputControl: Send + Sync {
    /// Current Accessibility permission state — a value, never an error, and
    /// never triggers the OS prompt.
    fn permission(&self) -> InputPermission;

    /// Trigger the OS Accessibility prompt (or the Settings round-trip if the
    /// user previously denied). Returns the resulting granted state.
    fn request_permission(&self) -> bool;

    /// Synthesize one HID action into the foreground application and read back
    /// what the OS observed afterwards. Never hangs silently: every failure
    /// path resolves to an [`InputError`]. The [`ActionReport`] carries
    /// best-effort post-action evidence (cursor readback, focused element) so
    /// the caller can verify the action's EFFECT, not just its dispatch;
    /// backends without a readback return [`ActionReport::default`].
    async fn perform(&self, action: InputAction) -> Result<ActionReport, InputError>;
}

/// Decorator over any [`InputControl`] that runs a `yield_focus` hook before
/// every KEYBOARD action (`type-text` / `key-press`) reaches the backend — and
/// only those. Mouse actions pass straight through: they are routed by screen
/// position, not by key window, so there is no focus to yield for them.
///
/// Why it exists (M005 follow-up): synthesized keystrokes are delivered to the
/// GLOBAL key window, and Third Eye's overlay is a nonactivating NSPanel that
/// keeps key status even while another app is active — the Spotlight trait the
/// overlay borrows for its no-focus-steal design. So after `focus_app` had
/// verifiably fronted Chrome, a `type-text` still landed in the overlay's own
/// prompt (the "asked it to search in Chrome and it typed into Third Eye"
/// report). The hook — wired to `crate::overlay::yield_key_focus` in
/// production — makes the overlay resign key first, so the keystrokes land in
/// the app the model fronted.
///
/// The hook is fallible ON PURPOSE: if the yield does not complete, the
/// keystrokes would go into the wrong window, so the action fails typed
/// (`input-failed`) instead — R007's no-silent-wrongness applied to keyboard
/// focus. The hook may block (a main-thread round-trip with a completion
/// handshake), so it runs on the blocking pool, never the async worker.
pub struct KeyboardFocusYield {
    inner: std::sync::Arc<dyn InputControl>,
    yield_focus: std::sync::Arc<dyn Fn() -> Result<(), String> + Send + Sync>,
}

impl KeyboardFocusYield {
    pub fn new(
        inner: std::sync::Arc<dyn InputControl>,
        yield_focus: std::sync::Arc<dyn Fn() -> Result<(), String> + Send + Sync>,
    ) -> Self {
        Self { inner, yield_focus }
    }
}

#[async_trait]
impl InputControl for KeyboardFocusYield {
    fn permission(&self) -> InputPermission {
        self.inner.permission()
    }

    fn request_permission(&self) -> bool {
        self.inner.request_permission()
    }

    async fn perform(&self, action: InputAction) -> Result<ActionReport, InputError> {
        if matches!(action, InputAction::TypeText { .. } | InputAction::KeyPress { .. }) {
            let yield_focus = self.yield_focus.clone();
            // The hook blocks on a main-thread handshake; keep the async worker
            // clean. The keystrokes MUST NOT race the handoff, so the failure of
            // either the join or the hook itself refuses the action typed.
            let yielded = tokio::task::spawn_blocking(move || yield_focus())
                .await
                .map_err(|e| InputError::InputFailed {
                    detail: format!("key-focus yield task panicked: {e}"),
                })?;
            if let Err(detail) = yielded {
                let err = InputError::InputFailed {
                    detail: format!(
                        "keyboard focus was not yielded, refusing to type into the wrong window: {detail}"
                    ),
                };
                log::error!("input: {} ({err})", err.kind());
                return Err(err);
            }
        }
        self.inner.perform(action).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// Minimal in-memory backend proving the trait is implementable and
    /// object-safe — the same shape the later backend tests will use. Records
    /// the last action so delegation can be asserted through `dyn`.
    struct MockInput {
        fail_with: Option<InputError>,
        last: std::sync::Mutex<Option<InputAction>>,
    }

    impl MockInput {
        fn ok() -> Self {
            Self { fail_with: None, last: std::sync::Mutex::new(None) }
        }

        fn failing(err: InputError) -> Self {
            Self { fail_with: Some(err), last: std::sync::Mutex::new(None) }
        }
    }

    #[async_trait]
    impl InputControl for MockInput {
        fn permission(&self) -> InputPermission {
            InputPermission { granted: self.fail_with.is_none(), supported: true }
        }

        fn request_permission(&self) -> bool {
            self.fail_with.is_none()
        }

        async fn perform(&self, action: InputAction) -> Result<ActionReport, InputError> {
            if let Some(err) = &self.fail_with {
                return Err(err.clone());
            }
            *self.last.lock().unwrap() = Some(action);
            Ok(ActionReport::default())
        }
    }

    #[tokio::test]
    async fn trait_is_object_safe_and_performs_through_dyn() {
        let backend: Arc<dyn InputControl> = Arc::new(MockInput::ok());
        backend.perform(InputAction::click(MouseButton::Left)).await.unwrap();
        assert!(backend.permission().granted);
        assert!(backend.request_permission());
    }

    #[tokio::test]
    async fn errors_propagate_through_dyn_with_kind() {
        let backend: Arc<dyn InputControl> =
            Arc::new(MockInput::failing(InputError::PermissionDenied { detail: "AX denied".into() }));
        let err = backend.perform(InputAction::TypeText { text: "hi".into() }).await.unwrap_err();
        assert_eq!(err.kind(), "permission-denied");
        assert!(!backend.permission().granted);
    }

    #[test]
    fn error_json_shape_is_the_ipc_contract() {
        // The UI matches on `kind` and reads camelCase fields; a change here is
        // a breaking IPC change and must be coordinated with the frontend.
        let denied = InputError::PermissionDenied { detail: "AX denied".into() };
        let v = serde_json::to_value(&denied).unwrap();
        assert_eq!(v["kind"], "permission-denied");
        assert_eq!(v["detail"], "AX denied");

        let unsupported =
            InputError::Unsupported { platform: "linux".into(), detail: "no backend".into() };
        let v = serde_json::to_value(&unsupported).unwrap();
        assert_eq!(v["kind"], "unsupported");
        assert_eq!(v["platform"], "linux");
        assert_eq!(v["detail"], "no backend");

        let failed = InputError::InputFailed { detail: "post failed".into() };
        let v = serde_json::to_value(&failed).unwrap();
        assert_eq!(v["kind"], "input-failed");
        assert_eq!(v["detail"], "post failed");
    }

    #[test]
    fn kind_matches_serde_tag_for_every_variant() {
        let all = [
            InputError::Disabled { detail: String::new() },
            InputError::PermissionDenied { detail: String::new() },
            InputError::Unsupported { platform: String::new(), detail: String::new() },
            InputError::InputFailed { detail: String::new() },
        ];
        for err in all {
            let v = serde_json::to_value(&err).unwrap();
            assert_eq!(v["kind"], err.kind(), "kind()/serde tag drift for {err:?}");
        }
    }

    #[test]
    fn disabled_is_the_kind_tagged_structural_refusal() {
        // The UI's arming walkthrough and the model both key on `kind`; the
        // disabled variant is the structural-inertness refusal (D038).
        let err = InputError::disabled();
        assert_eq!(err.kind(), "disabled");
        let v = serde_json::to_value(&err).unwrap();
        assert_eq!(v["kind"], "disabled");
        assert!(v["detail"].as_str().unwrap().contains("Settings"));
        assert!(err.to_string().contains("disabled"), "display must name the kind");
    }

    #[test]
    fn unsupported_here_names_this_platform() {
        let err = InputError::unsupported_here();
        assert_eq!(err.kind(), "unsupported");
        match err {
            InputError::Unsupported { platform, .. } => {
                assert_eq!(platform, std::env::consts::OS);
            }
            other => panic!("expected unsupported, got {other:?}"),
        }
    }

    #[test]
    fn error_display_names_kind_and_detail() {
        let err = InputError::PermissionDenied { detail: "AX denied".into() };
        let msg = err.to_string();
        assert!(msg.contains("permission-denied"), "kind missing: {msg}");
        assert!(msg.contains("AX denied"), "detail missing: {msg}");
    }

    #[test]
    fn action_report_serializes_camel_case_and_omits_unobserved_fields() {
        // The `verified` block in the input_action tool result is model-facing
        // wire contract: camelCase keys, and unobserved fields are ABSENT (not
        // null) so a small model never has to reason about nulls.
        let empty = serde_json::to_value(ActionReport::default()).unwrap();
        assert_eq!(empty, serde_json::json!({}), "a no-evidence report must be empty");

        let full = ActionReport {
            cursor: Some(CursorPosition { x: 640, y: 220 }),
            focus: Some(FocusReport {
                app: Some("Google Chrome".into()),
                role: Some("AXTextField".into()),
                title: Some("Address and search bar".into()),
                value: Some("farts".into()),
            }),
            text_entered: Some(true),
        };
        let v = serde_json::to_value(&full).unwrap();
        assert_eq!(v["cursor"]["x"], 640);
        assert_eq!(v["cursor"]["y"], 220);
        assert_eq!(v["focus"]["app"], "Google Chrome");
        assert_eq!(v["focus"]["role"], "AXTextField");
        assert_eq!(v["focus"]["title"], "Address and search bar");
        assert_eq!(v["focus"]["value"], "farts");
        assert_eq!(v["textEntered"], true, "camelCase is the wire contract");
    }

    #[test]
    fn permission_serializes_camel_case() {
        let p = InputPermission { granted: false, supported: true };
        let v = serde_json::to_value(p).unwrap();
        assert_eq!(v["granted"], false);
        assert_eq!(v["supported"], true);
    }

    #[test]
    fn action_json_round_trips_with_tag() {
        let actions = [
            InputAction::MouseMove { x: 10, y: 20 },
            InputAction::click(MouseButton::Right),
            InputAction::click_at(MouseButton::Left, 30, 40),
            InputAction::TypeText { text: "hello".into() },
            InputAction::KeyPress { key: "return".into() },
        ];
        for action in actions {
            let v = serde_json::to_value(&action).unwrap();
            assert!(v.get("action").is_some(), "action tag missing for {action:?}");
            let back: InputAction = serde_json::from_value(v).unwrap();
            assert_eq!(back, action);
        }

        // The tag values and field casing are the wire contract.
        let v = serde_json::to_value(InputAction::MouseMove { x: 3, y: 4 }).unwrap();
        assert_eq!(v["action"], "mouse-move");
        assert_eq!(v["x"], 3);
        assert_eq!(v["y"], 4);

        // A bare click omits x/y entirely (skip_serializing_if) — the coordless
        // wire shape older callers and the model both still emit.
        let v = serde_json::to_value(InputAction::click(MouseButton::Middle)).unwrap();
        assert_eq!(v["action"], "mouse-click");
        assert_eq!(v["button"], "middle");
        assert!(v.get("x").is_none(), "bare click must not carry x");
        assert!(v.get("y").is_none(), "bare click must not carry y");

        // A coordinate-bearing click carries both — the aim-then-click the model
        // emits after screen_query; parsing a stray x/y no longer drops it.
        let aimed: InputAction =
            serde_json::from_str(r#"{"action":"mouse-click","button":"left","x":640,"y":220}"#)
                .unwrap();
        assert_eq!(aimed, InputAction::click_at(MouseButton::Left, 640, 220));
        assert_eq!(aimed.aim_target(), Some((640, 220)));
        assert!(InputAction::click(MouseButton::Left).aim_target().is_none());

        // Half-specified aim is malformed — validate() rejects it before it can
        // silently degrade to a click-at-cursor.
        let half: InputAction =
            serde_json::from_str(r#"{"action":"mouse-click","button":"left","x":640}"#).unwrap();
        assert!(half.validate().is_err(), "x-without-y must be rejected");
    }

    #[test]
    fn action_kind_matches_the_action_tag_and_ignores_payload() {
        // The whitelist grants by kind, so the kind must be payload-blind: two
        // TypeText actions with different text are one kind. And a kind must
        // serialize the same kebab string as its action's `action` tag.
        let cases = [
            (InputAction::MouseMove { x: 1, y: 2 }, ActionKind::MouseMove, "mouse-move"),
            (InputAction::click(MouseButton::Left), ActionKind::MouseClick, "mouse-click"),
            (InputAction::TypeText { text: "a".into() }, ActionKind::TypeText, "type-text"),
            (InputAction::KeyPress { key: "return".into() }, ActionKind::KeyPress, "key-press"),
        ];
        for (action, kind, tag) in cases {
            assert_eq!(action.kind(), kind, "{action:?} kind mismatch");
            assert_eq!(serde_json::to_value(kind).unwrap(), tag, "kind tag drift for {kind:?}");
            // The kind string equals the action's own `action` tag.
            assert_eq!(serde_json::to_value(&action).unwrap()["action"], tag);
        }

        // Payload-blind: different text, same kind.
        assert_eq!(
            InputAction::TypeText { text: "hello".into() }.kind(),
            InputAction::TypeText { text: "world".into() }.kind(),
        );
    }

    /// A backend that appends to a shared journal, so hook-vs-backend ordering
    /// is observable — the property KeyboardFocusYield exists to guarantee.
    struct JournalingInput {
        journal: Arc<std::sync::Mutex<Vec<String>>>,
    }

    #[async_trait]
    impl InputControl for JournalingInput {
        fn permission(&self) -> InputPermission {
            InputPermission { granted: true, supported: true }
        }

        fn request_permission(&self) -> bool {
            true
        }

        async fn perform(&self, action: InputAction) -> Result<ActionReport, InputError> {
            self.journal.lock().unwrap().push(format!("perform:{}", action.kind_str()));
            Ok(ActionReport::default())
        }
    }

    fn yield_harness(
        hook_result: Result<(), String>,
    ) -> (KeyboardFocusYield, Arc<std::sync::Mutex<Vec<String>>>) {
        let journal = Arc::new(std::sync::Mutex::new(Vec::new()));
        let inner = Arc::new(JournalingInput { journal: journal.clone() });
        let hook_journal = journal.clone();
        let wrapped = KeyboardFocusYield::new(
            inner,
            Arc::new(move || {
                hook_journal.lock().unwrap().push("yield".to_string());
                hook_result.clone()
            }),
        );
        (wrapped, journal)
    }

    #[tokio::test]
    async fn keyboard_actions_yield_focus_before_the_backend_acts() {
        // The whole point of the decorator: the overlay must have resigned key
        // BEFORE the first keystroke posts, or the text lands in Third Eye's own
        // prompt. Ordering in the journal is the proof.
        let (backend, journal) = yield_harness(Ok(()));
        backend.perform(InputAction::TypeText { text: "farts".into() }).await.unwrap();
        backend.perform(InputAction::KeyPress { key: "return".into() }).await.unwrap();
        assert_eq!(
            *journal.lock().unwrap(),
            vec!["yield", "perform:type-text", "yield", "perform:key-press"],
        );
    }

    #[tokio::test]
    async fn mouse_actions_pass_through_without_yielding() {
        // Mouse events route by screen position, not key window — yielding for
        // them would blink the panel ordering for nothing.
        let (backend, journal) = yield_harness(Ok(()));
        backend.perform(InputAction::MouseMove { x: 10, y: 20 }).await.unwrap();
        backend.perform(InputAction::click_at(MouseButton::Left, 30, 40)).await.unwrap();
        assert_eq!(
            *journal.lock().unwrap(),
            vec!["perform:mouse-move", "perform:mouse-click"],
        );
    }

    #[tokio::test]
    async fn failed_yield_refuses_the_keystrokes_typed_and_never_reaches_backend() {
        // If the overlay could not resign key, typing would land in the wrong
        // window — the action must fail typed instead of "succeeding" wrong.
        let (backend, journal) = yield_harness(Err("main thread busy".into()));
        let err =
            backend.perform(InputAction::TypeText { text: "hi".into() }).await.unwrap_err();
        assert_eq!(err.kind(), "input-failed");
        assert!(err.to_string().contains("wrong window"), "detail must say why: {err}");
        assert_eq!(*journal.lock().unwrap(), vec!["yield"], "backend must not be reached");

        // Mouse actions are unaffected by a broken yield hook.
        backend.perform(InputAction::click(MouseButton::Left)).await.unwrap();
        assert_eq!(*journal.lock().unwrap(), vec!["yield", "perform:mouse-click"]);
    }

    #[tokio::test]
    async fn yield_decorator_delegates_permission_surfaces() {
        let (backend, _journal) = yield_harness(Ok(()));
        assert!(backend.permission().granted);
        assert!(backend.permission().supported);
        assert!(backend.request_permission());
    }

    #[test]
    fn focus_app_kind_serializes_kebab_case() {
        // The `focus_app` tool has no InputAction payload, but its ActionKind is
        // the unit the ApprovalGate gates and the whitelist grants by; it rides
        // the same camelCase/kebab contract as the input kinds (`focus-app`).
        assert_eq!(serde_json::to_value(ActionKind::FocusApp).unwrap(), "focus-app");
        let back: ActionKind = serde_json::from_str("\"focus-app\"").unwrap();
        assert_eq!(back, ActionKind::FocusApp);
        // Grantable through the whitelist like any other kind.
        let mut wl = std::collections::HashSet::new();
        wl.insert(ActionKind::FocusApp);
        assert!(wl.contains(&ActionKind::FocusApp));
    }
}
