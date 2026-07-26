//! The single guarded cloud-client construction choke point (M004 S03).
//!
//! [`build_cloud_client`] is the ONLY path that builds a remote-provider
//! [`OpenAiClient`], and it does so behind two mandatory gates:
//!
//! 1. **Opt-in.** If [`CloudOptIn`] is off, construction fails typed
//!    ([`CloudClientError::OptinDisabled`]) *before the keystore is touched* —
//!    an opted-out app can never even look up a key.
//! 2. **Key presence.** Opt-in on but no stored key → typed
//!    [`CloudClientError::NoApiKey`]; a keystore read error → typed
//!    [`CloudClientError::StoreFailed`].
//!
//! On success the bearer-authed client is wrapped in a [`GuardedClient`] at
//! the provider's real HTTPS endpoint. Those endpoints are `External` under
//! [`EndpointTrust`], so the M003 guard redacts or blocks every request —
//! this construction site is the reason `scripts/check-guard-mounts.sh`
//! allowlists `cloud/client.rs` (co-located with its `GuardedClient::new`
//! wrap).
//!
//! [`CloudTransport`] is the test seam: production is `default()` (plain
//! prod endpoints, system trust roots), while the S03 integration test
//! injects a fixture root cert, a loopback `.resolve` for `cloud.test`, and
//! an endpoint override so a real TLS wire terminates at a local mock.
//!
//! [`EndpointTrust`]: crate::llm::guard::EndpointTrust

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;

use crate::llm::guard::{GuardState, GuardedClient};
use crate::llm::openai::OpenAiClient;

use super::keystore::{CloudKeyError, CloudProvider, KeyStore};
use super::optin::CloudOptIn;

/// Fail fast on an unreachable provider — same posture as the loopback
/// client, so "offline" is a banner, not a hang.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);

/// OpenAI's real API base. [`OpenAiClient`] appends `/v1/chat/completions`,
/// yielding the canonical `https://api.openai.com/v1/chat/completions`.
pub const OPENAI_ENDPOINT: &str = "https://api.openai.com";

/// Anthropic's OpenAI-compatible API base (heavy-lane provider selection is
/// refined in S04; classification as `External` is what this slice needs).
pub const ANTHROPIC_ENDPOINT: &str = "https://api.anthropic.com";

/// The real HTTPS base for a provider — the single definition site both the
/// client construction and the trust classification read from.
pub fn provider_endpoint(provider: CloudProvider) -> &'static str {
    match provider {
        CloudProvider::Openai => OPENAI_ENDPOINT,
        CloudProvider::Anthropic => ANTHROPIC_ENDPOINT,
    }
}

/// Why guarded cloud-client construction refused. Kebab-case `kind` tag +
/// camelCase fields over IPC, the same convention as [`CloudKeyError`] and
/// `OcrError`; details never carry key material.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(
    tag = "kind",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase"
)]
pub enum CloudClientError {
    /// Cloud opt-in is off — no remote client may be constructed. Returned
    /// before the keystore is ever consulted.
    OptinDisabled,
    /// Opt-in is on but no API key is stored for the provider.
    NoApiKey { provider: CloudProvider },
    /// The OS credential store failed the lookup (locked keychain, no
    /// secret-service session). Absence is NOT this — that is `NoApiKey`.
    StoreFailed { detail: String },
}

impl CloudClientError {
    /// Stable machine-readable name mirroring the serde `kind` tag.
    pub fn kind(&self) -> &'static str {
        match self {
            CloudClientError::OptinDisabled => "optin-disabled",
            CloudClientError::NoApiKey { .. } => "no-api-key",
            CloudClientError::StoreFailed { .. } => "store-failed",
        }
    }
}

/// Transport tweaks for the cloud client — the test seam. Production is
/// [`default()`](Default::default): no endpoint override, system trust roots,
/// no resolve override. The S03 integration test builds one carrying the
/// fixture root cert, a `cloud.test → 127.0.0.1:{port}` resolve, and the
/// matching endpoint override so a real TLS handshake terminates at the mock.
#[derive(Default, Clone)]
pub struct CloudTransport {
    endpoint: Option<String>,
    root_cert: Option<reqwest::Certificate>,
    resolve: Option<(String, SocketAddr)>,
}

impl CloudTransport {
    /// Override the endpoint the client targets (tests point it at the mock's
    /// `https://cloud.test:{port}`; production uses [`provider_endpoint`]).
    pub fn with_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = Some(endpoint.into());
        self
    }

    /// Trust an additional root certificate (the committed `cloud.test`
    /// fixture) on top of the system roots.
    pub fn with_root_certificate(mut self, cert: reqwest::Certificate) -> Self {
        self.root_cert = Some(cert);
        self
    }

    /// Pin `host` to a fixed socket address, bypassing DNS — lets the test
    /// resolve `cloud.test` to a loopback listener while the TLS SNI/cert
    /// still validate against the real hostname.
    pub fn with_resolve(mut self, host: impl Into<String>, addr: SocketAddr) -> Self {
        self.resolve = Some((host.into(), addr));
        self
    }

    /// The endpoint this transport targets for `provider` — the override when
    /// set, else the provider's real HTTPS base.
    fn endpoint_for(&self, provider: CloudProvider) -> String {
        self.endpoint
            .clone()
            .unwrap_or_else(|| provider_endpoint(provider).to_string())
    }

    /// Build the reqwest client carrying the connect timeout plus any TLS
    /// trust / resolve tweaks. Static config, so construction cannot fail.
    fn http_client(&self) -> reqwest::Client {
        let mut builder = reqwest::Client::builder().connect_timeout(CONNECT_TIMEOUT);
        if let Some(cert) = &self.root_cert {
            builder = builder.add_root_certificate(cert.clone());
        }
        if let Some((host, addr)) = &self.resolve {
            builder = builder.resolve(host, *addr);
        }
        builder
            .build()
            .expect("reqwest client construction cannot fail with static config")
    }
}

/// Build a guarded cloud client — the single production construction path for
/// a remote-provider client. Gates on opt-in, then key presence; on success
/// returns an `Arc<GuardedClient>` whose inner [`OpenAiClient`] carries the
/// bearer key and the provider's `External` HTTPS endpoint, so the M003 guard
/// redacts or blocks every outbound request.
pub fn build_cloud_client(
    optin: &CloudOptIn,
    keystore: &KeyStore,
    provider: CloudProvider,
    guard: Arc<GuardState>,
    transport: &CloudTransport,
) -> Result<Arc<GuardedClient>, CloudClientError> {
    // Gate 1: opted out → refuse before the keystore is ever consulted.
    if !optin.enabled() {
        log::debug!(
            "cloud: construction refused for {} (opt-in off)",
            provider.account()
        );
        return Err(CloudClientError::OptinDisabled);
    }

    // Gate 2: a present key is required; absence is typed, not a store error.
    let key = match keystore.get_key(provider) {
        Ok(Some(key)) => key,
        Ok(None) => {
            log::info!("cloud: no api key stored for {}", provider.account());
            return Err(CloudClientError::NoApiKey { provider });
        }
        Err(e) => {
            log::error!(
                "cloud: keystore read failed for {} ({})",
                provider.account(),
                e.kind()
            );
            return Err(CloudClientError::StoreFailed {
                detail: store_detail(e),
            });
        }
    };

    let endpoint = transport.endpoint_for(provider);
    let client = OpenAiClient::new(&endpoint)
        .with_api_key(key)
        .with_http_client(transport.http_client());
    // The one guard wrap for cloud traffic: the endpoint is External, so
    // every request is redacted or blocked before the socket write.
    let guarded = GuardedClient::new(Arc::new(client), guard);
    log::info!(
        "cloud: built guarded client for {} at {endpoint} (trust={:?})",
        provider.account(),
        guarded.trust()
    );
    Ok(Arc::new(guarded))
}

/// Detail for a `StoreFailed`, preserving the keystore's own message without
/// ever carrying key material (keyring Display is platform codes only).
fn store_detail(e: CloudKeyError) -> String {
    match e {
        CloudKeyError::StoreFailed { detail } => detail,
        other => other.kind().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::guard::EndpointTrust;

    /// A keystore pointed at a unique per-run service, drop-guarded so the
    /// real OS store is left clean even on panic (keystore.rs precedent).
    struct TestStore {
        store: KeyStore,
    }

    impl TestStore {
        fn new(tag: &str) -> Self {
            use std::time::{SystemTime, UNIX_EPOCH};
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let service = format!(
                "com.slastrina.thirdeye.test.client.{tag}.{}.{nanos}",
                std::process::id()
            );
            Self {
                store: KeyStore::with_service(&service),
            }
        }
    }

    impl Drop for TestStore {
        fn drop(&mut self) {
            for provider in CloudProvider::ALL {
                let _ = self.store.delete_key(provider);
            }
        }
    }

    fn guard() -> Arc<GuardState> {
        Arc::new(GuardState::new())
    }

    #[test]
    fn opt_in_off_yields_optin_disabled_without_touching_the_keystore() {
        // The keystore points at a never-created service; getting
        // optin-disabled (never store-failed / no-api-key) proves the opt-in
        // gate short-circuited before any keystore read.
        let optin = CloudOptIn::new(); // default off
        let keystore = KeyStore::with_service("com.third-eye.test.client.never-read");
        let err = build_cloud_client(
            &optin,
            &keystore,
            CloudProvider::Openai,
            guard(),
            &CloudTransport::default(),
        )
        .err()
        .expect("opt-in off must refuse construction");
        assert_eq!(err.kind(), "optin-disabled");
        assert_eq!(err, CloudClientError::OptinDisabled);
    }

    #[test]
    fn opt_in_on_empty_keystore_yields_no_api_key() {
        let _keychain = crate::cloud::real_keychain_test_lock();
        let optin = CloudOptIn::new();
        optin.set_enabled(true);
        let ts = TestStore::new("empty");
        let err = build_cloud_client(
            &optin,
            &ts.store,
            CloudProvider::Openai,
            guard(),
            &CloudTransport::default(),
        )
        .err()
        .expect("opt-in on + empty keystore must refuse construction");
        assert_eq!(err.kind(), "no-api-key");
        assert_eq!(
            err,
            CloudClientError::NoApiKey {
                provider: CloudProvider::Openai
            }
        );
    }

    #[test]
    fn cloud_endpoints_classify_external() {
        // The whole point of routing cloud through the guard: the real
        // provider bases are External, so the guard governs every request.
        for provider in CloudProvider::ALL {
            assert_eq!(
                EndpointTrust::classify(provider_endpoint(provider)),
                EndpointTrust::External,
                "{} endpoint must be External",
                provider.account()
            );
        }
    }

    #[test]
    fn successful_build_wraps_in_guarded_client_with_external_trust() {
        let _keychain = crate::cloud::real_keychain_test_lock();
        // Opt-in on + a seeded key → an Arc<GuardedClient> at the provider's
        // real HTTPS endpoint, classified External so the guard is engaged.
        // No network call is made — construction only.
        let optin = CloudOptIn::new();
        optin.set_enabled(true);
        let ts = TestStore::new("built");
        ts.store
            .set_key(CloudProvider::Openai, "sk-test-CLIENT-key")
            .expect("seed key in the real store");

        let client = build_cloud_client(
            &optin,
            &ts.store,
            CloudProvider::Openai,
            guard(),
            &CloudTransport::default(),
        )
        .expect("opt-in on + key present builds");
        assert_eq!(client.trust(), EndpointTrust::External);
        assert_eq!(
            crate::llm::LlmClient::endpoint(client.as_ref()),
            OPENAI_ENDPOINT,
            "the guarded client targets the provider's real HTTPS base"
        );
    }

    #[test]
    fn error_kind_matches_serde_tag_for_every_variant() {
        let all = [
            CloudClientError::OptinDisabled,
            CloudClientError::NoApiKey {
                provider: CloudProvider::Anthropic,
            },
            CloudClientError::StoreFailed {
                detail: "keychain locked".into(),
            },
        ];
        for err in all {
            let v = serde_json::to_value(&err).unwrap();
            assert_eq!(v["kind"], err.kind());
        }
    }

    #[test]
    fn no_api_key_error_serializes_provider_kebab_case() {
        let err = CloudClientError::NoApiKey {
            provider: CloudProvider::Openai,
        };
        let v = serde_json::to_value(&err).unwrap();
        assert_eq!(v["kind"], "no-api-key");
        assert_eq!(v["provider"], "openai");
    }

    #[test]
    fn transport_defaults_to_the_providers_real_endpoint() {
        let prod = CloudTransport::default();
        assert_eq!(prod.endpoint_for(CloudProvider::Openai), OPENAI_ENDPOINT);
        assert_eq!(
            prod.endpoint_for(CloudProvider::Anthropic),
            ANTHROPIC_ENDPOINT
        );
        // The override seam wins when set (the S03 mock's endpoint).
        let overridden = CloudTransport::default().with_endpoint("https://cloud.test:8443");
        assert_eq!(
            overridden.endpoint_for(CloudProvider::Openai),
            "https://cloud.test:8443"
        );
    }
}
