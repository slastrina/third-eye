//! Multi-model routing (R003): named lanes in front of the [`LlmClient`] seam.
//!
//! A [`ModelRouter`] owns an ordered set of [`Lane`]s — canonically
//! [`THIN_LANE`] for quick prompts and [`HEAVY_LANE`] for deep work — each
//! delegating to its own client (an [`OpenAiClient`] pinned to that lane's
//! model id, or unpinned for single-model fallback). The router itself
//! implements [`LlmClient`], so `commands.rs` and every future wrapper
//! (M003's privacy guard) see one client and the S02 single-flight /
//! `llm://*` event contracts are untouched.
//!
//! Routing state is queryable as a value ([`ModelInfo`], mirroring
//! `llm_health`'s health-as-value pattern) and observable in logs:
//! every request logs its lane + model id at debug level at stream start,
//! and every lane switch logs old → new at info level.

use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use serde::Serialize;

use super::guard::{GuardState, GuardedClient};
use super::openai::OpenAiClient;
use super::{ChatRequest, LlmClient, LlmError, LlmHealth, StreamOutcome, TokenSink};

/// Lane for quick, low-latency prompts (small model). The default lane.
pub const THIN_LANE: &str = "thin";
/// Lane for deep work (large model).
pub const HEAVY_LANE: &str = "heavy";

/// Placeholder shown in logs and `ModelInfo` when a lane has no pinned model
/// id (LM Studio then serves whatever single model is loaded).
const DEFAULT_MODEL_LABEL: &str = "default";

/// One named routing lane: a display name, the model id it pins (if any),
/// and the client that serves it.
pub struct Lane {
    name: String,
    model_id: Option<String>,
    client: Arc<dyn LlmClient>,
}

impl Lane {
    pub fn new(
        name: impl Into<String>,
        model_id: Option<String>,
        client: Arc<dyn LlmClient>,
    ) -> Self {
        Self { name: name.into(), model_id, client }
    }

    fn model_label(&self) -> &str {
        self.model_id.as_deref().unwrap_or(DEFAULT_MODEL_LABEL)
    }
}

/// Queryable routing state: the active lane plus every configured lane and
/// its model id. Serialized camelCase — this JSON shape is the `model_info`
/// IPC contract with the UI (T02/T03).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelInfo {
    pub active_lane: String,
    pub endpoint: String,
    pub lanes: Vec<LaneInfo>,
}

/// One lane as seen by the UI. `model_id` is `None` when the lane is
/// unpinned (single-model fallback).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LaneInfo {
    pub name: String,
    pub model_id: Option<String>,
}

/// Routes each request to the active lane's client. Object-shape-compatible
/// with any other [`LlmClient`]: errors, partial text, and health pass
/// through the delegated client unchanged.
pub struct ModelRouter {
    /// Re-pinnable at runtime (S07 settings): read per request, written only
    /// by `set_lane_model`. Lock ordering is always `lanes` before `active`;
    /// neither lock is ever held across an await.
    lanes: RwLock<Vec<Lane>>,
    /// Index into `lanes`. Read per request, written only by `set_active`.
    active: RwLock<usize>,
    /// All lanes target the same LM Studio instance; cached so `endpoint()`
    /// can return a borrow without touching the lock.
    endpoint: String,
    /// Shared privacy-guard telemetry (M003 S02). `set_lane_model` rebuilds
    /// lane clients at runtime, so the router must hold the same
    /// [`GuardState`] its construction path wrapped with — otherwise a re-pin
    /// would silently produce an unguarded client.
    guard: Arc<GuardState>,
}

impl ModelRouter {
    /// Build a router over `lanes`; the first lane is active. Panics on an
    /// empty lane list — a router with nothing to route to is a wiring bug.
    /// Callers providing pre-built (mock) lanes get a private guard state;
    /// production goes through [`thin_heavy`](Self::thin_heavy), which wires
    /// the app-shared one.
    pub fn new(lanes: Vec<Lane>) -> Self {
        Self::with_guard(lanes, Arc::new(GuardState::new()))
    }

    /// [`new`](Self::new) with an explicit shared [`GuardState`].
    pub fn with_guard(lanes: Vec<Lane>, guard: Arc<GuardState>) -> Self {
        assert!(!lanes.is_empty(), "ModelRouter requires at least one lane");
        let endpoint = lanes[0].client.endpoint().to_string();
        Self { lanes: RwLock::new(lanes), active: RwLock::new(0), endpoint, guard }
    }

    /// The canonical thin/heavy pair against one OpenAI-compatible endpoint.
    /// A lane with `None` sends no `model` field, so a single-model
    /// deployment keeps working with whatever LM Studio has loaded.
    ///
    /// This is the production construction choke point (M003 S02): every
    /// lane client is wrapped in [`GuardedClient`] here, so the active-lane
    /// path (chat, tool loop) and the [`lane_client`](Self::lane_client) path
    /// (distillation, nudge classification) can only ever reach guarded
    /// clients.
    pub fn thin_heavy(
        endpoint: &str,
        thin_model: Option<String>,
        heavy_model: Option<String>,
        guard: Arc<GuardState>,
    ) -> Self {
        let lane = |name: &str, model: &Option<String>| {
            let client = match model {
                Some(id) => OpenAiClient::new(endpoint).with_model(id.clone()),
                None => OpenAiClient::new(endpoint),
            };
            let guarded = GuardedClient::new(Arc::new(client), guard.clone());
            Lane::new(name, model.clone(), Arc::new(guarded))
        };
        let lanes = vec![lane(THIN_LANE, &thin_model), lane(HEAVY_LANE, &heavy_model)];
        Self::with_guard(lanes, guard)
    }

    /// Switch the active lane. Unknown lane names are rejected with an error
    /// naming the lane and the known set; the active lane is left unchanged.
    /// Returns the updated routing state on success.
    pub fn set_active(&self, name: &str) -> Result<ModelInfo, String> {
        let lanes = self.lanes.read().unwrap();
        let idx = lane_index(&lanes, name)?;
        let mut active = self.active.write().unwrap();
        if *active != idx {
            log::info!(
                "llm: lane switch {} → {} (model={})",
                lanes[*active].name,
                lanes[idx].name,
                lanes[idx].model_label()
            );
            *active = idx;
        }
        drop(active);
        drop(lanes);
        Ok(self.info())
    }

    /// Re-pin a lane to `model` (S07 settings): rebuilds the lane's client
    /// against the shared endpoint so the *next* request uses the new pin.
    /// `None` unpins the lane — requests then omit the `model` key entirely
    /// (single-model fallback). In-flight streams are unaffected: they hold
    /// the client `Arc` they snapshotted at stream start and finish on it.
    /// Unknown lanes are rejected naming the lane and the known set, leaving
    /// every lane unchanged. Returns the updated routing state on success.
    pub fn set_lane_model(&self, name: &str, model: Option<String>) -> Result<ModelInfo, String> {
        let mut lanes = self.lanes.write().unwrap();
        let idx = lane_index(&lanes, name)?;
        let old = lanes[idx].model_label().to_string();
        let client = match &model {
            Some(id) => OpenAiClient::new(&self.endpoint).with_model(id.clone()),
            None => OpenAiClient::new(&self.endpoint),
        };
        // Rebuilt clients go through the same guard wrap as construction —
        // a runtime re-pin must never produce an unguarded client.
        let guarded = GuardedClient::new(Arc::new(client), self.guard.clone());
        lanes[idx].model_id = model;
        lanes[idx].client = Arc::new(guarded);
        log::info!("llm: lane {name} re-pin {old} → {}", lanes[idx].model_label());
        drop(lanes);
        Ok(self.info())
    }

    /// Swap a lane's client verbatim — the M004 S05 cloud-routing seam.
    /// Unlike [`set_lane_model`](Self::set_lane_model), which rebuilds a local
    /// [`OpenAiClient`] and re-wraps it in a fresh [`GuardedClient`], this
    /// injects an already-guarded client (the cloud `Arc<GuardedClient>` that
    /// `build_cloud_client` produced) *verbatim* — no re-wrap, so the guard the
    /// caller mounted is exactly the guard on the wire. The lane's `model_id`
    /// is deliberately left untouched: it is the record of the lane's *local*
    /// pin, so a later revert can restore it via
    /// [`set_lane_model`](Self::set_lane_model). Same lock discipline as
    /// `set_lane_model` — the `lanes` write lock is dropped before returning and
    /// never held across an await, so in-flight streams finish on the client
    /// `Arc` they snapshotted at start. Unknown lanes are rejected naming the
    /// lane and the known set, leaving every lane unchanged. Returns the updated
    /// routing state on success.
    pub fn set_lane_client(
        &self,
        name: &str,
        client: Arc<dyn LlmClient>,
    ) -> Result<ModelInfo, String> {
        let mut lanes = self.lanes.write().unwrap();
        let idx = lane_index(&lanes, name)?;
        lanes[idx].client = client;
        log::info!(
            "llm: lane {name} client swap → {} (model pin {} preserved)",
            lanes[idx].client.endpoint(),
            lanes[idx].model_label()
        );
        drop(lanes);
        Ok(self.info())
    }

    /// Current routing state as a value (health-as-value pattern).
    pub fn info(&self) -> ModelInfo {
        let lanes = self.lanes.read().unwrap();
        let active = *self.active.read().unwrap();
        ModelInfo {
            active_lane: lanes[active].name.clone(),
            endpoint: self.endpoint.clone(),
            lanes: lanes
                .iter()
                .map(|l| LaneInfo { name: l.name.clone(), model_id: l.model_id.clone() })
                .collect(),
        }
    }

    /// Snapshot a named lane's model label and client regardless of the
    /// active lane — the S02 ingestion seam: distillation stays pinned to
    /// the thin lane even while the user chats on heavy. Callers snapshot
    /// per request, so a runtime re-pin applies to their *next* call, exactly
    /// like [`set_lane_model`](Self::set_lane_model)'s in-flight semantics.
    /// Unknown lanes are rejected naming the lane and the known set.
    pub fn lane_client(&self, name: &str) -> Result<(String, Arc<dyn LlmClient>), String> {
        let lanes = self.lanes.read().unwrap();
        let idx = lane_index(&lanes, name)?;
        Ok((lanes[idx].model_label().to_string(), lanes[idx].client.clone()))
    }

    /// The shared guard telemetry this router's clients record into — the
    /// S02→S03 boundary artifact.
    pub fn guard_state(&self) -> Arc<GuardState> {
        self.guard.clone()
    }

    /// Snapshot the active lane without holding the lock across an await.
    fn active_lane(&self) -> (String, String, Arc<dyn LlmClient>) {
        let lanes = self.lanes.read().unwrap();
        let idx = *self.active.read().unwrap();
        let lane = &lanes[idx];
        (lane.name.clone(), lane.model_label().to_string(), lane.client.clone())
    }
}

/// Position of `name` in `lanes`, or the canonical unknown-lane error naming
/// the rejected lane and the known set (shared by `set_active` /
/// `set_lane_model` so the two rejections cannot drift).
fn lane_index(lanes: &[Lane], name: &str) -> Result<usize, String> {
    lanes.iter().position(|l| l.name == name).ok_or_else(|| {
        let known: Vec<&str> = lanes.iter().map(|l| l.name.as_str()).collect();
        format!("unknown lane \"{name}\" (known lanes: {})", known.join(", "))
    })
}

#[async_trait]
impl LlmClient for ModelRouter {
    fn endpoint(&self) -> &str {
        &self.endpoint
    }

    async fn stream_chat(
        &self,
        request: &ChatRequest,
        on_token: TokenSink<'_>,
    ) -> Result<StreamOutcome, LlmError> {
        let (lane, model, client) = self.active_lane();
        log::debug!(
            "llm: routing via lane={lane} model={model} endpoint={}",
            client.endpoint()
        );
        client.stream_chat(request, on_token).await
    }

    async fn health(&self) -> LlmHealth {
        let (_, _, client) = self.active_lane();
        client.health().await
    }
}

#[cfg(test)]
mod tests {
    use super::super::openai::test_support::*;
    use super::*;
    use crate::llm::ChatMessage;
    use std::sync::Mutex;

    fn req(messages: Vec<ChatMessage>) -> ChatRequest {
        ChatRequest::new(messages)
    }

    /// Fresh guard telemetry for tests that construct through the production
    /// choke point.
    fn test_guard() -> Arc<GuardState> {
        Arc::new(GuardState::new())
    }

    /// Mock lane client that answers with its tag, so tests can see which
    /// lane a request was delegated to.
    struct TaggedClient {
        tag: &'static str,
        online: bool,
        fail_with: Option<LlmError>,
    }

    impl TaggedClient {
        fn ok(tag: &'static str) -> Arc<dyn LlmClient> {
            Arc::new(Self { tag, online: true, fail_with: None })
        }

        fn failing(tag: &'static str, err: LlmError) -> Arc<dyn LlmClient> {
            Arc::new(Self { tag, online: false, fail_with: Some(err) })
        }
    }

    #[async_trait]
    impl LlmClient for TaggedClient {
        fn endpoint(&self) -> &str {
            "http://mock.invalid"
        }

        async fn stream_chat(
            &self,
            _request: &ChatRequest,
            on_token: TokenSink<'_>,
        ) -> Result<StreamOutcome, LlmError> {
            if let Some(err) = &self.fail_with {
                return Err(err.clone());
            }
            on_token(self.tag);
            Ok(StreamOutcome { text: self.tag.into(), token_count: 1, tool_calls: Vec::new() })
        }

        async fn health(&self) -> LlmHealth {
            LlmHealth { online: self.online, endpoint: self.endpoint().into() }
        }
    }

    fn thin_heavy_mock() -> ModelRouter {
        ModelRouter::new(vec![
            Lane::new(THIN_LANE, Some("thin-1b".into()), TaggedClient::ok("thin-reply")),
            Lane::new(HEAVY_LANE, Some("heavy-7b".into()), TaggedClient::ok("heavy-reply")),
        ])
    }

    async fn chat(router: &ModelRouter) -> (Result<StreamOutcome, LlmError>, Vec<String>) {
        let seen = Mutex::new(Vec::new());
        let result = router
            .stream_chat(&req(vec![ChatMessage::user("hi")]), &|t| {
                seen.lock().unwrap().push(t.to_string())
            })
            .await;
        (result, seen.into_inner().unwrap())
    }

    #[tokio::test]
    async fn first_lane_is_active_by_default() {
        let router = thin_heavy_mock();
        let (result, seen) = chat(&router).await;
        assert_eq!(result.unwrap().text, "thin-reply");
        assert_eq!(seen, vec!["thin-reply"], "tokens must flow through the router");
    }

    #[tokio::test]
    async fn set_active_switches_the_delegated_client() {
        let router = thin_heavy_mock();
        router.set_active(HEAVY_LANE).unwrap();
        let (result, seen) = chat(&router).await;
        assert_eq!(result.unwrap().text, "heavy-reply");
        assert_eq!(seen, vec!["heavy-reply"]);

        // And back: switching is not one-way.
        router.set_active(THIN_LANE).unwrap();
        let (result, _) = chat(&router).await;
        assert_eq!(result.unwrap().text, "thin-reply");
    }

    #[tokio::test]
    async fn unknown_lane_is_rejected_naming_lane_and_known_set() {
        let router = thin_heavy_mock();
        let err = router.set_active("turbo").unwrap_err();
        assert!(err.contains("turbo"), "error must name the rejected lane: {err}");
        assert!(err.contains("thin") && err.contains("heavy"), "error must list known lanes: {err}");

        // The active lane must be unchanged after a rejected switch.
        assert_eq!(router.info().active_lane, THIN_LANE);
        let (result, _) = chat(&router).await;
        assert_eq!(result.unwrap().text, "thin-reply");
    }

    #[tokio::test]
    async fn errors_pass_through_unchanged_with_partial_text() {
        let interrupted = LlmError::Interrupted {
            endpoint: "http://mock.invalid".into(),
            partial_text: "half an ans".into(),
            detail: "connection reset".into(),
        };
        let router = ModelRouter::new(vec![Lane::new(
            THIN_LANE,
            None,
            TaggedClient::failing("thin", interrupted.clone()),
        )]);
        let (result, seen) = chat(&router).await;
        let err = result.unwrap_err();
        assert_eq!(err, interrupted, "the router must not rewrap or lose error fields");
        assert_eq!(err.partial_text(), Some("half an ans"));
        assert!(seen.is_empty());
    }

    #[tokio::test]
    async fn health_passes_through_the_active_lane() {
        let router = ModelRouter::new(vec![
            Lane::new(THIN_LANE, None, TaggedClient::ok("thin")),
            Lane::new(
                HEAVY_LANE,
                None,
                TaggedClient::failing(
                    "heavy",
                    LlmError::Offline { endpoint: "http://mock.invalid".into(), detail: "down".into() },
                ),
            ),
        ]);
        assert!(router.health().await.online);
        router.set_active(HEAVY_LANE).unwrap();
        assert!(!router.health().await.online);
    }

    #[test]
    fn model_info_serializes_camel_case() {
        // The UI reads activeLane / lanes[].modelId; a change here is a
        // breaking IPC change and must be coordinated with src/chat.ts.
        let router = thin_heavy_mock();
        let v = serde_json::to_value(router.info()).unwrap();
        assert_eq!(v["activeLane"], "thin");
        assert_eq!(v["endpoint"], "http://mock.invalid");
        assert_eq!(v["lanes"][0]["name"], "thin");
        assert_eq!(v["lanes"][0]["modelId"], "thin-1b");
        assert_eq!(v["lanes"][1]["name"], "heavy");
        assert_eq!(v["lanes"][1]["modelId"], "heavy-7b");
    }

    #[test]
    fn model_info_has_null_model_id_for_unpinned_lane() {
        let router = ModelRouter::new(vec![Lane::new(THIN_LANE, None, TaggedClient::ok("t"))]);
        let v = serde_json::to_value(router.info()).unwrap();
        assert!(v["lanes"][0]["modelId"].is_null());
    }

    #[test]
    fn set_active_returns_the_updated_info() {
        let router = thin_heavy_mock();
        let info = router.set_active(HEAVY_LANE).unwrap();
        assert_eq!(info.active_lane, "heavy");
        assert_eq!(router.info().active_lane, "heavy");
    }

    #[tokio::test]
    async fn thin_heavy_routes_pinned_model_ids_over_the_wire() {
        // End-to-end through real OpenAiClients: the thin lane's request
        // carries its model id.
        let parts = vec![sse_token("ok"), "data: [DONE]\n\n".to_string()];
        let (endpoint, captured) = spawn_capturing_server(chunked_200(&parts, true)).await;
        let router =
            ModelRouter::thin_heavy(&endpoint, Some("thin-1b".into()), Some("heavy-7b".into()), test_guard());
        router.stream_chat(&req(vec![ChatMessage::user("quick one")]), &|_| {}).await.unwrap();
        assert_eq!(captured_body_json(&captured)["model"], "thin-1b");
    }

    #[tokio::test]
    async fn thin_heavy_unpinned_lane_omits_model_key_over_the_wire() {
        // Single-model fallback end-to-end: no model ids configured → the
        // outbound JSON has no "model" key at all.
        let parts = vec![sse_token("ok"), "data: [DONE]\n\n".to_string()];
        let (endpoint, captured) = spawn_capturing_server(chunked_200(&parts, true)).await;
        let router = ModelRouter::thin_heavy(&endpoint, None, None, test_guard());
        router.stream_chat(&req(vec![ChatMessage::user("hi")]), &|_| {}).await.unwrap();
        let body = captured_body_json(&captured);
        assert!(!body.as_object().unwrap().contains_key("model"), "got: {body}");
    }

    #[tokio::test]
    async fn router_forwards_tools_over_the_wire_unchanged() {
        // The router is a pass-through for the whole ChatRequest: tool
        // definitions reach the wire exactly as the caller attached them.
        let parts = vec![sse_token("ok"), "data: [DONE]\n\n".to_string()];
        let (endpoint, captured) = spawn_capturing_server(chunked_200(&parts, true)).await;
        let router = ModelRouter::thin_heavy(&endpoint, None, None, test_guard());
        let request = req(vec![ChatMessage::user("hi")]).with_tools(vec![
            crate::llm::ToolDefinition {
                name: "memory_search".into(),
                description: "d".into(),
                parameters: serde_json::json!({"type": "object"}),
            },
        ]);
        router.stream_chat(&request, &|_| {}).await.unwrap();
        let body = captured_body_json(&captured);
        assert_eq!(body["tools"][0]["function"]["name"], "memory_search");
    }

    #[test]
    fn thin_heavy_builds_the_canonical_lane_pair() {
        let router = ModelRouter::thin_heavy("http://x:1/", Some("a".into()), None, test_guard());
        let info = router.info();
        assert_eq!(info.active_lane, "thin");
        assert_eq!(info.endpoint, "http://x:1", "endpoint must be normalized");
        assert_eq!(info.lanes.len(), 2);
        assert_eq!(info.lanes[0].model_id.as_deref(), Some("a"));
        assert_eq!(info.lanes[1].model_id, None);
    }

    #[test]
    #[should_panic(expected = "at least one lane")]
    fn empty_lane_list_is_a_construction_bug() {
        ModelRouter::new(vec![]);
    }

    #[tokio::test]
    async fn set_lane_model_repinned_lane_sends_new_model_over_the_wire() {
        // The S07 settings contract: after a runtime re-pin, the next request
        // on that lane carries the new "model" key.
        let parts = vec![sse_token("ok"), "data: [DONE]\n\n".to_string()];
        let (endpoint, captured) = spawn_capturing_server(chunked_200(&parts, true)).await;
        let router = ModelRouter::thin_heavy(&endpoint, Some("thin-1b".into()), None, test_guard());
        router.set_lane_model(THIN_LANE, Some("qwen2.5-14b".into())).unwrap();
        router.stream_chat(&req(vec![ChatMessage::user("hi")]), &|_| {}).await.unwrap();
        assert_eq!(captured_body_json(&captured)["model"], "qwen2.5-14b");
    }

    #[tokio::test]
    async fn set_lane_model_unpinned_lane_omits_model_key_over_the_wire() {
        // Explicit unpin: the re-pinned lane's requests carry no "model" key
        // at all, so a single-model LM Studio serves whatever it has loaded.
        let parts = vec![sse_token("ok"), "data: [DONE]\n\n".to_string()];
        let (endpoint, captured) = spawn_capturing_server(chunked_200(&parts, true)).await;
        let router = ModelRouter::thin_heavy(&endpoint, Some("thin-1b".into()), None, test_guard());
        router.set_lane_model(THIN_LANE, None).unwrap();
        router.stream_chat(&req(vec![ChatMessage::user("hi")]), &|_| {}).await.unwrap();
        let body = captured_body_json(&captured);
        assert!(!body.as_object().unwrap().contains_key("model"), "got: {body}");
    }

    #[test]
    fn set_lane_model_updates_info_and_returns_it() {
        let router = thin_heavy_mock();
        let info = router.set_lane_model(HEAVY_LANE, Some("heavy-70b".into())).unwrap();
        assert_eq!(info.lanes[1].model_id.as_deref(), Some("heavy-70b"));
        assert_eq!(router.info().lanes[1].model_id.as_deref(), Some("heavy-70b"));
        // The other lane and the active lane are untouched.
        assert_eq!(info.lanes[0].model_id.as_deref(), Some("thin-1b"));
        assert_eq!(info.active_lane, THIN_LANE);
    }

    #[test]
    fn set_lane_model_rejects_unknown_lane_leaving_lanes_unchanged() {
        let router = thin_heavy_mock();
        let err = router.set_lane_model("turbo", Some("x".into())).unwrap_err();
        assert!(err.contains("turbo"), "error must name the rejected lane: {err}");
        assert!(err.contains("thin") && err.contains("heavy"), "error must list known lanes: {err}");
        let info = router.info();
        assert_eq!(info.lanes[0].model_id.as_deref(), Some("thin-1b"));
        assert_eq!(info.lanes[1].model_id.as_deref(), Some("heavy-7b"));
    }

    #[tokio::test]
    async fn lane_client_returns_the_named_lane_regardless_of_active() {
        // The S02 ingestion contract: distillation pins the thin lane even
        // while the user chats on heavy.
        let router = thin_heavy_mock();
        router.set_active(HEAVY_LANE).unwrap();
        let (model, client) = router.lane_client(THIN_LANE).unwrap();
        assert_eq!(model, "thin-1b");
        let seen = Mutex::new(Vec::new());
        let outcome = client
            .stream_chat(&req(vec![ChatMessage::user("hi")]), &|t| {
                seen.lock().unwrap().push(t.to_string())
            })
            .await
            .unwrap();
        assert_eq!(outcome.text, "thin-reply");
        assert_eq!(*seen.lock().unwrap(), vec!["thin-reply"]);
    }

    #[test]
    fn lane_client_labels_an_unpinned_lane_default() {
        let router = ModelRouter::new(vec![Lane::new(THIN_LANE, None, TaggedClient::ok("t"))]);
        let (model, _) = router.lane_client(THIN_LANE).unwrap();
        assert_eq!(model, "default");
    }

    #[test]
    fn lane_client_rejects_unknown_lane_naming_known_set() {
        let router = thin_heavy_mock();
        let err = router.lane_client("turbo").err().expect("unknown lane must be rejected");
        assert!(err.contains("turbo"), "error must name the rejected lane: {err}");
        assert!(err.contains("thin") && err.contains("heavy"), "error must list known lanes: {err}");
    }

    #[tokio::test]
    async fn in_flight_stream_is_unaffected_by_a_concurrent_repin() {
        // A stream snapshots its client Arc at start; re-pinning the lane
        // mid-stream must not touch it. The gated mock blocks until released,
        // guaranteeing the re-pin happens while the stream is in flight.
        struct GatedClient {
            gate: Arc<tokio::sync::Notify>,
        }

        #[async_trait]
        impl LlmClient for GatedClient {
            fn endpoint(&self) -> &str {
                "http://mock.invalid"
            }

            async fn stream_chat(
                &self,
                _request: &ChatRequest,
                on_token: TokenSink<'_>,
            ) -> Result<StreamOutcome, LlmError> {
                self.gate.notified().await;
                on_token("old-client-reply");
                Ok(StreamOutcome {
                    text: "old-client-reply".into(),
                    token_count: 1,
                    tool_calls: Vec::new(),
                })
            }

            async fn health(&self) -> LlmHealth {
                LlmHealth { online: true, endpoint: self.endpoint().into() }
            }
        }

        let gate = Arc::new(tokio::sync::Notify::new());
        let router = Arc::new(ModelRouter::new(vec![Lane::new(
            THIN_LANE,
            Some("old".into()),
            Arc::new(GatedClient { gate: gate.clone() }),
        )]));

        let stream_router = router.clone();
        let handle = tokio::spawn(async move {
            stream_router.stream_chat(&req(vec![ChatMessage::user("hi")]), &|_| {}).await
        });
        tokio::task::yield_now().await;

        router.set_lane_model(THIN_LANE, Some("new".into())).unwrap();
        assert_eq!(router.info().lanes[0].model_id.as_deref(), Some("new"));

        gate.notify_one();
        let outcome = handle.await.unwrap().unwrap();
        assert_eq!(outcome.text, "old-client-reply", "in-flight stream must finish on its snapshot");
    }

    // --- M004 S05: the cloud-routing injection seam. set_lane_client swaps a
    // --- lane's already-guarded client verbatim (no re-wrap) and preserves the
    // --- local model pin so a revert can restore it.

    #[tokio::test]
    async fn set_lane_client_swaps_the_delegated_client_verbatim() {
        // The heavy lane starts on a local reply; injecting a new client makes
        // the *next* heavy request answer with the injected client's tag.
        let router = thin_heavy_mock();
        router.set_active(HEAVY_LANE).unwrap();
        let (before, _) = chat(&router).await;
        assert_eq!(before.unwrap().text, "heavy-reply");

        router.set_lane_client(HEAVY_LANE, TaggedClient::ok("cloud-reply")).unwrap();
        let (after, seen) = chat(&router).await;
        assert_eq!(after.unwrap().text, "cloud-reply", "the injected client must serve the lane");
        assert_eq!(seen, vec!["cloud-reply"]);
    }

    #[test]
    fn set_lane_client_preserves_the_local_model_pin_for_revert() {
        // The revert contract: model_id is the record of the lane's local pin,
        // so injecting a cloud client must leave it intact — set_lane_model can
        // then rebuild the same local model on opt-in-off.
        let router = thin_heavy_mock();
        router.set_lane_client(HEAVY_LANE, TaggedClient::ok("cloud")).unwrap();
        assert_eq!(
            router.info().lanes[1].model_id.as_deref(),
            Some("heavy-7b"),
            "the heavy lane's local model pin must survive a client swap"
        );
    }

    #[tokio::test]
    async fn set_lane_client_injects_the_clients_own_guard_not_a_re_wrap() {
        // "Verbatim" proof: inject a GuardedClient carrying its OWN GuardState;
        // a blocked request must increment THAT guard, never the router's — the
        // router does not re-wrap the injected client in its own guard.
        let router_guard = test_guard();
        let router = ModelRouter::thin_heavy(TEST_NET_1, None, None, router_guard.clone());

        let injected_guard = test_guard();
        let cloud = OpenAiClient::new(TEST_NET_1);
        let guarded = GuardedClient::new(Arc::new(cloud), injected_guard.clone());
        router.set_lane_client(HEAVY_LANE, Arc::new(guarded)).unwrap();
        router.set_active(HEAVY_LANE).unwrap();

        let err = router.stream_chat(&low_confidence_req(), &|_| {}).await.unwrap_err();
        assert_eq!(err.kind(), "guard-blocked", "the injected guarded client must still block");
        assert_eq!(injected_guard.blocked_count(), 1, "the injected client's own guard fired");
        assert_eq!(router_guard.blocked_count(), 0, "the router did not re-wrap with its guard");
    }

    #[test]
    fn set_lane_client_rejects_unknown_lane_leaving_lanes_unchanged() {
        let router = thin_heavy_mock();
        let err = router
            .set_lane_client("turbo", TaggedClient::ok("x"))
            .err()
            .expect("unknown lane must be rejected");
        assert!(err.contains("turbo"), "error must name the rejected lane: {err}");
        assert!(err.contains("thin") && err.contains("heavy"), "error must list known lanes: {err}");
        let info = router.info();
        assert_eq!(info.lanes[0].model_id.as_deref(), Some("thin-1b"));
        assert_eq!(info.lanes[1].model_id.as_deref(), Some("heavy-7b"));
    }

    // --- M003 S02 integration proof: the guard is mounted at construction,
    // --- so every call path a consumer can reach is guarded.

    /// TEST-NET-1 (RFC 5737): reserved documentation address — nothing can
    /// listen there, so an actual connect attempt surfaces as `offline`.
    const TEST_NET_1: &str = "http://192.0.2.1:9";

    /// A request the guard must refuse on an external endpoint: the pinned
    /// Low-confidence redaction condition (Luhn-failing digits beside card
    /// context, see privacy::mod tests).
    fn low_confidence_req() -> ChatRequest {
        req(vec![ChatMessage::user("credit card: 4111 1111 1111 1112")])
    }

    #[tokio::test]
    async fn guarded_router_blocks_external_on_active_lane_path() {
        // The chat/tool-loop path: stream_chat on the router itself.
        let guard = test_guard();
        let router = ModelRouter::thin_heavy(TEST_NET_1, None, None, guard.clone());
        let err = router.stream_chat(&low_confidence_req(), &|_| {}).await.unwrap_err();
        assert_eq!(
            err.kind(),
            "guard-blocked",
            "guarded path must block before connect; offline would mean a connect was attempted"
        );
        assert_eq!(err.endpoint(), TEST_NET_1);
        assert_eq!(guard.blocked_count(), 1);
    }

    #[tokio::test]
    async fn guarded_router_blocks_external_on_lane_client_path() {
        // The distillation/nudge path: lane_client hands back the inner
        // per-lane client — which must already be the guarded one.
        let guard = test_guard();
        let router =
            ModelRouter::thin_heavy(TEST_NET_1, Some("thin-test".into()), None, guard.clone());
        let (_, client) = router.lane_client(THIN_LANE).unwrap();
        let err = client.stream_chat(&low_confidence_req(), &|_| {}).await.unwrap_err();
        assert_eq!(err.kind(), "guard-blocked");
        assert_eq!(guard.blocked_count(), 1);
        assert_eq!(
            guard.last_error().unwrap().kind(),
            "guard-blocked",
            "the shared GuardState must have recorded the lane_client block"
        );
    }

    #[tokio::test]
    async fn guarded_router_blocks_attachments_after_runtime_repin() {
        // set_lane_model rebuilds the lane client — the rebuilt client must
        // be guarded too, or a re-pin would reopen the pipe.
        let guard = test_guard();
        let router = ModelRouter::thin_heavy(TEST_NET_1, None, None, guard.clone());
        router.set_lane_model(THIN_LANE, Some("repinned".into())).unwrap();
        let request = req(vec![ChatMessage::user("look at this")
            .with_attachments(vec![crate::llm::Attachment { base64_png: "QUJD".into() }])]);
        let err = router.stream_chat(&request, &|_| {}).await.unwrap_err();
        assert_eq!(err.kind(), "guard-blocked");
        assert_eq!(guard.blocked_count(), 1);
    }

    #[tokio::test]
    async fn unguarded_client_against_test_net_returns_offline_not_guard_blocked() {
        // The kind difference is the proof: an unguarded client actually
        // attempts the connect (offline after the connect timeout), while the
        // guarded paths above return guard-blocked without ever connecting.
        let client = OpenAiClient::new(TEST_NET_1);
        let err = client.stream_chat(&low_confidence_req(), &|_| {}).await.unwrap_err();
        assert_eq!(err.kind(), "offline");
    }

    #[tokio::test]
    async fn guarded_router_loopback_wire_body_is_byte_identical() {
        // Loopback pass-through end-to-end through the mounted guard: a
        // secret-carrying message reaches the capture server unredacted,
        // exactly as today (the existing wire tests above prove shape; this
        // one pins content through the guard specifically).
        let parts = vec![sse_token("ok"), "data: [DONE]\n\n".to_string()];
        let (endpoint, captured) = spawn_capturing_server(chunked_200(&parts, true)).await;
        let router = ModelRouter::thin_heavy(&endpoint, None, None, test_guard());
        router
            .stream_chat(&req(vec![ChatMessage::user("password: hunter2")]), &|_| {})
            .await
            .unwrap();
        let body = captured_body_json(&captured);
        assert_eq!(body["messages"][0]["content"], "password: hunter2");
    }
}
