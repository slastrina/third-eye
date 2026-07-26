//! S01 closure proof (T03): redacted observations all the way to disk and
//! wire.
//!
//! Drives seeded observations — a password, Luhn-valid cards (spaced and
//! contiguous), and prefixed API keys — through the production redaction
//! engine, the [`WatcherState`] broadcast seam, and the real
//! [`run_loop`] ingest pipeline into a file-backed [`MemoryStore`], with
//! distillation riding real HTTP against a capturing scripted server.
//! Nothing in the production path is mocked — only the model endpoint is
//! scripted.
//!
//! The proof is byte-level, at both exits (slice must-have 5):
//! - every captured distillation request body contains zero seed-secret
//!   bytes and does contain the typed placeholders;
//! - the on-disk SQLite store file and any `-wal`/`-shm` siblings contain
//!   zero seed-secret bytes, scanned both before and after the closing
//!   checkpoint.
//!
//! Observations are built through the same public `privacy::redact` engine
//! the watcher's `build_observation` mount calls (that mount is pinned by
//! the watcher unit tests); this test proves what redaction guarantees
//! downstream of the mount.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use third_eye_lib::llm::guard::GuardState;
use third_eye_lib::llm::router::{ModelRouter, THIN_LANE};
use third_eye_lib::memory::ingest::{run_loop, IngestState, BATCH_SIZE};
use third_eye_lib::memory::MemoryStore;
use third_eye_lib::watcher::{TextObservation, WatcherState};

/// The exact secret byte sequences seeded into observations. If any of these
/// reaches a request body or a store file, redaction was bypassed.
const SEED_SECRETS: [&str; 5] = [
    "hunter2",
    "4242 4242 4242 4242",
    "4111111111111111",
    "sk-abcdefghijklmnop1234",
    "ghp_AbCdEf123456789012345678901234567890",
];

/// The byte-exact placeholder vocabulary (pinned in privacy unit tests) that
/// must appear instead.
const PLACEHOLDERS: [&str; 3] = [
    "[REDACTED:password]",
    "[REDACTED:card]",
    "[REDACTED:api-key]",
];

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
// Scripted HTTP server (same shape as tests/chat_tool_calling.rs): one
// pre-baked SSE response per accepted connection, in order, capturing every
// request's raw bytes for the wire-body scans.
// ---------------------------------------------------------------------------

mod scripted {
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// Serve `responses[i]` on the i-th accepted connection (closing each
    /// with `connection: close` so reqwest never reuses a dead socket), and
    /// expose the captured request bytes per connection.
    pub async fn spawn(responses: Vec<Vec<u8>>) -> (String, Arc<Mutex<Vec<Vec<u8>>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let captured: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));
        let cap = captured.clone();
        tokio::spawn(async move {
            for response in responses {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let mut buf: Vec<u8> = Vec::new();
                let mut tmp = [0u8; 4096];
                while !request_complete(&buf) {
                    match sock.read(&mut tmp).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => buf.extend_from_slice(&tmp[..n]),
                    }
                }
                cap.lock().unwrap().push(buf);
                let _ = sock.write_all(&response).await;
                let _ = sock.shutdown().await;
            }
        });
        (format!("http://{addr}"), captured)
    }

    /// True once `buf` holds the full request: complete headers plus
    /// `content-length` bytes of body.
    fn request_complete(buf: &[u8]) -> bool {
        let text = String::from_utf8_lossy(buf);
        let Some(header_end) = text.find("\r\n\r\n") else {
            return false;
        };
        let content_length = text
            .lines()
            .find_map(|l| {
                l.to_ascii_lowercase()
                    .strip_prefix("content-length:")?
                    .trim()
                    .parse::<usize>()
                    .ok()
            })
            .unwrap_or(0);
        buf.len() >= header_end + 4 + content_length
    }

    pub fn sse_token(token: &str) -> String {
        format!(
            "data: {}\n\n",
            serde_json::json!({"choices": [{"delta": {"content": token}}]})
        )
    }

    /// HTTP/1.1 200 chunked SSE response, terminated and connection-closed.
    pub fn sse_200(parts: &[String]) -> Vec<u8> {
        let mut resp = String::from(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
             transfer-encoding: chunked\r\nconnection: close\r\n\r\n",
        );
        for part in parts {
            resp.push_str(&format!("{:x}\r\n{part}\r\n", part.len()));
        }
        resp.push_str("0\r\n\r\n");
        resp.into_bytes()
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

/// An innocent, distinctive line that must survive redaction verbatim all
/// the way onto the wire — proof the pipeline redacts secrets, not text
/// wholesale.
const INNOCENT_MARKER: &str = "tokio broadcast channel documentation about lagged receivers";

/// Two full ingest batches of mutually non-near-duplicate, OCR-shaped
/// screen texts. Batch 1 seeds a password, a spaced Luhn-valid card, and an
/// sk- API key; batch 2 seeds a ghp_ token and a contiguous Luhn-valid card.
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

/// The S01 integration proof: seeded secret-bearing observations flow
/// through redaction → broadcast → the real ingest loop → real HTTP
/// distillation → file-backed store, and neither exit surface ever holds a
/// seed-secret byte.
#[tokio::test(flavor = "multi_thread")]
async fn redacted_pipeline_leaks_no_secret_bytes_to_store_or_wire() {
    // Two batches → two distillations → two captured request bodies. The
    // scripted replies echo placeholders (as a model summarizing redacted
    // text plausibly would), so the stored summaries also pin the
    // placeholder vocabulary on disk.
    let reply1 = scripted::sse_200(&[
        scripted::sse_token("User tested a login flow using [REDACTED:password] on staging.\n"),
        scripted::sse_token("User exercised checkout with test card [REDACTED:card]."),
        "data: [DONE]\n\n".to_string(),
    ]);
    let reply2 = scripted::sse_200(&[
        scripted::sse_token("User rotated an API token [REDACTED:api-key] in the vault.\n"),
        scripted::sse_token("User reviewed a flagged card [REDACTED:card] support ticket."),
        "data: [DONE]\n\n".to_string(),
    ]);
    let (endpoint, captured) = scripted::spawn(vec![reply1, reply2]).await;

    let scratch = ScratchDb::new("privacy-ingest");
    let store = Arc::new(MemoryStore::open(&scratch.path).expect("open file-backed store"));
    let ingest = Arc::new(IngestState::new());
    let router = Arc::new(ModelRouter::thin_heavy(
        &endpoint,
        Some("thin-test".into()),
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
    // batch is one real HTTP distillation round-trip against the capture
    // server.
    drop(watcher);
    tokio::time::timeout(std::time::Duration::from_secs(60), task)
        .await
        .expect("ingest loop must drain two scripted distillations inside 60s")
        .expect("ingest loop must not panic");

    // The pipeline really ran: two distillations, two summaries each.
    let status = ingest.status();
    assert!(
        status.last_error.is_none(),
        "distillation failed: {:?}",
        status.last_error
    );
    assert_eq!(status.distilled_count, 4, "2 batches x 2 summary lines");
    assert_eq!(store.count().unwrap(), 4);

    // --- Wire proof: every captured distillation request body ---
    let requests = captured.lock().unwrap().clone();
    assert_eq!(requests.len(), 2, "one HTTP request per full batch");
    for (i, raw) in requests.iter().enumerate() {
        assert_no_secret_bytes(raw, &format!("captured request {i}"));
    }
    let body1 = String::from_utf8_lossy(&requests[0]).into_owned();
    let body2 = String::from_utf8_lossy(&requests[1]).into_owned();
    // Batch 1 carried password + spaced card + sk- key; batch 2 carried the
    // ghp_ token + contiguous card. Placeholders ride the prompt verbatim
    // (no JSON escaping applies to these byte sequences).
    for placeholder in PLACEHOLDERS {
        assert!(
            body1.contains(placeholder),
            "request 0 must carry {placeholder}: {body1}"
        );
    }
    assert!(
        body2.contains("[REDACTED:api-key]"),
        "request 1 must carry the api-key placeholder"
    );
    assert!(
        body2.contains("[REDACTED:card]"),
        "request 1 must carry the card placeholder"
    );
    // Redaction is surgical: innocent screen text reaches the model intact.
    assert!(
        body1.contains(INNOCENT_MARKER),
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
    // the post-close scan sees every stored page in the main db.
    drop(store);
    scan_store_files(&scratch.path, "post-checkpoint");
    let db_bytes = std::fs::read(&scratch.path).expect("read checkpointed db");
    for placeholder in PLACEHOLDERS {
        let needle = placeholder.as_bytes();
        assert!(
            db_bytes.windows(needle.len()).any(|w| w == needle),
            "stored summaries must pin {placeholder} in the checkpointed db"
        );
    }
}
