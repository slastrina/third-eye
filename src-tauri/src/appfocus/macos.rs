//! Live macOS app-focus backend: launch-or-focus with frontmost verification.
//!
//! Resolution order for a `focus` request:
//! 1. Match the name against the *running* apps (NSWorkspace roster) and
//!    activate the match via `NSRunningApplication::activateWithOptions`.
//! 2. Verify the app actually became frontmost. On macOS 14+ cooperative
//!    activation the activate call can return `true` while the system quietly
//!    drops the request (Third Eye's panels are nonactivating, so it is never
//!    the active app and holds no activation privilege) — and activating an
//!    app whose windows are all closed fronts nothing visible. Both read as
//!    "the tool said it opened Chrome and nothing happened", so acceptance is
//!    never reported as success.
//! 3. When activation did not verifiably front the app, or nothing running
//!    matched, route through Launch Services (`/usr/bin/open`): for a running
//!    app that sends the reopen event that makes a window-less app show a
//!    window; for a not-running app it resolves an installed `*.app` bundle
//!    by name and launches it. Success is again only the verified-frontmost
//!    outcome.
//!
//! App activation and Launch Services opens need no TCC entitlement (unlike
//! Screen Recording or Accessibility), so there is no permission preflight —
//! the failure classes stay `not-found` (nothing running or installed
//! matched) and `activation-failed` (matched but never fronted).
//!
//! objc2-app-kit is generated ObjC bindings; every message send is `unsafe` at
//! the ABI level but the crate exposes them as safe `pub fn`s. All AppKit
//! handles live inside synchronous helpers — none is held across an `await`,
//! so the `focus` future stays `Send` for the tool loop's spawned task. The
//! name-matching logic ([`best_match`], [`names_match`]) and the bundle scan
//! ([`installed_apps`]) are pure/fs-only free functions, unit-tested without
//! activating anything.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use objc2_app_kit::{NSApplicationActivationOptions, NSWorkspace};

use super::{AppFocus, AppFocusError, FocusedApp};

/// How long a plain activation of a running app gets to verifiably front it
/// before the Launch Services fallback kicks in.
const ACTIVATE_VERIFY_MS: u64 = 1_200;

/// How long a Launch Services reopen of a *running* app gets to front it.
/// Generous because this path already means the plain activation was dropped —
/// the system is being uncooperative or the app is rebuilding a window.
const REOPEN_VERIFY_MS: u64 = 5_000;

/// How long a fresh launch gets to front the app — cold starts of heavy apps
/// (Chrome, Xcode) take seconds. Paid in full only on failure; success
/// returns at the first poll that sees the app frontmost.
const LAUNCH_VERIFY_MS: u64 = 8_000;

/// Frontmost-poll cadence during verification.
const VERIFY_POLL_MS: u64 = 150;

/// The live macOS backend: launch-or-focus by name, success = verified
/// frontmost.
pub struct MacosAppFocus;

/// Best-effort match of a requested app name against candidate app names,
/// case-insensitive: an exact (whole-name) match wins first, then the first
/// substring match. Returns the index into `candidates`, or `None` when nothing
/// matches. Pure and total — the whole matching policy in one testable place,
/// with no message send, so every branch is exercised without activating an app.
///
/// Exact-before-substring matters for disambiguation: a request for `"Safari"`
/// must front Safari, not "Safari Technology Preview", when both run.
pub fn best_match(requested: &str, candidates: &[String]) -> Option<usize> {
    let needle = requested.trim().to_lowercase();
    if needle.is_empty() {
        return None;
    }
    if let Some(i) = candidates.iter().position(|c| c.to_lowercase() == needle) {
        return Some(i);
    }
    candidates
        .iter()
        .position(|c| c.to_lowercase().contains(&needle))
}

/// Loose two-way containment match (case-insensitive, trimmed) between an
/// expected app name and the frontmost app's localized name. Loose because
/// the two come from different namespaces — a bundle's file stem
/// ("Google Chrome" from `Google Chrome.app`) versus the runtime localized
/// name — and an exact-equality check would call a genuinely fronted app a
/// failure over a cosmetic suffix.
pub fn names_match(a: &str, b: &str) -> bool {
    let a = a.trim().to_lowercase();
    let b = b.trim().to_lowercase();
    !a.is_empty() && !b.is_empty() && (a.contains(&b) || b.contains(&a))
}

/// The standard Launch Services install roots, scanned in order. User apps
/// first so a per-user install shadows a system copy the same way Finder's
/// "Open" does.
fn installed_app_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(home) = std::env::var_os("HOME") {
        roots.push(PathBuf::from(home).join("Applications"));
    }
    roots.push(PathBuf::from("/Applications"));
    roots.push(PathBuf::from("/System/Applications"));
    roots.push(PathBuf::from("/System/Applications/Utilities"));
    roots
}

/// Scan `roots` (non-recursively) for `*.app` bundles: (display name = file
/// stem, bundle path). Missing/unreadable roots are skipped — an absent
/// `~/Applications` is normal, never an error. Takes the roots as a parameter
/// so tests scan a temp directory instead of the live disk.
pub fn installed_apps(roots: &[PathBuf]) -> Vec<(String, PathBuf)> {
    let mut apps = Vec::new();
    for root in roots {
        let Ok(entries) = std::fs::read_dir(root) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("app") {
                if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                    apps.push((stem.to_string(), path));
                }
            }
        }
    }
    apps
}

/// Count the app's on-screen, layer-0 windows via CGWindowList — the
/// evidence for the "frontmost but nothing visible" trap (an app running
/// with every window closed fronts nothing; the model must open a window
/// or say so). `None` when the list call fails; needs no permission.
fn visible_window_count(pid: i32) -> Option<usize> {
    #[repr(C)]
    struct __CFArray(std::ffi::c_void);
    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        fn CGWindowListCopyWindowInfo(option: u32, relative: u32) -> *const __CFArray;
    }
    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFArrayGetCount(array: *const __CFArray) -> isize;
        fn CFArrayGetValueAtIndex(array: *const __CFArray, idx: isize) -> *const std::ffi::c_void;
        fn CFDictionaryGetValue(
            dict: *const std::ffi::c_void,
            key: *const std::ffi::c_void,
        ) -> *const std::ffi::c_void;
        fn CFNumberGetValue(
            number: *const std::ffi::c_void,
            the_type: i32,
            value_ptr: *mut std::ffi::c_void,
        ) -> bool;
        fn CFRelease(cf: *mut std::ffi::c_void);
    }
    const OPTION_ON_SCREEN_ONLY: u32 = 1 << 0;
    const NULL_WINDOW_ID: u32 = 0;
    const K_CF_NUMBER_SINT32: i32 = 3;
    use objc2_core_foundation::CFString;
    unsafe {
        let list = CGWindowListCopyWindowInfo(OPTION_ON_SCREEN_ONLY, NULL_WINDOW_ID);
        if list.is_null() {
            return None;
        }
        let owner_key = CFString::from_str("kCGWindowOwnerPID");
        let layer_key = CFString::from_str("kCGWindowLayer");
        let mut count = 0usize;
        for i in 0..CFArrayGetCount(list) {
            let dict = CFArrayGetValueAtIndex(list, i);
            let read_i32 = |key: &CFString| -> Option<i32> {
                let value =
                    CFDictionaryGetValue(dict, key as *const CFString as *const std::ffi::c_void);
                if value.is_null() {
                    return None;
                }
                let mut out: i32 = 0;
                CFNumberGetValue(
                    value,
                    K_CF_NUMBER_SINT32,
                    &mut out as *mut i32 as *mut std::ffi::c_void,
                )
                .then_some(out)
            };
            if read_i32(&owner_key) == Some(pid) && read_i32(&layer_key) == Some(0) {
                count += 1;
            }
        }
        CFRelease(list as *mut std::ffi::c_void);
        Some(count)
    }
}

/// Pid of a running app by localized name (for the window count readback).
fn pid_for_app_name(name: &str) -> Option<i32> {
    let workspace = NSWorkspace::sharedWorkspace();
    let apps = workspace.runningApplications();
    for app in apps.iter() {
        if app.localizedName().map(|n| n.to_string()).as_deref() == Some(name) {
            return Some(app.processIdentifier());
        }
    }
    None
}

/// Assemble the verified report: the fronted name plus the visible-window
/// evidence (the "frontmost but nothing on screen" trap detector).
fn focused_report(app: String, launched: bool) -> FocusedApp {
    let visible_windows = pid_for_app_name(&app).and_then(visible_window_count);
    if visible_windows == Some(0) {
        log::warn!("focus_app: {app:?} is frontmost with ZERO visible windows");
    }
    FocusedApp {
        app,
        launched,
        visible_windows,
    }
}
/// Snapshot the localized names of the currently running apps. Apps without a
/// localized name (rare background helpers) are skipped — they are never a
/// user-visible target the model would name.
fn running_app_names(workspace: &NSWorkspace) -> Vec<String> {
    workspace
        .runningApplications()
        .iter()
        .filter_map(|app| app.localizedName().map(|n| n.to_string()))
        .collect()
}

/// The frontmost app's display name, or `None` when it cannot be determined.
///
/// Queried through `lsappinfo` (a direct launchservicesd client), NOT
/// `NSWorkspace::frontmostApplication`: that AppKit property is KVO-backed
/// state that only refreshes while a main run loop pumps, so a process
/// without one (the cargo test harness) — and any read racing the run loop —
/// sees a frozen snapshot and calls a genuinely fronted app a failure.
async fn frontmost_app_name() -> Option<String> {
    let front = tokio::process::Command::new("/usr/bin/lsappinfo")
        .arg("front")
        .output()
        .await
        .ok()?;
    let asn = String::from_utf8_lossy(&front.stdout).trim().to_string();
    if asn.is_empty() {
        return None;
    }
    let info = tokio::process::Command::new("/usr/bin/lsappinfo")
        .args(["info", "-only", "name", &asn])
        .output()
        .await
        .ok()?;
    parse_ls_display_name(&String::from_utf8_lossy(&info.stdout))
}

/// Extract the value from an lsappinfo `"LSDisplayName"="Finder"` line. Pure
/// so the format assumption is pinned by a unit test.
pub fn parse_ls_display_name(output: &str) -> Option<String> {
    let (_, value) = output.split_once("\"LSDisplayName\"=")?;
    let value = value.trim().strip_prefix('"')?;
    Some(value[..value.find('"')?].to_string())
}

/// What one synchronous pass over the running roster did with the request.
/// Plain owned data only — no AppKit handle escapes into the async caller.
enum RunningActivation {
    /// A running app matched; the activate request was sent. `accepted` is the
    /// OS's claim, which verification treats as a hint, not an outcome.
    Requested { matched: String, accepted: bool },
    /// Nothing running matched (or the match quit mid-read); `candidates` is
    /// the roster snapshot for the launch fallback / not-found payload.
    NotRunning { candidates: Vec<String> },
}

/// Match `app_name` against the running apps and request activation of the
/// match. Fully synchronous: every Retained AppKit handle drops before return.
fn activate_running(app_name: &str) -> RunningActivation {
    let workspace = NSWorkspace::sharedWorkspace();
    let apps = workspace.runningApplications();
    // Read localized names once so the match index lines up with the roster.
    let names: Vec<Option<String>> = apps
        .iter()
        .map(|app| app.localizedName().map(|n| n.to_string()))
        .collect();
    let candidates: Vec<String> = names.iter().flatten().cloned().collect();

    let Some(matched_idx) = best_match(app_name, &candidates) else {
        return RunningActivation::NotRunning { candidates };
    };

    // Map the candidate index (which skipped un-named apps) back to the app
    // handle: candidates are the flattened names in roster order, so the
    // Nth candidate is the Nth app that had a name.
    let matched = candidates[matched_idx].clone();
    let app = apps
        .iter()
        .zip(names.iter())
        .filter_map(|(app, name)| name.as_ref().map(|_| app))
        .nth(matched_idx);
    match app {
        Some(app) => {
            let accepted =
                app.activateWithOptions(NSApplicationActivationOptions::ActivateAllWindows);
            RunningActivation::Requested { matched, accepted }
        }
        // The roster changed between the name snapshot and this lookup — the
        // matched app is gone. Fall through to the launch path, which will
        // find it installed and start it fresh.
        None => RunningActivation::NotRunning { candidates },
    }
}

/// Poll until the frontmost app matches `expected`, returning its actual
/// localized name — the value `screen_query`'s per-app filter must be fed,
/// since `attribute_app` labels elements with runtime localized names.
async fn verify_fronted(expected: &str, timeout_ms: u64) -> Option<String> {
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        if let Some(front) = frontmost_app_name().await {
            if names_match(&front, expected) {
                return Some(front);
            }
        }
        if Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(VERIFY_POLL_MS)).await;
    }
}

/// Open `target` through Launch Services and verify `expected` fronted.
/// `open -a <name>` on a running app activates it *and* sends the reopen that
/// makes a window-less app show a window; `open <bundle path>` launches a
/// not-running app. Ok carries the verified frontmost localized name; Err a
/// human-readable detail for `activation-failed`.
async fn open_and_verify(
    target: OpenTarget<'_>,
    expected: &str,
    timeout_ms: u64,
) -> Result<String, String> {
    let mut cmd = tokio::process::Command::new("/usr/bin/open");
    match target {
        OpenTarget::Name(name) => {
            cmd.arg("-a").arg(name);
        }
        OpenTarget::Bundle(path) => {
            cmd.arg(path);
        }
    }
    let output = cmd
        .output()
        .await
        .map_err(|e| format!("could not run /usr/bin/open: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "open exited with {}: {}",
            output.status,
            stderr.trim()
        ));
    }
    verify_fronted(expected, timeout_ms).await.ok_or_else(|| {
        format!(
            "{expected:?} was opened but never came to the front within {timeout_ms}ms — \
             macOS may have denied the activation"
        )
    })
}

/// What `open_and_verify` should hand Launch Services.
enum OpenTarget<'a> {
    /// A running app's localized name (`open -a` — activate + reopen).
    Name(&'a str),
    /// An installed bundle path (`open` — launch).
    Bundle(&'a Path),
}

#[async_trait]
impl AppFocus for MacosAppFocus {
    async fn focus(&self, app_name: &str) -> Result<FocusedApp, AppFocusError> {
        match activate_running(app_name) {
            RunningActivation::Requested { matched, accepted } => {
                if accepted {
                    if let Some(front) = verify_fronted(&matched, ACTIVATE_VERIFY_MS).await {
                        log::info!("focus_app: activated {front:?}");
                        return Ok(focused_report(front, false));
                    }
                }
                // The OS refused, quietly dropped the request (cooperative
                // activation), or the app has no window to bring forward —
                // Launch Services activate+reopen is the recovery for all
                // three.
                log::info!(
                    "focus_app: plain activation of {matched:?} did not front it \
                     (accepted={accepted}); retrying via Launch Services reopen"
                );
                match open_and_verify(OpenTarget::Name(&matched), &matched, REOPEN_VERIFY_MS).await
                {
                    Ok(front) => {
                        log::info!("focus_app: fronted {front:?} via reopen");
                        Ok(focused_report(front, false))
                    }
                    Err(detail) => {
                        let err = AppFocusError::ActivationFailed { detail };
                        log::warn!("focus_app: {} ({err})", err.kind());
                        Err(err)
                    }
                }
            }
            RunningActivation::NotRunning { candidates } => {
                // Nothing running matched — resolve an installed bundle and
                // launch it. The scan is per-request: installs are rare and a
                // roster-freshness bug would be worse than the ~ms of readdir.
                let installed = installed_apps(&installed_app_roots());
                let names: Vec<String> = installed.iter().map(|(name, _)| name.clone()).collect();
                let Some(idx) = best_match(app_name, &names) else {
                    let err = AppFocusError::NotFound {
                        requested: app_name.to_string(),
                        candidates,
                    };
                    log::warn!("focus_app: {} ({err})", err.kind());
                    return Err(err);
                };
                let (name, path) = installed[idx].clone();
                log::info!("focus_app: {app_name:?} not running; launching {:?}", path);
                match open_and_verify(OpenTarget::Bundle(&path), &name, LAUNCH_VERIFY_MS).await {
                    Ok(front) => {
                        log::info!("focus_app: launched and fronted {front:?}");
                        Ok(focused_report(front, true))
                    }
                    Err(detail) => {
                        let err = AppFocusError::ActivationFailed { detail };
                        log::warn!("focus_app: {} ({err})", err.kind());
                        Err(err)
                    }
                }
            }
        }
    }

    async fn running_apps(&self) -> Vec<String> {
        running_app_names(&NSWorkspace::sharedWorkspace())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn roster() -> Vec<String> {
        vec![
            "Google Chrome".into(),
            "Zed".into(),
            "Safari".into(),
            "Safari Technology Preview".into(),
        ]
    }

    #[test]
    fn best_match_exact_name_case_insensitive() {
        let apps = roster();
        assert_eq!(best_match("Google Chrome", &apps), Some(0));
        assert_eq!(best_match("google chrome", &apps), Some(0));
        assert_eq!(best_match("ZED", &apps), Some(1));
    }

    #[test]
    fn best_match_prefers_exact_over_substring() {
        // "Safari" is a substring of "Safari Technology Preview" too, but the
        // exact whole-name match must win.
        let apps = roster();
        assert_eq!(best_match("Safari", &apps), Some(2));
        assert_eq!(best_match("safari", &apps), Some(2));
    }

    #[test]
    fn best_match_substring_when_no_exact() {
        let apps = roster();
        // "chrome" is nobody's exact name but a substring of "Google Chrome".
        assert_eq!(best_match("chrome", &apps), Some(0));
        // A substring that only matches the Technology Preview entry.
        assert_eq!(best_match("Technology", &apps), Some(3));
    }

    #[test]
    fn best_match_no_match_is_none() {
        let apps = roster();
        assert_eq!(best_match("Firefox", &apps), None);
        assert_eq!(best_match("", &apps), None);
        assert_eq!(
            best_match("   ", &apps),
            None,
            "blank request matches nothing"
        );
    }

    #[test]
    fn best_match_trims_the_request() {
        let apps = roster();
        assert_eq!(best_match("  Zed  ", &apps), Some(1));
    }

    #[test]
    fn names_match_is_loose_both_ways_and_never_blank() {
        // Same name, different case/padding.
        assert!(names_match("Google Chrome", "google chrome "));
        // Bundle stem vs runtime localized name, either direction.
        assert!(names_match("Google Chrome", "Google Chrome Beta"));
        assert!(names_match("Google Chrome Beta", "Google Chrome"));
        // Genuinely different apps do not match.
        assert!(!names_match("Safari", "Google Chrome"));
        // A blank side must never match everything.
        assert!(!names_match("", "Google Chrome"));
        assert!(!names_match("Google Chrome", "   "));
    }

    #[test]
    fn parse_ls_display_name_pins_the_lsappinfo_format() {
        assert_eq!(
            parse_ls_display_name("\"LSDisplayName\"=\"Finder\"\n"),
            Some("Finder".to_string())
        );
        assert_eq!(
            parse_ls_display_name("\"LSDisplayName\"=\"Google Chrome\""),
            Some("Google Chrome".to_string())
        );
        // Unexpected shapes degrade to None, never a panic mid-verification.
        assert_eq!(parse_ls_display_name(""), None);
        assert_eq!(parse_ls_display_name("\"LSDisplayName\"="), None);
        assert_eq!(parse_ls_display_name("garbage"), None);
    }

    #[test]
    fn installed_apps_scans_only_app_bundles_and_skips_missing_roots() {
        let root =
            std::env::temp_dir().join(format!("third-eye-appfocus-scan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("Google Chrome.app")).unwrap();
        std::fs::create_dir_all(root.join("Zed.app")).unwrap();
        // A non-bundle directory and a plain file must both be skipped.
        std::fs::create_dir_all(root.join("Utilities")).unwrap();
        std::fs::write(root.join("readme.txt"), "not an app").unwrap();

        let missing = root.join("no-such-dir");
        let apps = installed_apps(&[missing, root.clone()]);
        let mut names: Vec<&str> = apps.iter().map(|(n, _)| n.as_str()).collect();
        names.sort();
        assert_eq!(names, ["Google Chrome", "Zed"]);
        // The stem→path pairing points at the bundle itself.
        let chrome = apps.iter().find(|(n, _)| n == "Google Chrome").unwrap();
        assert_eq!(chrome.1, root.join("Google Chrome.app"));

        // best_match composes over the scanned names — the launch path's
        // fuzzy resolution ("chrome" → the Chrome bundle).
        let scanned: Vec<String> = apps.iter().map(|(n, _)| n.clone()).collect();
        let idx = best_match("chrome", &scanned).unwrap();
        assert_eq!(scanned[idx], "Google Chrome");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Live run of the full backend against the real workspace (MEM038
    /// precedent) — activates a real app, ignored in the default suite. Focusing
    /// a name that is neither running nor installed must fail *typed*
    /// (not-found), never panic.
    #[tokio::test]
    #[ignore = "activates a real app and reads the live workspace (slice UAT)"]
    async fn real_app_focus_smoke() {
        let backend: Arc<dyn AppFocus> = Arc::new(MacosAppFocus);
        let running = backend.running_apps().await;
        println!("focus_app: {} running app(s): {running:?}", running.len());

        // A guaranteed-absent name must be typed not-found carrying a roster.
        // Not compared element-wise against `running`: background helpers
        // (npm/MCP children) churn between the two roster reads.
        match backend.focus("no-such-app-zzz").await {
            Ok(f) => panic!("unexpected activation of {:?}", f.app),
            Err(err) => {
                assert_eq!(err.kind(), "not-found");
                if let AppFocusError::NotFound { candidates, .. } = err {
                    assert!(!candidates.is_empty(), "candidates must carry the roster");
                }
            }
        }

        // If Finder is running (it always is), fronting it must succeed and
        // report focused-not-launched.
        if running.iter().any(|a| a == "Finder") {
            let focused = backend.focus("Finder").await.unwrap();
            assert_eq!(focused.app, "Finder");
            assert!(!focused.launched, "Finder was already running");
        }
    }

    /// Live launch-path proof: focusing an app that is NOT running must launch
    /// it, verify it fronted, and report `launched: true`. Uses Calculator
    /// (small, instant, stateless) and quits it afterwards so the UAT leaves
    /// no app behind. Skips silently when Calculator is already running —
    /// then it cannot prove the launch path.
    #[tokio::test]
    #[ignore = "launches and quits a real app (slice UAT)"]
    async fn real_app_launch_smoke() {
        let backend: Arc<dyn AppFocus> = Arc::new(MacosAppFocus);
        if backend
            .running_apps()
            .await
            .iter()
            .any(|a| a == "Calculator")
        {
            println!("focus_app launch smoke: Calculator already running — skipping");
            return;
        }
        let focused = backend.focus("Calculator").await.unwrap();
        assert_eq!(focused.app, "Calculator");
        assert!(
            focused.launched,
            "Calculator was not running — this must be a launch"
        );
        // Clean up: quit the app the test started.
        let _ = std::process::Command::new("/usr/bin/osascript")
            .args(["-e", "quit app \"Calculator\""])
            .status();
    }
}
