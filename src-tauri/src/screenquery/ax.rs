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
        // keep alive in `arrays` until the walk ends. Windows before menus.
        let mut arrays: Vec<CFRetained<CFArray>> = Vec::new();
        let mut queue = windows_first_queue(root, &mut arrays);
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

/// Seed a DFS queue with the app's top-level children so that WINDOWS are
/// walked before the MENU BAR (the stack pops last-pushed first). Chrome's
/// menus alone are hundreds of items and exhausted the visit budget before
/// the window was reached (ui_action live probe, 2026-09-03). The children
/// array is kept alive in `arrays` like every other level.
unsafe fn windows_first_queue(
    root: *mut std::ffi::c_void,
    arrays: &mut Vec<CFRetained<CFArray>>,
) -> Vec<(*mut std::ffi::c_void, usize)> {
    let mut queue = Vec::new();
    if let Some(children) = copy_children(root) {
        let count = children.count() as usize;
        let mut windows = Vec::new();
        let mut menus = Vec::new();
        for i in 0..count {
            let child = children.value_at_index(i as isize) as *mut std::ffi::c_void;
            if child.is_null() {
                continue;
            }
            match copy_string(child, "AXRole").as_deref() {
                Some("AXMenuBar") | Some("AXMenuBarItem") | Some("AXMenu") => menus.push(child),
                _ => windows.push(child),
            }
        }
        for m in menus {
            queue.push((m, 1));
        }
        for w in windows {
            queue.push((w, 1));
        }
        arrays.push(children);
    }
    queue
}

// ---------------------------------------------------------------------------
// ui_action (system tools S2): act on an element by identity, not pixels
// ---------------------------------------------------------------------------

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXUIElementPerformAction(element: *mut std::ffi::c_void, action: *const CFString) -> i32;
    fn AXUIElementCreateSystemWide() -> *mut std::ffi::c_void;
}

/// What `ui_action` does to the matched element.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AxAct {
    Press,
    SetValue(String),
    Focus,
}

/// What the OS reported after the action — the tool's `verified` block.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AxActionReport {
    pub matched_role: String,
    pub matched_title: String,
    /// The element's AXValue after the action (text fields, checkboxes).
    pub value_after: Option<String>,
    /// System-wide focused element after the action, as "AXRole: title".
    pub focused_after: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AxActionError {
    /// No element matched; `candidates` are the labels that were there.
    NotFound {
        candidates: Vec<String>,
    },
    /// Several elements matched loosely and none exactly.
    Ambiguous {
        candidates: Vec<String>,
    },
    /// The element does not support that action (a label has no AXPress).
    Unsupported {
        detail: String,
    },
    Failed {
        detail: String,
    },
}

impl AxActionError {
    pub fn kind(&self) -> &'static str {
        match self {
            AxActionError::NotFound { .. } => "not-found",
            AxActionError::Ambiguous { .. } => "ambiguous",
            AxActionError::Unsupported { .. } => "unsupported",
            AxActionError::Failed { .. } => "action-failed",
        }
    }
}

impl std::fmt::Display for AxActionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AxActionError::NotFound { candidates } => write!(
                f,
                "no element with that title in the focused app; on screen: {}",
                candidates.join(" | ")
            ),
            AxActionError::Ambiguous { candidates } => write!(
                f,
                "several elements match — name one exactly: {}",
                candidates.join(" | ")
            ),
            AxActionError::Unsupported { detail } | AxActionError::Failed { detail } => {
                f.write_str(detail)
            }
        }
    }
}

/// Normalize a role filter: "button" → "AXButton", "AXLink" stays.
pub fn normalize_role(role: &str) -> String {
    let r = role.trim();
    if r.is_empty() {
        return String::new();
    }
    if r.to_ascii_lowercase().starts_with("ax") {
        format!("AX{}", &r[2..])
    } else {
        let mut chars = r.chars();
        let first = chars
            .next()
            .map(|c| c.to_ascii_uppercase())
            .unwrap_or_default();
        format!("AX{first}{}", chars.as_str())
    }
}

/// Choose the element `title` names among `candidates` (role, label). An
/// exact (case-insensitive) label wins; otherwise exactly one containing
/// match; several loose matches are ambiguous; none is not-found. Pure —
/// the whole matching policy.
pub fn pick_target(
    candidates: &[(String, String)],
    title: &str,
    role: Option<&str>,
) -> Result<usize, AxActionError> {
    let want = title.trim().to_lowercase();
    let role = role.map(normalize_role).filter(|r| !r.is_empty());
    let pool: Vec<(usize, &(String, String))> = candidates
        .iter()
        .enumerate()
        .filter(|(_, (r, _))| {
            role.as_deref()
                .is_none_or(|want_role| r.eq_ignore_ascii_case(want_role))
        })
        .collect();
    if let Some((i, _)) = pool
        .iter()
        .find(|(_, (_, l))| l.trim().to_lowercase() == want)
    {
        return Ok(*i);
    }
    let loose: Vec<&(usize, &(String, String))> = pool
        .iter()
        .filter(|(_, (_, l))| l.to_lowercase().contains(&want))
        .collect();
    let labels = |items: &[&(usize, &(String, String))]| -> Vec<String> {
        items
            .iter()
            .take(8)
            .map(|(_, (r, l))| format!("{r} {l:?}"))
            .collect()
    };
    match loose.len() {
        1 => Ok(loose[0].0),
        0 => Err(AxActionError::NotFound {
            candidates: labels(&pool.iter().collect::<Vec<_>>()),
        }),
        _ => Err(AxActionError::Ambiguous {
            candidates: labels(&loose),
        }),
    }
}

unsafe fn focused_summary() -> Option<String> {
    let system = AXUIElementCreateSystemWide();
    if system.is_null() {
        return None;
    }
    let attr = CFString::from_str("AXFocusedUIElement");
    let mut el: *mut std::ffi::c_void = std::ptr::null_mut();
    let err = AXUIElementCopyAttributeValue(system, &*attr, &mut el);
    CFRelease(system);
    if err != 0 || el.is_null() {
        return None;
    }
    let role = copy_string(el, "AXRole").unwrap_or_default();
    let title = copy_string(el, "AXTitle")
        .or_else(|| copy_string(el, "AXDescription"))
        .unwrap_or_default();
    CFRelease(el);
    Some(format!("{role}: {title}"))
}

/// Find the element `title` (optionally `role`) names in `pid`'s
/// interactive tree and perform `act` on it, then read the element and the
/// system focus back. Blocking — lift onto `spawn_blocking`. The walk
/// shares the interactive harvest's budgets; the chosen element stays
/// alive because its parent arrays are held until the action is done.
pub fn perform_ui_action_blocking(
    pid: i32,
    title: &str,
    role: Option<&str>,
    act: AxAct,
) -> Result<AxActionReport, AxActionError> {
    let deadline = std::time::Instant::now() + INTERACTIVE_BUDGET;
    unsafe {
        let root = AXUIElementCreateApplication(pid);
        if root.is_null() {
            return Err(AxActionError::Failed {
                detail: "the app has no accessibility tree (not running?)".into(),
            });
        }
        enable_chromium_accessibility(root);
        let mut arrays: Vec<CFRetained<CFArray>> = Vec::new();
        // The stack pops LAST-pushed first. The app's children are windows
        // and the menu bar; Chrome's menus alone are hundreds of items and
        // exhausted the budget before the window was reached (live probe
        // 2026-09-03). Push the menu bar FIRST (walked last) and windows
        // last (walked first): the controls the user sees come first.
        let mut queue = windows_first_queue(root, &mut arrays);
        let mut found: Vec<(*mut std::ffi::c_void, String, String)> = Vec::new();
        let mut visited = 0usize;
        while let Some((el, depth)) = queue.pop() {
            visited += 1;
            if visited > MAX_VISITED
                || found.len() >= MAX_ELEMENTS
                || std::time::Instant::now() >= deadline
            {
                break;
            }
            if let Some(r) = copy_string(el, "AXRole") {
                if INTERACTIVE_ROLES.contains(&r.as_str()) {
                    if let Some(label) = element_label(el, &r) {
                        found.push((el, r, label));
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
        let candidates: Vec<(String, String)> = found
            .iter()
            .map(|(_, r, l)| (r.clone(), l.clone()))
            .collect();
        let idx = match pick_target(&candidates, title, role) {
            Ok(i) => i,
            Err(e) => {
                CFRelease(root);
                return Err(e);
            }
        };
        let (el, matched_role, matched_title) = found[idx].clone();
        let err = match &act {
            AxAct::Press => {
                let action = CFString::from_str("AXPress");
                AXUIElementPerformAction(el, &*action)
            }
            AxAct::SetValue(value) => {
                let attr = CFString::from_str("AXValue");
                let v = CFString::from_str(value);
                AXUIElementSetAttributeValue(
                    el,
                    &*attr,
                    &*v as *const CFString as *const std::ffi::c_void,
                )
            }
            AxAct::Focus => {
                let attr = CFString::from_str("AXFocused");
                match objc2_core_foundation::kCFBooleanTrue {
                    Some(yes) => AXUIElementSetAttributeValue(
                        el,
                        &*attr,
                        yes as *const objc2_core_foundation::CFBoolean as *const std::ffi::c_void,
                    ),
                    None => -1,
                }
            }
        };
        let result = if err == 0 {
            std::thread::sleep(std::time::Duration::from_millis(150));
            Ok(AxActionReport {
                matched_role,
                matched_title,
                value_after: copy_string(el, "AXValue"),
                focused_after: focused_summary(),
            })
        } else {
            // -25205 kAXErrorActionUnsupported, -25206 kAXErrorAttributeUnsupported.
            let detail = format!(
                "{} on {matched_role} {matched_title:?} failed (AX error {err})",
                match &act {
                    AxAct::Press => "press",
                    AxAct::SetValue(_) => "set_value",
                    AxAct::Focus => "focus",
                }
            );
            Err(if err == -25205 || err == -25206 || err == -25201 {
                AxActionError::Unsupported { detail }
            } else {
                AxActionError::Failed { detail }
            })
        };
        drop(arrays);
        CFRelease(root);
        result
    }
}

#[cfg(test)]
mod ui_action_tests {
    use super::*;

    fn c(role: &str, label: &str) -> (String, String) {
        (role.into(), label.into())
    }

    #[test]
    fn roles_normalize_to_ax_prefix() {
        assert_eq!(normalize_role("button"), "AXButton");
        assert_eq!(normalize_role("AXLink"), "AXLink");
        assert_eq!(normalize_role("axTextField"), "AXTextField");
        assert_eq!(normalize_role("  "), "");
    }

    #[test]
    fn exact_beats_loose_and_role_filters() {
        let cands = vec![
            c("AXButton", "Save"),
            c("AXButton", "Save As…"),
            c("AXLink", "Save"),
            c("AXTextField", "Search"),
        ];
        assert_eq!(pick_target(&cands, "save", None), Ok(0), "first exact wins");
        assert_eq!(pick_target(&cands, "save", Some("link")), Ok(2));
        assert_eq!(
            pick_target(&cands, "save as", None),
            Ok(1),
            "one loose match"
        );
        assert_eq!(pick_target(&cands, "sea", Some("textfield")), Ok(3));
    }

    #[test]
    fn ambiguity_and_absence_are_typed_with_candidates() {
        let cands = vec![
            c("AXButton", "Add to cart"),
            c("AXButton", "Add to wishlist"),
        ];
        match pick_target(&cands, "add to", None) {
            Err(AxActionError::Ambiguous { candidates }) => assert_eq!(candidates.len(), 2),
            other => panic!("{other:?}"),
        }
        match pick_target(&cands, "checkout", None) {
            Err(AxActionError::NotFound { candidates }) => {
                assert_eq!(candidates.len(), 2, "what IS there rides back")
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(
            pick_target(&cands, "add to cart", Some("link")),
            Err(AxActionError::NotFound { candidates: vec![] }),
            "a role filter that matches nothing lists nothing"
        );
    }
}
