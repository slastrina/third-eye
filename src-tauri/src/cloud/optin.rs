//! Cloud opt-in state (M004 S03). One shared [`CloudOptIn`] is the single
//! gate that lets any remote-provider client be constructed: it defaults OFF
//! (the local-only default is untouched) and only [`build_cloud_client`] ever
//! reads it. Persistence follows the watcher/nudges applier pattern
//! (MEM049/MEM053): the in-memory toggle is mutated only through
//! [`apply_cloud_opt_in`], persisted as `cloudOptin` in settings.json, and on
//! a persist failure the toggle is rolled back so an unpersisted opt-in can
//! never silently revert (or, worse, silently persist) across a restart.
//!
//! S04 surfaces the toggle to the user: [`set_cloud_optin`] /
//! [`cloud_optin_status`] are the IPC surface, [`apply_cloud_opt_in`] now
//! emits the [`CLOUD_OPTIN_EVENT`] broadcast so every webview stays truthful,
//! and the heavy-lane provider selection ([`CloudHeavyProvider`]) is persisted
//! and readable here (live routing through `build_cloud_client` stays S05's).
//!
//! [`build_cloud_client`]: super::client::build_cloud_client

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State};

use super::keystore::CloudProvider;

/// Cloud opt-in broadcast: every opt-in mutation emits the resulting
/// [`CloudOptInStatus`] app-wide, so the ACL-admitted Settings webview (and
/// any other window) stays truthful whichever surface flipped the toggle —
/// the privacy/watcher `emit_state` precedent.
pub const CLOUD_OPTIN_EVENT: &str = "cloud://optin";

/// Queryable opt-in state: `{ enabled, persistError }` — the same
/// health-as-value shape as `PrivacyStatus`/`AutostartStatus` (R007).
/// `persistError` carries the most recent persist failure so a toggle that
/// could not be saved stays visible after the fact (never an IPC rejection).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CloudOptInStatus {
    pub enabled: bool,
    pub persist_error: Option<String>,
}

/// The one shared cloud opt-in core. Pure in-memory state — persistence and
/// any broadcast live in the appliers — so the default-off and toggle
/// invariants are unit-testable without a Tauri runtime. `enabled` defaults
/// to `false`: cloud egress is impossible until the user opts in.
pub struct CloudOptIn {
    enabled: AtomicBool,
    /// Most recent persist failure (kept until a save succeeds), the surface
    /// S04's status will render — same shape as the watcher/nudge cores.
    persist_error: Mutex<Option<String>>,
}

impl Default for CloudOptIn {
    fn default() -> Self {
        Self {
            enabled: AtomicBool::new(false),
            persist_error: Mutex::new(None),
        }
    }
}

impl CloudOptIn {
    /// Cloud starts opted out; persisted state is applied in `setup()`.
    pub fn new() -> Self {
        Self::default()
    }

    pub fn enabled(&self) -> bool {
        self.enabled.load(Ordering::SeqCst)
    }

    /// Set the toggle, returning the previous value so the applier can roll
    /// back on a persist failure.
    pub fn set_enabled(&self, enabled: bool) -> bool {
        self.enabled.swap(enabled, Ordering::SeqCst)
    }

    /// Record (or clear) the most recent persist failure.
    pub fn record_persist_error(&self, error: Option<String>) {
        *self.persist_error.lock().unwrap() = error;
    }

    /// The most recent persist failure, if any.
    pub fn persist_error(&self) -> Option<String> {
        self.persist_error.lock().unwrap().clone()
    }

    /// Current opt-in state as health-as-value — never an error, safe to poll.
    pub fn status(&self) -> CloudOptInStatus {
        CloudOptInStatus {
            enabled: self.enabled(),
            persist_error: self.persist_error(),
        }
    }
}

/// The one shared opt-in applier. Persists to settings.json; on persist
/// failure the in-memory toggle is rolled back (an unpersisted opt-in must
/// never silently take or revert across a restart) and the error naming the
/// persist path stays queryable. Broadcasts the resulting [`CloudOptInStatus`]
/// app-wide ([`CLOUD_OPTIN_EVENT`]) so every webview updates live, then
/// returns that same status — the value the calling window renders without a
/// second query.
pub fn apply_cloud_opt_in(app: &AppHandle, desired: bool, via: &str) -> CloudOptInStatus {
    let state = app.state::<CloudOptIn>();
    let previous = state.set_enabled(desired);
    match crate::config::save_cloud_optin(app, desired) {
        Ok(()) => {
            state.record_persist_error(None);
            log::info!(
                "cloud: opt-in {} (via {via})",
                if desired { "enabled" } else { "disabled" }
            );
        }
        Err(e) => {
            state.set_enabled(previous);
            log::error!("cloud: {e}");
            state.record_persist_error(Some(e));
        }
    }
    let status = state.status();
    // Broadcast failure is cosmetic (the truth stays queryable via
    // `cloud_optin_status`), so it is logged, never bubbled.
    if let Err(e) = app.emit(CLOUD_OPTIN_EVENT, status.clone()) {
        log::warn!("cloud: opt-in broadcast failed: {e}");
    }
    // Re-evaluate heavy-lane routing (S05): flipping opt-in on may route the
    // heavy lane to cloud; flipping it off reverts it to local. Reads the
    // *effective* toggle (already rolled back on a persist failure), so a
    // failed persist never routes to cloud.
    super::routing::apply_cloud_routing(app);
    status
}

/// Set the cloud opt-in toggle from the UI. Returns the resulting
/// [`CloudOptInStatus`] instead of erroring — a persist failure is data the
/// caller can render, the same contract as `set_privacy_mode` /
/// `set_watcher_enabled` (R007).
#[tauri::command]
pub fn set_cloud_optin(app: AppHandle, enable: bool) -> CloudOptInStatus {
    apply_cloud_opt_in(&app, enable, "ipc")
}

/// Current cloud opt-in state — health-as-value beside `privacy_status` and
/// `watcher_status` (R007): a value at any time, never an error.
#[tauri::command]
pub fn cloud_optin_status(state: State<'_, CloudOptIn>) -> CloudOptInStatus {
    state.status()
}

/// Apply the persisted opt-in at startup (called from `setup()`). In-memory
/// only: no re-save, no broadcast — nothing is listening yet. An absent key
/// keeps the default (off); load failures are logged inside `config`, never
/// fatal, and likewise leave the safe default in place.
pub fn apply_persisted_cloud_opt_in(app: &AppHandle) {
    if let Some(enabled) = crate::config::load_cloud_optin(app) {
        app.state::<CloudOptIn>().set_enabled(enabled);
        log::info!("cloud: applied persisted opt-in (enabled={enabled})");
    }
}

/// The heavy-lane cloud provider selection (S04). Which remote provider the
/// heavy lane should target once cloud routing lands (S05) — persisted here so
/// the choice survives a restart and the Settings surface can render it.
/// `None` means "no cloud provider selected"; the default, and what garbage in
/// the store falls back to. Pure in-memory state — persistence lives in the
/// applier — mirroring [`CloudOptIn`].
#[derive(Debug, Default)]
pub struct CloudHeavyProvider {
    provider: Mutex<Option<CloudProvider>>,
    persist_error: Mutex<Option<String>>,
}

impl CloudHeavyProvider {
    pub fn new() -> Self {
        Self::default()
    }

    /// The currently selected heavy-lane provider, if any.
    pub fn provider(&self) -> Option<CloudProvider> {
        *self.provider.lock().unwrap()
    }

    /// Set the selection, returning the previous value so the applier can roll
    /// back on a persist failure.
    pub fn set_provider(&self, provider: Option<CloudProvider>) -> Option<CloudProvider> {
        std::mem::replace(&mut self.provider.lock().unwrap(), provider)
    }

    /// Record (or clear) the most recent persist failure.
    pub fn record_persist_error(&self, error: Option<String>) {
        *self.persist_error.lock().unwrap() = error;
    }

    /// Current selection as health-as-value — never an error, safe to poll.
    pub fn status(&self) -> CloudHeavyProviderStatus {
        CloudHeavyProviderStatus {
            provider: self.provider(),
            persist_error: self.persist_error.lock().unwrap().clone(),
        }
    }
}

/// Queryable heavy-lane provider selection: `{ provider, persistError }` —
/// health-as-value like [`CloudOptInStatus`]. `provider` is the kebab-case
/// provider name or `null` (unselected); `persistError` carries the most
/// recent persist failure so a selection that could not be saved stays
/// visible (never an IPC rejection).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CloudHeavyProviderStatus {
    pub provider: Option<CloudProvider>,
    pub persist_error: Option<String>,
}

/// The one shared heavy-provider applier. Persists to settings.json; on
/// persist failure the in-memory selection is rolled back (an unpersisted
/// selection must never silently take or revert across a restart) and the
/// error naming the persist path stays queryable. Returns the resulting
/// status — the value the calling window renders without a second query. No
/// broadcast: the selection is display-only until S05 routes it.
pub fn apply_cloud_heavy_provider(
    app: &AppHandle,
    desired: Option<CloudProvider>,
    via: &str,
) -> CloudHeavyProviderStatus {
    let state = app.state::<CloudHeavyProvider>();
    let previous = state.set_provider(desired);
    match crate::config::save_cloud_heavy_provider(app, desired) {
        Ok(()) => {
            state.record_persist_error(None);
            log::info!(
                "cloud: heavy provider set to {} (via {via})",
                desired.map(|p| p.account()).unwrap_or("none")
            );
        }
        Err(e) => {
            state.set_provider(previous);
            log::error!("cloud: {e}");
            state.record_persist_error(Some(e));
        }
    }
    // Re-evaluate heavy-lane routing (S05): a new provider selection with
    // opt-in on routes the heavy lane to it; clearing it reverts to local.
    // Reads the *effective* selection (already rolled back on a persist
    // failure), so a failed persist never routes to cloud.
    super::routing::apply_cloud_routing(app);
    state.status()
}

/// Apply the persisted heavy-provider selection at startup (called from
/// `setup()`). In-memory only: no re-save, no broadcast — nothing is listening
/// yet. An absent/garbage key keeps the default (unselected); load failures
/// are logged inside `config`, never fatal.
pub fn apply_persisted_cloud_heavy_provider(app: &AppHandle) {
    if let Some(provider) = crate::config::load_cloud_heavy_provider(app) {
        app.state::<CloudHeavyProvider>()
            .set_provider(Some(provider));
        log::info!(
            "cloud: applied persisted heavy provider ({})",
            provider.account()
        );
    }
}

/// Set the heavy-lane cloud provider from the UI (`null` clears it). Returns
/// the resulting [`CloudHeavyProviderStatus`] instead of erroring — a persist
/// failure is data the caller can render (R007).
#[tauri::command]
pub fn set_cloud_heavy_provider(
    app: AppHandle,
    provider: Option<CloudProvider>,
) -> CloudHeavyProviderStatus {
    apply_cloud_heavy_provider(&app, provider, "ipc")
}

/// Current heavy-lane provider selection — health-as-value, never an error.
#[tauri::command]
pub fn cloud_heavy_provider(state: State<'_, CloudHeavyProvider>) -> CloudHeavyProviderStatus {
    state.status()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opt_in_defaults_off() {
        // The local-only default: cloud egress is impossible until opt-in.
        let optin = CloudOptIn::new();
        assert!(!optin.enabled(), "cloud opt-in must default to off");
        assert_eq!(optin.persist_error(), None);
    }

    #[test]
    fn set_enabled_toggles_and_returns_previous_for_rollback() {
        let optin = CloudOptIn::new();
        assert!(!optin.set_enabled(true), "previous value was off");
        assert!(optin.enabled());
        assert!(optin.set_enabled(false), "previous value was on");
        assert!(!optin.enabled());
    }

    #[test]
    fn persist_errors_are_queryable_and_clearable() {
        let optin = CloudOptIn::new();
        optin.record_persist_error(Some("failed to persist cloudOptin=true".into()));
        assert!(optin.persist_error().unwrap().contains("cloudOptin"));
        optin.record_persist_error(None);
        assert_eq!(optin.persist_error(), None);
    }

    #[test]
    fn event_name_is_the_ipc_contract() {
        // src/cloud-state.ts (T02) and e2e/cloud.spec.ts (T03) listen on this
        // exact string — the watcher://state / capture://privacy precedent.
        assert_eq!(CLOUD_OPTIN_EVENT, "cloud://optin");
    }

    #[test]
    fn optin_status_is_health_as_value_camelcase() {
        // The broadcast/command payload contract: { enabled, persistError }.
        let optin = CloudOptIn::new();
        optin.set_enabled(true);
        optin.record_persist_error(Some("failed to persist cloudOptin=true".into()));
        let status = optin.status();
        assert!(status.enabled);
        let v = serde_json::to_value(&status).unwrap();
        let obj = v.as_object().unwrap();
        assert_eq!(obj.len(), 2, "opt-in status shape drifted: {obj:?}");
        assert_eq!(obj["enabled"], true);
        assert!(obj["persistError"].as_str().unwrap().contains("cloudOptin"));
    }

    #[test]
    fn optin_status_serializes_no_error_as_null() {
        let status = CloudOptIn::new().status();
        let v = serde_json::to_value(&status).unwrap();
        assert_eq!(v["enabled"], false);
        assert!(
            v["persistError"].is_null(),
            "absent persist error must be JSON null"
        );
    }

    #[test]
    fn heavy_provider_defaults_unselected_and_toggles_returning_previous() {
        let hp = CloudHeavyProvider::new();
        assert_eq!(
            hp.provider(),
            None,
            "heavy provider must default to unselected"
        );
        assert_eq!(
            hp.set_provider(Some(CloudProvider::Openai)),
            None,
            "previous was none"
        );
        assert_eq!(hp.provider(), Some(CloudProvider::Openai));
        assert_eq!(
            hp.set_provider(Some(CloudProvider::Anthropic)),
            Some(CloudProvider::Openai),
            "set_provider returns the previous selection for rollback"
        );
        assert_eq!(hp.set_provider(None), Some(CloudProvider::Anthropic));
        assert_eq!(hp.provider(), None);
    }

    #[test]
    fn heavy_provider_status_is_health_as_value_camelcase() {
        // { provider, persistError }: provider is the kebab-case wire name or
        // null; persistError carries a save failure as data, never a reject.
        let hp = CloudHeavyProvider::new();
        hp.set_provider(Some(CloudProvider::Anthropic));
        let v = serde_json::to_value(hp.status()).unwrap();
        let obj = v.as_object().unwrap();
        assert_eq!(obj.len(), 2, "heavy provider status shape drifted: {obj:?}");
        assert_eq!(obj["provider"], "anthropic");
        assert!(obj["persistError"].is_null());

        hp.set_provider(None);
        hp.record_persist_error(Some("failed to persist cloudHeavyProvider".into()));
        let v = serde_json::to_value(hp.status()).unwrap();
        assert!(
            v["provider"].is_null(),
            "unselected provider must be JSON null"
        );
        assert!(v["persistError"]
            .as_str()
            .unwrap()
            .contains("cloudHeavyProvider"));
    }
}
