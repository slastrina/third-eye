//! S03 live wire proof: a guarded cloud client, built through the single
//! [`build_cloud_client`] choke point, talks to a real TLS-terminating mock
//! HTTPS provider and the M003 privacy guard governs every byte on the wire.
//!
//! The mock terminates TLS with the committed self-signed RSA-2048 fixture
//! (`tests/fixtures/cloud-test-{cert,key}.pem`, CN/SAN `cloud.test`, PKCS#8
//! key) using the same `native-tls` backend reqwest already links. (RSA, not
//! EC: macOS SecureTransport's `Identity::from_pkcs8` rejects an EC PKCS#8 key
//! with errSecUnknownFormat; RSA imports cleanly, and the openssl-CLI /
//! committed-PEM / no-rcgen-or-ring posture the plan asked for is unchanged.)
//! The client is pinned to
//! the loopback listener with reqwest `.resolve("cloud.test", …)` while SNI
//! and cert still validate against the real hostname, and trusts the fixture
//! via `add_root_certificate` — both knobs ride the [`CloudTransport`] test
//! seam that defaults to prod behavior.
//!
//! Proven over the decrypted wire:
//! - a streamed OpenAI-shape SSE completion arrives through the guarded client;
//! - the captured request body carries the typed `[REDACTED:password]`
//!   placeholder and never the seeded content secret;
//! - the `Authorization: Bearer` header carries the API key (bearer auth on a
//!   real TLS wire);
//! - an attachment-carrying request is blocked with a typed `guard-blocked`
//!   error and the mock receives zero new connections;
//! - guard telemetry counters move (a redaction is recorded, then a block);
//! - opt-in off yields the typed `optin-disabled` construction error, and the
//!   keystore is never consulted.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_native_tls::native_tls;

use third_eye_lib::cloud::client::{build_cloud_client, CloudTransport};
use third_eye_lib::cloud::keystore::{CloudProvider, KeyStore};
use third_eye_lib::cloud::optin::CloudOptIn;
use third_eye_lib::llm::guard::{EndpointTrust, GuardState};
use third_eye_lib::llm::{Attachment, ChatMessage, ChatRequest, LlmClient};
use third_eye_lib::privacy::DetectionKind;

// The committed fixture — a self-signed RSA-2048 cert (CN/SAN cloud.test, CA +
// serverAuth) and its PKCS#8 key. Both server identity (native-tls) and client
// trust (reqwest) load from these exact bytes, so the handshake is a genuine
// one. Validity is 397 days: macOS SecureTransport rejects TLS server certs
// whose lifetime exceeds ~398 days ("validity period exceeds the maximum
// allowed"), so the plan's 10-year cert is impossible here. Regenerate before
// notAfter with (from tests/fixtures/):
//   openssl req -x509 -newkey rsa:2048 -nodes -keyout cloud-test-key.pem \
//     -out cloud-test-cert.pem -days 397 -subj "/CN=cloud.test" \
//     -addext "subjectAltName=DNS:cloud.test" \
//     -addext "basicConstraints=critical,CA:TRUE" \
//     -addext "keyUsage=critical,digitalSignature,keyCertSign,keyEncipherment" \
//     -addext "extendedKeyUsage=serverAuth"
const CERT_PEM: &[u8] = include_bytes!("fixtures/cloud-test-cert.pem");
const KEY_PEM: &[u8] = include_bytes!("fixtures/cloud-test-key.pem");

/// A TLS-terminating mock OpenAI-compatible provider on loopback. Every
/// accepted connection is counted (the no-socket-write proof for the guard's
/// block path) and its decrypted request bytes captured for wire assertions.
struct MockProvider {
    addr: SocketAddr,
    captured: Arc<Mutex<Vec<Vec<u8>>>>,
    connections: Arc<AtomicUsize>,
}

impl MockProvider {
    /// Every decrypted request the mock has fully read, in arrival order.
    fn captured(&self) -> Vec<Vec<u8>> {
        self.captured.lock().unwrap().clone()
    }

    /// How many TCP connections the mock has accepted — a request that the
    /// guard blocks before the inner client's socket write never bumps this.
    fn connection_count(&self) -> usize {
        self.connections.load(Ordering::SeqCst)
    }
}

/// Stand up the mock, returning once it is bound and accepting. The accept
/// loop lives on a spawned task for the test's lifetime; each connection is
/// handled on its own task so several requests can be served in sequence.
async fn spawn_mock_provider() -> MockProvider {
    let identity =
        native_tls::Identity::from_pkcs8(CERT_PEM, KEY_PEM).expect("load fixture TLS identity");
    let acceptor = tokio_native_tls::TlsAcceptor::from(
        native_tls::TlsAcceptor::new(identity).expect("build native-tls acceptor"),
    );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let captured: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));
    let connections = Arc::new(AtomicUsize::new(0));

    let cap = captured.clone();
    let conns = connections.clone();
    tokio::spawn(async move {
        loop {
            let Ok((tcp, _)) = listener.accept().await else {
                break;
            };
            // Count at accept: a connection reaching TLS means the client
            // actually opened a socket to us.
            conns.fetch_add(1, Ordering::SeqCst);
            let acceptor = acceptor.clone();
            let cap = cap.clone();
            tokio::spawn(async move {
                let Ok(mut tls) = acceptor.accept(tcp).await else {
                    return;
                };
                // Read the full decrypted request (headers + content-length body).
                let mut buf: Vec<u8> = Vec::new();
                let mut tmp = [0u8; 4096];
                while !request_complete(&buf) {
                    match tls.read(&mut tmp).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => buf.extend_from_slice(&tmp[..n]),
                    }
                }
                cap.lock().unwrap().push(buf);
                let _ = tls.write_all(&sse_completion_response()).await;
                let _ = tls.flush().await;
                let _ = tls.shutdown().await;
            });
        }
    });

    MockProvider {
        addr,
        captured,
        connections,
    }
}

/// True once `buf` holds the full request: complete headers plus
/// `content-length` bytes of body (mirrors the in-crate llm test helper).
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

/// A chunked OpenAI-shape SSE completion: two content deltas then `[DONE]`.
fn sse_completion_response() -> Vec<u8> {
    let parts = [
        sse_token("Hel"),
        sse_token("lo"),
        "data: [DONE]\n\n".to_string(),
    ];
    let mut resp = String::from(
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n\r\n",
    );
    for part in parts {
        resp.push_str(&format!("{:x}\r\n{part}\r\n", part.len()));
    }
    resp.push_str("0\r\n\r\n");
    resp.into_bytes()
}

fn sse_token(token: &str) -> String {
    format!(
        "data: {}\n\n",
        serde_json::json!({ "choices": [{ "delta": { "content": token } }] })
    )
}

/// A keystore pointed at a unique per-run service, drop-guarded so the real OS
/// store is left clean even on panic (keystore.rs / keystore_live precedent).
struct TestStore {
    store: KeyStore,
}

impl TestStore {
    fn new(tag: &str, nanos: u128) -> Self {
        let service = format!(
            "com.slastrina.thirdeye.test.cloudwire.{tag}.{}.{nanos}",
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

/// Build the test transport: point the client at the mock's
/// `https://cloud.test:{port}`, trust the fixture root, and pin `cloud.test`
/// to the loopback listener so the real TLS handshake terminates locally.
fn test_transport(addr: SocketAddr) -> CloudTransport {
    let cert = reqwest::Certificate::from_pem(CERT_PEM).expect("parse fixture root cert");
    CloudTransport::default()
        .with_endpoint(format!("https://cloud.test:{}", addr.port()))
        .with_root_certificate(cert)
        .with_resolve("cloud.test", addr)
}

fn now_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn guarded_cloud_client_streams_over_tls_with_redacted_body_and_bearer_auth() {
    let nanos = now_nanos();
    let mock = spawn_mock_provider().await;

    // Opt-in on + a seeded key in a unique drop-guarded keystore service.
    let optin = CloudOptIn::new();
    optin.set_enabled(true);
    let ts = TestStore::new("stream", nanos);
    let api_key = format!("sk-test-CLOUD-WIRE-{nanos}");
    ts.store
        .set_key(CloudProvider::Openai, &api_key)
        .expect("seed api key in the real store");

    let guard = Arc::new(GuardState::new());
    let client = build_cloud_client(
        &optin,
        &ts.store,
        CloudProvider::Openai,
        guard.clone(),
        &test_transport(mock.addr),
    )
    .expect("opt-in on + key present builds a guarded cloud client");

    // The guarded client targets an External endpoint, so the guard governs it.
    assert_eq!(client.trust(), EndpointTrust::External);

    // A user message carrying a redactable content secret distinct from the
    // API key: after redaction the wire must show the placeholder, never the
    // raw value.
    let content_secret = format!("pw-NEVER-ON-WIRE-{nanos}");
    let seen = Mutex::new(String::new());
    let outcome = client
        .stream_chat(
            &ChatRequest::new(vec![ChatMessage::user(format!(
                "password: {content_secret}"
            ))]),
            &|t| seen.lock().unwrap().push_str(t),
        )
        .await
        .expect("streamed completion arrives through the guarded TLS client");

    // (a) The streamed OpenAI-shape SSE completion arrived and accumulated.
    assert_eq!(
        outcome.text, "Hello",
        "two content deltas accumulate to 'Hello'"
    );
    assert_eq!(outcome.token_count, 2);
    assert_eq!(
        *seen.lock().unwrap(),
        "Hello",
        "tokens were delivered in order"
    );
    assert_eq!(
        mock.connection_count(),
        1,
        "exactly one TLS connection served the stream"
    );

    // Inspect the decrypted request the mock actually received.
    let captured = mock.captured();
    assert_eq!(captured.len(), 1, "the mock captured exactly one request");
    let raw = &captured[0];
    let wire = String::from_utf8_lossy(raw).to_string();
    let wire_lower = wire.to_ascii_lowercase();

    // (b) The typed placeholder rides the wire; the raw content secret does not.
    assert!(
        wire.contains("[REDACTED:password]"),
        "redacted placeholder must be on the decrypted wire: {wire}"
    );
    assert!(
        !raw.windows(content_secret.len())
            .any(|w| w == content_secret.as_bytes()),
        "the seeded content secret must NEVER appear on the wire"
    );

    // (c) Bearer auth on the real TLS wire — the key rides the Authorization
    // header (headers are not redacted; only message content is).
    assert!(
        wire_lower.contains(&format!(
            "authorization: bearer {}",
            api_key.to_ascii_lowercase()
        )),
        "the API key must ride as an Authorization: Bearer header on the TLS wire"
    );

    // Guard telemetry moved: the password redaction was recorded on forward.
    assert_eq!(
        guard.redaction_count(DetectionKind::Password),
        1,
        "one password redaction must be counted on the confident forward"
    );
    assert_eq!(guard.blocked_count(), 0, "a clean forward blocks nothing");

    // (d) An attachment-carrying request is blocked typed, with ZERO new mock
    // connections — the guard refuses before the inner client's socket write.
    let connections_before = mock.connection_count();
    let attachment_req = ChatRequest::new(vec![ChatMessage::user("what is on my screen?")
        .with_attachments(vec![Attachment {
            base64_png: "QUJD".into(),
        }])]);
    let err = client
        .stream_chat(&attachment_req, &|_| {})
        .await
        .expect_err("an attachment request to an External endpoint must be blocked");
    assert_eq!(err.kind(), "guard-blocked");
    assert_eq!(
        mock.connection_count(),
        connections_before,
        "the blocked attachment request must open ZERO new TLS connections"
    );

    // Telemetry moved again: the block was recorded with its reason.
    assert_eq!(guard.blocked_count(), 1, "the attachment block was counted");
    let snapshot = guard.snapshot();
    assert_eq!(snapshot.blocked_count, 1);
    assert_eq!(
        snapshot.last_block_reason.map(|r| r.as_str()),
        Some("attachment-unredactable"),
        "the last block reason names the unredactable attachment"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn opt_in_off_yields_typed_construction_error_without_touching_the_keystore() {
    // (e) Opt-in off short-circuits before any keystore read: a keystore
    // pointed at a never-created service still yields optin-disabled (never
    // no-api-key / store-failed), proving the gate precedes the lookup.
    let optin = CloudOptIn::new(); // default off
    let keystore = KeyStore::with_service(&format!(
        "com.slastrina.thirdeye.test.cloudwire.never-read.{}.{}",
        std::process::id(),
        now_nanos()
    ));
    let err = build_cloud_client(
        &optin,
        &keystore,
        CloudProvider::Openai,
        Arc::new(GuardState::new()),
        &CloudTransport::default(),
    )
    .err()
    .expect("opt-in off must refuse construction");
    assert_eq!(err.kind(), "optin-disabled");
}
