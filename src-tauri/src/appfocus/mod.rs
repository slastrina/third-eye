//! App-focus boundary: the open-an-app / bring-it-to-the-front seam behind
//! the `focus_app` tool (M005).
//!
//! [`AppFocus`] is the object-safe abstraction the composite executor's
//! `FocusAppTool` holds as `Arc<dyn AppFocus>`, mirroring the S01
//! [`crate::input::commands::InputState`] and S02
//! [`crate::screenquery::commands::ScreenQueryState`] patterns. Resolution is
//! best-effort by localized app name: `screen_query` labels each on-screen text
//! element with its owning app, and `focus_app` brings the named app forward —
//! launching it first when it is not running — so the model can then aim an
//! `input_action` at it rather than at whatever happened to be frontmost.
//!
//! [`FocusedApp`] is the platform-neutral success shape: the localized name the
//! backend actually matched and fronted (plus whether it had to launch the
//! app), so the model (and UI) can confirm which app it opened when its
//! request was fuzzy. A success is only reported after the backend has
//! *verified* the app is frontmost — "the OS accepted the request" is not
//! success, because macOS cooperative activation can accept and then silently
//! drop it, which reads as "the model claimed it opened Chrome and nothing
//! happened".
//!
//! Failure taxonomy mirrors [`crate::screenquery::ScreenQueryError`] and
//! [`crate::input::InputError`]: every failure is a typed [`AppFocusError`]
//! variant, serialized kind-tagged with camelCase fields (R007), so the tool
//! surfaces name the failure class (`not-found` / `activation-failed` /
//! `unsupported`). App activation needs no TCC entitlement, so there is no
//! `permission-denied` variant; a `not-found` carries the running-app
//! candidates so the model can retry against a real name.
//!
//! Platform binding: macOS gets [`macos::MacosAppFocus`]
//! (NSWorkspace/NSRunningApplication activation); every other OS gets
//! [`fallback::FallbackAppFocus`], which returns typed `unsupported` errors so
//! Windows/Linux builds stay clean (R020).

pub mod commands;
pub mod fallback;
#[cfg(target_os = "macos")]
pub mod macos;

use async_trait::async_trait;
use serde::Serialize;

/// The app `focus_app` actually brought to the front — the localized name the
/// backend matched, so a fuzzy request (`"chrome"` → `"Google Chrome"`) reports
/// exactly which app was fronted. `launched` is true when the app was not
/// running and the backend launched it first — the model can tell the user
/// "opened" vs "switched to" truthfully. camelCase in JSON to ride the
/// tool-call contract.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FocusedApp {
    pub app: String,
    pub launched: bool,
    /// On-screen, layer-0 window count for the fronted app at verification
    /// time. `Some(0)` is the "frontmost but nothing visible" trap (app
    /// running with all windows closed) the model must react to; `None`
    /// when the platform can't count (fallback, or the list call failed).
    pub visible_windows: Option<usize>,
    /// Title of the window that is now in front for the app (its focused
    /// window, or the first visible one) — the model's evidence of WHAT is
    /// already showing ("eBay: nike shoes"), so a follow-up works in that
    /// window/tab instead of opening another. `None` when unreadable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub front_window: Option<String>,
}

/// The full app-focus failure taxonomy (R007). Serialized with a `kind` tag
/// (`not-found` / `activation-failed` / `unsupported`) and camelCase fields —
/// the same IPC error contract shape as [`crate::screenquery::ScreenQueryError`]
/// and [`crate::input::InputError`]; consumers match on `kind`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(
    tag = "kind",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase"
)]
pub enum AppFocusError {
    /// No running *or installed* app matched the requested name. `candidates`
    /// lists the currently running app names so the model can retry against a
    /// real one rather than guess again.
    NotFound {
        requested: String,
        candidates: Vec<String>,
    },
    /// A matching app was found but never made it to the front — the OS
    /// refused or silently dropped the activation request, the launch failed,
    /// or the app quit between the match and the activate.
    ActivationFailed { detail: String },
    /// App focus is not implemented on this platform. `platform` names the
    /// running OS so logs and status surfaces are self-explanatory.
    Unsupported { platform: String, detail: String },
}

impl AppFocusError {
    /// Stable machine-readable name, mirroring the serde `kind` tag. Used in
    /// error logs so grep for `not-found` / `activation-failed` / `unsupported`
    /// works.
    pub fn kind(&self) -> &'static str {
        match self {
            AppFocusError::NotFound { .. } => "not-found",
            AppFocusError::ActivationFailed { .. } => "activation-failed",
            AppFocusError::Unsupported { .. } => "unsupported",
        }
    }

    /// The `unsupported` error for the current platform — the one shape the
    /// fallback backend ever returns.
    pub fn unsupported_here() -> Self {
        AppFocusError::Unsupported {
            platform: std::env::consts::OS.to_string(),
            detail: "app focus is only implemented on macOS".to_string(),
        }
    }
}

impl std::fmt::Display for AppFocusError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AppFocusError::NotFound {
                requested,
                candidates,
            } => {
                write!(
                    f,
                    "app-focus not-found: no running or installed app matched {requested:?} (running: {})",
                    candidates.join(", ")
                )
            }
            AppFocusError::ActivationFailed { detail } => {
                write!(f, "app-focus activation-failed: {detail}")
            }
            AppFocusError::Unsupported { platform, detail } => {
                write!(f, "app-focus unsupported on {platform}: {detail}")
            }
        }
    }
}

impl std::error::Error for AppFocusError {}

/// The app-focus seam. Object-safe (`Arc<dyn AppFocus>`) so managed state, the
/// composite executor, and tests can hold any backend without knowing its
/// transport. `Send + Sync` so it can live in Tauri managed state like
/// [`crate::input::commands::InputState`].
///
/// `focus` best-effort matches `app_name` against the running apps and brings
/// the match to the front — launching an installed app when nothing running
/// matches — returning the localized name it verified frontmost. `running_apps`
/// lists the current running app names — the retry hint the tool surfaces on a
/// `not-found`, and the candidate set the `not-found` error carries.
#[async_trait]
pub trait AppFocus: Send + Sync {
    async fn focus(&self, app_name: &str) -> Result<FocusedApp, AppFocusError>;

    async fn running_apps(&self) -> Vec<String>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// Minimal in-memory backend proving the trait is implementable and
    /// object-safe — the same shape the tool tests will use. A fixed roster of
    /// running apps; `focus` matches case-insensitively (exact then substring).
    struct MockAppFocus {
        running: Vec<String>,
        fail_with: Option<AppFocusError>,
    }

    #[async_trait]
    impl AppFocus for MockAppFocus {
        async fn focus(&self, app_name: &str) -> Result<FocusedApp, AppFocusError> {
            if let Some(err) = &self.fail_with {
                return Err(err.clone());
            }
            let needle = app_name.to_lowercase();
            match self
                .running
                .iter()
                .find(|a| a.to_lowercase() == needle)
                .or_else(|| {
                    self.running
                        .iter()
                        .find(|a| a.to_lowercase().contains(&needle))
                }) {
                Some(app) => Ok(FocusedApp {
                    app: app.clone(),
                    launched: false,
                    visible_windows: None,
                    front_window: None,
                }),
                None => Err(AppFocusError::NotFound {
                    requested: app_name.to_string(),
                    candidates: self.running.clone(),
                }),
            }
        }

        async fn running_apps(&self) -> Vec<String> {
            self.running.clone()
        }
    }

    #[tokio::test]
    async fn trait_is_object_safe_and_focuses_through_dyn() {
        let backend: Arc<dyn AppFocus> = Arc::new(MockAppFocus {
            running: vec!["Google Chrome".into(), "Zed".into()],
            fail_with: None,
        });
        let focused = backend.focus("chrome").await.unwrap();
        assert_eq!(focused.app, "Google Chrome");
        assert_eq!(backend.running_apps().await, vec!["Google Chrome", "Zed"]);
    }

    #[tokio::test]
    async fn errors_propagate_through_dyn_with_kind() {
        let backend: Arc<dyn AppFocus> = Arc::new(MockAppFocus {
            running: vec!["Zed".into()],
            fail_with: None,
        });
        let err = backend.focus("Firefox").await.unwrap_err();
        assert_eq!(err.kind(), "not-found");
        match err {
            AppFocusError::NotFound {
                requested,
                candidates,
            } => {
                assert_eq!(requested, "Firefox");
                assert_eq!(candidates, vec!["Zed"]);
            }
            other => panic!("expected not-found, got {other:?}"),
        }
    }

    #[test]
    fn focused_json_shape_is_camel_case() {
        let f = FocusedApp {
            app: "Google Chrome".into(),
            launched: true,
            visible_windows: None,
            front_window: Some("eBay: nike shoes".into()),
        };
        let v = serde_json::to_value(&f).unwrap();
        assert_eq!(v["app"], "Google Chrome");
        assert_eq!(v["launched"], true);
        assert_eq!(v["frontWindow"], "eBay: nike shoes");
        let bare = FocusedApp {
            front_window: None,
            ..f
        };
        assert!(
            serde_json::to_value(&bare)
                .unwrap()
                .get("frontWindow")
                .is_none(),
            "an unreadable title is absent, not null"
        );
    }

    #[test]
    fn error_json_shape_is_the_ipc_contract() {
        // The tool surfaces match on `kind` and read camelCase fields; a change
        // here is a breaking IPC change.
        let not_found = AppFocusError::NotFound {
            requested: "chrome".into(),
            candidates: vec!["Zed".into(), "Finder".into()],
        };
        let v = serde_json::to_value(&not_found).unwrap();
        assert_eq!(v["kind"], "not-found");
        assert_eq!(v["requested"], "chrome");
        assert_eq!(v["candidates"][0], "Zed");
        assert_eq!(v["candidates"][1], "Finder");

        let activation = AppFocusError::ActivationFailed {
            detail: "app quit".into(),
        };
        let v = serde_json::to_value(&activation).unwrap();
        assert_eq!(v["kind"], "activation-failed");
        assert_eq!(v["detail"], "app quit");

        let unsupported = AppFocusError::Unsupported {
            platform: "linux".into(),
            detail: "no backend".into(),
        };
        let v = serde_json::to_value(&unsupported).unwrap();
        assert_eq!(v["kind"], "unsupported");
        assert_eq!(v["platform"], "linux");
        assert_eq!(v["detail"], "no backend");
    }

    #[test]
    fn kind_matches_serde_tag_for_every_variant() {
        let all = [
            AppFocusError::NotFound {
                requested: String::new(),
                candidates: Vec::new(),
            },
            AppFocusError::ActivationFailed {
                detail: String::new(),
            },
            AppFocusError::Unsupported {
                platform: String::new(),
                detail: String::new(),
            },
        ];
        for err in all {
            let v = serde_json::to_value(&err).unwrap();
            assert_eq!(v["kind"], err.kind(), "kind()/serde tag drift for {err:?}");
        }
    }

    #[test]
    fn error_display_names_kind_and_detail() {
        let err = AppFocusError::ActivationFailed {
            detail: "activate refused".into(),
        };
        let msg = err.to_string();
        assert!(msg.contains("activation-failed"), "kind missing: {msg}");
        assert!(msg.contains("activate refused"), "detail missing: {msg}");

        // not-found names the requested app and lists the candidates.
        let err = AppFocusError::NotFound {
            requested: "chrome".into(),
            candidates: vec!["Zed".into()],
        };
        let msg = err.to_string();
        assert!(msg.contains("not-found"), "kind missing: {msg}");
        assert!(msg.contains("chrome"), "requested missing: {msg}");
        assert!(msg.contains("Zed"), "candidates missing: {msg}");
    }

    #[test]
    fn unsupported_here_names_this_platform() {
        let err = AppFocusError::unsupported_here();
        assert_eq!(err.kind(), "unsupported");
        match err {
            AppFocusError::Unsupported { platform, .. } => {
                assert_eq!(platform, std::env::consts::OS);
            }
            other => panic!("expected unsupported, got {other:?}"),
        }
    }
}
