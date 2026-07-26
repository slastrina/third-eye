//! Accessibility-tree element harvest (accuracy v2, 2026-07-27): the focused
//! app's REAL interactive controls — buttons, links, fields — with their
//! exact frames in global top-left screen points.
//!
//! Why this exists: OCR boxes are quantized text guesses; AX frames are the
//! authoritative hit rectangles the app itself reports. An AX-sourced
//! element clicks dead center on the actual control, and its role tells the
//! model *what* it is clicking, not just what the label says.
//!
//! Contract: best-effort and additive. Any failure — no AX support in the
//! target app, a denied attribute, a wedged tree — yields fewer (or zero)
//! elements, never an error; the screen_query result then stands on OCR
//! alone. The walk is bounded (depth and element count) so one pathological
//! app (an electron soup of thousands of nodes) cannot stall the tool.
//! Coordinates are transient, never persisted (R011).

use objc2_core_foundation::{CFArray, CFRetained, CFString, CGRect};

use super::ScreenElement;

/// Roles harvested as clickable targets. Menu items and tabs are included:
/// when visible they carry real frames; hidden ones report zero-size frames
/// and are dropped by the size gate.
const INTERACTIVE_ROLES: &[&str] = &[
    "AXButton",
    "AXLink",
    "AXTextField",
    "AXTextArea",
    "AXSearchField",
    "AXCheckBox",
    "AXRadioButton",
    "AXPopUpButton",
    "AXComboBox",
    "AXMenuItem",
    "AXTab",
    "AXMenuButton",
    "AXDisclosureTriangle",
];

/// Bounded walk limits: depth covers real app hierarchies (Chrome's web
/// area sits ~8 deep); the element cap keeps the tool result model-sized.
const MAX_DEPTH: usize = 12;
const MAX_ELEMENTS: usize = 400;
/// Nodes visited cap — a tree can be enormous without yielding elements.
const MAX_VISITED: usize = 6000;

/// Wall-clock budgets. Every AX attribute copy is a SYNCHRONOUS IPC round
/// trip into the target app — a heavy Chrome page can mean six figures of
/// them, which wedged a whole run for minutes (the app-froze report,
/// 2026-07-27: the stop flag is only honored between tools, and ghost mode
/// made the stuck overlay look dead). The walks now bail at the deadline
/// with whatever they have.
const INTERACTIVE_BUDGET: std::time::Duration = std::time::Duration::from_millis(2500);
const PAGE_TEXT_BUDGET: std::time::Duration = std::time::Duration::from_millis(4000);

// Raw HIServices FFI, the input/macos.rs precedent (no binding crate).
#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXUIElementCreateApplication(pid: i32) -> *mut std::ffi::c_void;
    fn AXUIElementCopyAttributeValue(
        element: *mut std::ffi::c_void,
        attribute: *const CFString,
        value: *mut *mut std::ffi::c_void,
    ) -> i32;
    fn AXValueGetValue(
        value: *mut std::ffi::c_void,
        the_type: u32,
        value_ptr: *mut std::ffi::c_void,
    ) -> bool;
    fn AXUIElementSetAttributeValue(
        element: *mut std::ffi::c_void,
        attribute: *const CFString,
        value: *const std::ffi::c_void,
    ) -> i32;
}
#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFRelease(cf: *mut std::ffi::c_void);
    fn CFGetTypeID(cf: *mut std::ffi::c_void) -> usize;
    fn CFStringGetTypeID() -> usize;
    fn CFArrayGetTypeID() -> usize;
}

/// `kAXValueTypeCGRect` — HIServices' AXValue type tag for CGRect payloads.
const AX_VALUE_TYPE_CGRECT: u32 = 3;

/// Chromium (Chrome/Electron/Brave…) builds its AX tree LAZILY — only once
/// an assistive client announces itself. Without this, the first harvests
/// of a Chrome window see a near-empty tree, screen_query falls back to
/// OCR text whose boxes are not the links' real hitboxes, and clicks on
/// Google results take several attempts (2026-07-27 report). Setting
/// `AXManualAccessibility` on the app element is Chromium's documented
/// opt-in; non-Chromium apps return attribute-unsupported, harmlessly.
unsafe fn enable_chromium_accessibility(app_element: *mut std::ffi::c_void) {
    let attr = CFString::from_str("AXManualAccessibility");
    if let Some(yes) = objc2_core_foundation::kCFBooleanTrue {
        let err = AXUIElementSetAttributeValue(
            app_element,
            &*attr,
            yes as *const objc2_core_foundation::CFBoolean as *const std::ffi::c_void,
        );
        if err != 0 {
            log::trace!("ax: AXManualAccessibility not accepted (err {err}) — not Chromium");
        }
    }
}

/// Copy one string attribute (None when absent / not a CFString).
unsafe fn copy_string(element: *mut std::ffi::c_void, name: &str) -> Option<String> {
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

/// The element's frame in global top-left screen points, via the AXFrame
/// AXValue (kAXValueTypeCGRect).
unsafe fn copy_frame(element: *mut std::ffi::c_void) -> Option<CGRect> {
    let attr = CFString::from_str("AXFrame");
    let mut value: *mut std::ffi::c_void = std::ptr::null_mut();
    if AXUIElementCopyAttributeValue(element, &*attr, &mut value) != 0 || value.is_null() {
        return None;
    }
    let mut rect = CGRect::default();
    let ok = AXValueGetValue(
        value,
        AX_VALUE_TYPE_CGRECT,
        &mut rect as *mut CGRect as *mut std::ffi::c_void,
    );
    CFRelease(value);
    ok.then_some(rect)
}

/// The element's children as a retained CFArray of AXUIElements.
unsafe fn copy_children(element: *mut std::ffi::c_void) -> Option<CFRetained<CFArray>> {
    let attr = CFString::from_str("AXChildren");
    let mut value: *mut std::ffi::c_void = std::ptr::null_mut();
    if AXUIElementCopyAttributeValue(element, &*attr, &mut value) != 0 || value.is_null() {
        return None;
    }
    if CFGetTypeID(value) != CFArrayGetTypeID() {
        CFRelease(value);
        return None;
    }
    // Take ownership of the +1 retained array returned by the copy.
    Some(CFRetained::from_raw(std::ptr::NonNull::new_unchecked(
        value as *mut CFArray,
    )))
}

/// The one AX label worth showing the model: title, else description, else
/// (for fields) the current value's head. Empty means the control has no
/// usable handle and is skipped — an unlabeled target the model cannot name
/// is noise.
unsafe fn element_label(element: *mut std::ffi::c_void, role: &str) -> Option<String> {
    let title = copy_string(element, "AXTitle")
        .filter(|t| !t.trim().is_empty())
        .or_else(|| copy_string(element, "AXDescription").filter(|t| !t.trim().is_empty()));
    if title.is_some() {
        return title;
    }
    // Fields identify by content or placeholder; secure fields never leak.
    if role == "AXSecureTextField" {
        return None;
    }
    copy_string(element, "AXValue")
        .filter(|v| !v.trim().is_empty())
        .map(|v| v.chars().take(80).collect())
        .or_else(|| copy_string(element, "AXPlaceholderValue").filter(|t| !t.trim().is_empty()))
}

/// Harvest the interactive elements of `pid`'s AX tree. Blocking (AX IPC is
/// synchronous) — callers lift it onto `spawn_blocking`. `app_name` stamps
/// each element's `app` field, matching OCR attribution's namespace.
pub fn interactive_elements_blocking(pid: i32, app_name: &str) -> Vec<ScreenElement> {
    let deadline = std::time::Instant::now() + INTERACTIVE_BUDGET;
    let mut out = walk_interactive(pid, app_name, deadline);
    // Chromium's tree materializes ASYNCHRONOUSLY after the activation the
    // walk just requested — an empty first harvest with budget left gets
    // one short settle + re-walk, so the SAME query already sees the links.
    if out.is_empty()
        && std::time::Instant::now() + std::time::Duration::from_millis(400) < deadline
    {
        std::thread::sleep(std::time::Duration::from_millis(300));
        out = walk_interactive(pid, app_name, deadline);
    }
    out
}

fn walk_interactive(pid: i32, app_name: &str, deadline: std::time::Instant) -> Vec<ScreenElement> {
    let mut out = Vec::new();
    unsafe {
        let root = AXUIElementCreateApplication(pid);
        if root.is_null() {
            return out;
        }
        enable_chromium_accessibility(root);
        // Iterative DFS with owned queue of (element, depth). Every element
        // pointer in the queue is retained by its parent CFArray, which we
        // keep alive in `arrays` until the walk ends.
        let mut arrays: Vec<CFRetained<CFArray>> = Vec::new();
        let mut queue: Vec<(*mut std::ffi::c_void, usize)> = vec![(root, 0)];
        let mut visited = 0usize;
        while let Some((el, depth)) = queue.pop() {
            visited += 1;
            if visited > MAX_VISITED
                || out.len() >= MAX_ELEMENTS
                || std::time::Instant::now() >= deadline
            {
                break;
            }
            if let Some(role) = copy_string(el, "AXRole") {
                if INTERACTIVE_ROLES.contains(&role.as_str()) {
                    if let (Some(frame), Some(label)) = (copy_frame(el), element_label(el, &role)) {
                        let (x, y) = (frame.origin.x, frame.origin.y);
                        let (w, h) = (frame.size.width, frame.size.height);
                        // Zero/degenerate frames are hidden controls (closed
                        // menus, collapsed toolbars) — not clickable.
                        if w >= 3.0 && h >= 3.0 {
                            out.push(ScreenElement {
                                text: label,
                                x: x.round() as i32,
                                y: y.round() as i32,
                                width: w.round() as i32,
                                height: h.round() as i32,
                                cx: (x + w / 2.0).round() as i32,
                                cy: (y + h / 2.0).round() as i32,
                                app: Some(app_name.to_string()),
                                role: Some(role.clone()),
                            });
                        }
                    }
                }
            }
            if depth < MAX_DEPTH {
                if let Some(children) = copy_children(el) {
                    let count = children.count() as usize;
                    for i in 0..count {
                        let child = children.value_at_index(i as isize) as *mut std::ffi::c_void;
                        if !child.is_null() {
                            queue.push((child, depth + 1));
                        }
                    }
                    arrays.push(children);
                }
            }
        }
        CFRelease(root);
        drop(arrays);
    }
    log::debug!(
        "screen_query: AX harvest for {app_name:?} (pid {pid}) yielded {} element(s)",
        out.len()
    );
    out
}

/// Text-bearing roles for the page dump, in reading order via DFS.
const TEXT_ROLES: &[&str] = &["AXStaticText", "AXHeading", "AXTextArea", "AXTextField"];

/// Cap on harvested page text (chars) — the dump enters model context.
pub const PAGE_TEXT_MAX_CHARS: usize = 14_000;
/// Text walks go deeper and wider than the clickable-element walk: a long
/// article is thousands of nodes that yield text.
const TEXT_MAX_DEPTH: usize = 24;
const TEXT_MAX_VISITED: usize = 30_000;

/// Dump the readable text of `pid`'s AX tree in tree order — the
/// "read this page to me" primitive (continuity fix, 2026-07-27): a page
/// the model opened last turn is still on screen, and a follow-up about
/// its content should be answered by READING it, not by claiming no
/// access. Best-effort like the element harvest: any failure yields less
/// (or empty) text, never an error. Blocking — lift onto `spawn_blocking`.
pub fn page_text_blocking(pid: i32) -> String {
    let mut out = String::new();
    let deadline = std::time::Instant::now() + PAGE_TEXT_BUDGET;
    unsafe {
        let root = AXUIElementCreateApplication(pid);
        if root.is_null() {
            return out;
        }
        enable_chromium_accessibility(root);
        let mut arrays: Vec<CFRetained<CFArray>> = Vec::new();
        // A stack popped from the END walks depth-first in document order
        // when children are pushed reversed.
        let mut stack: Vec<(*mut std::ffi::c_void, usize)> = vec![(root, 0)];
        let mut visited = 0usize;
        while let Some((el, depth)) = stack.pop() {
            visited += 1;
            if visited > TEXT_MAX_VISITED
                || out.len() >= PAGE_TEXT_MAX_CHARS
                || std::time::Instant::now() >= deadline
            {
                out.push_str("\n[…truncated]");
                break;
            }
            if let Some(role) = copy_string(el, "AXRole") {
                if TEXT_ROLES.contains(&role.as_str()) && role != "AXSecureTextField" {
                    let text = copy_string(el, "AXValue")
                        .filter(|t| !t.trim().is_empty())
                        .or_else(|| copy_string(el, "AXTitle").filter(|t| !t.trim().is_empty()));
                    if let Some(text) = text {
                        if !out.is_empty() {
                            out.push('\n');
                        }
                        out.push_str(text.trim());
                    }
                }
            }
            if depth < TEXT_MAX_DEPTH {
                if let Some(children) = copy_children(el) {
                    let count = children.count() as usize;
                    for i in (0..count).rev() {
                        let child = children.value_at_index(i as isize) as *mut std::ffi::c_void;
                        if !child.is_null() {
                            stack.push((child, depth + 1));
                        }
                    }
                    arrays.push(children);
                }
            }
        }
        CFRelease(root);
        drop(arrays);
    }
    log::debug!(
        "read_page: harvested {} char(s) of text from pid {pid}",
        out.len()
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Live AX walk against the Finder (always running on a mac session).
    /// Needs Accessibility trust — ignored in the default suite, run with
    /// `cargo test -- --ignored ax_harvest` on a granted machine.
    #[test]
    #[ignore]
    fn ax_harvest_finds_finder_controls() {
        let pid =
            crate::appfocus::macos::pid_for_app_name("Finder").expect("Finder is always running");
        let elements = interactive_elements_blocking(pid, "Finder");
        for el in &elements {
            assert!(el.role.is_some());
            assert!(el.width >= 3 && el.height >= 3);
            assert!(!el.text.trim().is_empty());
            // The precomputed centre sits inside the frame.
            assert!(el.cx >= el.x && el.cx <= el.x + el.width);
            assert!(el.cy >= el.y && el.cy <= el.y + el.height);
        }
        eprintln!("harvested {} Finder element(s)", elements.len());
    }
}
