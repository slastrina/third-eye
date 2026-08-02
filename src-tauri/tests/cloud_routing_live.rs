//! S05 live routed-lane wire proof: the guarded cloud client, injected into a
//! real [`ModelRouter`]'s heavy lane via the T01 [`set_lane_client`] seam, is
//! driven **through the router** and the M003 privacy guard governs every byte
//! on the wire. Where `cloud_client_live.rs` (S03) proves the *constructor* over
//! TLS, this proves the *routed path* — the R017 closure: `build_cloud_client`'s
//! product reaches the wire only through `ModelRouter::stream_chat`.
//!
//! The mock terminates TLS with the committed self-signed RSA-2048 fixture
//! (`tests/fixtures/cloud-test-{cert,key}.pem`, CN/SAN `cloud.test`) over the
//! same `native-tls` backend reqwest links; the client is pinned to the loopback
//! listener via reqwest `.resolve("cloud.test", …)` through the [`CloudTransport`]
//! test seam. Harness mirrors `cloud_client_live.rs` deliberately — an
//! integration test binary cannot import another's items — but every assertion
//! here rides `ModelRouter`, not `build_cloud_client` directly.
//!
//! Proven over the decrypted wire, through the router's heavy lane:
//! - a routed heavy-lane stream carries the typed `[REDACTED:password]`
//!   placeholder and never the seeded content secret;
//! - the `Authorization: Bearer` header carries the API key on the TLS wire;
//! - an attachment-carrying routed request is guard-blocked with ZERO new mock
//!   connections and the router's shared `GuardState.blocked_count` increments —
//!   the same guard the `privacy://state` broadcast reads;
//! - opt-in off never injects: the heavy lane stays on its local client, and the
//!   cloud mock receives zero connections (offline-first default intact).

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_native_tls::native_tls;

use third_eye_lib::cloud::client::{build_cloud_client, CloudTransport};
use third_eye_lib::cloud::keystore::{CloudProvider, KeyStore};
use third_eye_lib::cloud::optin::CloudOptIn;
use third_eye_lib::llm::guard::{EndpointTrust, GuardState};
use third_eye_lib::llm::router::{Lane, ModelRouter, HEAVY_LANE, THIN_LANE};
use third_eye_lib::llm::{
    Attachment, ChatMessage, ChatRequest, LlmClient, LlmError, LlmHealth, StreamOutcome, TokenSink,
};
use third_eye_lib::privacy::DetectionKind;

// The committed fixture — a self-signed RSA-2048 cert (CN/SAN cloud.test) and
// its PKCS#8 key; see cloud_client_live.rs for the regeneration recipe and the
// macOS SecureTransport RSA/validity constraints.
const CERT_PEM: &[u8] = include_bytes!("fixtures/cloud-test-cert.pem");
const KEY_PEM: &[u8] = include_bytes!("fixtures/cloud-test-key.pem");

/// A TLS-terminating mock OpenAI-compatible provider on loopback. Every accepted
/// connection is counted (the no-socket-write proof for the guard's block path)
/// and its decrypted request bytes captured for wire assertions.
struct MockProvider {
    addr: SocketAddr,
    captured: Arc<Mutex<Vec<Vec<u8>>>>,
    connections: Arc<AtomicUsize>,
}

impl MockProvider {
    fn captured(&self) -> Vec<Vec<u8>> {
        self.captured.lock().unwrap().clone()
    }

    fn connection_count(&self) -> usize {
        self.connections.load(Ordering::SeqCst)
    }
}

/// Stand up the mock, returning once it is bound and accepting.
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
/// store is left clean even on panic.
struct TestStore {
    store: KeyStore,
}

impl TestStore {
    fn new(tag: &str, nanos: u128) -> Self {
        let service = format!(
            "com.slastrina.thirdeye.test.cloudroute.{tag}.{}.{nanos}",
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

/// Point the client at the mock's `https://cloud.test:{port}`, trust the fixture
/// root, and pin `cloud.test` to the loopback listener so the real TLS handshake
/// terminates locally.
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

/// The heavy lane's *local* client stand-in: answers with a fixed tag and counts
/// how many times it served, so the opt-in-off test can prove the routed request
/// stayed local (this served, the cloud mock did not connect). Endpoint is a
/// non-connectable placeholder — it is never dialed; the tag is returned inline.
struct LocalHeavy {
    served: Arc<AtomicUsize>,
}

impl LocalHeavy {
    fn new() -> (Arc<Self>, Arc<AtomicUsize>) {
        let served = Arc::new(AtomicUsize::new(0));
        (
            Arc::new(Self {
                served: served.clone(),
            }),
            served,
        )
    }
}

#[async_trait]
impl LlmClient for LocalHeavy {
    fn endpoint(&self) -> &str {
        "http://local-heavy.invalid"
    }

    async fn stream_chat(
        &self,
        _request: &ChatRequest,
        on_token: TokenSink<'_>,
    ) -> Result<StreamOutcome, LlmError> {
        self.served.fetch_add(1, Ordering::SeqCst);
        on_token("local-heavy-reply");
        Ok(StreamOutcome {
            text: "local-heavy-reply".into(),
            token_count: 1,
            tool_calls: Vec::new(),
            prompt_tokens: None,
            completion_tokens: None,
        })
    }

    async fn health(&self) -> LlmHealth {
        LlmHealth {
            online: true,
            endpoint: self.endpoint().into(),
        }
    }
}

/// A thin-lane placeholder — present so the router mirrors the production
/// thin/heavy pair; never routed to in these tests.
struct InertThin;

#[async_trait]
impl LlmClient for InertThin {
    fn endpoint(&self) -> &str {
        "http://thin.invalid"
    }

    async fn stream_chat(
        &self,
        _request: &ChatRequest,
        _on_token: TokenSink<'_>,
    ) -> Result<StreamOutcome, LlmError> {
        Ok(StreamOutcome {
            text: "thin".into(),
            token_count: 0,
            tool_calls: Vec::new(),
            prompt_tokens: None,
            completion_tokens: None,
        })
    }

    async fn health(&self) -> LlmHealth {
        LlmHealth {
            online: true,
            endpoint: self.endpoint().into(),
        }
    }
}

/// Build a thin/heavy router whose heavy lane starts on `heavy` (a local client),
/// sharing `guard` — the same GuardState the routed cloud client will record
/// into, exactly as `apply_cloud_routing` wires `router.guard_state()`.
fn routed_router(heavy: Arc<dyn LlmClient>, guard: Arc<GuardState>) -> ModelRouter {
    ModelRouter::with_guard(
        vec![
            Lane::new(THIN_LANE, None, Arc::new(InertThin)),
            Lane::new(HEAVY_LANE, Some("local-heavy".into()), heavy),
        ],
        guard,
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn routed_heavy_lane_redacts_on_wire_carries_bearer_and_blocks_attachments() {
    let nanos = now_nanos();
    let mock = spawn_mock_provider().await;

    // Opt-in on + a seeded key in a unique drop-guarded keystore service.
    let optin = CloudOptIn::new();
    optin.set_enabled(true);
    let ts = TestStore::new("stream", nanos);
    let api_key = format!("sk-test-ROUTE-WIRE-{nanos}");
    ts.store
        .set_key(CloudProvider::Openai, &api_key)
        .expect("seed api key in the real store");

    // A real router with a local heavy lane and a shared guard. The cloud client
    // is built with THIS guard (router.guard_state()) so a routed block feeds the
    // same telemetry the privacy://state broadcast reads.
    let (local_heavy, _local_served) = LocalHeavy::new();
    let guard = Arc::new(GuardState::new());
    let router = routed_router(local_heavy, guard.clone());

    let cloud = build_cloud_client(
        &optin,
        &ts.store,
        CloudProvider::Openai,
        router.guard_state(),
        &test_transport(mock.addr),
    )
    .expect("opt-in on + key present builds a guarded cloud client");

    // The routed client targets an External endpoint, so the guard governs it.
    assert_eq!(cloud.trust(), EndpointTrust::External);

    // Inject verbatim (T01 seam) and route the heavy lane.
    router
        .set_lane_client(HEAVY_LANE, cloud)
        .expect("heavy lane accepts the injected cloud client");
    router
        .set_active(HEAVY_LANE)
        .expect("heavy lane is a construction invariant");

    // A user message carrying a redactable content secret distinct from the API
    // key: after redaction the wire must show the placeholder, never the raw
    // value.
    let content_secret = format!("pw-NEVER-ON-WIRE-{nanos}");
    let seen = Mutex::new(String::new());
    let outcome = router
        .stream_chat(
            &ChatRequest::new(vec![ChatMessage::user(format!(
                "password: {content_secret}"
            ))]),
            &|t| seen.lock().unwrap().push_str(t),
        )
        .await
        .expect("streamed completion arrives through the routed heavy lane");

    // (a) The streamed OpenAI-shape SSE completion arrived through the router.
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
        "exactly one TLS connection served the routed stream"
    );

    // Inspect the decrypted request the mock actually received.
    let captured = mock.captured();
    assert_eq!(
        captured.len(),
        1,
        "the mock captured exactly one routed request"
    );
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
        "the seeded content secret must NEVER appear on the routed wire"
    );

    // (c) Bearer auth on the real TLS wire — the key rides the Authorization
    // header (headers are not redacted; only message content is).
    assert!(
        wire_lower.contains(&format!(
            "authorization: bearer {}",
            api_key.to_ascii_lowercase()
        )),
        "the API key must ride as an Authorization: Bearer header on the routed TLS wire"
    );

    // The router's shared guard recorded the redaction on forward.
    assert_eq!(
        guard.redaction_count(DetectionKind::Password),
        1,
        "one password redaction must be counted on the routed forward"
    );
    assert_eq!(
        guard.blocked_count(),
        0,
        "a clean routed forward blocks nothing"
    );

    // (d) An attachment-carrying routed request is blocked typed, with ZERO new
    // mock connections — the guard refuses before the inner client's socket
    // write — and the router's shared blocked_count increments (the same counter
    // the Settings data-guard-blocked row renders).
    let connections_before = mock.connection_count();
    let attachment_req = ChatRequest::new(vec![ChatMessage::user("what is on my screen?")
        .with_attachments(vec![Attachment {
            base64_png: "QUJD".into(),
        }])]);
    let err = router
        .stream_chat(&attachment_req, &|_| {})
        .await
        .expect_err("an attachment request routed to an External endpoint must be blocked");
    assert_eq!(err.kind(), "guard-blocked");
    assert_eq!(
        mock.connection_count(),
        connections_before,
        "the blocked attachment request must open ZERO new TLS connections"
    );

    assert_eq!(
        guard.blocked_count(),
        1,
        "the routed attachment block was counted on the shared guard"
    );
    let snapshot = guard.snapshot();
    assert_eq!(snapshot.blocked_count, 1);
    assert_eq!(
        snapshot.last_block_reason.map(|r| r.as_str()),
        Some("attachment-unredactable"),
        "the last block reason names the unredactable attachment"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn opt_in_off_leaves_heavy_lane_local_with_zero_cloud_connections() {
    // (e) The offline-first default through the router: opt-in off means the
    // cloud build is refused (optin-disabled), so the applier never injects and
    // the heavy lane stays on its local client. A routed heavy request is served
    // locally and the cloud mock receives zero connections.
    let mock = spawn_mock_provider().await;

    let optin = CloudOptIn::new(); // default off
    let ts = TestStore::new("offline", now_nanos());

    let (local_heavy, local_served) = LocalHeavy::new();
    let router = routed_router(local_heavy, Arc::new(GuardState::new()));

    // The fail-safe gate: opt-in off refuses construction before any keystore
    // read — so nothing is injected and the heavy lane keeps its local client.
    let refused = build_cloud_client(
        &optin,
        &ts.store,
        CloudProvider::Openai,
        router.guard_state(),
        &test_transport(mock.addr),
    )
    .err()
    .expect("opt-in off must refuse the cloud build (never inject)");
    assert_eq!(refused.kind(), "optin-disabled");

    // Route a heavy-lane request: it must be served by the LOCAL client.
    router
        .set_active(HEAVY_LANE)
        .expect("heavy lane is a construction invariant");
    let seen = Mutex::new(String::new());
    let outcome = router
        .stream_chat(
            &ChatRequest::new(vec![ChatMessage::user("summarize this")]),
            &|t| seen.lock().unwrap().push_str(t),
        )
        .await
        .expect("the local heavy lane serves the routed request");

    assert_eq!(
        outcome.text, "local-heavy-reply",
        "opt-in off keeps the heavy lane local"
    );
    assert_eq!(*seen.lock().unwrap(), "local-heavy-reply");
    assert_eq!(
        local_served.load(Ordering::SeqCst),
        1,
        "the local heavy client served the request"
    );
    assert_eq!(
        mock.connection_count(),
        0,
        "opt-in off must open ZERO connections to the cloud mock"
    );
}
