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
//!   But `AXIsProcessTrusted()` alone is NOT sufficient to *post* synthetic
//!   events. macOS gates event synthesis behind a distinct privilege —
//!   post-event access — read by `CGPreflightPostEventAccess()` (macOS 11+).
//!   `AXIsProcessTrusted()` can return `true` (the AX tree is readable) while
//!   `CGPreflightPostEventAccess()` returns `false`, in which case every
//!   `CGEventPost` enigo issues is silently dropped: the call "succeeds" and
//!   nothing moves. This bit us under `cargo tauri dev` — the responsible
//!   process (the launching terminal) holds the AX grant, but post-event access
//!   was never granted to it, so clicks/keystrokes vanished while the tool
//!   reported `ok`. So [`has_permission`] requires BOTH privileges, and
//!   [`request_permission`] requests BOTH — the AX prompt via enigo and the
//!   post-event prompt via `CGRequestPostEventAccess()`. See Apple DTS thread
//!   758554 and CoreGraphics' `CG*PostEventAccess` docs.
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
use enigo::{Button, Coordinate, Direction, Enigo, Key, Keyboard, Mouse, Settings};
use objc2_core_foundation::CFString;

use super::{
    ActionReport, CursorPosition, FocusReport, InputAction, InputControl, InputError,
    InputPermission, MouseButton,
};

// Raw FFI instead of a binding crate: `AXIsProcessTrusted` is a stable,
// ABI-simple Accessibility (HIServices) call, and the pinned-dependency policy
// favors no new crates for it — the same call the capture layer makes for
// CoreGraphics (see capture/macos.rs:33-35). It lives in the ApplicationServices
// umbrella framework. `bool` is ABI-compatible with C `_Bool`.
#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXIsProcessTrusted() -> bool;
}

// Post-event access (macOS 11+): the SEPARATE privilege that actually gates
// `CGEventPost`. `AXIsProcessTrusted()` covering the AX *read* tree does not
// imply the process may *post* synthetic events; that is this pair. Read-only
// preflight + a prompting request, same C-ABI `bool` shape as the AX call. In
// CoreGraphics, sibling to `CGPreflightScreenCaptureAccess` (capture/macos.rs).
#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGPreflightPostEventAccess() -> bool;
    fn CGRequestPostEventAccess() -> bool;
}

/// Whether Accessibility (TCC) permission is currently granted — the AX *read*
/// tree only. Read-only: never triggers the system prompt, safe on any thread.
/// Kept separate from [`has_permission`] so a caller can tell "AX granted but
/// posting denied" apart from "nothing granted".
pub fn has_ax_trust() -> bool {
    // Safety: no arguments, no pointers; reads TCC state and returns a bool.
    unsafe { AXIsProcessTrusted() }
}

/// Whether this process may POST synthetic events (`CGEventPost`) right now.
/// Read-only preflight: never prompts, safe on any thread. Distinct from
/// [`has_ax_trust`] — the two grants are independent, and posting is the one
/// HID actually needs.
pub fn has_post_event_access() -> bool {
    // Safety: no arguments, no pointers; reads the post-event grant, returns bool.
    unsafe { CGPreflightPostEventAccess() }
}

/// Whether HID is usable: BOTH the AX trust (so enigo constructs) AND
/// post-event access (so its `CGEventPost` actually lands). Requiring both is
/// what turns a silently-dropped click into a truthful `permission-denied` —
/// the AX-only check returned `true` while events vanished. Read-only, never
/// prompts.
pub fn has_permission() -> bool {
    has_ax_trust() && has_post_event_access()
}

/// Show the permission prompts (or the Settings round-trip macOS falls back to
/// after a denial) for BOTH privileges HID needs, and return the resulting
/// granted state.
///
/// enigo has no standalone "prompt" call, so the AX prompt comes from a
/// throwaway `Enigo` with `open_prompt_to_get_permissions: true` — the only
/// place in the crate that is allowed to prompt. Post-event access has its own
/// prompt, `CGRequestPostEventAccess()`, requested alongside. The `Enigo` is
/// dropped immediately; nothing is stored. Returns the real combined state via
/// the read-only preflights rather than assuming either prompt succeeded.
pub fn request_permission() -> bool {
    let settings = Settings {
        open_prompt_to_get_permissions: true,
        ..Settings::default()
    };
    // Constructing Enigo is what surfaces the AX TCC prompt; we don't need the
    // handle afterwards. If construction fails we still report the real state.
    match Enigo::new(&settings) {
        Ok(_enigo) => {}
        Err(e) => log::warn!("input: permission prompt construction failed: {e}"),
    }
    // Post-event access is a distinct grant with its own prompt — request it too,
    // or a machine that granted AX but not posting stays silently broken.
    // Safety: no arguments, no pointers; may present system UI, returns bool.
    let post_requested = unsafe { CGRequestPostEventAccess() };
    let granted = has_permission();
    log::info!(
        "input: permission requested, granted={granted} (ax_trust={}, post_event={}, post_request_returned={post_requested})",
        has_ax_trust(),
        has_post_event_access(),
    );
    granted
}

/// Current permission state as the IPC health-as-value shape.
pub fn permission_status() -> InputPermission {
    InputPermission {
        granted: has_permission(),
        supported: true,
    }
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

    async fn perform(&self, action: InputAction) -> Result<ActionReport, InputError> {
        // Read-only preflight (never prompts): give the typed permission error
        // the walkthrough keys on instead of enigo prompting mid-action (R007).
        // Both grants must hold — post-event access is the one that silently
        // dropped events when only AX trust was checked, so name whichever is
        // missing rather than blaming AX for a posting denial.
        if !has_permission() {
            let detail = match (has_ax_trust(), has_post_event_access()) {
                (false, false) => {
                    "Accessibility and post-event access not granted (AXIsProcessTrusted and \
                     CGPreflightPostEventAccess both false)"
                }
                (true, false) => {
                    "post-event access not granted (CGPreflightPostEventAccess false) — the \
                     process may read the accessibility tree but cannot post synthetic events; \
                     grant it in System Settings → Privacy & Security → Accessibility"
                }
                (false, true) => "Accessibility not granted (AXIsProcessTrusted false)",
                (true, true) => unreachable!("has_permission() false but both grants true"),
            };
            let err = InputError::PermissionDenied {
                detail: detail.into(),
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

// Read the live system cursor in the SAME top-left-origin point space
// `CGEventPost` writes to: a nil-source CGEvent's location is the current
// cursor. This is the ground truth enigo's `button()` implicitly clicks at —
// see [`wait_for_cursor_commit`] for why we must read it ourselves.
#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGEventCreate(source: *const std::ffi::c_void) -> *mut std::ffi::c_void;
    fn CGEventGetLocation(event: *mut std::ffi::c_void) -> CGPointRaw;
    fn CFRelease(cf: *mut std::ffi::c_void);
}
#[repr(C)]
#[derive(Clone, Copy)]
struct CGPointRaw {
    x: f64,
    y: f64,
}

/// Current cursor position in top-left-origin logical points.
fn cursor_location() -> Result<(f64, f64), InputError> {
    // Safety: CGEventCreate(nil) returns a retained event whose location is the
    // live cursor; released immediately. No arguments beyond the null source.
    unsafe {
        let ev = CGEventCreate(std::ptr::null());
        if ev.is_null() {
            return Err(InputError::InputFailed {
                detail: "CGEventCreate(nil) returned null reading the cursor".into(),
            });
        }
        let p = CGEventGetLocation(ev);
        CFRelease(ev);
        Ok((p.x, p.y))
    }
}

// Post-action focus readback (HIServices AX C API): the system-wide focused
// element is the OS's own answer to "which app and which field has keyboard
// focus right now" — the ground truth every silent HID failure so far lied
// about (keystrokes into the overlay's own prompt, clicks that never focused
// the target field). Raw FFI like `AXIsProcessTrusted` above: stable C ABI,
// no binding crate carries it. Reading these attributes needs the AX trust
// this backend already requires.
#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXUIElementCreateSystemWide() -> *mut std::ffi::c_void;
    fn AXUIElementCopyAttributeValue(
        element: *mut std::ffi::c_void,
        attribute: *const CFString,
        value: *mut *mut std::ffi::c_void,
    ) -> i32;
    fn AXUIElementGetPid(element: *mut std::ffi::c_void, pid: *mut i32) -> i32;
}
#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFGetTypeID(cf: *mut std::ffi::c_void) -> usize;
    fn CFStringGetTypeID() -> usize;
}

/// Copy one AX attribute of `element` as a Rust string, or `None` when the
/// attribute is absent, unreadable, or not a CFString. Never errors: the
/// readback is evidence-gathering, and a target app without AX support just
/// yields an emptier report.
unsafe fn copy_string_attr(element: *mut std::ffi::c_void, name: &str) -> Option<String> {
    let attr = CFString::from_str(name);
    let mut value: *mut std::ffi::c_void = std::ptr::null_mut();
    if AXUIElementCopyAttributeValue(element, &*attr, &mut value) != 0 || value.is_null() {
        return None;
    }
    let out = if CFGetTypeID(value) == CFStringGetTypeID() {
        Some((*(value as *const CFString)).to_string())
    } else {
        None
    };
    CFRelease(value);
    out
}

/// Localized name of the app owning `pid` — the SAME namespace `focus_app`
/// verification and `screen_query` attribution report, so the model can compare
/// them directly.
fn app_name_for_pid(pid: i32) -> Option<String> {
    let app = objc2_app_kit::NSRunningApplication::runningApplicationWithProcessIdentifier(pid)?;
    app.localizedName().map(|n| n.to_string())
}

/// One snapshot of the system-wide keyboard-focused element: owning app, role,
/// title/description, and its CURRENT full value (truncated later by
/// [`redact_focus`] — matching against typed text needs the full value first).
/// `None` when nothing holds focus or the AX read fails. Secure text fields
/// never yield a value — their content must not enter model context.
fn read_focused_element() -> Option<FocusReport> {
    unsafe {
        let system_wide = AXUIElementCreateSystemWide();
        if system_wide.is_null() {
            return None;
        }
        let attr = CFString::from_str("AXFocusedUIElement");
        let mut el: *mut std::ffi::c_void = std::ptr::null_mut();
        let err = AXUIElementCopyAttributeValue(system_wide, &*attr, &mut el);
        CFRelease(system_wide);
        if err != 0 || el.is_null() {
            return None;
        }
        let mut pid: i32 = 0;
        let app = (AXUIElementGetPid(el, &mut pid) == 0)
            .then(|| app_name_for_pid(pid))
            .flatten();
        let role = copy_string_attr(el, "AXRole");
        let title = copy_string_attr(el, "AXTitle")
            .filter(|t| !t.trim().is_empty())
            .or_else(|| copy_string_attr(el, "AXDescription"));
        let value = if role.as_deref() == Some("AXSecureTextField") {
            None
        } else {
            copy_string_attr(el, "AXValue")
        };
        CFRelease(el);
        Some(FocusReport {
            app,
            role,
            title,
            value,
        })
    }
}

/// Max characters of a focused element's value that may enter the report (and
/// thus model context). The TAIL is kept — that is where just-typed text lands.
const VALUE_EXCERPT_CHARS: usize = 160;

/// Char-boundary-safe tail excerpt, prefixed with `…` when truncated.
fn tail_excerpt(s: &str, max_chars: usize) -> String {
    let count = s.chars().count();
    if count <= max_chars {
        return s.to_string();
    }
    let tail: String = s.chars().skip(count - max_chars).collect();
    format!("…{tail}")
}

/// Bound a focus snapshot for the model-facing report: the value excerpt only.
fn redact_focus(f: FocusReport) -> FocusReport {
    FocusReport {
        value: f.value.map(|v| tail_excerpt(&v, VALUE_EXCERPT_CHARS)),
        ..f
    }
}

/// How long a click/key-press gets for the OS to settle keyboard focus before
/// the single post-action focus read. Fixed (not a poll): with no expected
/// value there is nothing to poll FOR, and reading too early would report the
/// pre-action focus as if it were the outcome.
const FOCUS_SETTLE: std::time::Duration = std::time::Duration::from_millis(150);

/// Post-action focus snapshot for actions with no expected text: settle, read
/// once, redact.
fn observe_focus_settled() -> Option<FocusReport> {
    std::thread::sleep(FOCUS_SETTLE);
    read_focused_element().map(redact_focus)
}

/// Post-`type-text` observation: poll (bounded) until the focused element's
/// value contains the typed text's tail, then report the snapshot and whether
/// it matched. AX value propagation lags the keystrokes by tens of
/// milliseconds, so a single immediate read would under-report success.
/// `matched == Some(false)` after the bound is honest uncertainty — some
/// targets (canvases, games, password fields) never echo — and the model is
/// told to treat it as "not confirmed", not as proof of failure.
fn observe_text_entry(text: &str) -> (Option<FocusReport>, Option<bool>) {
    // Compare on the typed text's tail: long runs may scroll out of AXValue's
    // head, but the most recent keystrokes are at the end.
    let chars: Vec<char> = text.chars().collect();
    let needle: String = if chars.len() > 64 {
        chars[chars.len() - 64..].iter().collect()
    } else {
        text.to_string()
    };
    if needle.trim().is_empty() {
        // Whitespace-only input is unverifiable by containment; report the
        // settled focus without claiming either way.
        return (observe_focus_settled(), None);
    }
    const POLL: std::time::Duration = std::time::Duration::from_millis(50);
    const TRIES: u32 = 14; // ≤ ~700ms
    let mut last: Option<FocusReport> = None;
    for _ in 0..TRIES {
        if let Some(snap) = read_focused_element() {
            let hit = snap.value.as_deref().is_some_and(|v| v.contains(&needle));
            last = Some(snap);
            if hit {
                return (last.map(redact_focus), Some(true));
            }
        }
        std::thread::sleep(POLL);
    }
    (last.map(redact_focus), Some(false))
}

/// Block (bounded) until the system cursor actually reads the commanded point.
///
/// Why this exists (M008 follow-up — the "search in Chrome did nothing" bug):
/// enigo's `button()` does NOT click at the point we just moved to. It reads
/// the CURRENT system cursor (`NSEvent::mouseLocation`) and posts the
/// mouse-down there — and a synthetic move posted through the HID tap takes a
/// few milliseconds to commit in the window server. So `move_mouse(x, y)`
/// followed immediately by `button()` fires the click at the STALE pre-move
/// cursor position (wherever the user's cursor happened to sit), while the
/// visible cursor lands on the target. Every call reports ok; the click lands
/// somewhere else entirely. Waiting for the readback to match the command
/// closes the race deterministically, and a timeout surfaces a genuinely
/// dropped move as a typed `input-failed` instead of a silent misclick (R007).
fn wait_for_cursor_commit(x: i32, y: i32) -> Result<CursorPosition, InputError> {
    // 5ms × 60 = 300ms bound: commit latency is single-digit ms in practice;
    // the bound only trips when the move never landed (e.g. posting silently
    // denied) or something else is fighting for the cursor. Ok carries the
    // READ-BACK position (not the command echoed) — the report's evidence.
    const POLL: std::time::Duration = std::time::Duration::from_millis(5);
    const TRIES: u32 = 60;
    const TOLERANCE: f64 = 3.0;
    let mut last = (f64::NAN, f64::NAN);
    for _ in 0..TRIES {
        last = cursor_location()?;
        if (last.0 - x as f64).abs() <= TOLERANCE && (last.1 - y as f64).abs() <= TOLERANCE {
            return Ok(CursorPosition {
                x: last.0.round() as i32,
                y: last.1.round() as i32,
            });
        }
        std::thread::sleep(POLL);
    }
    Err(InputError::InputFailed {
        detail: format!(
            "cursor never committed to the commanded point ({x},{y}) — it reads \
             ({:.0},{:.0}); the synthesized move was likely dropped by the OS",
            last.0, last.1
        ),
    })
}

/// The per-action blocking stage: build a throwaway `Enigo`, synthesize the one
/// action, drop the handle — then read back what the OS observed (cursor
/// position, focused element) into the [`ActionReport`]. Runs on a
/// `spawn_blocking` thread. Every enigo failure (construction or event post)
/// collapses onto `InputFailed` — the permission-denied case is already handled
/// by the caller's preflight, and a construction failure here after a passing
/// preflight is a genuine synthesis fault, not a permission one. Observation
/// failures never fail a performed action: they just leave report fields empty.
fn perform_blocking(action: InputAction) -> Result<ActionReport, InputError> {
    // Don't prompt on the action path — permission was already verified, and a
    // prompt here would violate health-as-value.
    let settings = Settings {
        open_prompt_to_get_permissions: false,
        ..Settings::default()
    };
    let mut enigo = Enigo::new(&settings).map_err(|e| InputError::InputFailed {
        detail: format!("enigo init failed: {e}"),
    })?;

    let report = match action {
        InputAction::MouseMove { x, y } => {
            enigo
                .move_mouse(x, y, Coordinate::Abs)
                .map_err(|e| InputError::InputFailed {
                    detail: format!("move_mouse failed: {e}"),
                })?;
            // Completing only once the cursor readback matches means a
            // follow-up coordless click (the model's move-then-click pattern,
            // milliseconds apart in one tool turn) fires at the committed
            // point, never a stale one.
            let cursor = wait_for_cursor_commit(x, y)?;
            ActionReport {
                cursor: Some(cursor),
                ..ActionReport::default()
            }
        }
        InputAction::MouseClick { button, x, y } => {
            // A coordinate-bearing click moves to the target first (the model's
            // "click at (x,y)"); a coordless click fires at the cursor. Both
            // x and y are validated present-together upstream, so `if let`
            // on the pair is enough here.
            if let (Some(x), Some(y)) = (x, y) {
                enigo
                    .move_mouse(x, y, Coordinate::Abs)
                    .map_err(|e| InputError::InputFailed {
                        detail: format!("move before click failed: {e}"),
                    })?;
                // enigo's button() clicks at the SYSTEM cursor, not at (x,y) —
                // without this wait the click fires at the stale pre-move
                // position (see wait_for_cursor_commit docs).
                wait_for_cursor_commit(x, y)?;
            }
            enigo
                .button(map_button(button), Direction::Click)
                .map_err(|e| InputError::InputFailed {
                    detail: format!("button click failed: {e}"),
                })?;
            // Evidence: where the click really landed, and what took keyboard
            // focus — a click on a text field should read back that field.
            let cursor = cursor_location().ok().map(|(px, py)| CursorPosition {
                x: px.round() as i32,
                y: py.round() as i32,
            });
            ActionReport {
                cursor,
                focus: observe_focus_settled(),
                text_entered: None,
            }
        }
        InputAction::TypeText { text } => {
            enigo.text(&text).map_err(|e| InputError::InputFailed {
                detail: format!("text entry failed: {e}"),
            })?;
            let (focus, text_entered) = observe_text_entry(&text);
            ActionReport {
                cursor: None,
                focus,
                text_entered,
            }
        }
        InputAction::KeyPress { key } => {
            let k = key_from_str(&key)?;
            enigo
                .key(k, Direction::Click)
                .map_err(|e| InputError::InputFailed {
                    detail: format!("key press failed: {e}"),
                })?;
            ActionReport {
                cursor: None,
                focus: observe_focus_settled(),
                text_entered: None,
            }
        }
    };
    // Diagnostic trail without content: the value excerpt stays out of logs.
    log::debug!(
        "input: verified — cursor={:?} focus.app={:?} focus.role={:?} textEntered={:?}",
        report.cursor,
        report.focus.as_ref().and_then(|f| f.app.as_deref()),
        report.focus.as_ref().and_then(|f| f.role.as_deref()),
        report.text_entered,
    );
    Ok(report)
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
        // Neither preflight prompts, so calling twice in a test is safe and must
        // agree with the status shape.
        let first = has_permission();
        let second = has_permission();
        assert_eq!(first, second);
        let status = permission_status();
        assert!(status.supported);
        assert_eq!(status.granted, first);
    }

    #[test]
    fn has_permission_requires_both_ax_trust_and_post_event_access() {
        // The bug this guards: AX trust alone let events be silently dropped.
        // has_permission() must be the AND of the two independent grants, so a
        // machine with AX but no post-event access reports NOT granted (typed
        // permission-denied) rather than a false ok. All three preflights are
        // read-only and never prompt, so this is safe in the default suite.
        let ax = has_ax_trust();
        let post = has_post_event_access();
        assert_eq!(
            has_permission(),
            ax && post,
            "has_permission must require BOTH AX trust and post-event access"
        );
        // Whenever posting is denied, HID must read as not-granted regardless of
        // AX — the exact case that produced silent no-op clicks.
        if !post {
            assert!(
                !has_permission(),
                "posting denied must mean HID not granted"
            );
        }
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

    #[test]
    fn tail_excerpt_keeps_the_tail_and_respects_char_boundaries() {
        assert_eq!(tail_excerpt("short", 160), "short");
        assert_eq!(tail_excerpt("", 160), "");
        // Truncation keeps the END (where just-typed text lands) and marks it.
        let long: String = "a".repeat(200) + "farts";
        let cut = tail_excerpt(&long, 10);
        assert_eq!(cut, "…aaaaafarts");
        // Multi-byte chars must never split — count in chars, not bytes.
        let uni = "é".repeat(20);
        assert_eq!(tail_excerpt(&uni, 5), format!("…{}", "é".repeat(5)));
    }

    #[test]
    fn redact_focus_truncates_only_the_value() {
        let report = FocusReport {
            app: Some("Google Chrome".into()),
            role: Some("AXTextField".into()),
            title: Some("Address and search bar".into()),
            value: Some("x".repeat(500)),
        };
        let redacted = redact_focus(report);
        assert_eq!(redacted.app.as_deref(), Some("Google Chrome"));
        assert_eq!(redacted.role.as_deref(), Some("AXTextField"));
        let value = redacted.value.unwrap();
        assert_eq!(
            value.chars().count(),
            VALUE_EXCERPT_CHARS + 1, // the … marker
            "the value excerpt must be bounded before it reaches model context"
        );
        // No value stays no value.
        assert_eq!(redact_focus(FocusReport::default()).value, None);
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
        match backend
            .perform(InputAction::MouseMove { x: 200, y: 200 })
            .await
        {
            Ok(report) => {
                assert!(
                    backend.permission().granted,
                    "input succeeded but permission reads false"
                );
                // The move's report must carry the read-back cursor — the
                // verification evidence, not an echo of the command.
                let cursor = report
                    .cursor
                    .expect("a mouse-move report carries the cursor");
                assert!((cursor.x - 200).abs() <= 3 && (cursor.y - 200).abs() <= 3);
            }
            Err(err) => {
                // In an unpermitted environment the only acceptable outcome is
                // the typed permission error the walkthrough keys on.
                assert_eq!(err.kind(), "permission-denied", "unexpected: {err}");
                assert!(!backend.permission().granted);
            }
        }
    }

    // The CURRENT cursor position in the SAME global coordinate space
    // `CGEventPost` writes to (CoreGraphics points, top-left origin) — the
    // ground truth for "where did the move actually land", NOT enigo's
    // pixel-flipped `location()`. Thin infallible wrapper over the production
    // readback the click path itself uses.
    fn cursor_point() -> (f64, f64) {
        cursor_location().expect("cursor readback")
    }

    /// THE decisive targeting probe: does a move to a commanded point actually
    /// land the cursor at that point? Moves to several fixed points and reads the
    /// live cursor back in CoreGraphics point space. If the readback ≈ command,
    /// the input backend clicks in logical-point space (so screen_query must feed
    /// it points — the M005 coordinate fix). A large systematic gap here means a
    /// residual scale/space mismatch. Ignored (moves the real cursor, needs AX).
    #[tokio::test]
    #[ignore = "moves the real cursor; needs Accessibility (targeting UAT)"]
    async fn move_lands_the_cursor_at_the_commanded_point() {
        let backend: Arc<dyn InputControl> = Arc::new(MacosInput);
        if !has_permission() {
            eprintln!("skipping: HID not permitted");
            return;
        }
        // Points well inside any plausible logical desktop, away from edges.
        for (cx, cy) in [(300, 300), (600, 400), (900, 500)] {
            backend
                .perform(InputAction::MouseMove { x: cx, y: cy })
                .await
                .expect("move ok");
            // Give the event loop a beat to settle the cursor.
            tokio::time::sleep(std::time::Duration::from_millis(60)).await;
            let (rx, ry) = cursor_point();
            let (dx, dy) = ((rx - cx as f64).abs(), (ry - cy as f64).abs());
            eprintln!("commanded ({cx},{cy}) → cursor ({rx:.0},{ry:.0})  Δ=({dx:.0},{dy:.0})");
            assert!(
                dx <= 3.0 && dy <= 3.0,
                "cursor did NOT land at the commanded point: commanded ({cx},{cy}), \
                 landed ({rx:.0},{ry:.0}), Δ=({dx:.0},{dy:.0}) — input space ≠ command space",
            );
        }
    }

    /// The full-chain proof: capture the real screen, take a real recognized
    /// element (in logical points, post-M005-fix), move the cursor to its centre,
    /// and confirm the live cursor lands inside the element's box. This is the
    /// end-to-end evidence the FixedScreen probe could not give — real capture,
    /// real conversion, real cursor. Ignored (moves the cursor, needs both grants).
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "captures screen + moves the real cursor; needs Screen Recording + Accessibility (targeting UAT)"]
    async fn screen_query_element_centre_lands_the_cursor_inside_the_box() {
        use crate::ocr::macos::extract_elements_blocking;
        if !has_permission() {
            eprintln!("skipping: HID not permitted");
            return;
        }
        let elements = tokio::task::spawn_blocking(|| {
            extract_elements_blocking(crate::ocr::OCR_MAX_DIMENSION)
        })
        .await
        .expect("join")
        .expect("screen query ok");
        // Pick a reasonably-sized element with a real app attribution — a genuine
        // on-screen target, not a stray menu-bar glyph.
        let target = elements
            .iter()
            .filter(|e| e.width >= 30 && e.height >= 10)
            .find(|e| e.app.as_deref().map(|a| !a.is_empty()).unwrap_or(false))
            .or_else(|| elements.iter().find(|e| e.width >= 30 && e.height >= 10))
            .cloned();
        let Some(t) = target else {
            eprintln!("no suitable element on screen; skipping");
            return;
        };
        let (cx, cy) = (t.x + t.width / 2, t.y + t.height / 2);
        eprintln!(
            "target {:?} box ({},{}) {}x{} → centre ({cx},{cy})",
            t.text, t.x, t.y, t.width, t.height
        );
        let backend: Arc<dyn InputControl> = Arc::new(MacosInput);
        backend
            .perform(InputAction::MouseMove { x: cx, y: cy })
            .await
            .expect("move ok");
        tokio::time::sleep(std::time::Duration::from_millis(60)).await;
        let (rx, ry) = cursor_point();
        eprintln!(
            "cursor landed at ({rx:.0},{ry:.0}); element box x∈[{},{}] y∈[{},{}]",
            t.x,
            t.x + t.width,
            t.y,
            t.y + t.height
        );
        assert!(
            rx >= t.x as f64 - 2.0
                && rx <= (t.x + t.width) as f64 + 2.0
                && ry >= t.y as f64 - 2.0
                && ry <= (t.y + t.height) as f64 + 2.0,
            "cursor landed OUTSIDE the element box — coordinate space still wrong: \
             centre ({cx},{cy}) landed ({rx:.0},{ry:.0}), box ({},{})..({},{})",
            t.x,
            t.y,
            t.x + t.width,
            t.y + t.height,
        );
    }

    fn osascript(script: &str) -> String {
        let out = std::process::Command::new("osascript")
            .arg("-e")
            .arg(script)
            .output()
            .expect("osascript spawn");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// Reproduces the in-app "search in Chrome" action chain exactly as the
    /// tool loop delivers it: click the address bar, type the query, press
    /// Return — three separate `perform` calls, back-to-back. The tool loop
    /// runs same-turn tool calls in a plain `for` loop with no inter-action
    /// delay (toolloop.rs run_tool_loop), so `TE_SETTLE_MS=0` mimics the live
    /// failure mode and a generous value (e.g. 400) isolates whether Chrome's
    /// click→omnibox-focus handoff is the race. Ground truth is the tab's URL
    /// read back over AppleScript: an omnibox search navigates, keystrokes
    /// swallowed by the page body leave the URL untouched. Uses a fresh tab on
    /// a neutral page so the user's tabs are never clobbered. Ignored: drives
    /// live Chrome and synthesizes real input (targeting UAT).
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "drives live Google Chrome and synthesizes real input (targeting UAT)"]
    async fn chrome_address_bar_click_type_return_executes_a_search() {
        if !has_permission() {
            eprintln!("skipping: HID not permitted");
            return;
        }
        let settle: u64 = std::env::var("TE_SETTLE_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        // A neutral page that swallows stray typing without side effects: if the
        // omnibox never got focus, the keystrokes vanish into the page and the
        // URL stays example.com — the exact silent no-op under investigation.
        osascript(
            "tell application \"Google Chrome\"\n\
             activate\n\
             make new tab at end of tabs of front window with properties {URL:\"https://example.com\"}\n\
             end tell",
        );
        tokio::time::sleep(std::time::Duration::from_millis(2500)).await;
        let bounds = osascript("tell application \"Google Chrome\" to get bounds of front window");
        let nums: Vec<i32> = bounds
            .split(',')
            .filter_map(|s| s.trim().parse().ok())
            .collect();
        assert_eq!(nums.len(), 4, "unexpected bounds reply: {bounds:?}");
        // Omnibox: past the nav buttons horizontally, tab-strip + half the
        // toolbar down — the same row OCR located "google.com" text on (+64).
        let (ax, ay) = (nums[0] + 320, nums[1] + 64);
        eprintln!(
            "TE probe: settle={settle}ms, window bounds={bounds}, clicking omnibox at ({ax},{ay})"
        );

        let backend: Arc<dyn InputControl> = Arc::new(MacosInput);
        let click_report = backend
            .perform(InputAction::MouseClick {
                button: MouseButton::Left,
                x: Some(ax),
                y: Some(ay),
            })
            .await
            .expect("click");
        eprintln!("TE probe click report: {click_report:?}");
        // The report is the in-process verification surface the tool loop now
        // returns to the model — it must independently agree with the
        // AppleScript ground truth below: the click focused Chrome's omnibox.
        let focus = click_report
            .focus
            .as_ref()
            .expect("click report carries focus");
        assert!(
            focus.app.as_deref().is_some_and(|a| a.contains("Chrome")),
            "click report must attribute focus to Chrome: {focus:?}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(settle)).await;
        // Bisect diagnostics: where did the cursor actually land, which app is
        // frontmost, and what does Chrome think has keyboard focus?
        let (rx, ry) = cursor_point();
        let front = osascript(
            "tell application \"System Events\" to get name of first process whose frontmost is true",
        );
        let focused = osascript(
            "tell application \"System Events\" to tell process \"Google Chrome\"\n\
             set el to value of attribute \"AXFocusedUIElement\"\n\
             get {role of el, description of el}\n\
             end tell",
        );
        eprintln!(
            "TE probe post-click: cursor=({rx:.0},{ry:.0}) commanded=({ax},{ay}) frontmost={front:?} chrome-focused={focused:?}"
        );
        let type_report = backend
            .perform(InputAction::TypeText {
                text: "farts".into(),
            })
            .await
            .expect("type");
        eprintln!("TE probe type report: {type_report:?}");
        assert_eq!(
            type_report.text_entered,
            Some(true),
            "the type-text report must confirm the text was observed in the focused field: \
             {type_report:?}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(settle)).await;
        let focused = osascript(
            "tell application \"System Events\" to tell process \"Google Chrome\"\n\
             set el to value of attribute \"AXFocusedUIElement\"\n\
             get {role of el, description of el, value of el}\n\
             end tell",
        );
        eprintln!("TE probe post-type: chrome-focused={focused:?}");
        let return_report = backend
            .perform(InputAction::KeyPress {
                key: "return".into(),
            })
            .await
            .expect("return");
        eprintln!("TE probe return report: {return_report:?}");
        tokio::time::sleep(std::time::Duration::from_millis(3000)).await;

        let url = osascript(
            "tell application \"Google Chrome\" to get URL of active tab of front window",
        );
        eprintln!("TE probe: final URL = {url}");
        assert!(
            url.contains("farts"),
            "the search never executed (URL is {url:?}) — keystrokes did not reach the omnibox at settle={settle}ms",
        );
    }
}
