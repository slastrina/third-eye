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

/// Public read of the live cursor for the HUD follower (no permission
/// needed — CGEventCreate(nil) is a plain read).
pub fn current_cursor() -> Option<(f64, f64)> {
    cursor_location().ok()
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
    fn AXUIElementCreateApplication(pid: i32) -> *mut std::ffi::c_void;
    fn AXUIElementCopyAttributeValue(
        element: *mut std::ffi::c_void,
        attribute: *const CFString,
        value: *mut *mut std::ffi::c_void,
    ) -> i32;
    fn AXUIElementGetPid(element: *mut std::ffi::c_void, pid: *mut i32) -> i32;
    fn AXUIElementSetAttributeValue(
        element: *mut std::ffi::c_void,
        attribute: *const CFString,
        value: *const std::ffi::c_void,
    ) -> i32;
    fn AXUIElementCopyElementAtPosition(
        application: *mut std::ffi::c_void,
        x: f32,
        y: f32,
        element: *mut *mut std::ffi::c_void,
    ) -> i32;
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

/// The UI element under `(x, y)` in logical screen points — the AX hit-test
/// answering "what will this click actually hit". Same report shape and
/// redaction rules as the focus readback. `None` when nothing is there (bare
/// desktop), the hit-test fails, or the app under the point exposes no AX
/// tree — evidence-gathering only, never fails the action.
fn element_at_point(x: i32, y: i32) -> Option<FocusReport> {
    unsafe {
        let system_wide = AXUIElementCreateSystemWide();
        if system_wide.is_null() {
            return None;
        }
        let mut el: *mut std::ffi::c_void = std::ptr::null_mut();
        let err = AXUIElementCopyElementAtPosition(system_wide, x as f32, y as f32, &mut el);
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
        Some(redact_focus(FocusReport {
            app,
            role,
            title,
            value,
        }))
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

/// What a `type-text` run delivers to the keyboard, piece by piece: literal
/// text goes through enigo's unicode entry, but every newline becomes a REAL
/// Return keypress. enigo posts a `\n` as a unicode-string event (U+200B +
/// LF on keycode 0 — the `a` key): Terminal renders a stray `<200b>` inside
/// the command plus an `a` on the next prompt, browsers ignore it, and the
/// verification needle never matches — the 2026-08-30 teach-mode incident
/// ("typed the command but never pressed Enter, kept retyping it"). `\r\n`
/// and a lone `\r` count as one Return. Pure — the split is unit-tested.
#[derive(Debug, Clone, PartialEq, Eq)]
enum TypePiece {
    Text(String),
    Return,
}

fn type_pieces(text: &str) -> Vec<TypePiece> {
    // A small model that "ends the command with \n" often JSON-escapes it
    // twice and sends the two CHARACTERS backslash + n (2026-08-30 screenshot:
    // Terminal showed `curl -s ifconfig.me\n` typed literally). Trailing
    // literal escapes are a submit intent, never text — peel them into
    // Returns. Mid-text ones stay literal (`printf("hi\n")` in an editor).
    let mut text = text;
    let mut trailing_returns = 0usize;
    while let Some(rest) = text
        .strip_suffix("\\n")
        .or_else(|| text.strip_suffix("\\r"))
    {
        text = rest.strip_suffix("\\r").unwrap_or(rest);
        trailing_returns += 1;
    }
    let mut out = Vec::new();
    let mut buf = String::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\r' | '\n' => {
                if c == '\r' && chars.peek() == Some(&'\n') {
                    chars.next();
                }
                if !buf.is_empty() {
                    out.push(TypePiece::Text(std::mem::take(&mut buf)));
                }
                out.push(TypePiece::Return);
            }
            _ => buf.push(c),
        }
    }
    if !buf.is_empty() {
        out.push(TypePiece::Text(buf));
    }
    out.extend(std::iter::repeat_n(TypePiece::Return, trailing_returns));
    out
}

/// The text `observe_text_entry` looks for in the focused element: the tail
/// (≤64 chars) of the LAST literal piece — long runs may scroll out of
/// AXValue's head, but the most recent keystrokes are at the end. The
/// submitting newline is not part of it: after `cmd\n` a terminal shows the
/// command's output and a search box has navigated away, so demanding the
/// `\n` in the value would fail every submit and send the model into a
/// retype loop. `None` when nothing verifiable was typed.
fn verification_needle(text: &str) -> Option<String> {
    let last = type_pieces(text)
        .into_iter()
        .filter_map(|p| match p {
            TypePiece::Text(t) if !t.trim().is_empty() => Some(t),
            _ => None,
        })
        .next_back()?;
    let chars: Vec<char> = last.chars().collect();
    Some(if chars.len() > 64 {
        chars[chars.len() - 64..].iter().collect()
    } else {
        last
    })
}

/// The focused element of the FRONTMOST app (its AXFocusedUIElement),
/// retained; the caller releases. Per-app rather than system-wide: the
/// system-wide query answers kAXErrorCannotComplete (-25204) from helper
/// processes, the app element never does (selection probe 2026-09-03).
unsafe fn frontmost_focused_element() -> Result<*mut std::ffi::c_void, String> {
    let pid = crate::capture::macos::frontmost_app_pid()
        .ok_or_else(|| "no frontmost app window".to_string())?;
    let app = AXUIElementCreateApplication(pid);
    if app.is_null() {
        return Err("the frontmost app has no accessibility tree".into());
    }
    let attr = CFString::from_str("AXFocusedUIElement");
    let mut el: *mut std::ffi::c_void = std::ptr::null_mut();
    let err = AXUIElementCopyAttributeValue(app, &*attr, &mut el);
    CFRelease(app);
    if err != 0 || el.is_null() {
        return Err(format!("nothing has keyboard focus (AX error {err})"));
    }
    Ok(el)
}

/// The focused element's selected text, with the focus snapshot
/// (text_selection S4). `None` when nothing is focused; `Some((None,
/// focus))` when the element exposes no selection (a canvas, a web page
/// without a text control) — the caller falls back to cmd+c.
pub fn selected_text_blocking() -> Option<(Option<String>, FocusReport)> {
    unsafe {
        let el = frontmost_focused_element().ok()?;
        let selected = copy_string_attr(el, "AXSelectedText").filter(|s| !s.is_empty());
        let mut pid: i32 = 0;
        let app = (AXUIElementGetPid(el, &mut pid) == 0)
            .then(|| app_name_for_pid(pid))
            .flatten();
        let role = copy_string_attr(el, "AXRole");
        let title =
            copy_string_attr(el, "AXTitle").or_else(|| copy_string_attr(el, "AXDescription"));
        let value = if role.as_deref() == Some("AXSecureTextField") {
            None
        } else {
            copy_string_attr(el, "AXValue")
        };
        CFRelease(el);
        Some((
            selected,
            redact_focus(FocusReport {
                app,
                role,
                title,
                value,
            }),
        ))
    }
}

/// Replace the focused element's selection with `text` (inserting at the
/// caret when nothing is selected) through AXSelectedText. `Ok(false)` =
/// the element does not support it (the caller falls back to cmd+v).
pub fn set_selected_text_blocking(text: &str) -> Result<bool, String> {
    unsafe {
        let el = frontmost_focused_element()?;
        let sel = CFString::from_str("AXSelectedText");
        let value = CFString::from_str(text);
        let err = AXUIElementSetAttributeValue(
            el,
            &*sel,
            &*value as *const CFString as *const std::ffi::c_void,
        );
        CFRelease(el);
        match err {
            0 => Ok(true),
            -25205 | -25206 | -25201 | -25200 => Ok(false),
            other => Err(format!("setting the selection failed (AX error {other})")),
        }
    }
}

/// Post-`type-text` observation: poll (bounded) until the focused element's
/// value contains the typed text's tail, then report the snapshot and whether
/// it matched. AX value propagation lags the keystrokes by tens of
/// milliseconds, so a single immediate read would under-report success.
/// `matched == Some(false)` after the bound is honest uncertainty — some
/// targets (canvases, games, password fields) never echo — and the model is
/// told to treat it as "not confirmed", not as proof of failure.
fn observe_text_entry(text: &str) -> (Option<FocusReport>, Option<bool>) {
    let Some(needle) = verification_needle(text) else {
        // Whitespace-only input (or a bare Return) is unverifiable by
        // containment; report the settled focus without claiming either way.
        return (observe_focus_settled(), None);
    };
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

// libdispatch: hop keyboard synthesis onto the main thread. HIToolbox's
// Text Services Manager (reached by enigo's layout-dependent keycode
// resolution for Key::Unicode — every letter shortcut like cmd+C) asserts
// the main queue on its cache-miss path since recent macOS; calling it from
// a tokio blocking worker is the crash class behind the 2026-07-26/27
// SIGTRAP reports (dispatch_assert_queue_fail in islGetInputSourceList…).
#[link(name = "System", kind = "dylib")]
extern "C" {
    static _dispatch_main_q: std::ffi::c_void;
    fn dispatch_async_f(
        queue: *const std::ffi::c_void,
        context: *mut std::ffi::c_void,
        work: extern "C" fn(*mut std::ffi::c_void),
    );
    fn pthread_main_np() -> std::ffi::c_int;
}

/// How long a main-thread keyboard hop may wait before failing typed. In
/// the running app the main run loop services GCD within milliseconds; the
/// timeout only fires in headless contexts (test harnesses) where blocking
/// forever — or crashing like before — are the alternatives.
const MAIN_HOP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// A throwaway enigo handle for one keyboard action (main thread only —
/// callers hop via [`on_main_keyboard`]). Never opens the permission prompt:
/// the caller's preflight already settled that.
fn keyboard() -> Result<Enigo, String> {
    Enigo::new(&Settings {
        open_prompt_to_get_permissions: false,
        ..Settings::default()
    })
    .map_err(|e| format!("enigo init failed: {e}"))
}

/// One real Return keypress on the main thread — the keystroke a newline in
/// `type-text` stands for.
fn press_return_on_main() -> Result<(), InputError> {
    on_main_keyboard(|| {
        keyboard()?
            .key(Key::Return, Direction::Click)
            .map_err(|e| format!("return press failed: {e}"))
    })?
    .map_err(|detail| InputError::InputFailed { detail })
}

/// Run `f` on the main thread and return its result. Keyboard synthesis
/// ONLY — mouse events have no main-thread requirement and their glide
/// sleeps must stay off the main run loop. On timeout the action fails
/// typed (`input-failed`) instead of crashing the process the way the
/// off-main TSM call did; if the hop then fires late its result is
/// discarded harmlessly (dead channel).
fn on_main_keyboard<T, F>(f: F) -> Result<T, InputError>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    if unsafe { pthread_main_np() } == 1 {
        return Ok(f());
    }
    let (tx, rx) = std::sync::mpsc::channel::<T>();
    type Job = Box<dyn FnOnce() + Send>;
    let job: Job = Box::new(move || {
        let _ = tx.send(f());
    });
    extern "C" fn trampoline(context: *mut std::ffi::c_void) {
        // Re-box exactly what was leaked below; runs once on the main queue.
        let job = unsafe { Box::from_raw(context as *mut Box<dyn FnOnce() + Send>) };
        job();
    }
    let context = Box::into_raw(Box::new(job)) as *mut std::ffi::c_void;
    unsafe {
        dispatch_async_f(
            &_dispatch_main_q as *const std::ffi::c_void,
            context,
            trampoline,
        );
    }
    rx.recv_timeout(MAIN_HOP_TIMEOUT)
        .map_err(|_| InputError::InputFailed {
            detail: "main-thread keyboard dispatch timed out (no run loop?) — the key event was \
                     not synthesized"
                .into(),
        })
}

/// Glide the cursor from wherever it is to `(x, y)` in eased steps instead
/// of teleporting. Purely visual pacing (~190ms worst case): the user asked
/// to SEE the assistant's pointer travel — an instant jump reads as the
/// cursor "appearing" somewhere, a glide reads as an action they can follow
/// (the HUD's follower badge tracks the real cursor, so it rides along).
/// Ease-in-out so departure and arrival are legible. A missing readback of
/// the start point degrades to a direct move — the glide is never the
/// reason an action fails.
fn glide_cursor_to(enigo: &mut Enigo, x: i32, y: i32) -> Result<(), InputError> {
    const STEPS: i32 = 16;
    const STEP_MS: u64 = 11;
    if let Ok((sx, sy)) = cursor_location() {
        let (sx, sy) = (sx.round() as i32, sy.round() as i32);
        let far = (x - sx).abs().max((y - sy).abs()) > 4;
        if far {
            for step in 1..=STEPS {
                // Smoothstep easing: t² · (3 − 2t).
                let t = step as f64 / STEPS as f64;
                let eased = t * t * (3.0 - 2.0 * t);
                let gx = sx + ((x - sx) as f64 * eased).round() as i32;
                let gy = sy + ((y - sy) as f64 * eased).round() as i32;
                enigo
                    .move_mouse(gx, gy, Coordinate::Abs)
                    .map_err(|e| InputError::InputFailed {
                        detail: format!("glide move failed: {e}"),
                    })?;
                std::thread::sleep(std::time::Duration::from_millis(STEP_MS));
            }
            return Ok(());
        }
    }
    enigo
        .move_mouse(x, y, Coordinate::Abs)
        .map_err(|e| InputError::InputFailed {
            detail: format!("move_mouse failed: {e}"),
        })
}

/// How much text still gets the visible per-character typing rhythm. Past
/// this it is paste-length content — animating it would take many seconds
/// for no legibility gain, so it goes out in one burst.
const TYPE_ANIMATE_MAX_CHARS: usize = 200;

/// Ceiling on the whole animated entry, so a near-threshold text never
/// crawls: the per-char delay shrinks as the text grows.
const TYPE_ANIMATE_TOTAL_MS: u64 = 1600;

/// Fastest per-char cadence worth animating at all.
const TYPE_CHAR_MS_MAX: u64 = 26;

/// The per-character cadence for `count` characters, or `None` when the
/// text should burst instead (empty, or paste-length past the animate cap).
/// Pure — the pacing policy is testable without a keyboard.
fn paced_char_delay(count: usize) -> Option<std::time::Duration> {
    if count == 0 || count > TYPE_ANIMATE_MAX_CHARS {
        return None;
    }
    Some(std::time::Duration::from_millis(
        (TYPE_ANIMATE_TOTAL_MS / count as u64).min(TYPE_CHAR_MS_MAX),
    ))
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
            glide_cursor_to(&mut enigo, x, y)?;
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
        InputAction::MouseClick {
            button,
            x,
            y,
            clicks,
        } => {
            // A coordinate-bearing click moves to the target first (the model's
            // "click at (x,y)"); a coordless click fires at the cursor. Both
            // x and y are validated present-together upstream, so `if let`
            // on the pair is enough here.
            if let (Some(x), Some(y)) = (x, y) {
                glide_cursor_to(&mut enigo, x, y)?;
                // enigo's button() clicks at the SYSTEM cursor, not at (x,y) —
                // without this wait the click fires at the stale pre-move
                // position (see wait_for_cursor_commit docs).
                wait_for_cursor_commit(x, y)?;
            }
            // Hit-test BEFORE the button events: this is the element the
            // mousedown will hit, read while it still exists (a link click
            // navigates the page away moments later).
            let clicked_element = cursor_location()
                .ok()
                .and_then(|(px, py)| element_at_point(px.round() as i32, py.round() as i32));
            // Multi-click (validated 1..=3 upstream): rapid same-position
            // clicks inside the system double-click interval register as
            // double/triple clicks.
            for i in 0..clicks.unwrap_or(1).max(1) {
                if i > 0 {
                    std::thread::sleep(std::time::Duration::from_millis(60));
                }
                enigo
                    .button(map_button(button), Direction::Click)
                    .map_err(|e| InputError::InputFailed {
                        detail: format!("button click failed: {e}"),
                    })?;
            }
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
                clicked_element,
            }
        }
        InputAction::MouseDrag {
            button,
            from_x,
            from_y,
            to_x,
            to_y,
        } => {
            // Press at the origin, glide in steps (apps track motion, and an
            // instant teleport breaks drag recognition), release at the
            // destination. Each waypoint is a real synthesized move.
            enigo
                .move_mouse(from_x, from_y, Coordinate::Abs)
                .map_err(|e| InputError::InputFailed {
                    detail: format!("move to drag origin failed: {e}"),
                })?;
            wait_for_cursor_commit(from_x, from_y)?;
            enigo
                .button(map_button(button), Direction::Press)
                .map_err(|e| InputError::InputFailed {
                    detail: format!("drag press failed: {e}"),
                })?;
            const STEPS: i32 = 14;
            let mut glide_error = None;
            for step in 1..=STEPS {
                let gx = from_x + (to_x - from_x) * step / STEPS;
                let gy = from_y + (to_y - from_y) * step / STEPS;
                if let Err(e) = enigo.move_mouse(gx, gy, Coordinate::Abs) {
                    glide_error = Some(format!("drag glide failed: {e}"));
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(12));
            }
            // The button is DOWN: release unconditionally, even on a glide
            // failure — a stuck pressed button would hold the user's mouse.
            let released = enigo.button(map_button(button), Direction::Release);
            if let Some(detail) = glide_error {
                return Err(InputError::InputFailed { detail });
            }
            released.map_err(|e| InputError::InputFailed {
                detail: format!("drag release failed: {e}"),
            })?;
            let cursor = wait_for_cursor_commit(to_x, to_y)?;
            ActionReport {
                cursor: Some(cursor),
                focus: observe_focus_settled(),
                text_entered: None,
                clicked_element: None,
            }
        }
        InputAction::Scroll {
            x,
            y,
            delta_x,
            delta_y,
        } => {
            if let (Some(x), Some(y)) = (x, y) {
                enigo
                    .move_mouse(x, y, Coordinate::Abs)
                    .map_err(|e| InputError::InputFailed {
                        detail: format!("move before scroll failed: {e}"),
                    })?;
                wait_for_cursor_commit(x, y)?;
            }
            let dy = delta_y.unwrap_or(0);
            let dx = delta_x.unwrap_or(0);
            if dy != 0 {
                enigo
                    .scroll(dy, enigo::Axis::Vertical)
                    .map_err(|e| InputError::InputFailed {
                        detail: format!("vertical scroll failed: {e}"),
                    })?;
            }
            if dx != 0 {
                enigo
                    .scroll(dx, enigo::Axis::Horizontal)
                    .map_err(|e| InputError::InputFailed {
                        detail: format!("horizontal scroll failed: {e}"),
                    })?;
            }
            let cursor = cursor_location().ok().map(|(px, py)| CursorPosition {
                x: px.round() as i32,
                y: py.round() as i32,
            });
            ActionReport {
                cursor,
                focus: None,
                text_entered: None,
                clicked_element: None,
            }
        }
        InputAction::TypeText { text } => {
            // Keyboard synthesis happens ON THE MAIN THREAD (TSM assert —
            // see on_main_keyboard); the pacing sleeps stay here, off-main,
            // so the UI never freezes for the typing rhythm. Newlines are
            // delivered as real Return keypresses (see type_pieces).
            let pieces = type_pieces(&text);
            let units: usize = pieces
                .iter()
                .map(|p| match p {
                    TypePiece::Text(t) => t.chars().count(),
                    TypePiece::Return => 1,
                })
                .sum();
            match paced_char_delay(units) {
                None => {
                    for piece in pieces {
                        match piece {
                            TypePiece::Text(burst) => {
                                on_main_keyboard(move || {
                                    keyboard()?
                                        .text(&burst)
                                        .map_err(|e| format!("text entry failed: {e}"))
                                })?
                                .map_err(|detail| InputError::InputFailed { detail })?;
                            }
                            TypePiece::Return => press_return_on_main()?,
                        }
                    }
                }
                Some(delay) => {
                    let mut i = 0usize;
                    let pace = |i: usize| {
                        if i > 0 && !delay.is_zero() {
                            std::thread::sleep(delay);
                        }
                    };
                    for piece in pieces {
                        match piece {
                            TypePiece::Text(run) => {
                                for ch in run.chars() {
                                    pace(i);
                                    on_main_keyboard(move || {
                                        let mut buf = [0u8; 4];
                                        keyboard()?.text(ch.encode_utf8(&mut buf)).map_err(|e| {
                                            format!("text entry failed at char {i}: {e}")
                                        })
                                    })?
                                    .map_err(|detail| InputError::InputFailed { detail })?;
                                    i += 1;
                                }
                            }
                            TypePiece::Return => {
                                pace(i);
                                press_return_on_main()?;
                                i += 1;
                            }
                        }
                    }
                }
            }
            let (focus, text_entered) = observe_text_entry(&text);
            ActionReport {
                cursor: None,
                focus,
                text_entered,
                clicked_element: None,
            }
        }
        InputAction::KeyPress { key, modifiers } => {
            let k = key_from_str(&key)?;
            let held: Vec<Key> = modifiers
                .as_deref()
                .unwrap_or(&[])
                .iter()
                .map(|m| modifier_key(m))
                .collect::<Result<_, _>>()?;
            // The whole hold-click-release sequence runs on the main thread:
            // resolving a Key::Unicode (any letter shortcut) walks the TSM
            // keyboard layout, which asserts the main queue (the crash-log
            // class this replaces). No sleeps inside — the hop is brief.
            on_main_keyboard(move || {
                let mut e = Enigo::new(&Settings {
                    open_prompt_to_get_permissions: false,
                    ..Settings::default()
                })
                .map_err(|e| format!("enigo init failed: {e}"))?;
                for m in &held {
                    e.key(*m, Direction::Press)
                        .map_err(|e| format!("modifier press failed: {e}"))?;
                }
                let pressed = e.key(k, Direction::Click);
                for m in held.iter().rev() {
                    let _ = e.key(*m, Direction::Release);
                }
                pressed.map_err(|e| format!("key press failed: {e}"))
            })?
            .map_err(|detail| InputError::InputFailed { detail })?;
            ActionReport {
                cursor: None,
                focus: observe_focus_settled(),
                text_entered: None,
                clicked_element: None,
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
/// Map a validated modifier name to its enigo key. Validation upstream
/// guarantees the vocabulary; unknown still errors typed, never panics.
fn modifier_key(name: &str) -> Result<Key, InputError> {
    match name {
        "cmd" => Ok(Key::Meta),
        "ctrl" => Ok(Key::Control),
        "alt" => Ok(Key::Alt),
        "shift" => Ok(Key::Shift),
        other => Err(InputError::InputFailed {
            detail: format!("unknown modifier {other:?} (cmd|ctrl|alt|shift)"),
        }),
    }
}

/// string maps to `Key::Unicode`. Anything else is a typed `input-failed` so the
/// model gets an actionable error instead of a silent no-op (R007).
fn key_from_str(key: &str) -> Result<Key, InputError> {
    let named = match key.to_ascii_lowercase().as_str() {
        "return" | "enter" | "newline" | "linefeed" | "\n" | "\r" | "\r\n" => Some(Key::Return),
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
    fn typing_pace_animates_short_text_and_bursts_long_text() {
        // Empty and paste-length inputs burst (no pacing at all).
        assert_eq!(paced_char_delay(0), None);
        assert_eq!(paced_char_delay(TYPE_ANIMATE_MAX_CHARS + 1), None);
        // Short text gets the full visible cadence…
        assert_eq!(
            paced_char_delay(10),
            Some(std::time::Duration::from_millis(TYPE_CHAR_MS_MAX))
        );
        // …and the cadence compresses near the cap so the whole entry stays
        // inside the total budget instead of crawling.
        let at_cap = paced_char_delay(TYPE_ANIMATE_MAX_CHARS).unwrap();
        assert!(at_cap.as_millis() as u64 * TYPE_ANIMATE_MAX_CHARS as u64 <= TYPE_ANIMATE_TOTAL_MS);
        assert!(!at_cap.is_zero());
    }

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

    /// Incident 2026-08-30 (teach mode, Terminal): `cmd\n` must type the
    /// command and press a REAL Return — never hand the `\n` to enigo's
    /// unicode path.
    #[test]
    fn type_pieces_turn_newlines_into_return_presses() {
        use TypePiece::*;
        assert_eq!(
            type_pieces("echo hi\n"),
            vec![Text("echo hi".into()), Return]
        );
        assert_eq!(
            type_pieces("a\r\nb\rc\n\n"),
            vec![
                Text("a".into()),
                Return,
                Text("b".into()),
                Return,
                Text("c".into()),
                Return,
                Return
            ],
            "\\r\\n is one Return; a lone \\r is one Return; consecutive newlines each press"
        );
        assert_eq!(type_pieces("plain"), vec![Text("plain".into())]);
        assert_eq!(type_pieces("\n"), vec![Return]);
        assert!(type_pieces("").is_empty());
    }

    /// 2026-08-30 screenshot: the 9B sent the two characters `\` `n` (double
    /// JSON escaping) and Terminal typed them literally. A TRAILING literal
    /// escape is a submit; a mid-text one is text the user asked for.
    #[test]
    fn trailing_literal_backslash_n_is_a_return_but_mid_text_stays_literal() {
        use TypePiece::*;
        assert_eq!(
            type_pieces(r"curl -s ifconfig.me\n"),
            vec![Text("curl -s ifconfig.me".into()), Return]
        );
        assert_eq!(
            type_pieces(r"ls\r\n"),
            vec![Text("ls".into()), Return],
            r"a literal \r\n is one Return"
        );
        assert_eq!(
            type_pieces(r"echo hi\n\n"),
            vec![Text("echo hi".into()), Return, Return]
        );
        assert_eq!(
            type_pieces(r#"printf("hi\n")"#),
            vec![Text(r#"printf("hi\n")"#.into())],
            "code being typed keeps its escapes"
        );
        assert_eq!(
            verification_needle(r"curl -s ifconfig.me\n").as_deref(),
            Some("curl -s ifconfig.me")
        );
    }

    /// The submit newline is not something the field can echo back: the
    /// needle is the last literal run, so `cmd\n` verifies on `cmd` instead
    /// of failing every submit and driving a retype loop.
    #[test]
    fn verification_needle_skips_the_submitting_newline() {
        assert_eq!(verification_needle("echo hi\n").as_deref(), Some("echo hi"));
        assert_eq!(
            verification_needle("first\nsecond").as_deref(),
            Some("second")
        );
        assert_eq!(verification_needle("plain").as_deref(), Some("plain"));
        assert_eq!(
            verification_needle("\n"),
            None,
            "a bare Return is unverifiable"
        );
        assert_eq!(verification_needle("   "), None);
        let long: String = "x".repeat(100);
        assert_eq!(
            verification_needle(&long).map(|n| n.chars().count()),
            Some(64),
            "tail-64 of the last run"
        );
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
        // A model that "presses" a newline character means Return, never a
        // unicode LF event (which Terminal renders as `<200b>` garbage).
        assert_eq!(key_from_str("\n").unwrap(), Key::Return);
        assert_eq!(key_from_str("\r").unwrap(), Key::Return);
        assert_eq!(key_from_str("newline").unwrap(), Key::Return);
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
                clicks: None,
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
                modifiers: None,
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
