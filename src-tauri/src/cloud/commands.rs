//! IPC surface for the cloud keystore (S02). Presence only, ever: key bytes
//! flow inbound once (set_cloud_api_key) and are never serialized outbound —
//! there is no command that returns a key, and [`CloudKeyStatus`] carries
//! booleans only (pinned by test). Every mutation returns the fresh status
//! so the calling window renders truth without a second query.

use serde::Serialize;
use tauri::State;

use super::keystore::{CloudKeyError, CloudProvider, KeyStore};

/// Managed state: one production [`KeyStore`] for the app's lifetime.
pub struct CloudKeysState {
    store: KeyStore,
}

impl CloudKeysState {
    pub fn new() -> Self {
        Self { store: KeyStore::new() }
    }

    /// The keystore handle — S03's client construction reads keys through
    /// this (and through the crate-internal `get_key` only).
    pub fn store(&self) -> &KeyStore {
        &self.store
    }
}

impl Default for CloudKeysState {
    fn default() -> Self {
        Self::new()
    }
}

/// Presence-per-provider snapshot — the entire outbound IPC vocabulary of
/// the keystore. Booleans only; adding any string field here should trip
/// the `status_carries_presence_booleans_only` contract test.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CloudKeyStatus {
    pub openai_present: bool,
    pub anthropic_present: bool,
}

fn status(store: &KeyStore) -> Result<CloudKeyStatus, CloudKeyError> {
    Ok(CloudKeyStatus {
        openai_present: store.key_present(CloudProvider::Openai)?,
        anthropic_present: store.key_present(CloudProvider::Anthropic)?,
    })
}

/// IPC: store an API key for a provider. The key crosses IPC inbound here —
/// the one legitimate crossing — and is handed straight to the OS store.
#[tauri::command]
pub fn set_cloud_api_key(
    state: State<CloudKeysState>,
    provider: CloudProvider,
    key: String,
) -> Result<CloudKeyStatus, CloudKeyError> {
    state.store.set_key(provider, &key).map_err(|e| {
        log::error!("cloud: set key failed for {} ({})", provider.account(), e.kind());
        e
    })?;
    status(&state.store)
}

/// IPC: delete a provider's stored key. Deleting an absent key succeeds.
#[tauri::command]
pub fn delete_cloud_api_key(
    state: State<CloudKeysState>,
    provider: CloudProvider,
) -> Result<CloudKeyStatus, CloudKeyError> {
    state.store.delete_key(provider).map_err(|e| {
        log::error!("cloud: delete key failed for {} ({})", provider.account(), e.kind());
        e
    })?;
    status(&state.store)
}

/// IPC: presence snapshot for the Settings surface (S04 renders it).
#[tauri::command]
pub fn cloud_key_status(state: State<CloudKeysState>) -> Result<CloudKeyStatus, CloudKeyError> {
    status(&state.store).map_err(|e| {
        log::error!("cloud: key status query failed ({})", e.kind());
        e
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The outbound IPC contract: exactly two camelCase presence booleans.
    /// Any new field — above all a string that could carry key material —
    /// fails this test and forces a deliberate contract change.
    #[test]
    fn status_carries_presence_booleans_only() {
        let s = CloudKeyStatus { openai_present: true, anthropic_present: false };
        let v = serde_json::to_value(s).unwrap();
        let obj = v.as_object().unwrap();
        assert_eq!(obj.len(), 2, "status must stay presence-only: {obj:?}");
        assert_eq!(obj["openaiPresent"], true);
        assert_eq!(obj["anthropicPresent"], false);
        assert!(obj.values().all(|value| value.is_boolean()));
    }
}
