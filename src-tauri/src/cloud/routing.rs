//! Heavy-lane cloud routing applier (M004 S05). The seam that turns the
//! persisted opt-in ([`CloudOptIn`]) + provider selection ([`CloudHeavyProvider`])
//! into a live heavy-lane client swap on the running [`ModelRouter`] — closing
//! the R017 loop: [`build_cloud_client`] finally has a production caller.
//!
//! The rule is fail-safe, always:
//!
//! - **Opt-in ON + a provider selected + a stored key** → [`build_cloud_client`]
//!   returns an `Arc<GuardedClient>` at the provider's `External` HTTPS
//!   endpoint, injected verbatim into the heavy lane via
//!   [`set_lane_client`](ModelRouter::set_lane_client). The M003 guard is
//!   mandatory middleware on the routed path, unchanged.
//! - **Opt-in OFF, no provider, or ANY typed [`CloudClientError`]** → the heavy
//!   lane reverts to its local model via
//!   [`set_lane_model`](ModelRouter::set_lane_model). The revert target is the
//!   store's tri-state pin ([`load_lane_model`]) when the store has spoken, else
//!   the lane's *live* local pin — the record `set_lane_client` deliberately
//!   preserves — so the `THIRD_EYE_HEAVY_MODEL` env fallback is never clobbered
//!   and the local-only default stays intact.
//!
//! A build failure is logged and leaves the lane local; it never panics and
//! never leaks key material (the typed error carries only a kind + safe
//! detail). No new observability surface: the applier logs at info on a route
//! or revert and at error on a typed build failure, and the guard telemetry on
//! the routed client feeds the existing `privacy://state` broadcast.
//!
//! [`build_cloud_client`]: super::client::build_cloud_client
//! [`load_lane_model`]: crate::config::load_lane_model

use tauri::{AppHandle, Manager};

use crate::config::{load_lane_model, CODER_MODEL_KEY, HEAVY_MODEL_KEY};
use crate::llm::commands::LlmState;
use crate::llm::router::{ModelRouter, CODER_LANE, HEAVY_LANE};

use super::client::{build_cloud_client, CloudTransport};
use super::commands::CloudKeysState;
use super::keystore::CloudProvider;
use super::optin::{CloudCoderProvider, CloudHeavyProvider, CloudOptIn};

/// The heavy-lane routing decision from the current gates, factored out of the
/// Tauri runtime so it is unit-testable. Only the *selection* is decided here;
/// whether a cloud build actually succeeds (key presence, store health) is
/// [`build_cloud_client`](super::client::build_cloud_client)'s job.
#[derive(Debug, Clone, Copy, PartialEq)]
enum HeavyRoute {
    /// Opt-in on and a provider selected — attempt a guarded cloud build.
    TryCloud(CloudProvider),
    /// Opt-in off or no provider selected — the heavy lane belongs local.
    Local,
}

/// Decide the heavy-lane route from the two gates. Cloud is attempted only when
/// opt-in is on *and* a provider is selected; every other combination is local.
fn heavy_route(optin_enabled: bool, provider: Option<CloudProvider>) -> HeavyRoute {
    match (optin_enabled, provider) {
        (true, Some(provider)) => HeavyRoute::TryCloud(provider),
        _ => HeavyRoute::Local,
    }
}

/// Re-evaluate heavy-lane routing from the current opt-in + provider selection
/// and apply it to the running router. Called from [`apply_cloud_opt_in`],
/// [`apply_cloud_heavy_provider`], and at startup after the persisted selection
/// is restored. Idempotent and fail-safe: opt-in off / no provider / any typed
/// build failure leaves (or restores) the heavy lane on its local model.
///
/// [`apply_cloud_opt_in`]: super::optin::apply_cloud_opt_in
/// [`apply_cloud_heavy_provider`]: super::optin::apply_cloud_heavy_provider
pub fn apply_cloud_routing(app: &AppHandle) {
    let router = app.state::<LlmState>().router();
    let optin_enabled = app.state::<CloudOptIn>().enabled();
    // Each cloud-capable lane is routed independently from its own persisted
    // provider selection (coding-agent S6): the heavy lane can be on Claude
    // while the coder stays local, and vice versa.
    let heavy_provider = app.state::<CloudHeavyProvider>().provider();
    apply_lane_routing(
        app,
        &router,
        HEAVY_LANE,
        HEAVY_MODEL_KEY,
        optin_enabled,
        heavy_provider,
    );
    let coder_provider = app.state::<CloudCoderProvider>().provider();
    apply_lane_routing(
        app,
        &router,
        CODER_LANE,
        CODER_MODEL_KEY,
        optin_enabled,
        coder_provider,
    );
}

/// Route ONE lane from its gates: cloud when opt-in + provider + key all
/// hold, local otherwise — fail-safe on every typed build failure.
fn apply_lane_routing(
    app: &AppHandle,
    router: &ModelRouter,
    lane: &str,
    model_key: &str,
    optin_enabled: bool,
    provider: Option<CloudProvider>,
) {
    match heavy_route(optin_enabled, provider) {
        HeavyRoute::TryCloud(provider) => {
            // The router's own shared guard: the routed client's block/redaction
            // telemetry must feed the same GuardState the privacy://state
            // broadcast reads, so a routed guard block shows live in Settings.
            let guard = router.guard_state();
            let keys = app.state::<CloudKeysState>();
            match build_cloud_client(
                &app.state::<CloudOptIn>(),
                keys.store(),
                provider,
                guard,
                &CloudTransport::default(),
            ) {
                Ok(client) => {
                    if let Err(e) = router.set_lane_client(lane, client) {
                        // The lanes are construction invariants, so this is
                        // unreachable in practice; log and leave the lane as-is.
                        log::error!("cloud: {lane} lane route rejected by router: {e}");
                    } else {
                        log::info!("cloud: {lane} lane routed to {}", provider.account());
                    }
                }
                Err(e) => {
                    // Typed refusal (opt-in flipped off mid-apply, no stored
                    // key, or a store read failure): logged, lane stays local.
                    log::error!(
                        "cloud: {lane} lane cloud build refused ({}), staying local",
                        e.kind()
                    );
                    revert_lane_to_local(app, router, lane, model_key);
                }
            }
        }
        HeavyRoute::Local => revert_lane_to_local(app, router, lane, model_key),
    }
}

/// Rebuild one lane's local guarded client, undoing any prior cloud
/// injection. The target model is the store's persisted pin when the store has
/// spoken ([`load_lane_model`] returns `Some`, including an explicit unpin),
/// else the lane's *current* local pin — which `set_lane_client` preserves
/// across a cloud swap — so the `THIRD_EYE_*_MODEL` env fallbacks are never
/// lost. Idempotent: when the lane is already local this simply rebuilds the
/// same client.
fn revert_lane_to_local(app: &AppHandle, router: &ModelRouter, lane: &str, model_key: &str) {
    let target = match load_lane_model(app, model_key) {
        // The store spoke — its decision wins (Some(id) pins, None unpins).
        Some(pin) => pin,
        // No store key: keep the live local pin (env fallback or default).
        None => lane_model(router, lane),
    };
    match router.set_lane_model(lane, target.clone()) {
        Ok(_) => log::info!(
            "cloud: {lane} lane reverted to local ({})",
            target.as_deref().unwrap_or("default")
        ),
        Err(e) => log::error!("cloud: {lane} lane local revert rejected by router: {e}"),
    }
}

/// One lane's current model pin as recorded in [`ModelInfo`], the local
/// fallback the revert restores when the store has no persisted pin.
fn lane_model(router: &ModelRouter, lane: &str) -> Option<String> {
    router
        .info()
        .lanes
        .into_iter()
        .find(|l| l.name == lane)
        .and_then(|l| l.model_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opt_in_off_routes_local_regardless_of_provider() {
        // The offline-first default: opt-in off is always local, even with a
        // provider selected (a stale selection must never reach cloud).
        assert_eq!(heavy_route(false, None), HeavyRoute::Local);
        assert_eq!(
            heavy_route(false, Some(CloudProvider::Openai)),
            HeavyRoute::Local
        );
        assert_eq!(
            heavy_route(false, Some(CloudProvider::Anthropic)),
            HeavyRoute::Local
        );
    }

    #[test]
    fn opt_in_on_without_provider_routes_local() {
        // Opt-in alone is not enough — a provider must be chosen to leave local.
        assert_eq!(heavy_route(true, None), HeavyRoute::Local);
    }

    #[test]
    fn opt_in_on_with_provider_attempts_cloud() {
        // The only cloud-routing combination: opt-in on AND a provider selected.
        assert_eq!(
            heavy_route(true, Some(CloudProvider::Openai)),
            HeavyRoute::TryCloud(CloudProvider::Openai)
        );
        assert_eq!(
            heavy_route(true, Some(CloudProvider::Anthropic)),
            HeavyRoute::TryCloud(CloudProvider::Anthropic)
        );
    }
}
