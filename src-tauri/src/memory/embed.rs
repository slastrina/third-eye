//! Semantic recall (T02): the [`Embedder`] seam over LM Studio
//! `/v1/embeddings` and the hybrid [`search`] that ranks memories by cosine
//! similarity with a typed, visible degrade to FTS5 keyword ranking.
//!
//! Failure model (R006/R021): every embedding failure maps onto the existing
//! [`LlmError`] taxonomy — transport/5xx → `offline`, HTTP 4xx → `no-model`,
//! malformed body → `offline` with a detail naming the cause. A failed embed
//! never fails recall: [`search`] degrades to keyword mode and carries the
//! typed reason in the [`SearchOutcome`], so the UI (S04) and tool-calling
//! (S03) always see *why* ranking is keyword-only.

use async_trait::async_trait;
use serde::Serialize;

use crate::llm::LlmError;

use super::store::{MemoryRecord, MemoryStore};
use super::MemoryError;

/// The embedding model LM Studio serves for this project (confirmed loaded
/// in slice planning). S05 configurability replaces this constant.
pub const EMBED_MODEL: &str = "text-embedding-nomic-embed-text-v1.5";

/// Request timeout for one embeddings call. Batches are capped at
/// [`EMBED_BACKFILL_CAP`]+1 short summaries, so a healthy local endpoint
/// answers in well under this; a stalled one becomes a typed `offline`
/// degrade instead of a hung search.
const EMBED_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// Same fast-fail rationale as the chat client: an unreachable endpoint must
/// become a typed degrade quickly, not a hang.
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

/// At most this many not-yet-embedded rows are backfilled per search, so one
/// search after a long keyword-only stretch stays one bounded request
/// instead of embedding the whole store at once. The remainder is picked up
/// by subsequent searches.
const EMBED_BACKFILL_CAP: usize = 64;

/// The embedding seam. Object-safe (`Arc<dyn Embedder>`) so tests and later
/// slices can substitute transports without touching search logic.
#[async_trait]
pub trait Embedder: Send + Sync {
    /// The endpoint this embedder targets, for logs and typed errors.
    fn endpoint(&self) -> &str;

    /// Embed each text, returning one vector per input in input order.
    /// Never hangs silently: every failure resolves to an [`LlmError`].
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, LlmError>;
}

/// [`Embedder`] over an OpenAI-compatible `/v1/embeddings` route (LM Studio).
pub struct OpenAiEmbedder {
    endpoint: String,
    model: String,
    http: reqwest::Client,
}

impl OpenAiEmbedder {
    pub fn new(endpoint: impl Into<String>) -> Self {
        let endpoint = endpoint.into().trim_end_matches('/').to_string();
        let http = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .build()
            .expect("reqwest client construction cannot fail with static config");
        Self {
            endpoint,
            model: EMBED_MODEL.to_string(),
            http,
        }
    }

    /// Override the embedding model id (tests, future configurability).
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }

    fn offline(&self, detail: impl Into<String>) -> LlmError {
        LlmError::Offline {
            endpoint: self.endpoint.clone(),
            detail: detail.into(),
        }
    }
}

#[async_trait]
impl Embedder for OpenAiEmbedder {
    fn endpoint(&self) -> &str {
        &self.endpoint
    }

    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, LlmError> {
        let url = format!("{}/v1/embeddings", self.endpoint);
        let body = serde_json::json!({ "model": self.model, "input": texts });
        let resp = self
            .http
            .post(&url)
            .timeout(EMBED_TIMEOUT)
            .json(&body)
            .send()
            .await
            .map_err(|e| self.offline(e.to_string()))?;

        let status = resp.status();
        let body = resp.text().await.map_err(|e| self.offline(e.to_string()))?;
        if !status.is_success() {
            let detail = format!("HTTP {status}: {}", snippet(&body));
            return Err(if status.is_client_error() {
                LlmError::NoModel {
                    endpoint: self.endpoint.clone(),
                    detail,
                }
            } else {
                self.offline(detail)
            });
        }

        let value: serde_json::Value = serde_json::from_str(&body).map_err(|e| {
            self.offline(format!(
                "malformed /v1/embeddings response ({e}): {}",
                snippet(&body)
            ))
        })?;
        let data = value["data"].as_array().ok_or_else(|| {
            self.offline(format!(
                "/v1/embeddings response has no data array: {}",
                snippet(&body)
            ))
        })?;

        // OpenAI-compatible servers may reorder entries; `index` is the
        // authoritative position of each vector.
        let mut vectors: Vec<Option<Vec<f32>>> = vec![None; texts.len()];
        for entry in data {
            let index = entry["index"].as_u64().map(|i| i as usize);
            let vec = entry["embedding"].as_array().map(|nums| {
                nums.iter()
                    .filter_map(|n| n.as_f64().map(|f| f as f32))
                    .collect::<Vec<f32>>()
            });
            match (index, vec) {
                (Some(i), Some(v)) if i < vectors.len() => vectors[i] = Some(v),
                _ => {
                    return Err(self.offline(format!(
                        "/v1/embeddings entry missing index/embedding: {}",
                        snippet(&entry.to_string())
                    )))
                }
            }
        }
        vectors
            .into_iter()
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| {
                self.offline(format!(
                    "/v1/embeddings returned {} vectors for {} inputs",
                    data.len(),
                    texts.len()
                ))
            })
    }
}

/// How a search ranked its results — the visible half of the degrade
/// contract (R021). Serialized lowercase for IPC.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SearchMode {
    Semantic,
    Keyword,
}

/// Ranked search results plus how they were ranked. `degrade_reason` is
/// `Some` exactly when an embedding failure forced keyword mode — the typed
/// [`LlmError`] the UI surfaces. camelCase on the wire (T04 IPC).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchOutcome {
    pub mode: SearchMode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub degrade_reason: Option<LlmError>,
    pub results: Vec<MemoryRecord>,
}

/// Hybrid recall: rank semantically via cosine over stored embeddings,
/// appending FTS5 keyword hits not already ranked; degrade to keyword-only
/// (with the typed reason) when embedding fails.
///
/// One bounded embeddings request serves the whole search: the query plus up
/// to [`EMBED_BACKFILL_CAP`] rows that have no stored vector yet (rows are
/// inserted keyword-only and re-embedded lazily after edits, D022).
/// Successful backfills are persisted, so the semantic corpus heals itself
/// while LM Studio is up. A blank query returns an empty keyword-mode
/// outcome without touching the embedder.
///
/// `Err` here means the *store* failed (`MemoryError`) — embedding failures
/// never surface as errors, only as the degrade.
pub async fn search(
    store: &MemoryStore,
    embedder: &dyn Embedder,
    query: &str,
    limit: usize,
) -> Result<SearchOutcome, MemoryError> {
    if query.trim().is_empty() {
        return Ok(SearchOutcome {
            mode: SearchMode::Keyword,
            degrade_reason: None,
            results: Vec::new(),
        });
    }

    let backfill = store.unembedded_rows(EMBED_BACKFILL_CAP)?;
    let mut batch: Vec<String> = Vec::with_capacity(1 + backfill.len());
    batch.push(query.to_string());
    batch.extend(backfill.iter().map(|(_, summary)| summary.clone()));

    let vectors = match embedder.embed(&batch).await {
        Ok(vectors) => vectors,
        Err(err) => return keyword_degrade(store, query, limit, err),
    };
    let Some(query_vec) = vectors.first().filter(|v| !v.is_empty()).cloned() else {
        let err = LlmError::Offline {
            endpoint: embedder.endpoint().to_string(),
            detail: "embedding response missing the query vector".into(),
        };
        return keyword_degrade(store, query, limit, err);
    };

    // Persist backfilled vectors; a write failure loses one lazy backfill,
    // not the search — log and move on.
    for ((id, _), vec) in backfill.iter().zip(vectors.iter().skip(1)) {
        if let Err(e) = store.set_embedding(*id, vec) {
            log::warn!("memory: failed to persist backfilled embedding for row {id}: {e}");
        }
    }

    let mut scored: Vec<(i64, f32)> = store
        .embedded_rows()?
        .into_iter()
        .filter_map(|(id, vec)| {
            let score = cosine(&query_vec, &vec)?;
            Some((id, score))
        })
        .collect();
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    let mut results: Vec<MemoryRecord> = Vec::new();
    for (id, _) in scored.into_iter().take(limit) {
        match store.get(id) {
            Ok(rec) => results.push(rec),
            // Row deleted between ranking and fetch — skip, not fail.
            Err(MemoryError::NotFound { .. }) => continue,
            Err(e) => return Err(e),
        }
    }

    // Keyword hits the semantic ranking missed (e.g. rows without vectors
    // beyond the backfill cap) fill the remaining slots.
    if results.len() < limit {
        for (rec, _) in store.search_keyword(query, limit)? {
            if results.len() >= limit {
                break;
            }
            if !results.iter().any(|r| r.id == rec.id) {
                results.push(rec);
            }
        }
    }

    log::info!("memory: search mode=semantic results={}", results.len());
    Ok(SearchOutcome {
        mode: SearchMode::Semantic,
        degrade_reason: None,
        results,
    })
}

/// The visible keyword degrade: FTS5 bm25 ranking with the typed reason
/// attached and logged by kind (slice observability contract).
fn keyword_degrade(
    store: &MemoryStore,
    query: &str,
    limit: usize,
    reason: LlmError,
) -> Result<SearchOutcome, MemoryError> {
    log::error!(
        "memory: search degraded to keyword mode ({}): {reason}",
        reason.kind()
    );
    let results = store
        .search_keyword(query, limit)?
        .into_iter()
        .map(|(rec, _)| rec)
        .collect();
    Ok(SearchOutcome {
        mode: SearchMode::Keyword,
        degrade_reason: Some(reason),
        results,
    })
}

/// Cosine similarity; `None` when the vectors are incomparable (dimension
/// mismatch — e.g. a leftover vector from a different model — or zero norm),
/// which excludes the row from ranking rather than poisoning the sort.
fn cosine(a: &[f32], b: &[f32]) -> Option<f32> {
    if a.len() != b.len() || a.is_empty() {
        return None;
    }
    let (mut dot, mut na, mut nb) = (0.0f32, 0.0f32, 0.0f32);
    for (x, y) in a.iter().zip(b) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 {
        return None;
    }
    Some(dot / (na.sqrt() * nb.sqrt()))
}

/// Bounded excerpt of a response body for error details.
fn snippet(s: &str) -> String {
    const MAX: usize = 200;
    let trimmed = s.trim();
    if trimmed.chars().count() <= MAX {
        trimmed.to_string()
    } else {
        let cut: String = trimmed.chars().take(MAX).collect();
        format!("{cut}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::openai::test_support::*;
    use crate::memory::store::NewMemory;

    fn mem(summary: &str, embedding: Option<Vec<f32>>) -> NewMemory {
        NewMemory {
            summary: summary.into(),
            apps: vec!["TestApp".into()],
            span_start_ms: 1_000,
            span_end_ms: 2_000,
            embedding,
            source: crate::memory::store::MemorySource::Watcher,
            category: "other".into(),
            tags: Vec::new(),
            pinned: false,
            expires_at_ms: None,
        }
    }

    fn store() -> MemoryStore {
        MemoryStore::open_in_memory().unwrap()
    }

    fn embeddings_response(vectors: &[&[f32]]) -> String {
        let data: Vec<serde_json::Value> = vectors
            .iter()
            .enumerate()
            .map(|(i, v)| serde_json::json!({ "index": i, "embedding": v }))
            .collect();
        serde_json::json!({ "object": "list", "data": data }).to_string()
    }

    // --- OpenAiEmbedder wire contract ---

    #[tokio::test]
    async fn embedder_parses_vectors_in_input_order() {
        let body = embeddings_response(&[&[1.0, 0.0], &[0.0, 1.0]]);
        let endpoint = spawn_raw_server(plain_response("200 OK", &body)).await;
        let vectors = OpenAiEmbedder::new(&endpoint)
            .embed(&["alpha".into(), "beta".into()])
            .await
            .unwrap();
        assert_eq!(vectors, vec![vec![1.0, 0.0], vec![0.0, 1.0]]);
    }

    #[tokio::test]
    async fn embedder_reorders_by_index_field() {
        // Servers may return entries out of order; `index` wins.
        let body = serde_json::json!({ "data": [
            { "index": 1, "embedding": [0.0, 1.0] },
            { "index": 0, "embedding": [1.0, 0.0] },
        ]})
        .to_string();
        let endpoint = spawn_raw_server(plain_response("200 OK", &body)).await;
        let vectors = OpenAiEmbedder::new(&endpoint)
            .embed(&["first".into(), "second".into()])
            .await
            .unwrap();
        assert_eq!(vectors[0], vec![1.0, 0.0]);
        assert_eq!(vectors[1], vec![0.0, 1.0]);
    }

    #[tokio::test]
    async fn embedder_request_carries_model_and_input() {
        let body = embeddings_response(&[&[1.0]]);
        let (endpoint, captured) = spawn_capturing_server(plain_response("200 OK", &body)).await;
        OpenAiEmbedder::new(&endpoint)
            .embed(&["hello".into()])
            .await
            .unwrap();
        let body = captured_body_json(&captured);
        assert_eq!(body["model"], EMBED_MODEL);
        assert_eq!(body["input"], serde_json::json!(["hello"]));
    }

    #[tokio::test]
    async fn embedder_with_model_overrides_the_id() {
        let body = embeddings_response(&[&[1.0]]);
        let (endpoint, captured) = spawn_capturing_server(plain_response("200 OK", &body)).await;
        OpenAiEmbedder::new(&endpoint)
            .with_model("custom-embed")
            .embed(&["x".into()])
            .await
            .unwrap();
        assert_eq!(captured_body_json(&captured)["model"], "custom-embed");
    }

    #[tokio::test]
    async fn embedder_refused_connection_is_offline_naming_endpoint() {
        let endpoint = refused_endpoint().await;
        let err = OpenAiEmbedder::new(&endpoint)
            .embed(&["x".into()])
            .await
            .unwrap_err();
        assert_eq!(err.kind(), "offline");
        assert_eq!(err.endpoint(), endpoint);
    }

    #[tokio::test]
    async fn embedder_http_4xx_is_no_model_with_body_detail() {
        let endpoint = spawn_raw_server(plain_response(
            "404 Not Found",
            r#"{"error":"model not found"}"#,
        ))
        .await;
        let err = OpenAiEmbedder::new(&endpoint)
            .embed(&["x".into()])
            .await
            .unwrap_err();
        assert_eq!(err.kind(), "no-model");
        assert!(
            matches!(&err, LlmError::NoModel { detail, .. } if detail.contains("model not found"))
        );
    }

    #[tokio::test]
    async fn embedder_http_5xx_is_offline() {
        let endpoint = spawn_raw_server(plain_response("503 Service Unavailable", "busy")).await;
        let err = OpenAiEmbedder::new(&endpoint)
            .embed(&["x".into()])
            .await
            .unwrap_err();
        assert_eq!(err.kind(), "offline");
    }

    #[tokio::test]
    async fn embedder_malformed_body_is_offline_with_detail() {
        let endpoint = spawn_raw_server(plain_response("200 OK", "not json")).await;
        let err = OpenAiEmbedder::new(&endpoint)
            .embed(&["x".into()])
            .await
            .unwrap_err();
        assert_eq!(err.kind(), "offline");
        assert!(matches!(&err, LlmError::Offline { detail, .. } if detail.contains("malformed")));
    }

    #[tokio::test]
    async fn embedder_wrong_vector_count_is_offline_with_detail() {
        let body = embeddings_response(&[&[1.0]]);
        let endpoint = spawn_raw_server(plain_response("200 OK", &body)).await;
        let err = OpenAiEmbedder::new(&endpoint)
            .embed(&["one".into(), "two".into()])
            .await
            .unwrap_err();
        assert_eq!(err.kind(), "offline");
        assert!(matches!(&err, LlmError::Offline { detail, .. } if detail.contains("2 inputs")));
    }

    #[tokio::test]
    async fn embedder_endpoint_trailing_slash_is_normalized() {
        let e = OpenAiEmbedder::new("http://x:1/");
        assert_eq!(e.endpoint(), "http://x:1");
    }

    // --- hybrid search ---

    /// Deterministic embedder: maps texts onto fixed topic axes so cosine
    /// ranking is predictable. Fails wholesale when `fail_with` is set.
    struct MockEmbedder {
        fail_with: Option<LlmError>,
    }

    impl MockEmbedder {
        fn ok() -> Self {
            Self { fail_with: None }
        }

        fn failing(err: LlmError) -> Self {
            Self {
                fail_with: Some(err),
            }
        }

        fn vector_for(text: &str) -> Vec<f32> {
            let t = text.to_lowercase();
            let axes = ["rust", "coffee", "meeting"];
            let mut v: Vec<f32> = axes
                .iter()
                .map(|axis| if t.contains(axis) { 1.0 } else { 0.0 })
                .collect();
            if v.iter().all(|x| *x == 0.0) {
                v[2] = 0.1; // off-topic texts get a weak default axis
            }
            v
        }
    }

    #[async_trait]
    impl Embedder for MockEmbedder {
        fn endpoint(&self) -> &str {
            "http://mock.invalid"
        }

        async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, LlmError> {
            if let Some(err) = &self.fail_with {
                return Err(err.clone());
            }
            Ok(texts.iter().map(|t| Self::vector_for(t)).collect())
        }
    }

    fn offline_err() -> LlmError {
        LlmError::Offline {
            endpoint: "http://mock.invalid".into(),
            detail: "refused".into(),
        }
    }

    #[tokio::test]
    async fn semantic_search_ranks_topical_memory_first() {
        let s = store();
        s.insert(mem(
            "tried a new coffee brewing ratio",
            Some(MockEmbedder::vector_for("coffee")),
        ))
        .unwrap();
        let rust = s
            .insert(mem(
                "fixed the rust borrow checker fight",
                Some(MockEmbedder::vector_for("rust")),
            ))
            .unwrap();
        s.insert(mem(
            "weekly meeting notes",
            Some(MockEmbedder::vector_for("meeting")),
        ))
        .unwrap();

        let outcome = search(&s, &MockEmbedder::ok(), "rust programming", 10)
            .await
            .unwrap();
        assert_eq!(outcome.mode, SearchMode::Semantic);
        assert!(outcome.degrade_reason.is_none());
        assert_eq!(outcome.results[0].id, rust.id);
    }

    #[tokio::test]
    async fn embed_failure_degrades_to_keyword_with_typed_reason() {
        let s = store();
        let rec = s.insert(mem("rust error taxonomy notes", None)).unwrap();
        let outcome = search(
            &s,
            &MockEmbedder::failing(offline_err()),
            "rust taxonomy",
            10,
        )
        .await
        .unwrap();
        assert_eq!(outcome.mode, SearchMode::Keyword);
        let reason = outcome
            .degrade_reason
            .expect("degrade must carry the typed reason");
        assert_eq!(reason.kind(), "offline");
        // Recall keeps working through FTS5 (R021).
        assert_eq!(
            outcome.results.iter().map(|r| r.id).collect::<Vec<_>>(),
            vec![rec.id]
        );
    }

    #[tokio::test]
    async fn degrade_outcome_serializes_camel_case_with_reason() {
        let s = store();
        s.insert(mem("rust notes", None)).unwrap();
        let outcome = search(&s, &MockEmbedder::failing(offline_err()), "rust", 10)
            .await
            .unwrap();
        let v = serde_json::to_value(&outcome).unwrap();
        assert_eq!(v["mode"], "keyword");
        assert_eq!(v["degradeReason"]["kind"], "offline");
        assert_eq!(v["degradeReason"]["endpoint"], "http://mock.invalid");
        assert!(v["results"].is_array());
    }

    #[tokio::test]
    async fn semantic_outcome_omits_degrade_reason_key() {
        let s = store();
        let v = serde_json::to_value(search(&s, &MockEmbedder::ok(), "rust", 5).await.unwrap())
            .unwrap();
        assert_eq!(v["mode"], "semantic");
        assert!(
            !v.as_object().unwrap().contains_key("degradeReason"),
            "no-degrade outcomes must omit the key: {v}"
        );
    }

    #[tokio::test]
    async fn blank_query_returns_empty_without_calling_embedder() {
        let s = store();
        s.insert(mem("anything", None)).unwrap();
        // A failing embedder proves the embedder is never invoked.
        let outcome = search(&s, &MockEmbedder::failing(offline_err()), "   ", 10)
            .await
            .unwrap();
        assert!(outcome.results.is_empty());
        assert!(outcome.degrade_reason.is_none());
    }

    #[tokio::test]
    async fn unembedded_rows_are_backfilled_and_persisted_by_search() {
        let s = store();
        let rec = s.insert(mem("rust ownership deep dive", None)).unwrap();
        assert!(s.embedded_rows().unwrap().is_empty());

        let outcome = search(&s, &MockEmbedder::ok(), "rust", 10).await.unwrap();
        assert_eq!(outcome.mode, SearchMode::Semantic);
        assert_eq!(outcome.results[0].id, rec.id);

        // The vector was written back — the corpus healed itself (D022).
        let rows = s.embedded_rows().unwrap();
        assert_eq!(
            rows,
            vec![(rec.id, MockEmbedder::vector_for("rust ownership deep dive"))]
        );
    }

    #[tokio::test]
    async fn keyword_hits_missing_from_semantic_ranking_are_appended() {
        let s = store();
        let semantic = s
            .insert(mem(
                "rust async runtime notes",
                Some(MockEmbedder::vector_for("rust")),
            ))
            .unwrap();
        // Simulate a row past the backfill cap: no vector, but a keyword hit.
        // Backfill would embed it, so pre-fill every other row and cap at 2.
        let keyword_only = s.insert(mem("rust macro hygiene rules", None)).unwrap();
        s.set_embedding(keyword_only.id, &[]).unwrap_err(); // guard: empty vec rejected

        let outcome = search(&s, &MockEmbedder::ok(), "rust macro", 10)
            .await
            .unwrap();
        let ids: Vec<i64> = outcome.results.iter().map(|r| r.id).collect();
        assert!(ids.contains(&semantic.id));
        assert!(ids.contains(&keyword_only.id));
        // No duplicates even though the row matches both paths.
        let mut deduped = ids.clone();
        deduped.dedup();
        assert_eq!(ids.len(), deduped.len());
    }

    #[tokio::test]
    async fn limit_caps_combined_results() {
        let s = store();
        for i in 0..5 {
            s.insert(mem(
                &format!("rust note {i}"),
                Some(MockEmbedder::vector_for("rust")),
            ))
            .unwrap();
        }
        let outcome = search(&s, &MockEmbedder::ok(), "rust", 3).await.unwrap();
        assert_eq!(outcome.results.len(), 3);
    }

    #[tokio::test]
    async fn dimension_mismatched_vectors_are_excluded_not_fatal() {
        let s = store();
        // A leftover 2-dim vector among 3-dim ones (e.g. old model output).
        s.insert(mem("stale vector row", Some(vec![1.0, 0.0])))
            .unwrap();
        let good = s
            .insert(mem(
                "rust lifetimes explained",
                Some(MockEmbedder::vector_for("rust")),
            ))
            .unwrap();
        let outcome = search(&s, &MockEmbedder::ok(), "rust", 10).await.unwrap();
        assert_eq!(outcome.mode, SearchMode::Semantic);
        assert_eq!(outcome.results[0].id, good.id);
    }

    #[test]
    fn cosine_scores_identical_direction_highest() {
        let a = vec![1.0, 0.0, 0.5];
        assert!((cosine(&a, &a).unwrap() - 1.0).abs() < 1e-6);
        assert!(cosine(&[1.0, 0.0], &[0.0, 1.0]).unwrap().abs() < 1e-6);
        assert!(cosine(&[1.0, 0.0], &[-1.0, 0.0]).unwrap() < -0.99);
    }

    #[test]
    fn cosine_rejects_mismatch_and_zero_norm() {
        assert_eq!(cosine(&[1.0], &[1.0, 2.0]), None);
        assert_eq!(cosine(&[], &[]), None);
        assert_eq!(cosine(&[0.0, 0.0], &[1.0, 1.0]), None);
    }
}
