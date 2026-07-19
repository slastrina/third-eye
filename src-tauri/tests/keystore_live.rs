//! S02 live proof: the cloud keystore round-trips an API key through the
//! real OS credential store (macOS Keychain here), and the stored bytes
//! never land in the app's settings/config files — proven by byte-scan of
//! the real app data dir after the store, not by inspection.
//!
//! Prompt-safe by construction: the test uses a unique per-run service name
//! (never the production service), and an item created and read back by the
//! same process sits inside its creator ACL. A drop guard deletes the item
//! even if an assertion fails mid-test.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use third_eye_lib::cloud::keystore::{CloudProvider, KeyStore};

/// Deletes the test's keychain items on scope exit, pass or fail.
struct Cleanup {
    store: KeyStore,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        for provider in CloudProvider::ALL {
            let _ = self.store.delete_key(provider);
        }
    }
}

/// Recursively byte-scan `dir` for `needle`; returns scanned file count.
/// Panics with the offending path if the needle is found anywhere.
fn assert_absent_under(dir: &Path, needle: &[u8], scanned: &mut usize) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return, // dir vanished or unreadable — nothing stored here
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            assert_absent_under(&path, needle, scanned);
        } else if let Ok(bytes) = std::fs::read(&path) {
            *scanned += 1;
            let hit = bytes
                .windows(needle.len())
                .any(|window| window == needle);
            assert!(
                !hit,
                "seeded key bytes leaked into {} — key material must live only in the OS credential store",
                path.display()
            );
        }
    }
}

#[test]
fn key_round_trips_through_the_real_credential_store_and_never_lands_on_disk() {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let service = format!("com.slastrina.thirdeye.test.live.{}.{nanos}", std::process::id());
    // Unique, high-entropy, grep-proof seed — if these exact bytes show up
    // in any config file, only this run could have put them there.
    let seeded = format!("sk-test-SEEDED-{nanos}-{}-NEVER-ON-DISK", std::process::id());

    let cleanup = Cleanup { store: KeyStore::with_service(&service) };
    let store = &cleanup.store;

    // Store → present, against the real platform store. The byte-identical
    // read-back lives in the in-crate unit test
    // (cloud::keystore::tests::key_round_trips_byte_identical_through_the_real_store):
    // get_key is pub(crate), so this test — like any code outside the crate,
    // frontend bridge included — structurally cannot read key bytes back.
    store
        .set_key(CloudProvider::Openai, &seeded)
        .expect("set_key against the real credential store");
    assert!(store.key_present(CloudProvider::Openai).unwrap(), "stored key must be present");
    // The other provider's slot stays independent — no cross-talk.
    assert!(!store.key_present(CloudProvider::Anthropic).unwrap());

    // Byte-scan the real app data dir (tauri store settings.json and
    // friends) and the repo-side config files for the seeded bytes.
    let mut scanned = 0usize;
    if let Some(home) = std::env::var_os("HOME") {
        let app_data = Path::new(&home)
            .join("Library/Application Support/com.slastrina.thirdeye");
        assert_absent_under(&app_data, seeded.as_bytes(), &mut scanned);
    }
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    for repo_config in ["tauri.conf.json", "capabilities"] {
        assert_absent_under(&manifest_dir.join(repo_config), seeded.as_bytes(), &mut scanned);
        if let Ok(bytes) = std::fs::read(manifest_dir.join(repo_config)) {
            scanned += 1;
            assert!(!bytes.windows(seeded.len()).any(|w| w == seeded.as_bytes()));
        }
    }
    println!("byte-scan: seeded key absent from {scanned} settings/config files");
    assert!(scanned > 0, "byte-scan must actually scan files to prove anything");

    // Delete → typed absence (Ok(false), never an error).
    store.delete_key(CloudProvider::Openai).expect("delete stored key");
    assert!(!store.key_present(CloudProvider::Openai).unwrap(), "deleted key must be absent");
}
