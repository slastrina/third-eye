//! Live macOS app-focus backend: list the running apps via NSWorkspace,
//! best-effort match the requested name, and bring the match to the front via
//! `NSRunningApplication::activateWithOptions`.
//!
//! App activation needs no TCC entitlement (unlike Screen Recording or
//! Accessibility), so there is no permission preflight — the only failure
//! classes are `not-found` (no running app matched) and `activation-failed`
//! (the match quit, or the OS refused the activate request).
//!
//! objc2-app-kit is generated ObjC bindings; every message send is `unsafe` at
//! the ABI level but the crate exposes them as safe `pub fn`s. The name-matching
//! logic is extracted into the pure [`best_match`] free function so it is
//! unit-tested exhaustively without activating anything; only the workspace
//! roster read and the activate call touch the live system.

use async_trait::async_trait;
use objc2_app_kit::{NSApplicationActivationOptions, NSWorkspace};

use super::{AppFocus, AppFocusError, FocusedApp};

/// The live macOS backend: one NSWorkspace roster read per `focus`, matched by
/// localized name and activated (bringing all its windows forward).
pub struct MacosAppFocus;

/// Best-effort match of a requested app name against the running app names,
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
    candidates.iter().position(|c| c.to_lowercase().contains(&needle))
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

#[async_trait]
impl AppFocus for MacosAppFocus {
    async fn focus(&self, app_name: &str) -> Result<FocusedApp, AppFocusError> {
        let workspace = NSWorkspace::sharedWorkspace();
        let apps = workspace.runningApplications();
        // Read localized names once so the match index lines up with the roster.
        let names: Vec<Option<String>> =
            apps.iter().map(|app| app.localizedName().map(|n| n.to_string())).collect();
        let candidates: Vec<String> = names.iter().flatten().cloned().collect();

        let Some(matched_idx) = best_match(app_name, &candidates) else {
            let err = AppFocusError::NotFound {
                requested: app_name.to_string(),
                candidates,
            };
            log::warn!("focus_app: {} ({err})", err.kind());
            return Err(err);
        };

        // Map the candidate index (which skipped un-named apps) back to the app
        // handle: candidates are the flattened names in roster order, so the
        // Nth candidate is the Nth app that had a name.
        let matched_name = candidates[matched_idx].clone();
        let app = apps
            .iter()
            .zip(names.iter())
            .filter_map(|(app, name)| name.as_ref().map(|n| (app, n.clone())))
            .nth(matched_idx)
            .map(|(app, _)| app);
        let Some(app) = app else {
            // The roster changed between the name snapshot and this lookup — the
            // matched app is gone. Typed activation-failed, never a panic.
            let err = AppFocusError::ActivationFailed {
                detail: format!("app {matched_name:?} left the roster before activation"),
            };
            log::warn!("focus_app: {} ({err})", err.kind());
            return Err(err);
        };

        // activateWithOptions returns false if the app quit or is of a type that
        // cannot be activated — a typed activation-failed, never a silent no-op.
        let activated = app.activateWithOptions(NSApplicationActivationOptions::ActivateAllWindows);
        if activated {
            log::info!("focus_app: activated {matched_name:?}");
            Ok(FocusedApp { app: matched_name })
        } else {
            let err = AppFocusError::ActivationFailed {
                detail: format!("the OS refused to activate {matched_name:?}"),
            };
            log::warn!("focus_app: {} ({err})", err.kind());
            Err(err)
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
        assert_eq!(best_match("   ", &apps), None, "blank request matches nothing");
    }

    #[test]
    fn best_match_trims_the_request() {
        let apps = roster();
        assert_eq!(best_match("  Zed  ", &apps), Some(1));
    }

    /// Live run of the full backend against the real workspace (MEM038
    /// precedent) — activates a real app, ignored in the default suite. Focusing
    /// a name that is not running must fail *typed* (not-found), never panic.
    #[tokio::test]
    #[ignore = "activates a real app and reads the live workspace (slice UAT)"]
    async fn real_app_focus_smoke() {
        let backend: Arc<dyn AppFocus> = Arc::new(MacosAppFocus);
        let running = backend.running_apps().await;
        println!("focus_app: {} running app(s): {running:?}", running.len());

        // A guaranteed-absent name must be typed not-found carrying the roster.
        match backend.focus("no-such-app-zzz").await {
            Ok(f) => panic!("unexpected activation of {:?}", f.app),
            Err(err) => {
                assert_eq!(err.kind(), "not-found");
                if let AppFocusError::NotFound { candidates, .. } = err {
                    assert_eq!(candidates, running);
                }
            }
        }

        // If Finder is running (it always is), fronting it must succeed.
        if running.iter().any(|a| a == "Finder") {
            let focused = backend.focus("Finder").await.unwrap();
            assert_eq!(focused.app, "Finder");
        }
    }
}
