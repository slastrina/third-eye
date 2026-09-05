//! Update checks (2026-09-05, parity with caffeinate-menubar): one GET to
//! GitHub's releases/latest, compared to this build's version. Manual from
//! Settings, and daily while the app runs when the persisted toggle is on
//! (default on — disclosed in Settings and the privacy page; nothing is
//! sent besides the app's version in the User-Agent). Never downloads or
//! installs anything: it tells the user, who updates through Homebrew or
//! the Releases page.

use std::time::Duration;

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};

pub const UPDATE_CHECKS_KEY: &str = "updateChecksEnabled";
pub const UPDATE_STATE_EVENT: &str = "updates://state";
pub const RELEASES_LATEST: &str =
    "https://api.github.com/repos/slastrina/third-eye/releases/latest";
pub const RELEASES_PAGE: &str = "https://github.com/slastrina/third-eye/releases";
const RECHECK_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
const FIRST_CHECK_DELAY: Duration = Duration::from_secs(20);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(8);

/// Settings → Status "Updates" row — health as value, never an error.
#[derive(Debug, Clone, PartialEq, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct UpdateStatus {
    pub enabled: bool,
    pub current: String,
    /// The newest published version when it is newer than `current`.
    pub available: Option<String>,
    pub release_url: Option<String>,
    pub checked_at_ms: Option<i64>,
    pub error: Option<String>,
}

/// Compare dotted versions numerically (a missing component is 0, a
/// pre-release suffix is ignored). Pure.
pub fn is_newer(remote: &str, current: &str) -> bool {
    fn parts(v: &str) -> Vec<u64> {
        v.trim_start_matches('v')
            .split(['-', '+'])
            .next()
            .unwrap_or("")
            .split('.')
            .map(|p| p.parse().unwrap_or(0))
            .collect()
    }
    let (r, c) = (parts(remote), parts(current));
    for i in 0..r.len().max(c.len()) {
        let (a, b) = (
            r.get(i).copied().unwrap_or(0),
            c.get(i).copied().unwrap_or(0),
        );
        if a != b {
            return a > b;
        }
    }
    false
}

/// The tag in a releases/latest payload, without its `v`. Pure.
pub fn parse_latest(payload: &serde_json::Value) -> Option<(String, String)> {
    let tag = payload
        .get("tag_name")?
        .as_str()?
        .trim_start_matches('v')
        .to_string();
    let url = payload
        .get("html_url")
        .and_then(|u| u.as_str())
        .unwrap_or(RELEASES_PAGE)
        .to_string();
    Some((tag, url))
}

pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

pub fn load_enabled(app: &AppHandle) -> bool {
    use tauri_plugin_store::StoreExt;
    let Ok(store) = app.store(crate::config::SETTINGS_STORE) else {
        return true;
    };
    !matches!(
        store.get(UPDATE_CHECKS_KEY),
        Some(serde_json::Value::Bool(false))
    )
}

pub fn save_enabled(app: &AppHandle, enabled: bool) -> Result<(), String> {
    use tauri_plugin_store::StoreExt;
    let store = app
        .store(crate::config::SETTINGS_STORE)
        .map_err(|e| format!("failed to open settings store: {e}"))?;
    store.set(UPDATE_CHECKS_KEY, serde_json::json!(enabled));
    store
        .save()
        .map_err(|e| format!("failed to persist {UPDATE_CHECKS_KEY}={enabled}: {e}"))
}

/// The last result, so Settings can render it without a new request.
#[derive(Default)]
pub struct UpdateState(std::sync::Mutex<UpdateStatus>);

impl UpdateState {
    pub fn snapshot(&self) -> UpdateStatus {
        self.0.lock().map(|s| s.clone()).unwrap_or_default()
    }
    fn set(&self, s: UpdateStatus) {
        if let Ok(mut cur) = self.0.lock() {
            *cur = s;
        }
    }
}

/// One check: GET releases/latest, compare, store, broadcast.
pub async fn check_now(app: &AppHandle) -> UpdateStatus {
    let current = current_version().to_string();
    let enabled = load_enabled(app);
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let mut status = UpdateStatus {
        enabled,
        current: current.clone(),
        checked_at_ms: Some(now_ms),
        ..UpdateStatus::default()
    };
    let result = async {
        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .user_agent(format!("ThirdEye/{current}"))
            .build()
            .map_err(|e| e.to_string())?;
        let resp = client
            .get(RELEASES_LATEST)
            .header("Accept", "application/vnd.github+json")
            .send()
            .await
            .map_err(|e| format!("could not reach GitHub: {e}"))?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None); // no release published yet
        }
        if !resp.status().is_success() {
            return Err(format!("GitHub answered {}", resp.status()));
        }
        let payload: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
        Ok(parse_latest(&payload))
    }
    .await;
    match result {
        Ok(Some((remote, url))) if is_newer(&remote, &current) => {
            log::info!("updates: {remote} available (running {current})");
            status.available = Some(remote);
            status.release_url = Some(url);
        }
        Ok(_) => log::debug!("updates: {current} is current"),
        Err(e) => {
            log::warn!("updates: check failed: {e}");
            status.error = Some(e);
        }
    }
    app.state::<UpdateState>().set(status.clone());
    if let Err(e) = app.emit(UPDATE_STATE_EVENT, status.clone()) {
        log::warn!("updates: {UPDATE_STATE_EVENT} emit failed: {e}");
    }
    status
}

/// Daily checks while the toggle is on; the first one after a short delay
/// so boot stays quiet. Idempotent per launch.
pub fn spawn_periodic(app: &AppHandle) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(FIRST_CHECK_DELAY).await;
        loop {
            if load_enabled(&app) {
                let _ = check_now(&app).await;
            }
            tokio::time::sleep(RECHECK_INTERVAL).await;
        }
    });
}

#[tauri::command]
pub fn update_status(state: tauri::State<'_, UpdateState>, app: AppHandle) -> UpdateStatus {
    let mut s = state.snapshot();
    s.enabled = load_enabled(&app);
    if s.current.is_empty() {
        s.current = current_version().into();
    }
    s
}

#[tauri::command]
pub async fn check_for_updates(app: AppHandle) -> UpdateStatus {
    check_now(&app).await
}

#[tauri::command]
pub fn set_update_checks(
    app: AppHandle,
    enable: bool,
    state: tauri::State<'_, UpdateState>,
) -> UpdateStatus {
    let mut s = state.snapshot();
    match save_enabled(&app, enable) {
        Ok(()) => s.enabled = enable,
        Err(e) => s.error = Some(e),
    }
    if s.current.is_empty() {
        s.current = current_version().into();
    }
    state.set(s.clone());
    let _ = app.emit(UPDATE_STATE_EVENT, s.clone());
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_comparison_is_numeric_and_ignores_prerelease_suffixes() {
        assert!(is_newer("0.2.0", "0.1.0"));
        assert!(is_newer("v0.10.0", "0.9.9"));
        assert!(is_newer("1.0.0", "0.99.99"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.1.1"));
        assert!(is_newer("0.1.1", "0.1"), "missing component is 0");
        assert!(
            !is_newer("0.1.0-beta.1", "0.1.0"),
            "suffix ignored, not newer"
        );
    }

    #[test]
    fn latest_payload_parses_tag_and_url() {
        let v = serde_json::json!({"tag_name":"v0.2.0","html_url":"https://github.com/slastrina/third-eye/releases/tag/v0.2.0"});
        assert_eq!(
            parse_latest(&v),
            Some((
                "0.2.0".into(),
                "https://github.com/slastrina/third-eye/releases/tag/v0.2.0".into()
            ))
        );
        assert_eq!(parse_latest(&serde_json::json!({})), None);
        assert_eq!(
            parse_latest(&serde_json::json!({"tag_name":"0.3.0"}))
                .unwrap()
                .1,
            RELEASES_PAGE
        );
    }
}
