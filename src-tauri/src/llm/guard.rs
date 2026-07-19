//! Fail-closed privacy guard middleware on the LLM pipe (R016, D029/D030).
//!
//! Pure policy module — no mounts here. [`GuardedClient`] wraps any
//! [`LlmClient`] and [`GuardedEmbedder`] wraps any [`Embedder`]; T02 makes
//! them the only reachable construction path for production traffic.
//!
//! Policy, decided entirely before the inner (socket-writing) component is
//! ever invoked:
//! - [`EndpointTrust::Loopback`] → forward unchanged, byte-identical.
//! - [`EndpointTrust::External`] → block with typed
//!   [`LlmError::GuardBlocked`] if the request carries attachments (pixels
//!   are unredactable, D030), the redaction engine fails, or any redaction
//!   is [`RedactionConfidence::Low`]; otherwise forward a redacted clone.
//!
//! [`GuardState`] records kinds-and-counts-only telemetry — never original
//! or redacted text — as the S02→S03 boundary artifact.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde::Serialize;

use crate::memory::embed::Embedder;
use crate::privacy::{
    self, Detection, DetectionKind, RedactionConfidence, RedactionError, RedactionOutcome,
};

use super::{ChatRequest, LlmClient, LlmError, LlmHealth, StreamOutcome, TokenSink};

/// How much the guard trusts an endpoint. Classification is pure and
/// deterministic: URL-string parsing only, no DNS — a hostname that *would*
/// resolve to loopback is still [`External`](EndpointTrust::External),
/// because resolving it would make trust nondeterministic and networked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum EndpointTrust {
    /// Literal `localhost`, a `127.0.0.0/8` IPv4, or `::1`.
    Loopback,
    /// Everything else — including unparseable URLs (fail closed).
    External,
}

impl EndpointTrust {
    /// Classify an endpoint URL. Anything that does not parse to a literal
    /// loopback host is [`External`](EndpointTrust::External).
    pub fn classify(endpoint: &str) -> Self {
        let Ok(url) = url::Url::parse(endpoint) else {
            return EndpointTrust::External;
        };
        match url.host() {
            Some(url::Host::Domain(host)) if host.eq_ignore_ascii_case("localhost") => {
                EndpointTrust::Loopback
            }
            Some(url::Host::Ipv4(ip)) if ip.is_loopback() => EndpointTrust::Loopback,
            Some(url::Host::Ipv6(ip)) if ip.is_loopback() => EndpointTrust::Loopback,
            _ => EndpointTrust::External,
        }
    }
}

/// Why the guard refused to send a request. Kebab-case on the wire (the
/// `reason` field of the `guard-blocked` error kind) and in logs — S03's
/// last-blocked surface stays machine-readable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum GuardBlockReason {
    /// The request carries image attachments; pixels cannot be redacted
    /// (D030), so an external send is refused outright.
    AttachmentUnredactable,
    /// The redaction engine returned a typed error — fail closed rather
    /// than send unredacted text.
    RedactionFailed,
    /// The engine could not vouch for its own redaction
    /// ([`RedactionConfidence::Low`]) — fail closed.
    LowConfidence,
}

impl GuardBlockReason {
    /// Stable machine-readable name, mirroring the serde tag.
    pub fn as_str(self) -> &'static str {
        match self {
            GuardBlockReason::AttachmentUnredactable => "attachment-unredactable",
            GuardBlockReason::RedactionFailed => "redaction-failed",
            GuardBlockReason::LowConfidence => "low-confidence",
        }
    }
}

impl std::fmt::Display for GuardBlockReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Mutation-notification seam (S03): installed once at startup with the
/// `privacy://state` emit closure, called with a fresh [`GuardTelemetry`]
/// snapshot after every telemetry mutation. `Arc` so the seam can be invoked
/// after every internal lock is released.
type GuardNotifier = Arc<dyn Fn(GuardTelemetry) + Send + Sync>;

/// Kinds-and-counts-only guard telemetry: per-kind redaction counters,
/// blocked count, last-block reason, typed last error. Never holds original
/// or redacted text — the [`LlmError::GuardBlocked`] it stores carries only
/// endpoint and reason. Shared as `Arc<GuardState>` between the guards, the
/// watcher's redaction site (T02), and S03's IPC surface.
///
/// Lock discipline: counters are atomics; the `Mutex`es guard plain
/// `Option` writes and are never held across an await. The notifier seam is
/// invoked only after every state lock is released (and outside the notifier
/// lock itself), so a notifier may safely re-enter [`GuardState::snapshot`].
#[derive(Default)]
pub struct GuardState {
    /// Per-kind applied-redaction counters, in [`DetectionKind::ALL`] order.
    redactions: [AtomicUsize; 3],
    blocked: AtomicUsize,
    last_block_reason: Mutex<Option<GuardBlockReason>>,
    last_error: Mutex<Option<LlmError>>,
    /// S03 notification seam — `None` until `setup()` installs the
    /// `privacy://state` emitter, keeping this module Tauri-free.
    notifier: Mutex<Option<GuardNotifier>>,
}

impl GuardState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Install the mutation notifier (S03). Called once from `setup()` with
    /// the `privacy://state` emit closure; tests install capturing closures.
    /// Installing replaces any previous notifier.
    pub fn set_notifier(&self, notifier: GuardNotifier) {
        *self.notifier.lock().unwrap() = Some(notifier);
    }

    /// Invoke the notifier (if installed) with a fresh snapshot. Every state
    /// and notifier lock is released before the closure runs, so the closure
    /// may re-enter the read API without deadlocking.
    fn notify(&self) {
        let notifier = self.notifier.lock().unwrap().clone();
        if let Some(notify) = notifier {
            notify(self.snapshot());
        }
    }

    /// Add applied-redaction counts. Called by the guards when a redacted
    /// request is actually forwarded, and by the watcher's redaction site
    /// (T02) so S03 counters reflect watcher detections too. Notifies the
    /// seam only when `detections` is non-empty — clean forwards mutate
    /// nothing, so there is nothing to broadcast.
    pub fn record_redactions(&self, detections: &[Detection]) {
        for d in detections {
            self.redactions[kind_index(d.kind)].fetch_add(d.count, Ordering::Relaxed);
        }
        if !detections.is_empty() {
            self.notify();
        }
    }

    /// Record one guard block: bump the blocked count and remember the
    /// reason and typed error (kinds only — the error carries no text),
    /// then notify the seam after both locks are released.
    pub fn record_block(&self, reason: GuardBlockReason, error: &LlmError) {
        self.blocked.fetch_add(1, Ordering::Relaxed);
        *self.last_block_reason.lock().unwrap() = Some(reason);
        *self.last_error.lock().unwrap() = Some(error.clone());
        self.notify();
    }

    pub fn redaction_count(&self, kind: DetectionKind) -> usize {
        self.redactions[kind_index(kind)].load(Ordering::Relaxed)
    }

    pub fn blocked_count(&self) -> usize {
        self.blocked.load(Ordering::Relaxed)
    }

    pub fn last_block_reason(&self) -> Option<GuardBlockReason> {
        *self.last_block_reason.lock().unwrap()
    }

    pub fn last_error(&self) -> Option<LlmError> {
        self.last_error.lock().unwrap().clone()
    }

    /// Serializable snapshot for S03's IPC surface. Zero-count kinds are
    /// omitted, matching [`RedactionOutcome::detections`] semantics.
    pub fn snapshot(&self) -> GuardTelemetry {
        GuardTelemetry {
            redactions: DetectionKind::ALL
                .into_iter()
                .filter(|kind| self.redaction_count(*kind) > 0)
                .map(|kind| Detection { kind, count: self.redaction_count(kind) })
                .collect(),
            blocked_count: self.blocked_count(),
            last_block_reason: self.last_block_reason(),
            last_error: self.last_error(),
        }
    }
}

fn kind_index(kind: DetectionKind) -> usize {
    DetectionKind::ALL
        .iter()
        .position(|k| *k == kind)
        .expect("DetectionKind::ALL covers every kind")
}

/// Snapshot of [`GuardState`] — kinds and counts only, camelCase for IPC.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GuardTelemetry {
    pub redactions: Vec<Detection>,
    pub blocked_count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_block_reason: Option<GuardBlockReason>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<LlmError>,
}

/// Emit the structured block log (endpoint + kebab-case reason only, never
/// text), build the typed error, and record it in [`GuardState`].
fn block(state: &GuardState, endpoint: &str, reason: GuardBlockReason) -> LlmError {
    log::warn!("guard: blocked endpoint={endpoint} reason={reason}");
    let err = LlmError::GuardBlocked { endpoint: endpoint.to_string(), reason };
    state.record_block(reason, &err);
    err
}

/// The redaction seam, injectable so tests can exercise the
/// engine-failure branch (the real engine's failure path is near-unreachable
/// by design). Production always passes [`privacy::redact`].
type Redactor<'a> = &'a dyn Fn(&str) -> Result<RedactionOutcome, RedactionError>;

/// External-path chat policy: block on attachments, engine failure, or any
/// low-confidence redaction; otherwise return the redacted clone. Redaction
/// is per-message (every role, including tool-result turns — memory content
/// rides in them) so long histories never spuriously trip the engine's
/// per-call scan cap. Redaction counters are recorded only when the whole
/// request passes — a blocked request never left, so nothing was redacted
/// outbound.
fn guard_chat_request(
    state: &GuardState,
    endpoint: &str,
    request: &ChatRequest,
    redact: Redactor<'_>,
) -> Result<ChatRequest, LlmError> {
    if request.messages.iter().any(|m| !m.attachments.is_empty()) {
        return Err(block(state, endpoint, GuardBlockReason::AttachmentUnredactable));
    }
    let mut guarded = request.clone();
    let mut counts = [0usize; 3];
    for msg in &mut guarded.messages {
        msg.content = redact_into(&msg.content, redact, &mut counts)
            .map_err(|reason| block(state, endpoint, reason))?;
    }
    state.record_redactions(&collect_detections(&counts));
    Ok(guarded)
}

/// External-path embedding policy: same fail-closed rules per input text
/// (embeddings carry no attachments).
fn guard_embed_texts(
    state: &GuardState,
    endpoint: &str,
    texts: &[String],
    redact: Redactor<'_>,
) -> Result<Vec<String>, LlmError> {
    let mut counts = [0usize; 3];
    let mut guarded = Vec::with_capacity(texts.len());
    for text in texts {
        guarded.push(
            redact_into(text, redact, &mut counts)
                .map_err(|reason| block(state, endpoint, reason))?,
        );
    }
    state.record_redactions(&collect_detections(&counts));
    Ok(guarded)
}

fn redact_into(
    text: &str,
    redact: Redactor<'_>,
    counts: &mut [usize; 3],
) -> Result<String, GuardBlockReason> {
    let outcome = redact(text).map_err(|_| GuardBlockReason::RedactionFailed)?;
    if outcome.confidence == RedactionConfidence::Low {
        return Err(GuardBlockReason::LowConfidence);
    }
    for d in &outcome.detections {
        counts[kind_index(d.kind)] += d.count;
    }
    Ok(outcome.text)
}

fn collect_detections(counts: &[usize; 3]) -> Vec<Detection> {
    DetectionKind::ALL
        .into_iter()
        .filter(|kind| counts[kind_index(*kind)] > 0)
        .map(|kind| Detection { kind, count: counts[kind_index(kind)] })
        .collect()
}

/// Fail-closed [`LlmClient`] middleware. Trust is classified once at
/// construction from the inner client's endpoint; the inner client is the
/// only socket-writing component, so a block path that never invokes it is
/// proven to happen before any socket write.
pub struct GuardedClient {
    inner: Arc<dyn LlmClient>,
    trust: EndpointTrust,
    state: Arc<GuardState>,
}

impl GuardedClient {
    pub fn new(inner: Arc<dyn LlmClient>, state: Arc<GuardState>) -> Self {
        let trust = EndpointTrust::classify(inner.endpoint());
        Self { inner, trust, state }
    }

    pub fn trust(&self) -> EndpointTrust {
        self.trust
    }
}

#[async_trait]
impl LlmClient for GuardedClient {
    fn endpoint(&self) -> &str {
        self.inner.endpoint()
    }

    async fn stream_chat(
        &self,
        request: &ChatRequest,
        on_token: TokenSink<'_>,
    ) -> Result<StreamOutcome, LlmError> {
        match self.trust {
            // Loopback traffic flows byte-identical to today.
            EndpointTrust::Loopback => self.inner.stream_chat(request, on_token).await,
            EndpointTrust::External => {
                let guarded = guard_chat_request(
                    &self.state,
                    self.inner.endpoint(),
                    request,
                    &privacy::redact,
                )?;
                self.inner.stream_chat(&guarded, on_token).await
            }
        }
    }

    /// Liveness probes carry no user content — pass through untouched.
    async fn health(&self) -> LlmHealth {
        self.inner.health().await
    }
}

/// Fail-closed [`Embedder`] middleware, mirroring [`GuardedClient`].
pub struct GuardedEmbedder {
    inner: Arc<dyn Embedder>,
    trust: EndpointTrust,
    state: Arc<GuardState>,
}

impl GuardedEmbedder {
    pub fn new(inner: Arc<dyn Embedder>, state: Arc<GuardState>) -> Self {
        let trust = EndpointTrust::classify(inner.endpoint());
        Self { inner, trust, state }
    }

    pub fn trust(&self) -> EndpointTrust {
        self.trust
    }
}

#[async_trait]
impl Embedder for GuardedEmbedder {
    fn endpoint(&self) -> &str {
        self.inner.endpoint()
    }

    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, LlmError> {
        match self.trust {
            EndpointTrust::Loopback => self.inner.embed(texts).await,
            EndpointTrust::External => {
                let guarded = guard_embed_texts(
                    &self.state,
                    self.inner.endpoint(),
                    texts,
                    &privacy::redact,
                )?;
                self.inner.embed(&guarded).await
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{Attachment, ChatMessage};
    use serde_json::json;

    // --- EndpointTrust: pure classification table, no DNS ---

    #[test]
    fn loopback_hosts_classify_loopback() {
        for endpoint in [
            "http://localhost:1234",
            "http://LOCALHOST:1234",
            "https://localhost/v1",
            "http://127.0.0.1:1234",
            "http://127.255.0.9:80",
            "http://[::1]:9",
        ] {
            assert_eq!(
                EndpointTrust::classify(endpoint),
                EndpointTrust::Loopback,
                "{endpoint} must be loopback"
            );
        }
    }

    #[test]
    fn everything_else_classifies_external_fail_closed() {
        for endpoint in [
            "http://192.168.1.50:1234",   // LAN IP
            "http://192.0.2.1:9",         // TEST-NET-1
            "https://api.openai.com/v1",  // real domain
            "http://127.0.0.1.evil.com",  // loopback-lookalike domain
            "http://[::ffff:7f00:1]:80",  // IPv4-mapped loopback is not ::1
            "http://my-macbook.local:1234", // could resolve to loopback — no DNS
            "localhost:1234",             // no scheme: parses as scheme, host None
            "not a url",
            "",
        ] {
            assert_eq!(
                EndpointTrust::classify(endpoint),
                EndpointTrust::External,
                "{endpoint} must be external"
            );
        }
    }

    // --- Mock inner client: call counter is the no-socket-write proof ---

    struct MockInner {
        endpoint: String,
        calls: AtomicUsize,
        seen: Mutex<Option<ChatRequest>>,
    }

    impl MockInner {
        fn new(endpoint: &str) -> Arc<Self> {
            Arc::new(Self {
                endpoint: endpoint.into(),
                calls: AtomicUsize::new(0),
                seen: Mutex::new(None),
            })
        }
    }

    #[async_trait]
    impl LlmClient for MockInner {
        fn endpoint(&self) -> &str {
            &self.endpoint
        }

        async fn stream_chat(
            &self,
            request: &ChatRequest,
            on_token: TokenSink<'_>,
        ) -> Result<StreamOutcome, LlmError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            *self.seen.lock().unwrap() = Some(request.clone());
            on_token("ok");
            Ok(StreamOutcome { text: "ok".into(), token_count: 1, tool_calls: Vec::new() })
        }

        async fn health(&self) -> LlmHealth {
            LlmHealth { online: true, endpoint: self.endpoint.clone() }
        }
    }

    const EXTERNAL: &str = "http://192.0.2.1:9";
    const LOOPBACK: &str = "http://127.0.0.1:1234";

    fn guarded(endpoint: &str) -> (GuardedClient, Arc<MockInner>, Arc<GuardState>) {
        let inner = MockInner::new(endpoint);
        let state = Arc::new(GuardState::new());
        let client = GuardedClient::new(inner.clone(), state.clone());
        (client, inner, state)
    }

    fn request(messages: Vec<ChatMessage>) -> ChatRequest {
        ChatRequest::new(messages)
    }

    // --- Loopback: byte-identical pass-through ---

    #[tokio::test]
    async fn loopback_request_passes_through_byte_identical() {
        let (client, inner, state) = guarded(LOOPBACK);
        assert_eq!(client.trust(), EndpointTrust::Loopback);
        // Secrets and attachments both survive untouched on loopback.
        let req = request(vec![ChatMessage::user("password: hunter2")
            .with_attachments(vec![Attachment { base64_png: "QUJD".into() }])]);
        let outcome = client.stream_chat(&req, &|_| {}).await.unwrap();
        assert_eq!(outcome.text, "ok");
        assert_eq!(inner.calls.load(Ordering::SeqCst), 1);
        assert_eq!(inner.seen.lock().unwrap().as_ref(), Some(&req));
        assert_eq!(state.blocked_count(), 0);
        assert_eq!(state.redaction_count(DetectionKind::Password), 0);
    }

    // --- External block paths: typed error, inner never invoked ---

    #[tokio::test]
    async fn external_attachment_request_blocks_before_any_socket_write() {
        let (client, inner, state) = guarded(EXTERNAL);
        assert_eq!(client.trust(), EndpointTrust::External);
        let req = request(vec![ChatMessage::user("what is on my screen?")
            .with_attachments(vec![Attachment { base64_png: "QUJD".into() }])]);
        let err = client.stream_chat(&req, &|_| {}).await.unwrap_err();
        assert_eq!(err.kind(), "guard-blocked");
        assert_eq!(err.endpoint(), EXTERNAL);
        assert!(matches!(
            err,
            LlmError::GuardBlocked { reason: GuardBlockReason::AttachmentUnredactable, .. }
        ));
        // The inner client is the only socket-writing component: zero calls
        // proves the block happened before any socket write.
        assert_eq!(inner.calls.load(Ordering::SeqCst), 0);
        assert_eq!(state.blocked_count(), 1);
        assert_eq!(state.last_block_reason(), Some(GuardBlockReason::AttachmentUnredactable));
        assert_eq!(state.last_error().unwrap().kind(), "guard-blocked");
    }

    #[tokio::test]
    async fn external_low_confidence_redaction_blocks_before_any_socket_write() {
        let (client, inner, state) = guarded(EXTERNAL);
        // Luhn-failing digit run beside card context: the engine's pinned
        // Low-confidence condition (privacy::mod tests).
        let req = request(vec![ChatMessage::user("credit card: 4111 1111 1111 1112")]);
        let err = client.stream_chat(&req, &|_| {}).await.unwrap_err();
        assert!(matches!(
            err,
            LlmError::GuardBlocked { reason: GuardBlockReason::LowConfidence, .. }
        ));
        assert_eq!(inner.calls.load(Ordering::SeqCst), 0);
        assert_eq!(state.blocked_count(), 1);
        assert_eq!(state.last_block_reason(), Some(GuardBlockReason::LowConfidence));
    }

    #[test]
    fn engine_failure_blocks_with_redaction_failed() {
        // The real engine's failure path is near-unreachable by design, so
        // the branch is proven through the injectable redactor seam.
        let state = GuardState::new();
        let failing: Redactor<'_> =
            &|_: &str| Err(RedactionError::PatternCompile { detector: "test" });
        let err = guard_chat_request(
            &state,
            EXTERNAL,
            &request(vec![ChatMessage::user("hi")]),
            failing,
        )
        .unwrap_err();
        assert!(matches!(
            err,
            LlmError::GuardBlocked { reason: GuardBlockReason::RedactionFailed, .. }
        ));
        assert_eq!(state.blocked_count(), 1);
        assert_eq!(state.last_block_reason(), Some(GuardBlockReason::RedactionFailed));
    }

    #[tokio::test]
    async fn blocked_request_records_no_redaction_counts() {
        // Message 1 would redact a password, but message 2 blocks the whole
        // request — nothing left the process, so nothing counts as redacted.
        let (client, inner, state) = guarded(EXTERNAL);
        let req = request(vec![
            ChatMessage::user("password: hunter2"),
            ChatMessage::user("credit card: 4111 1111 1111 1112"),
        ]);
        client.stream_chat(&req, &|_| {}).await.unwrap_err();
        assert_eq!(inner.calls.load(Ordering::SeqCst), 0);
        assert_eq!(state.redaction_count(DetectionKind::Password), 0);
        assert_eq!(state.blocked_count(), 1);
    }

    // --- External forward path: redacted clone, zero secret bytes ---

    #[tokio::test]
    async fn external_confident_request_forwards_redacted_clone() {
        let (client, inner, state) = guarded(EXTERNAL);
        let req = request(vec![
            ChatMessage::system("be brief"),
            ChatMessage::user("password: hunter2"),
            // Tool-result turns carry memory content and must be redacted too.
            ChatMessage::tool_result("call_1", "api token: A8f3kQ9zL2mX7pR4wN6vB1cJ"),
        ]);
        client.stream_chat(&req, &|_| {}).await.unwrap();
        assert_eq!(inner.calls.load(Ordering::SeqCst), 1);

        let seen = inner.seen.lock().unwrap().clone().unwrap();
        assert_eq!(seen.messages[0].content, "be brief");
        assert_eq!(seen.messages[1].content, "password: [REDACTED:password]");
        assert_eq!(seen.messages[2].content, "api token: [REDACTED:api-key]");
        // Zero seed-secret bytes anywhere in the serialized forwarded request.
        let wire = serde_json::to_string(&seen.messages).unwrap();
        assert!(!wire.contains("hunter2"));
        assert!(!wire.contains("A8f3kQ9zL2mX7pR4wN6vB1cJ"));

        assert_eq!(state.redaction_count(DetectionKind::Password), 1);
        assert_eq!(state.redaction_count(DetectionKind::ApiKey), 1);
        assert_eq!(state.blocked_count(), 0);
        // The caller's request is untouched — the guard forwards a clone.
        assert_eq!(req.messages[1].content, "password: hunter2");
    }

    #[tokio::test]
    async fn external_clean_request_forwards_with_no_detections() {
        let (client, inner, state) = guarded(EXTERNAL);
        let req = request(vec![ChatMessage::user("summarize my morning")]);
        client.stream_chat(&req, &|_| {}).await.unwrap();
        let seen = inner.seen.lock().unwrap().clone().unwrap();
        assert_eq!(seen, req);
        assert_eq!(state.blocked_count(), 0);
        for kind in DetectionKind::ALL {
            assert_eq!(state.redaction_count(kind), 0);
        }
    }

    #[tokio::test]
    async fn health_passes_through_on_external_endpoints() {
        // Probes carry no user content; the guard never blocks them.
        let (client, _, state) = guarded(EXTERNAL);
        let health = client.health().await;
        assert!(health.online);
        assert_eq!(health.endpoint, EXTERNAL);
        assert_eq!(state.blocked_count(), 0);
    }

    // --- GuardedEmbedder: same policy over the embedding seam ---

    struct MockEmbedder {
        endpoint: String,
        calls: AtomicUsize,
        seen: Mutex<Option<Vec<String>>>,
    }

    impl MockEmbedder {
        fn new(endpoint: &str) -> Arc<Self> {
            Arc::new(Self {
                endpoint: endpoint.into(),
                calls: AtomicUsize::new(0),
                seen: Mutex::new(None),
            })
        }
    }

    #[async_trait]
    impl Embedder for MockEmbedder {
        fn endpoint(&self) -> &str {
            &self.endpoint
        }

        async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, LlmError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            *self.seen.lock().unwrap() = Some(texts.to_vec());
            Ok(texts.iter().map(|_| vec![1.0]).collect())
        }
    }

    fn guarded_embedder(endpoint: &str) -> (GuardedEmbedder, Arc<MockEmbedder>, Arc<GuardState>) {
        let inner = MockEmbedder::new(endpoint);
        let state = Arc::new(GuardState::new());
        let embedder = GuardedEmbedder::new(inner.clone(), state.clone());
        (embedder, inner, state)
    }

    #[tokio::test]
    async fn loopback_embed_passes_texts_through_untouched() {
        let (embedder, inner, state) = guarded_embedder(LOOPBACK);
        assert_eq!(embedder.trust(), EndpointTrust::Loopback);
        let texts = vec!["password: hunter2".to_string()];
        embedder.embed(&texts).await.unwrap();
        assert_eq!(inner.seen.lock().unwrap().as_deref(), Some(texts.as_slice()));
        assert_eq!(state.redaction_count(DetectionKind::Password), 0);
    }

    #[tokio::test]
    async fn external_embed_forwards_redacted_texts_and_counts() {
        let (embedder, inner, state) = guarded_embedder(EXTERNAL);
        let texts =
            vec!["password: hunter2".to_string(), "weekly meeting notes".to_string()];
        let vectors = embedder.embed(&texts).await.unwrap();
        assert_eq!(vectors.len(), 2);
        let seen = inner.seen.lock().unwrap().clone().unwrap();
        assert_eq!(seen[0], "password: [REDACTED:password]");
        assert_eq!(seen[1], "weekly meeting notes");
        assert!(!seen.join("\n").contains("hunter2"));
        assert_eq!(state.redaction_count(DetectionKind::Password), 1);
        assert_eq!(state.blocked_count(), 0);
    }

    #[tokio::test]
    async fn external_embed_low_confidence_blocks_before_any_socket_write() {
        let (embedder, inner, state) = guarded_embedder(EXTERNAL);
        let err = embedder
            .embed(&["credit card: 4111 1111 1111 1112".to_string()])
            .await
            .unwrap_err();
        assert_eq!(err.kind(), "guard-blocked");
        assert_eq!(err.endpoint(), EXTERNAL);
        assert!(matches!(
            err,
            LlmError::GuardBlocked { reason: GuardBlockReason::LowConfidence, .. }
        ));
        assert_eq!(inner.calls.load(Ordering::SeqCst), 0);
        assert_eq!(state.blocked_count(), 1);
    }

    #[test]
    fn embed_engine_failure_blocks_with_redaction_failed() {
        let state = GuardState::new();
        let failing: Redactor<'_> =
            &|_: &str| Err(RedactionError::PatternCompile { detector: "test" });
        let err = guard_embed_texts(&state, EXTERNAL, &["hi".to_string()], failing)
            .unwrap_err();
        assert!(matches!(
            err,
            LlmError::GuardBlocked { reason: GuardBlockReason::RedactionFailed, .. }
        ));
        assert_eq!(state.blocked_count(), 1);
    }

    // --- GuardState: kinds-and-counts-only telemetry ---

    #[test]
    fn state_aggregates_redaction_counts_per_kind() {
        let state = GuardState::new();
        state.record_redactions(&[
            Detection { kind: DetectionKind::Password, count: 2 },
            Detection { kind: DetectionKind::ApiKey, count: 1 },
        ]);
        state.record_redactions(&[Detection { kind: DetectionKind::Password, count: 1 }]);
        assert_eq!(state.redaction_count(DetectionKind::Password), 3);
        assert_eq!(state.redaction_count(DetectionKind::ApiKey), 1);
        assert_eq!(state.redaction_count(DetectionKind::Card), 0);
    }

    #[test]
    fn snapshot_serializes_kinds_and_counts_only_camel_case() {
        let state = GuardState::new();
        state.record_redactions(&[Detection { kind: DetectionKind::Card, count: 2 }]);
        let err = block(&state, EXTERNAL, GuardBlockReason::LowConfidence);
        assert_eq!(err.kind(), "guard-blocked");

        let v = serde_json::to_value(state.snapshot()).unwrap();
        assert_eq!(v["redactions"], json!([{ "kind": "card", "count": 2 }]));
        assert_eq!(v["blockedCount"], 1);
        assert_eq!(v["lastBlockReason"], "low-confidence");
        assert_eq!(v["lastError"]["kind"], "guard-blocked");
        assert_eq!(v["lastError"]["endpoint"], EXTERNAL);
        assert_eq!(v["lastError"]["reason"], "low-confidence");
        // The whole snapshot never carries request text — only kinds/counts.
        let wire = v.to_string();
        assert!(!wire.contains("detail"), "no free-text fields in telemetry: {wire}");
    }

    #[test]
    fn empty_snapshot_omits_optional_fields() {
        let v = serde_json::to_value(GuardState::new().snapshot()).unwrap();
        assert_eq!(v["redactions"], json!([]));
        assert_eq!(v["blockedCount"], 0);
        let obj = v.as_object().unwrap();
        assert!(!obj.contains_key("lastBlockReason"));
        assert!(!obj.contains_key("lastError"));
    }

    // --- Notifier seam: one choke point for every mutation site (S03) ---

    /// Install a capturing notifier; returns the shared snapshot log.
    fn capturing_notifier(state: &GuardState) -> Arc<Mutex<Vec<GuardTelemetry>>> {
        let seen: Arc<Mutex<Vec<GuardTelemetry>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        state.set_notifier(Arc::new(move |snapshot| {
            sink.lock().unwrap().push(snapshot);
        }));
        seen
    }

    #[test]
    fn record_redactions_notifies_once_with_the_fresh_snapshot() {
        let state = GuardState::new();
        let seen = capturing_notifier(&state);
        state.record_redactions(&[Detection { kind: DetectionKind::Password, count: 2 }]);

        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1, "one mutation, one notification");
        assert_eq!(
            seen[0].redactions,
            vec![Detection { kind: DetectionKind::Password, count: 2 }]
        );
        assert_eq!(seen[0].blocked_count, 0);
    }

    #[test]
    fn record_redactions_with_no_detections_does_not_notify() {
        let state = GuardState::new();
        let seen = capturing_notifier(&state);
        state.record_redactions(&[]);
        assert!(seen.lock().unwrap().is_empty(), "clean forwards must not broadcast");
    }

    #[test]
    fn record_block_notifies_with_reason_and_typed_error() {
        let state = GuardState::new();
        let seen = capturing_notifier(&state);
        let err = LlmError::GuardBlocked {
            endpoint: EXTERNAL.into(),
            reason: GuardBlockReason::LowConfidence,
        };
        state.record_block(GuardBlockReason::LowConfidence, &err);

        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].blocked_count, 1);
        assert_eq!(seen[0].last_block_reason, Some(GuardBlockReason::LowConfidence));
        assert_eq!(seen[0].last_error.as_ref().unwrap().kind(), "guard-blocked");
    }

    #[test]
    fn unnotified_state_mutates_without_a_notifier_installed() {
        // Production before setup() (and every pre-S03 unit test) runs with
        // no notifier — mutations must be silent no-ops on the seam.
        let state = GuardState::new();
        state.record_redactions(&[Detection { kind: DetectionKind::ApiKey, count: 1 }]);
        assert_eq!(state.redaction_count(DetectionKind::ApiKey), 1);
    }

    #[test]
    fn notifier_may_reenter_the_read_api_without_deadlock() {
        // The seam fires after every lock is released, so an emit closure
        // that snapshots again (or a future reader) can never deadlock.
        let state = Arc::new(GuardState::new());
        let reentrant = state.clone();
        let ok = Arc::new(AtomicUsize::new(0));
        let hits = ok.clone();
        state.set_notifier(Arc::new(move |_| {
            let _ = reentrant.snapshot();
            let _ = reentrant.last_block_reason();
            hits.fetch_add(1, Ordering::SeqCst);
        }));
        let err = block(&state, EXTERNAL, GuardBlockReason::RedactionFailed);
        assert_eq!(err.kind(), "guard-blocked");
        state.record_redactions(&[Detection { kind: DetectionKind::Card, count: 1 }]);
        assert_eq!(ok.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn guarded_forward_and_block_paths_both_reach_the_seam() {
        // End-to-end through GuardedClient: the middleware's own mutation
        // sites (record_redactions on forward, record_block on block) hit
        // the same installed seam — one choke point for all sites.
        let (client, _, state) = guarded(EXTERNAL);
        let seen = capturing_notifier(&state);

        client
            .stream_chat(&request(vec![ChatMessage::user("password: hunter2")]), &|_| {})
            .await
            .unwrap();
        client
            .stream_chat(
                &request(vec![ChatMessage::user("credit card: 4111 1111 1111 1112")]),
                &|_| {},
            )
            .await
            .unwrap_err();

        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert_eq!(
            seen[0].redactions,
            vec![Detection { kind: DetectionKind::Password, count: 1 }]
        );
        assert_eq!(seen[1].blocked_count, 1);
        assert_eq!(seen[1].last_block_reason, Some(GuardBlockReason::LowConfidence));
        // Kinds-and-counts only, even through the seam: no secret bytes.
        let wire = serde_json::to_string(&*seen).unwrap();
        assert!(!wire.contains("hunter2"));
        assert!(!wire.contains("4111"));
    }

    #[test]
    fn block_reason_strings_are_kebab_case_pinned() {
        assert_eq!(GuardBlockReason::AttachmentUnredactable.as_str(), "attachment-unredactable");
        assert_eq!(GuardBlockReason::RedactionFailed.as_str(), "redaction-failed");
        assert_eq!(GuardBlockReason::LowConfidence.as_str(), "low-confidence");
        for reason in [
            GuardBlockReason::AttachmentUnredactable,
            GuardBlockReason::RedactionFailed,
            GuardBlockReason::LowConfidence,
        ] {
            assert_eq!(serde_json::to_value(reason).unwrap(), json!(reason.as_str()));
            assert_eq!(reason.to_string(), reason.as_str());
        }
    }
}
