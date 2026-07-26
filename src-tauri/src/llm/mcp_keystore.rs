//! OS credential store for remote MCP server bearer tokens (S05, R018).
//! Mirrors [`crate::cloud::keystore::KeyStore`] exactly — same `keyring` v4
//! platform stores (macOS Keychain, Windows Credential Manager, Linux
//! secret-service) and the same "bytes flow exactly two ways" invariant: a
//! token enters the store on [`McpAuthStore::set_token`] and leaves only via
//! the crate-internal [`McpAuthStore::get_token`] that the http connect path
//! (T04) reads to build the `Authorization` header. It is never logged,
//! serialized, or embedded in an error detail.
//!
//! The one shape difference from the cloud store: cloud is keyed by the closed
//! [`CloudProvider`](crate::cloud::keystore::CloudProvider) enum, but a remote
//! MCP server is identified by an arbitrary user-chosen account key — the
//! non-secret `auth_ref` persisted in `settings.json` by
//! [`McpServerConfig::auth_ref`](crate::llm::mcp::McpServerConfig::auth_ref).
//! So the account is a `&str` the caller supplies (conventionally namespaced
//! `mcp:<server-id>` by the frontend), stored under the shared bundle-id
//! service so it sits alongside the cloud entries in Keychain Access.

use crate::cloud::keystore::KEYCHAIN_SERVICE;

/// Keystore failure taxonomy — the MCP twin of
/// [`CloudKeyError`](crate::cloud::keystore::CloudKeyError). Serialized with a
/// `kind` tag over IPC (same convention as `ocr::OcrError`). Details never
/// contain token material.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(
    tag = "kind",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase"
)]
pub enum McpAuthError {
    /// The submitted account key was empty/whitespace — a token cannot be
    /// filed under a blank account. Refused before the OS store is touched.
    InvalidRef { detail: String },
    /// The submitted token was empty/whitespace — refused before the OS store
    /// is touched (deleting is an explicit separate operation, never a side
    /// effect of a blank field).
    InvalidToken { detail: String },
    /// The OS credential store failed the operation (locked keychain, no
    /// secret-service session, platform error). Absence is NOT an error — it
    /// maps to `Ok(None)` / `Ok(false)`.
    StoreFailed { detail: String },
}

impl McpAuthError {
    /// Stable machine-readable name mirroring the serde `kind` tag, so grep for
    /// `invalid-ref` / `invalid-token` / `store-failed` in logs works.
    pub fn kind(&self) -> &'static str {
        match self {
            McpAuthError::InvalidRef { .. } => "invalid-ref",
            McpAuthError::InvalidToken { .. } => "invalid-token",
            McpAuthError::StoreFailed { .. } => "store-failed",
        }
    }
}

fn store_failed(e: keyring::Error) -> McpAuthError {
    McpAuthError::StoreFailed {
        detail: e.to_string(),
    }
}

/// Handle to the OS credential store for MCP bearer tokens, scoped to one
/// service name. Accounts are the arbitrary `auth_ref` keys of individual
/// remote servers.
pub struct McpAuthStore {
    service: String,
}

impl McpAuthStore {
    /// Production store under the shared bundle-id [`KEYCHAIN_SERVICE`].
    pub fn new() -> Self {
        Self::with_service(KEYCHAIN_SERVICE)
    }

    /// Store under an explicit service name — the test seam. Live tests use a
    /// unique per-run name so they never collide with (or leak into) the real
    /// app's entries.
    pub fn with_service(service: &str) -> Self {
        Self {
            service: service.to_string(),
        }
    }

    fn entry(&self, account: &str) -> Result<keyring::Entry, McpAuthError> {
        let account = account.trim();
        if account.is_empty() {
            return Err(McpAuthError::InvalidRef {
                detail: "auth_ref is empty — a token needs a non-blank account key".into(),
            });
        }
        keyring::Entry::new(&self.service, account).map_err(store_failed)
    }

    /// Store a bearer token for `account`, replacing any prior one. Both the
    /// account key and token are trimmed; an effectively-empty account is
    /// `invalid-ref` and an effectively-empty token is `invalid-token`, each
    /// refused before the OS store is touched.
    pub fn set_token(&self, account: &str, token: &str) -> Result<(), McpAuthError> {
        let trimmed = token.trim();
        if trimmed.is_empty() {
            return Err(McpAuthError::InvalidToken {
                detail: "token is empty — use delete to remove a stored token".into(),
            });
        }
        let entry = self.entry(account)?;
        entry.set_password(trimmed).map_err(store_failed)?;
        log::info!(
            "mcp: auth token stored for account {} (token bytes never logged)",
            account.trim()
        );
        Ok(())
    }

    /// Read the token back. Crate-internal on purpose: the http connect path
    /// (T04) is the only consumer — this must never be exposed as a tauri
    /// command or serialized (R018 — the secret never crosses IPC outbound).
    pub(crate) fn get_token(&self, account: &str) -> Result<Option<String>, McpAuthError> {
        match self.entry(account)?.get_password() {
            Ok(token) => Ok(Some(token)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(store_failed(e)),
        }
    }

    /// Presence only — the shape the IPC status surface is built from.
    pub fn token_present(&self, account: &str) -> Result<bool, McpAuthError> {
        self.get_token(account).map(|t| t.is_some())
    }

    /// Delete the stored token. Deleting an absent token is a success (the
    /// user's intent — no token stored — already holds).
    pub fn delete_token(&self, account: &str) -> Result<(), McpAuthError> {
        match self.entry(account)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => {
                log::info!("mcp: auth token deleted for account {}", account.trim());
                Ok(())
            }
            Err(e) => Err(store_failed(e)),
        }
    }
}

impl Default for McpAuthStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Probe whether a platform credential store is actually reachable in this
    /// environment. Under the `gsd_exec` sandbox the keyring reports "No
    /// default store has been set" (MEM137/138/149), so the real-store tests
    /// below skip cleanly there rather than reporting a false regression.
    fn store_available(store: &McpAuthStore, account: &str) -> bool {
        !matches!(
            store.token_present(account),
            Err(McpAuthError::StoreFailed { .. })
        )
    }

    /// The IPC error contract: kind tag + camelCase fields, mirroring
    /// CloudKeyError. A change here is a breaking IPC change.
    #[test]
    fn error_json_shape_is_the_ipc_contract() {
        let invalid_ref = McpAuthError::InvalidRef {
            detail: "auth_ref is empty".into(),
        };
        let v = serde_json::to_value(&invalid_ref).unwrap();
        assert_eq!(v["kind"], "invalid-ref");
        assert_eq!(v["detail"], "auth_ref is empty");

        let invalid_token = McpAuthError::InvalidToken {
            detail: "token is empty".into(),
        };
        let v = serde_json::to_value(&invalid_token).unwrap();
        assert_eq!(v["kind"], "invalid-token");
        assert_eq!(v["detail"], "token is empty");

        let failed = McpAuthError::StoreFailed {
            detail: "keychain locked".into(),
        };
        let v = serde_json::to_value(&failed).unwrap();
        assert_eq!(v["kind"], "store-failed");
        assert_eq!(v["detail"], "keychain locked");
    }

    #[test]
    fn kind_matches_serde_tag_for_every_variant() {
        let all = [
            McpAuthError::InvalidRef {
                detail: String::new(),
            },
            McpAuthError::InvalidToken {
                detail: String::new(),
            },
            McpAuthError::StoreFailed {
                detail: String::new(),
            },
        ];
        for err in all {
            let v = serde_json::to_value(&err).unwrap();
            assert_eq!(v["kind"], err.kind());
        }
    }

    /// An empty/whitespace token is refused as `invalid-token` before the OS
    /// store is touched — never a store failure, never a silent delete.
    #[test]
    fn empty_token_is_refused_as_invalid_token() {
        let store = McpAuthStore::with_service("com.third-eye.test.never-created");
        for blank in ["", "   ", "\n\t"] {
            let err = store.set_token("mcp:weather", blank).unwrap_err();
            assert_eq!(err.kind(), "invalid-token");
        }
    }

    /// An empty/whitespace account key is refused as `invalid-ref` before the
    /// OS store is touched — a token cannot be filed under a blank account.
    #[test]
    fn empty_account_is_refused_as_invalid_ref() {
        let store = McpAuthStore::with_service("com.third-eye.test.never-created");
        for blank in ["", "   ", "\n\t"] {
            let err = store.set_token(blank, "tok-abc").unwrap_err();
            assert_eq!(err.kind(), "invalid-ref");
            // A blank account is also rejected on read paths, before any store hit.
            assert_eq!(
                store.token_present(blank).unwrap_err().kind(),
                "invalid-ref"
            );
            assert_eq!(store.delete_token(blank).unwrap_err().kind(), "invalid-ref");
        }
    }

    /// Real-store byte-identical round-trip: set → get → delete → absent, on an
    /// arbitrary per-server account key. A unit test (not integration) on
    /// purpose: `get_token` is pub(crate), so reading token bytes back is only
    /// possible from inside the crate — the visibility guarantee this test must
    /// not weaken. Unique per-run service + drop guard keep the real store clean
    /// even on panic. Skips when no platform store is reachable (sandbox).
    #[test]
    fn token_round_trips_byte_identical_through_the_real_store() {
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let service = format!(
            "com.slastrina.thirdeye.test.mcp.{}.{nanos}",
            std::process::id()
        );
        let account = "mcp:weather";
        let seeded = format!("bearer-UNIT-{nanos}");

        struct Guard(McpAuthStore, String);
        impl Drop for Guard {
            fn drop(&mut self) {
                let _ = self.0.delete_token(&self.1);
            }
        }
        let guard = Guard(McpAuthStore::with_service(&service), account.to_string());
        let store = &guard.0;

        if !store_available(store, account) {
            eprintln!("skipping: no platform credential store available (sandbox)");
            return;
        }

        store
            .set_token(account, &seeded)
            .expect("set against the real store");
        assert_eq!(
            store.get_token(account).unwrap().as_deref(),
            Some(seeded.as_str()),
            "round-tripped token must be byte-identical"
        );
        store.delete_token(account).expect("delete stored token");
        assert!(store.get_token(account).unwrap().is_none());
    }

    /// Two different server accounts under the same service are independent —
    /// setting one never surfaces on the other. Proves the per-server-id keying.
    #[test]
    fn distinct_accounts_are_independent() {
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let service = format!(
            "com.slastrina.thirdeye.test.mcp.multi.{}.{nanos}",
            std::process::id()
        );

        struct Guard(McpAuthStore, Vec<String>);
        impl Drop for Guard {
            fn drop(&mut self) {
                for a in &self.1 {
                    let _ = self.0.delete_token(a);
                }
            }
        }
        let a = format!("mcp:weather-{nanos}");
        let b = format!("mcp:calendar-{nanos}");
        let guard = Guard(
            McpAuthStore::with_service(&service),
            vec![a.clone(), b.clone()],
        );
        let store = &guard.0;

        if !store_available(store, &a) {
            eprintln!("skipping: no platform credential store available (sandbox)");
            return;
        }

        store.set_token(&a, "token-a").expect("set a");
        assert!(store.token_present(&a).unwrap());
        assert!(
            !store.token_present(&b).unwrap(),
            "b must be absent after only a was set"
        );
        store.set_token(&b, "token-b").expect("set b");
        assert_eq!(store.get_token(&a).unwrap().as_deref(), Some("token-a"));
        assert_eq!(store.get_token(&b).unwrap().as_deref(), Some("token-b"));
    }

    /// Real-store probe: presence of a never-created account is Ok(false) —
    /// absence is typed, not an error. Read-only against the platform store (no
    /// item is ever created), so it is safe everywhere a store exists; skips
    /// when none is reachable.
    #[test]
    fn absent_token_is_ok_false_not_an_error() {
        let store = McpAuthStore::with_service(&format!(
            "com.third-eye.test.mcp.absent-probe.{}",
            std::process::id()
        ));
        let account = "mcp:never-created";
        if !store_available(&store, account) {
            eprintln!("skipping: no platform credential store available (sandbox)");
            return;
        }
        assert!(!store.token_present(account).unwrap());
        assert!(store.get_token(account).unwrap().is_none());
    }
}
