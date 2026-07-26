//! S04 live seeded-secret pipeline proof (T02) — R015's remaining leg.
//!
//! `privacy_ingest.rs` proved store/wire cleanliness against a *scripted*
//! endpoint. This test proves the same byte-level guarantee with the *real*
//! pipeline talking to *real* LM Studio: real thin-lane distillation
//! traffic, real SSE streaming, real store writes — while a capturing
//! loopback forward-proxy records the true wire bytes for the negative
//! byte-scan. The proxy is a transparent TCP splice (no HTTP parsing, both
//! directions), so the recorded request bytes are exactly what the OpenAI
//! client put on the wire. Proxy-on-loopback keeps `EndpointTrust::Loopback`
//! so the mounted guard forwards traffic and distillation really runs.
//!
//! `#[ignore]` because it needs LM Studio serving a chat model. Run it
//! explicitly at closeout:
//!
//! ```sh
//! THIRD_EYE_ENDPOINT=http://127.0.0.1:1234 \
//!   THIRD_EYE_THIN_MODEL=<served-chat-model-id> \
//!   cargo test --manifest-path src-tauri/Cargo.toml \
//!   --test privacy_live -- --ignored --nocapture
//! ```
//!
//! `THIRD_EYE_ENDPOINT` names the *upstream* LM Studio the proxy forwards
//! to, resolved through the same `env_endpoint` rules production uses
//! (trim, trailing slash, unset/blank → project default). Leaving
//! `THIRD_EYE_THIN_MODEL` unset uses an unpinned lane — with several models
//! loaded LM Studio then rejects the request, so pin it (MEM088).
//!
//! Live assertions are negative byte-scans only (seed secrets absent from
//! every captured request stream and from the db/-wal/-shm files pre- and
//! post-checkpoint) plus placeholder presence in *request* bytes — model
//! output is nondeterministic, so nothing is asserted about reply content.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use third_eye_lib::llm::commands::env_endpoint;
use third_eye_lib::llm::guard::GuardState;
use third_eye_lib::llm::router::{ModelRouter, THIN_LANE};
use third_eye_lib::memory::ingest::{run_loop, IngestState, BATCH_SIZE};
use third_eye_lib::memory::MemoryStore;
use third_eye_lib::watcher::{TextObservation, WatcherState};

/// The exact secret byte sequences seeded into observations. If any of these
/// reaches a captured request stream or a store file, redaction was bypassed.
const SEED_SECRETS: [&str; 5] = [
    "hunter2",
    "4242 4242 4242 4242",
    "4111111111111111",
    "sk-abcdefghijklmnop1234",
    "ghp_AbCdEf123456789012345678901234567890",
];

/// The byte-exact placeholder vocabulary (pinned in privacy unit tests) that
/// must ride the outbound requests instead.
const PLACEHOLDERS: [&str; 3] = [
    "[REDACTED:password]",
    "[REDACTED:card]",
    "[REDACTED:api-key]",
];

/// An innocent, distinctive line that must survive redaction verbatim all
/// the way onto the wire — proof the pipeline redacts secrets, not text
/// wholesale.
const INNOCENT_MARKER: &str = "tokio broadcast channel documentation about lagged receivers";

/// A scratch db path under the OS temp dir, cleaned up on drop so failed
/// runs do not accumulate files.
struct ScratchDb {
    dir: PathBuf,
    path: PathBuf,
}

impl ScratchDb {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("third-eye-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("memory.db");
        Self { dir, path }
    }
}

impl Drop for ScratchDb {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

// ---------------------------------------------------------------------------
// Capturing forward-proxy: a loopback TcpListener that splices bytes
// transparently to the real upstream in both directions, recording only the
// client→upstream stream (the request bytes). One capture buffer per
// accepted connection, appended live, so keep-alive reuse and streaming
// responses need no HTTP framing knowledge.
// ---------------------------------------------------------------------------

mod proxy {
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    pub type Captured = Arc<Mutex<Vec<Vec<u8>>>>;

    /// The `host:port` authority of an `http://` endpoint as produced by
    /// `env_endpoint` (scheme-prefixed, no trailing slash).
    pub fn authority(endpoint: &str) -> String {
        let rest = endpoint
            .strip_prefix("http://")
            .unwrap_or_else(|| panic!("proxy upstream must be http://, got {endpoint:?}"));
        rest.split('/').next().unwrap().to_string()
    }

    /// Listen on `127.0.0.1:0`, forwarding every accepted connection to
    /// `upstream` (a `host:port` authority). Returns the proxy's own
    /// `http://` endpoint and the per-connection captured request streams.
    pub async fn spawn(upstream: String) -> (String, Captured) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let captured: Captured = Arc::new(Mutex::new(Vec::new()));
        let cap = captured.clone();
        tokio::spawn(async move {
            loop {
                let Ok((client, _)) = listener.accept().await else {
                    return;
                };
                let idx = {
                    let mut lock = cap.lock().unwrap();
                    lock.push(Vec::new());
                    lock.len() - 1
                };
                tokio::spawn(splice(client, upstream.clone(), cap.clone(), idx));
            }
        });
        (format!("http://{addr}"), captured)
    }

    /// Bidirectional byte splice between one client connection and a fresh
    /// upstream connection; client→upstream bytes are appended to
    /// `captured[idx]` as they flow.
    async fn splice(client: TcpStream, upstream: String, captured: Captured, idx: usize) {
        let Ok(server) = TcpStream::connect(&upstream).await else {
            eprintln!("proxy: connect to upstream {upstream} failed");
            return;
        };
        let (mut client_read, mut client_write) = client.into_split();
        let (mut server_read, mut server_write) = server.into_split();
        let up = tokio::spawn(async move {
            let mut buf = [0u8; 8192];
            loop {
                match client_read.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        captured.lock().unwrap()[idx].extend_from_slice(&buf[..n]);
                        if server_write.write_all(&buf[..n]).await.is_err() {
                            break;
                        }
                    }
                }
            }
            let _ = server_write.shutdown().await;
        });
        let down = tokio::spawn(async move {
            let _ = tokio::io::copy(&mut server_read, &mut client_write).await;
            let _ = client_write.shutdown().await;
        });
        let _ = up.await;
        let _ = down.await;
    }
}

/// Build one observation exactly as the watcher mount does: raw OCR-shaped
/// text passes through the production redaction engine before a
/// [`TextObservation`] exists. `expect` mirrors the mount's fail-closed
/// contract — a redaction failure here is a test failure, not a leak.
fn redacted_observation(raw: &str, app: &str, at: u64) -> TextObservation {
    let outcome = third_eye_lib::privacy::redact(raw).expect("redaction engine must not fail");
    TextObservation {
        text: outcome.text,
        app_context: Some(app.into()),
        captured_at: at,
    }
}

/// Two full ingest batches of mutually non-near-duplicate, OCR-shaped
/// screen texts (same seeds as privacy_ingest.rs). Batch 1 seeds a
/// password, a spaced Luhn-valid card, and an sk- API key; batch 2 seeds a
/// ghp_ token and a contiguous Luhn-valid card.
fn seeded_batches() -> Vec<TextObservation> {
    let batch1: [&str; BATCH_SIZE] = [
        "login form testing password: hunter2 submitted on the staging environment",
        "checkout flow paying with card 4242 4242 4242 4242 expiry 0428 order total",
        "configuring the api client with key sk-abcdefghijklmnop1234 for the deploy",
        "reviewing pull request diff for the ingest pipeline error handling paths",
        "terraform plan output shows three resources to add two to change zero destroy",
        "email drafting quarterly budget review meeting agenda for friday afternoon",
        INNOCENT_MARKER,
        "sqlite wal checkpoint semantics page about truncate and passive modes",
    ];
    let batch2: [&str; BATCH_SIZE] = [
        "vault export shows token ghp_AbCdEf123456789012345678901234567890 saved",
        "customer support ticket pan=4111111111111111 flagged for manual review",
        "grafana dashboard p99 latency spike investigation during the deploy window",
        "spreadsheet formulas for headcount planning next quarter engineering budget",
        "rust compiler borrow checker error explanation lifetime does not live long",
        "kubernetes pod eviction notes node drain cordon sequence for the upgrade",
        "recipe research sourdough hydration baker percentages overnight cold proof",
        "calendar review one on one meetings rescheduled to thursday next week",
    ];
    batch1
        .iter()
        .enumerate()
        .map(|(i, t)| redacted_observation(t, "Terminal", 1_000 + i as u64))
        .chain(
            batch2
                .iter()
                .enumerate()
                .map(|(i, t)| redacted_observation(t, "Safari", 10_000 + i as u64)),
        )
        .collect()
}

/// Assert `bytes` contains no seed-secret byte sequence. `what` names the
/// scanned surface in the failure message.
fn assert_no_secret_bytes(bytes: &[u8], what: &str) {
    for secret in SEED_SECRETS {
        let needle = secret.as_bytes();
        assert!(
            !bytes.windows(needle.len()).any(|w| w == needle),
            "seed secret {secret:?} leaked into {what}"
        );
    }
}

/// Scan the store file and any `-wal`/`-shm` siblings for secret bytes.
/// Returns which sibling files existed at scan time.
fn scan_store_files(db_path: &Path, stage: &str) -> (bool, bool) {
    let bytes = std::fs::read(db_path).expect("read store db file");
    assert!(
        !bytes.is_empty(),
        "store db file must not be empty ({stage})"
    );
    assert_no_secret_bytes(&bytes, &format!("store file ({stage})"));

    let mut existed = (false, false);
    for (i, suffix) in ["-wal", "-shm"].iter().enumerate() {
        let mut os = db_path.as_os_str().to_owned();
        os.push(suffix);
        let sibling = PathBuf::from(os);
        if sibling.exists() {
            if i == 0 {
                existed.0 = true;
            } else {
                existed.1 = true;
            }
            let bytes = std::fs::read(&sibling).expect("read wal/shm sibling");
            assert_no_secret_bytes(&bytes, &format!("{} ({stage})", sibling.display()));
        }
    }
    existed
}

/// The R015 live closure proof: seeded secret-bearing observations flow
/// through redaction → broadcast → the real ingest loop → real LM Studio
/// distillation (through the capturing proxy) → file-backed store, and
/// neither exit surface ever holds a seed-secret byte.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires LM Studio serving a chat model at THIRD_EYE_ENDPOINT (or the project default)"]
async fn live_pipeline_leaks_no_secret_bytes_to_store_or_wire() {
    let upstream = env_endpoint(std::env::var("THIRD_EYE_ENDPOINT").ok());
    let thin_model = std::env::var("THIRD_EYE_THIN_MODEL")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let (proxy_endpoint, captured) = proxy::spawn(proxy::authority(&upstream)).await;
    eprintln!("live upstream: {upstream} via capturing proxy {proxy_endpoint}");

    let scratch = ScratchDb::new("privacy-live");
    let store = Arc::new(MemoryStore::open(&scratch.path).expect("open file-backed store"));
    let ingest = Arc::new(IngestState::new());
    // The router points at the loopback proxy, so the mounted guard sees a
    // Loopback endpoint and forwards — traffic flows, and every byte crosses
    // the capture point on its way to real LM Studio.
    let router = Arc::new(ModelRouter::thin_heavy(
        &proxy_endpoint,
        thin_model,
        None,
        Arc::new(GuardState::new()),
    ));
    router
        .lane_client(THIN_LANE)
        .expect("thin lane must resolve");

    let watcher = WatcherState::new();
    let rx = watcher.subscribe();
    let task = tokio::spawn(run_loop(rx, store.clone(), ingest.clone(), router));

    let observations = seeded_batches();
    assert_eq!(observations.len(), 2 * BATCH_SIZE);
    // The redaction engine already stripped every seed secret before the
    // observation existed — same guarantee build_observation gives the loop.
    for obs in &observations {
        assert_no_secret_bytes(obs.text.as_bytes(), "a broadcast observation");
    }
    for obs in observations {
        watcher.publish(obs);
    }
    // Closing the channel lets the loop drain both batches and exit; each
    // batch is one real distillation round-trip against LM Studio.
    drop(watcher);
    tokio::time::timeout(std::time::Duration::from_secs(300), task)
        .await
        .expect("ingest loop should finish two live distillations well inside 5 minutes")
        .expect("ingest loop must not panic");

    // The pipeline really ran, live: no distillation error and stored output.
    let status = ingest.status();
    eprintln!("live ingest status: {status:?}");
    assert!(
        status.last_error.is_none(),
        "live distillation failed: {:?}",
        status.last_error
    );
    assert!(
        status.distilled_count >= 1,
        "at least one batch must distill"
    );
    assert!(
        store.count().unwrap() >= 1,
        "distilled summaries must be stored"
    );

    // --- Wire proof: every captured client→upstream byte stream ---
    let streams = captured.lock().unwrap().clone();
    let live_streams: Vec<&Vec<u8>> = streams.iter().filter(|s| !s.is_empty()).collect();
    eprintln!(
        "captured {} proxied connection stream(s), {} bytes total",
        live_streams.len(),
        live_streams.iter().map(|s| s.len()).sum::<usize>()
    );
    assert!(
        !live_streams.is_empty(),
        "distillation must have crossed the capturing proxy"
    );
    for (i, raw) in live_streams.iter().enumerate() {
        assert_no_secret_bytes(raw, &format!("captured request stream {i}"));
    }
    // Placeholders ride the prompts verbatim (no JSON escaping applies to
    // these byte sequences), so the combined request capture must carry the
    // full vocabulary: batch 1 seeds password + card + sk- key, batch 2 the
    // ghp_ token + contiguous card.
    let combined = live_streams
        .iter()
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect::<Vec<_>>()
        .join("\n");
    for placeholder in PLACEHOLDERS {
        assert!(
            combined.contains(placeholder),
            "captured wire bytes must carry {placeholder}"
        );
    }
    // Redaction is surgical: innocent screen text reaches the model intact.
    assert!(
        combined.contains(INNOCENT_MARKER),
        "innocent observation text must survive to the wire verbatim"
    );

    // --- File proof: db + WAL siblings, before and after checkpoint ---
    // Before closing: WAL mode means recent pages live in memory.db-wal, so
    // scanning the siblings now covers stored content the main file may not
    // yet hold.
    let (wal_before, _) = scan_store_files(&scratch.path, "pre-checkpoint");
    assert!(
        wal_before,
        "WAL-mode store must have a -wal sibling while open"
    );

    // Closing the last connection checkpoints the WAL into the main file, so
    // the post-close scan sees every stored page. (No placeholder assertion
    // on stored content — live model output is nondeterministic.)
    drop(store);
    scan_store_files(&scratch.path, "post-checkpoint");
}
