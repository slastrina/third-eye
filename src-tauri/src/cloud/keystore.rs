//! OS credential store for cloud API keys (S02). One `KeyStore` per app,
//! backed by `keyring` v4's platform stores: macOS Keychain, Windows
//! Credential Manager, Linux secret-service. Key bytes flow exactly two
//! ways — into the store on save, and out to S03's client construction via
//! the crate-internal [`KeyStore::get_key`] — and are never logged,
//! serialized, or embedded in error details (keyring error Display carries
//! platform codes/messages, not the secret).

use serde::{Deserialize, Serialize};

/// Service name for the app's production keychain entries. Matches the
/// tauri.conf.json bundle identifier so the item is recognizable in
/// Keychain Access. Tests inject unique per-run service names instead
/// (prompt-safe: an item created and read by the same process sits inside
/// its creator ACL).
pub const KEYCHAIN_SERVICE: &str = "com.slastrina.thirdeye";

/// The cloud providers a key can be stored for. Serialized kebab-case over
/// IPC ("openai" / "anthropic"), mirrored by [`CloudProvider::account`] for
/// the credential-store account name so the two can never drift.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CloudProvider {
    Openai,
    Anthropic,
}

impl CloudProvider {
    /// Every provider, for status assembly and exhaustive tests.
    pub const ALL: [CloudProvider; 2] = [CloudProvider::Openai, CloudProvider::Anthropic];

    /// Credential-store account name — the stable machine-readable id, kept
    /// identical to the serde wire name (pinned by test).
    pub fn account(&self) -> &'static str {
        match self {
            CloudProvider::Openai => "openai",
            CloudProvider::Anthropic => "anthropic",
        }
    }
}

/// Keystore failure taxonomy. Serialized with a `kind` tag over IPC, same
/// convention as `ocr::OcrError`. Details never contain key material.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case", rename_all_fields = "camelCase")]
pub enum CloudKeyError {
    /// The submitted key was empty/whitespace — refused before the OS store
    /// was touched.
    InvalidKey { detail: String },
    /// The OS credential store failed the operation (locked keychain, no
    /// secret-service session, platform error). Absence is NOT an error —
    /// it maps to `Ok(None)` / `Ok(false)`.
    StoreFailed { detail: String },
}

impl CloudKeyError {
    /// Stable machine-readable name mirroring the serde `kind` tag, so grep
    /// for `invalid-key` / `store-failed` in logs works.
    pub fn kind(&self) -> &'static str {
        match self {
            CloudKeyError::InvalidKey { .. } => "invalid-key",
            CloudKeyError::StoreFailed { .. } => "store-failed",
        }
    }
}

fn store_failed(e: keyring::Error) -> CloudKeyError {
    CloudKeyError::StoreFailed { detail: e.to_string() }
}

/// Handle to the OS credential store, scoped to one service name.
pub struct KeyStore {
    service: String,
}

impl KeyStore {
    /// Production store under [`KEYCHAIN_SERVICE`].
    pub fn new() -> Self {
        Self::with_service(KEYCHAIN_SERVICE)
    }

    /// Store under an explicit service name — the test seam. Live tests use
    /// a unique per-run name so they never collide with (or leak into) the
    /// real app's entries.
    pub fn with_service(service: &str) -> Self {
        Self { service: service.to_string() }
    }

    fn entry(&self, provider: CloudProvider) -> Result<keyring::Entry, CloudKeyError> {
        keyring::Entry::new(&self.service, provider.account()).map_err(store_failed)
    }

    /// Store a key, replacing any prior one. Whitespace is trimmed; an
    /// effectively-empty key is refused as `invalid-key` (deleting is an
    /// explicit separate operation, never a side effect of a blank field).
    pub fn set_key(&self, provider: CloudProvider, key: &str) -> Result<(), CloudKeyError> {
        let trimmed = key.trim();
        if trimmed.is_empty() {
            return Err(CloudKeyError::InvalidKey {
                detail: "key is empty — use delete to remove a stored key".into(),
            });
        }
        self.entry(provider)?.set_password(trimmed).map_err(store_failed)?;
        log::info!("cloud: api key stored for {} (key bytes never logged)", provider.account());
        Ok(())
    }

    /// Read the key back. Crate-internal on purpose: S03's client
    /// construction is the only consumer — this must never be exposed as a
    /// tauri command or serialized (R-key-never-crosses-IPC-outbound).
    pub(crate) fn get_key(&self, provider: CloudProvider) -> Result<Option<String>, CloudKeyError> {
        match self.entry(provider)?.get_password() {
            Ok(key) => Ok(Some(key)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(store_failed(e)),
        }
    }

    /// Presence only — the shape the IPC status surface is built from.
    pub fn key_present(&self, provider: CloudProvider) -> Result<bool, CloudKeyError> {
        self.get_key(provider).map(|k| k.is_some())
    }

    /// Delete the stored key. Deleting an absent key is a success (the
    /// user's intent — no key stored — already holds).
    pub fn delete_key(&self, provider: CloudProvider) -> Result<(), CloudKeyError> {
        match self.entry(provider)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => {
                log::info!("cloud: api key deleted for {}", provider.account());
                Ok(())
            }
            Err(e) => Err(store_failed(e)),
        }
    }
}

impl Default for KeyStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The IPC error contract: kind tag + camelCase fields, mirroring
    /// OcrError. A change here is a breaking IPC change.
    #[test]
    fn error_json_shape_is_the_ipc_contract() {
        let invalid = CloudKeyError::InvalidKey { detail: "key is empty".into() };
        let v = serde_json::to_value(&invalid).unwrap();
        assert_eq!(v["kind"], "invalid-key");
        assert_eq!(v["detail"], "key is empty");

        let failed = CloudKeyError::StoreFailed { detail: "keychain locked".into() };
        let v = serde_json::to_value(&failed).unwrap();
        assert_eq!(v["kind"], "store-failed");
        assert_eq!(v["detail"], "keychain locked");
    }

    #[test]
    fn kind_matches_serde_tag_for_every_variant() {
        let all = [
            CloudKeyError::InvalidKey { detail: String::new() },
            CloudKeyError::StoreFailed { detail: String::new() },
        ];
        for err in all {
            let v = serde_json::to_value(&err).unwrap();
            assert_eq!(v["kind"], err.kind());
        }
    }

    /// The production service name must track the bundle identifier — the
    /// keychain item's identity in Keychain Access is the app's identity.
    #[test]
    fn keychain_service_matches_bundle_identifier() {
        let conf: serde_json::Value =
            serde_json::from_str(include_str!("../../tauri.conf.json")).unwrap();
        assert_eq!(conf["identifier"], KEYCHAIN_SERVICE);
    }

    /// The credential-store account name and the serde wire name must never
    /// drift — both are user-visible identities of the same provider.
    #[test]
    fn provider_account_matches_serde_wire_name() {
        for provider in CloudProvider::ALL {
            let wire = serde_json::to_value(provider).unwrap();
            assert_eq!(wire, provider.account());
            let back: CloudProvider =
                serde_json::from_value(serde_json::Value::String(provider.account().into()))
                    .unwrap();
            assert_eq!(back, provider);
        }
    }

    /// Empty/whitespace keys are refused before the OS store is touched —
    /// the error kind is `invalid-key`, not a store failure.
    #[test]
    fn empty_key_is_refused_as_invalid_key() {
        let store = KeyStore::with_service("com.third-eye.test.never-created");
        for blank in ["", "   ", "\n\t"] {
            let err = store.set_key(CloudProvider::Openai, blank).unwrap_err();
            assert_eq!(err.kind(), "invalid-key");
        }
    }

    /// Real-store byte-identical round-trip: set → get → delete → absent.
    /// A unit test (not integration) on purpose: `get_key` is pub(crate),
    /// so reading key bytes back is only possible from inside the crate —
    /// the visibility guarantee this test must not weaken. Unique per-run
    /// service + drop guard keep the real store clean even on panic.
    #[test]
    fn key_round_trips_byte_identical_through_the_real_store() {
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let service =
            format!("com.slastrina.thirdeye.test.unit.{}.{nanos}", std::process::id());
        let seeded = format!("sk-test-UNIT-{nanos}");

        struct Guard(KeyStore);
        impl Drop for Guard {
            fn drop(&mut self) {
                let _ = self.0.delete_key(CloudProvider::Openai);
            }
        }
        let guard = Guard(KeyStore::with_service(&service));
        let store = &guard.0;

        store.set_key(CloudProvider::Openai, &seeded).expect("set against the real store");
        assert_eq!(
            store.get_key(CloudProvider::Openai).unwrap().as_deref(),
            Some(seeded.as_str()),
            "round-tripped key must be byte-identical"
        );
        store.delete_key(CloudProvider::Openai).expect("delete stored key");
        assert!(store.get_key(CloudProvider::Openai).unwrap().is_none());
    }

    /// Real-store probe: presence of a never-created entry is Ok(false) —
    /// absence is typed, not an error. Read-only against the platform store
    /// (no item is ever created), so it runs unprompted everywhere.
    #[test]
    fn absent_key_is_ok_false_not_an_error() {
        let store = KeyStore::with_service(&format!(
            "com.third-eye.test.absent-probe.{}",
            std::process::id()
        ));
        assert_eq!(store.key_present(CloudProvider::Anthropic).unwrap(), false);
        assert!(store.get_key(CloudProvider::Anthropic).unwrap().is_none());
    }
}
