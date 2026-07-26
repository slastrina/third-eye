//! Cloud provider support (M004): opt-in remote LLM providers behind the
//! same LlmClient abstraction, with the M003 privacy guard as mandatory
//! middleware. S02 ships the keystore layer: API keys live solely in the OS
//! credential store (macOS Keychain / Windows Credential Manager / Linux
//! secret-service). The only IPC surface is presence — key bytes never cross
//! IPC outbound, never land in settings/config files, and are never logged.

//! S03 adds the opt-in gate ([`optin`]) and the single guarded construction
//! choke point ([`client::build_cloud_client`]) that turns a stored key into
//! a bearer-authed remote client behind the M003 privacy guard.
//!
//! S04 surfaces opt-in to the user: [`optin`] gains the `set_cloud_optin` /
//! `cloud_optin_status` IPC commands, the `cloud://optin` broadcast, and the
//! persisted heavy-lane provider selection the Settings webview renders.

pub mod client;
pub mod commands;
pub mod keystore;
pub mod optin;
pub mod routing;

/// Serializes the tests that touch the REAL OS credential store (two in
/// keystore.rs, one in client.rs): under the default parallel test runner
/// concurrent keychain access intermittently fails with "No default store
/// has been set", flaking the suite. Poison is absorbed — one panicking
/// test must not cascade the lock into the others.
#[cfg(test)]
pub(crate) fn real_keychain_test_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}
