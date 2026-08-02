//! OpenAI-compatible streaming client for LM Studio.
//!
//! POSTs to `{endpoint}/v1/chat/completions` with `stream: true` and parses
//! the SSE response line-by-line, mapping every transport and protocol
//! failure onto the typed [`LlmError`] taxonomy (R006):
//!
//! - connection refused / timeout / HTTP 5xx before any token → `Offline`
//! - HTTP 4xx (LM Studio with no model loaded) → `NoModel`
//! - HTTP 4xx on a tools-carrying request whose body names tools →
//!   `ToolsUnsupported` (the model rejects tool calling; S03)
//! - any failure after tokens started (drop, malformed SSE) → `Interrupted`,
//!   carrying the partial text streamed so far
//!
//! Streamed `delta.tool_calls` fragments are accumulated by index (id/name
//! arrive on the first delta, arguments may split across many) into complete
//! [`ToolCall`]s on [`StreamOutcome`] for the S03 dispatch loop.
//!
//! A clean end-of-stream without a `[DONE]` marker is a successful
//! completion, not an error. A client optionally pins one model id
//! ([`OpenAiClient::with_model`]): when set, requests carry `"model"` so a
//! multi-model LM Studio instance routes to it; when unset the key is
//! omitted and single-model deployments use whatever is loaded (S03 lanes
//! build one pinned client per model).

use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures_util::StreamExt;

use super::{ChatRequest, LlmClient, LlmError, LlmHealth, ReasoningSink, StreamOutcome, TokenSink};

/// The single definition site for the default LM Studio endpoint — the
/// fallback when neither the persisted settings override (`llmEndpoint`,
/// S05) nor the `THIRD_EYE_ENDPOINT` env var names one.
pub const DEFAULT_ENDPOINT: &str = "http://localhost:1234";

/// Failing fast on an unreachable endpoint is what turns "offline" into a
/// banner instead of a hang.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
/// Health probes are fired repeatedly by the UI backoff loop; keep them cheap.
const HEALTH_TIMEOUT: Duration = Duration::from_secs(2);
/// Model listing backs the settings pickers — bounded so a stalled endpoint
/// shows an offline state instead of a spinner that never resolves.
const LIST_MODELS_TIMEOUT: Duration = Duration::from_secs(5);

pub struct OpenAiClient {
    endpoint: String,
    /// `Some` pins requests to one loaded model (S03 lane routing); `None`
    /// omits the `model` key entirely (single-model fallback).
    model: Option<String>,
    /// `Some` attaches `Authorization: Bearer` to every request (M004 cloud
    /// providers). Never logged, never in error details; the struct
    /// deliberately has no Debug derive so the key has no leak surface.
    api_key: Option<String>,
    http: reqwest::Client,
}

impl OpenAiClient {
    pub fn new(endpoint: impl Into<String>) -> Self {
        let endpoint = endpoint.into().trim_end_matches('/').to_string();
        let http = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .build()
            .expect("reqwest client construction cannot fail with static config");
        Self {
            endpoint,
            model: None,
            api_key: None,
            http,
        }
    }

    /// Pin this client to one model id. LM Studio validates the id only when
    /// ≥2 models are loaded (an unknown id then returns 400 → `NoModel`);
    /// single-model instances ignore the field.
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    /// The pinned model id, if any (surfaced by the S03 router's model_info).
    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    /// Attach an API key: every request carries `Authorization: Bearer`.
    /// The cloud construction path (M004 `cloud/client.rs`) is the only
    /// production caller; the loopback LM Studio lanes stay keyless.
    pub fn with_api_key(mut self, api_key: impl Into<String>) -> Self {
        self.api_key = Some(api_key.into());
        self
    }

    /// Replace the transport client. The cloud construction path injects a
    /// reqwest client carrying TLS trust/resolve tweaks built with the same
    /// connect timeout; nothing else should call this.
    pub fn with_http_client(mut self, http: reqwest::Client) -> Self {
        self.http = http;
        self
    }

    /// Client against the project-default LM Studio endpoint.
    pub fn default_endpoint() -> Self {
        Self::new(DEFAULT_ENDPOINT)
    }

    /// Apply bearer auth exactly when a key is configured.
    fn authorize(&self, rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.api_key {
            Some(key) => rb.bearer_auth(key),
            None => rb,
        }
    }

    /// The model ids the endpoint actually serves: GET `{endpoint}/v1/models`
    /// (the same route the health probe hits), parsed from the OpenAI list
    /// shape `{"data":[{"id":...}]}`. Every failure — transport, HTTP status,
    /// malformed body — maps to [`LlmError::Offline`] with a detail naming
    /// the cause: for the settings pickers, "can't list models" and
    /// "endpoint down" are the same actionable state (S07).
    pub async fn list_models(&self) -> Result<Vec<String>, LlmError> {
        let url = format!("{}/v1/models", self.endpoint);
        let resp = self
            .authorize(self.http.get(&url))
            .timeout(LIST_MODELS_TIMEOUT)
            .send()
            .await
            .map_err(|e| self.offline(e.to_string()))?;
        let status = resp.status();
        let body = resp.text().await.map_err(|e| self.offline(e.to_string()))?;
        if !status.is_success() {
            return Err(self.offline(format!("HTTP {status}: {}", snippet(&body))));
        }
        let value: serde_json::Value = serde_json::from_str(&body).map_err(|e| {
            self.offline(format!(
                "malformed /v1/models response ({e}): {}",
                snippet(&body)
            ))
        })?;
        let data = value["data"].as_array().ok_or_else(|| {
            self.offline(format!(
                "/v1/models response has no data array: {}",
                snippet(&body)
            ))
        })?;
        Ok(data
            .iter()
            .filter_map(|m| m["id"].as_str().map(str::to_owned))
            .collect())
    }

    fn offline(&self, detail: impl Into<String>) -> LlmError {
        LlmError::Offline {
            endpoint: self.endpoint.clone(),
            detail: detail.into(),
        }
    }
}

impl OpenAiClient {
    /// The single SSE-streaming core behind both [`LlmClient::stream_chat`] and
    /// [`LlmClient::stream_chat_reasoning`]. `on_reasoning` is `Some` only on the
    /// reasoning path (the `chat` command); when `None`, reasoning deltas are
    /// still parsed off the wire (so they never leak into the answer text) but
    /// dropped rather than forwarded. Reasoning never enters [`StreamOutcome`].
    async fn stream_chat_core(
        &self,
        request: &ChatRequest,
        on_token: TokenSink<'_>,
        on_reasoning: Option<ReasoningSink<'_>>,
    ) -> Result<StreamOutcome, LlmError> {
        let url = format!("{}/v1/chat/completions", self.endpoint);
        let mut body = serde_json::json!({
            "messages": request.messages,
            "stream": true,
            // Real token accounting (2026-08-03): the server appends a
            // final usage chunk; servers that ignore the option just
            // stream as before.
            "stream_options": { "include_usage": true },
        });
        if let Some(model) = &self.model {
            body["model"] = serde_json::json!(model);
        }
        // The tools key is present exactly when tools are: a tools-free
        // request stays byte-for-byte the pre-S03 wire shape.
        if !request.tools.is_empty() {
            body["tools"] = serde_json::json!(request.tools);
        }
        let started = Instant::now();
        log::debug!(
            "llm: request start endpoint={} model={} messages={} tools={}",
            self.endpoint,
            self.model.as_deref().unwrap_or("default"),
            request.messages.len(),
            request.tools.len()
        );

        let resp = self
            .authorize(self.http.post(&url))
            .json(&body)
            .send()
            .await
            .map_err(|e| self.offline(e.to_string()))?;

        let status = resp.status();
        if !status.is_success() {
            let body_text = resp.text().await.unwrap_or_default();
            let detail = format!("HTTP {status}: {}", snippet(&body_text));
            return Err(if status.is_client_error() {
                // A 4xx on a tools-carrying request whose body names tools —
                // or whose chat TEMPLATE failed to render (gemma builds throw
                // "Error rendering prompt with jinja template" the moment a
                // tools array is present) — is the model rejecting tool
                // calling: a distinct typed state, not the misleading "no
                // model loaded".
                let body_lower = body_text.to_ascii_lowercase();
                if !request.tools.is_empty()
                    && (body_lower.contains("tool")
                        || body_lower.contains("jinja")
                        || body_lower.contains("template"))
                {
                    log::error!(
                        "llm: tools-unsupported endpoint={} model={}: {detail}",
                        self.endpoint,
                        self.model.as_deref().unwrap_or("default")
                    );
                    LlmError::ToolsUnsupported {
                        endpoint: self.endpoint.clone(),
                        detail,
                    }
                } else {
                    LlmError::NoModel {
                        endpoint: self.endpoint.clone(),
                        detail,
                    }
                }
            } else {
                self.offline(detail)
            });
        }

        let mut stream = resp.bytes_stream();
        let mut buf: Vec<u8> = Vec::new();
        let mut text = String::new();
        let mut token_count = 0usize;
        let mut first_token_at: Option<Instant> = None;
        // Tool calls under reassembly, keyed by delta index (BTreeMap keeps
        // the model's call order on finalization).
        let mut tool_acc: std::collections::BTreeMap<usize, PartialToolCall> =
            std::collections::BTreeMap::new();
        let mut usage: Option<(u64, u64)> = None;

        let mut handle_line =
            |line: &str,
             text: &mut String,
             token_count: &mut usize,
             tool_acc: &mut std::collections::BTreeMap<usize, PartialToolCall>| {
                match parse_sse_line(line) {
                    Ok(SseLine::Token(token)) => {
                        if first_token_at.is_none() {
                            first_token_at = Some(Instant::now());
                            log::debug!(
                                "llm: first token after {:.0} ms",
                                started.elapsed().as_secs_f64() * 1000.0
                            );
                        }
                        text.push_str(&token);
                        *token_count += 1;
                        on_token(&token);
                        Ok(false)
                    }
                    Ok(SseLine::Reasoning(chunk)) => {
                        // A distinct stream: forwarded to the Thinking… surface when a
                        // sink is wired, never appended to `text` (the answer stays
                        // reasoning-free) and never counted as an answer token.
                        if let Some(sink) = on_reasoning {
                            sink(&chunk);
                        }
                        Ok(false)
                    }
                    Ok(SseLine::ToolCalls(deltas)) => {
                        for delta in deltas {
                            tool_acc.entry(delta.index).or_default().absorb(delta);
                        }
                        Ok(false)
                    }
                    Ok(SseLine::Usage { prompt, completion }) => {
                        usage = Some((prompt, completion));
                        Ok(false)
                    }
                    Ok(SseLine::Done) => Ok(true),
                    Ok(SseLine::Skip) => Ok(false),
                    Err(detail) => Err(LlmError::Interrupted {
                        endpoint: self.endpoint.clone(),
                        partial_text: text.clone(),
                        detail,
                    }),
                }
            };

        'stream: while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| LlmError::Interrupted {
                endpoint: self.endpoint.clone(),
                partial_text: text.clone(),
                detail: e.to_string(),
            })?;
            buf.extend_from_slice(&chunk);
            // Split on complete lines only: a TCP chunk may end mid-line (or
            // mid-multibyte-char), so bytes stay buffered until a newline.
            while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = buf.drain(..=pos).collect();
                let line = String::from_utf8_lossy(&line);
                if handle_line(
                    line.trim_end_matches(['\n', '\r']),
                    &mut text,
                    &mut token_count,
                    &mut tool_acc,
                )? {
                    break 'stream;
                }
            }
        }
        // A final line without a trailing newline is still data (clean EOF
        // without [DONE] is a valid successful completion).
        if !buf.is_empty() {
            let line = String::from_utf8_lossy(&buf).to_string();
            handle_line(
                line.trim_end_matches(['\n', '\r']),
                &mut text,
                &mut token_count,
                &mut tool_acc,
            )?;
        }

        let tool_calls = tool_acc
            .into_iter()
            .map(|(index, partial)| {
                partial
                    .finish(index)
                    .map_err(|detail| LlmError::Interrupted {
                        endpoint: self.endpoint.clone(),
                        partial_text: text.clone(),
                        detail,
                    })
            })
            .collect::<Result<Vec<_>, _>>()?;

        log::debug!(
            "llm: stream done: {} tokens, {} chars in {:.0} ms",
            token_count,
            text.len(),
            started.elapsed().as_secs_f64() * 1000.0
        );
        if !tool_calls.is_empty() {
            let names: Vec<&str> = tool_calls.iter().map(|c| c.name.as_str()).collect();
            log::info!(
                "llm: tool calls requested: {} [{}] endpoint={}",
                tool_calls.len(),
                names.join(", "),
                self.endpoint
            );
        }
        Ok(StreamOutcome {
            text,
            token_count,
            tool_calls,
            prompt_tokens: usage.map(|(p, _)| p),
            completion_tokens: usage.map(|(_, c)| c),
        })
    }
}

#[async_trait]
impl LlmClient for OpenAiClient {
    fn endpoint(&self) -> &str {
        &self.endpoint
    }

    fn model_id(&self) -> Option<&str> {
        self.model.as_deref()
    }

    async fn stream_chat(
        &self,
        request: &ChatRequest,
        on_token: TokenSink<'_>,
    ) -> Result<StreamOutcome, LlmError> {
        self.stream_chat_core(request, on_token, None).await
    }

    async fn stream_chat_reasoning(
        &self,
        request: &ChatRequest,
        on_token: TokenSink<'_>,
        on_reasoning: ReasoningSink<'_>,
    ) -> Result<StreamOutcome, LlmError> {
        self.stream_chat_core(request, on_token, Some(on_reasoning))
            .await
    }

    async fn health(&self) -> LlmHealth {
        let url = format!("{}/v1/models", self.endpoint);
        let online = match self
            .authorize(self.http.get(&url))
            .timeout(HEALTH_TIMEOUT)
            .send()
            .await
        {
            Ok(resp) => resp.status().is_success(),
            Err(e) => {
                log::debug!("llm: health probe failed for {}: {e}", self.endpoint);
                false
            }
        };
        LlmHealth {
            online,
            endpoint: self.endpoint.clone(),
        }
    }
}

/// One parsed SSE line.
#[derive(Debug)]
enum SseLine {
    /// A content delta to append and forward.
    Token(String),
    /// A reasoning delta (`delta.reasoning_content` / `delta.reasoning`) — a
    /// thinking-model's chain-of-thought, forwarded to the Thinking… surface
    /// but never appended to the answer text.
    Reasoning(String),
    /// Streamed `delta.tool_calls` fragments to accumulate by index.
    ToolCalls(Vec<ToolCallDelta>),
    /// The final usage chunk (`stream_options.include_usage`, 2026-08-03):
    /// the request's REAL prompt/completion token cost.
    Usage { prompt: u64, completion: u64 },
    /// The `data: [DONE]` terminator.
    Done,
    /// Anything to ignore: blank lines, comments, role-change deltas,
    /// null/empty content.
    Skip,
}

/// One entry of a streamed `choices[0].delta.tool_calls` array. The id and
/// function name arrive on the first delta for an index; `arguments` may
/// arrive as string fragments across several deltas — accumulation by
/// `index` reassembles them (one-complete-call-per-line is NOT guaranteed).
#[derive(Debug)]
struct ToolCallDelta {
    index: usize,
    id: Option<String>,
    name: Option<String>,
    arguments: Option<String>,
}

/// A tool call being reassembled from deltas sharing one index.
#[derive(Debug, Default)]
struct PartialToolCall {
    id: Option<String>,
    name: Option<String>,
    arguments: String,
}

impl PartialToolCall {
    fn absorb(&mut self, delta: ToolCallDelta) {
        if let Some(id) = delta.id {
            self.id.get_or_insert(id);
        }
        if let Some(name) = delta.name {
            self.name.get_or_insert(name);
        }
        if let Some(fragment) = delta.arguments {
            self.arguments.push_str(&fragment);
        }
    }

    /// Finish reassembly. A call without a function name is undispatchable —
    /// the stream is corrupt. A missing id gets a synthesized `call_{index}`:
    /// the id only has to be self-consistent across our own echo/result
    /// round-trip, and some OpenAI-compatible servers omit it.
    fn finish(self, index: usize) -> Result<super::ToolCall, String> {
        let Some(name) = self.name else {
            return Err(format!(
                "tool call at index {index} streamed without a function name"
            ));
        };
        Ok(super::ToolCall {
            id: self.id.unwrap_or_else(|| format!("call_{index}")),
            name,
            arguments: self.arguments,
        })
    }
}

/// Parse a single SSE line from an OpenAI-compatible stream. `Err` carries a
/// human-readable detail for the `Interrupted` error (malformed JSON on a
/// `data:` line means the stream is corrupt — we stop rather than guess).
fn parse_sse_line(line: &str) -> Result<SseLine, String> {
    let Some(data) = line.strip_prefix("data:") else {
        // Blank keep-alive lines, `event:`/`id:` fields, `:` comments.
        return Ok(SseLine::Skip);
    };
    let data = data.trim();
    if data == "[DONE]" {
        return Ok(SseLine::Done);
    }
    if data.is_empty() {
        return Ok(SseLine::Skip);
    }
    let value: serde_json::Value = serde_json::from_str(data)
        .map_err(|e| format!("malformed SSE data line ({e}): {}", snippet(data)))?;
    // The include_usage final chunk: usage present (choices typically empty).
    if let Some(usage) = value.get("usage").filter(|u| u.is_object()) {
        if let (Some(prompt), Some(completion)) = (
            usage.get("prompt_tokens").and_then(|t| t.as_u64()),
            usage.get("completion_tokens").and_then(|t| t.as_u64()),
        ) {
            return Ok(SseLine::Usage { prompt, completion });
        }
    }
    let delta = &value["choices"][0]["delta"];
    if let Some(content) = delta["content"].as_str() {
        if !content.is_empty() {
            return Ok(SseLine::Token(content.to_string()));
        }
    }
    // Thinking models stream their chain-of-thought in a separate field while
    // `content` is null/empty (which is why the answer pane otherwise fills with
    // blank newlines). LM Studio / DeepSeek use `reasoning_content`; some
    // OpenAI-compatible servers use `reasoning`. Either one, non-empty, is a
    // reasoning delta — kept OUT of the answer text.
    for key in ["reasoning_content", "reasoning"] {
        if let Some(reasoning) = delta[key].as_str() {
            if !reasoning.is_empty() {
                return Ok(SseLine::Reasoning(reasoning.to_string()));
            }
        }
    }
    // Tool-call rounds stream content as null with tool_calls fragments.
    if let Some(calls) = delta["tool_calls"].as_array() {
        let deltas: Vec<ToolCallDelta> = calls
            .iter()
            .map(|c| ToolCallDelta {
                // A missing index (single-call servers) means index 0.
                index: c["index"].as_u64().unwrap_or(0) as usize,
                id: c["id"].as_str().map(str::to_owned),
                name: c["function"]["name"].as_str().map(str::to_owned),
                arguments: c["function"]["arguments"].as_str().map(str::to_owned),
            })
            .collect();
        if !deltas.is_empty() {
            return Ok(SseLine::ToolCalls(deltas));
        }
    }
    // Role-change deltas, null content, finish_reason-only events.
    Ok(SseLine::Skip)
}

/// Bounded excerpt of a response body for error details — enough to name the
/// cause without dumping payloads into logs.
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

/// HTTP mock helpers shared with the S03 router tests (`llm/router.rs`):
/// pre-baked raw responses over a real TcpListener, optionally capturing the
/// request bytes so tests can assert the outbound JSON (the `model` field
/// contract).
#[cfg(test)]
pub(crate) mod test_support {
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// Serve one connection with a raw pre-baked HTTP response, then close.
    /// Chunked responses that omit the terminal `0\r\n\r\n` chunk simulate a
    /// mid-stream connection drop (reqwest reports an incomplete message).
    pub(crate) async fn spawn_raw_server(response: Vec<u8>) -> String {
        let (endpoint, _captured) = spawn_capturing_server(response).await;
        endpoint
    }

    /// Like [`spawn_raw_server`], but also exposes the raw request bytes so
    /// tests can assert what went over the wire. The request is fully read
    /// (headers + content-length body) before the response is sent, so once
    /// the client has seen the response the capture is complete.
    pub(crate) async fn spawn_capturing_server(response: Vec<u8>) -> (String, Arc<Mutex<Vec<u8>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let captured = Arc::new(Mutex::new(Vec::new()));
        let cap = captured.clone();
        tokio::spawn(async move {
            if let Ok((mut sock, _)) = listener.accept().await {
                let mut buf: Vec<u8> = Vec::new();
                let mut tmp = [0u8; 4096];
                while !request_complete(&buf) {
                    match sock.read(&mut tmp).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => buf.extend_from_slice(&tmp[..n]),
                    }
                }
                *cap.lock().unwrap() = buf;
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

    /// The JSON body of a captured request (panics if none was captured).
    pub(crate) fn captured_body_json(captured: &Arc<Mutex<Vec<u8>>>) -> serde_json::Value {
        let raw = captured.lock().unwrap().clone();
        let text = String::from_utf8_lossy(&raw);
        let body = text
            .split("\r\n\r\n")
            .nth(1)
            .expect("captured request has no body");
        serde_json::from_str(body).expect("captured request body is not JSON")
    }

    /// A port with nothing listening — bind, read the address, drop.
    pub(crate) async fn refused_endpoint() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        format!("http://{addr}")
    }

    pub(crate) fn sse_token(token: &str) -> String {
        format!(
            "data: {}\n\n",
            serde_json::json!({"choices": [{"delta": {"content": token}}]})
        )
    }

    /// One streamed `delta.tool_calls` SSE event with a single entry, in the
    /// OpenAI shape: id/name on the first delta for an index, `arguments`
    /// fragments on follow-ups. Omitted fields are absent from the JSON.
    pub(crate) fn sse_tool_delta(
        index: u64,
        id: Option<&str>,
        name: Option<&str>,
        arguments: Option<&str>,
    ) -> String {
        let mut call = serde_json::json!({ "index": index });
        if let Some(id) = id {
            call["id"] = id.into();
        }
        let mut function = serde_json::Map::new();
        if let Some(name) = name {
            function.insert("name".into(), name.into());
        }
        if let Some(args) = arguments {
            function.insert("arguments".into(), args.into());
        }
        if !function.is_empty() {
            call["function"] = function.into();
        }
        format!(
            "data: {}\n\n",
            serde_json::json!({"choices": [{"delta": {"content": null, "tool_calls": [call]}}]})
        )
    }

    /// HTTP/1.1 200 with chunked transfer encoding. `terminated` controls
    /// whether the terminal 0-chunk is sent (false = mid-stream drop).
    pub(crate) fn chunked_200(parts: &[String], terminated: bool) -> Vec<u8> {
        let mut resp = String::from(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n\r\n",
        );
        for part in parts {
            resp.push_str(&format!("{:x}\r\n{part}\r\n", part.len()));
        }
        if terminated {
            resp.push_str("0\r\n\r\n");
        }
        resp.into_bytes()
    }

    pub(crate) fn plain_response(status_line: &str, body: &str) -> Vec<u8> {
        format!(
            "HTTP/1.1 {status_line}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;
    use crate::llm::{ChatMessage, ToolDefinition};
    use std::sync::Mutex;

    fn req(messages: Vec<ChatMessage>) -> ChatRequest {
        ChatRequest::new(messages)
    }

    async fn run_chat(endpoint: &str) -> (Result<StreamOutcome, LlmError>, Vec<String>) {
        let client = OpenAiClient::new(endpoint);
        let seen = Mutex::new(Vec::new());
        let result = client
            .stream_chat(&req(vec![ChatMessage::user("hi")]), &|t| {
                seen.lock().unwrap().push(t.to_string())
            })
            .await;
        (result, seen.into_inner().unwrap())
    }

    #[tokio::test]
    async fn streams_tokens_and_accumulates_text() {
        let parts = vec![
            sse_token("Hel"),
            sse_token("lo"),
            "data: [DONE]\n\n".to_string(),
        ];
        let endpoint = spawn_raw_server(chunked_200(&parts, true)).await;
        let (result, seen) = run_chat(&endpoint).await;
        let outcome = result.unwrap();
        assert_eq!(outcome.text, "Hello");
        assert_eq!(outcome.token_count, 2);
        assert_eq!(seen, vec!["Hel", "lo"]);
    }

    #[tokio::test]
    async fn clean_eof_without_done_marker_is_success() {
        let parts = vec![sse_token("fin")];
        let endpoint = spawn_raw_server(chunked_200(&parts, true)).await;
        let (result, _) = run_chat(&endpoint).await;
        assert_eq!(result.unwrap().text, "fin");
    }

    #[tokio::test]
    async fn data_line_split_across_tcp_chunks_is_reassembled() {
        // One SSE event (with a multibyte char) cut mid-line across two
        // transfer chunks — the byte buffer must stitch it back together.
        let event = sse_token("héllo");
        let (a, b) = event.split_at(20);
        let endpoint = spawn_raw_server(chunked_200(&[a.to_string(), b.to_string()], true)).await;
        let (result, seen) = run_chat(&endpoint).await;
        assert_eq!(result.unwrap().text, "héllo");
        assert_eq!(seen, vec!["héllo"]);
    }

    #[tokio::test]
    async fn null_and_missing_deltas_are_skipped() {
        let parts = vec![
            "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\n\n".to_string(),
            "data: {\"choices\":[{\"delta\":{\"content\":null}}]}\n\n".to_string(),
            sse_token("real"),
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n".to_string(),
            "data: [DONE]\n\n".to_string(),
        ];
        let endpoint = spawn_raw_server(chunked_200(&parts, true)).await;
        let (result, seen) = run_chat(&endpoint).await;
        let outcome = result.unwrap();
        assert_eq!(outcome.text, "real");
        assert_eq!(outcome.token_count, 1);
        assert_eq!(seen, vec!["real"]);
    }

    #[tokio::test]
    async fn connection_refused_maps_to_offline_naming_endpoint() {
        let endpoint = refused_endpoint().await;
        let (result, seen) = run_chat(&endpoint).await;
        let err = result.unwrap_err();
        assert_eq!(err.kind(), "offline");
        assert_eq!(err.endpoint(), endpoint);
        assert!(
            err.to_string().contains(&endpoint),
            "display must name endpoint: {err}"
        );
        assert!(seen.is_empty());
    }

    #[tokio::test]
    async fn http_400_maps_to_no_model_with_body_detail() {
        let body = r#"{"error":"No model loaded"}"#;
        let endpoint = spawn_raw_server(plain_response("400 Bad Request", body)).await;
        let (result, _) = run_chat(&endpoint).await;
        let err = result.unwrap_err();
        assert_eq!(err.kind(), "no-model");
        assert!(
            matches!(&err, LlmError::NoModel { detail, .. } if detail.contains("No model loaded"))
        );
    }

    #[tokio::test]
    async fn http_5xx_before_tokens_maps_to_offline() {
        let endpoint = spawn_raw_server(plain_response("503 Service Unavailable", "busy")).await;
        let (result, _) = run_chat(&endpoint).await;
        assert_eq!(result.unwrap_err().kind(), "offline");
    }

    #[tokio::test]
    async fn mid_stream_drop_preserves_partial_text() {
        // Unterminated chunked body: connection dies after two tokens.
        let parts = vec![sse_token("par"), sse_token("tial")];
        let endpoint = spawn_raw_server(chunked_200(&parts, false)).await;
        let (result, seen) = run_chat(&endpoint).await;
        let err = result.unwrap_err();
        assert_eq!(err.kind(), "interrupted");
        assert_eq!(err.partial_text(), Some("partial"));
        assert_eq!(
            seen,
            vec!["par", "tial"],
            "tokens before the drop must have been delivered"
        );
    }

    #[tokio::test]
    async fn malformed_sse_json_interrupts_and_preserves_partial() {
        let parts = vec![sse_token("good"), "data: {broken\n\n".to_string()];
        let endpoint = spawn_raw_server(chunked_200(&parts, true)).await;
        let (result, _) = run_chat(&endpoint).await;
        let err = result.unwrap_err();
        assert_eq!(err.kind(), "interrupted");
        assert_eq!(err.partial_text(), Some("good"));
    }

    #[tokio::test]
    async fn health_reports_online_when_models_endpoint_responds() {
        let endpoint = spawn_raw_server(plain_response("200 OK", r#"{"data":[]}"#)).await;
        let health = OpenAiClient::new(&endpoint).health().await;
        assert!(health.online);
        assert_eq!(health.endpoint, endpoint);
    }

    #[tokio::test]
    async fn health_reports_offline_when_unreachable_never_errors() {
        let endpoint = refused_endpoint().await;
        let health = OpenAiClient::new(&endpoint).health().await;
        assert!(!health.online);
        assert_eq!(health.endpoint, endpoint);
    }

    #[test]
    fn parse_skips_non_data_lines_and_comments() {
        for line in ["", ": keep-alive", "event: message", "id: 7", "data:"] {
            assert!(
                matches!(parse_sse_line(line), Ok(SseLine::Skip)),
                "line: {line:?}"
            );
        }
    }

    #[test]
    fn parse_recognizes_done_marker() {
        assert!(matches!(parse_sse_line("data: [DONE]"), Ok(SseLine::Done)));
    }

    #[test]
    fn parse_recognizes_reasoning_content_and_reasoning_fields() {
        // Both field spellings map to Reasoning; content-carrying deltas still win.
        let rc = r#"data: {"choices":[{"delta":{"reasoning_content":"hmm"}}]}"#;
        assert!(matches!(parse_sse_line(rc), Ok(SseLine::Reasoning(s)) if s == "hmm"));
        let r = r#"data: {"choices":[{"delta":{"reasoning":"pondering"}}]}"#;
        assert!(matches!(parse_sse_line(r), Ok(SseLine::Reasoning(s)) if s == "pondering"));
        // An empty reasoning field is a Skip, not an empty Reasoning delta.
        let empty = r#"data: {"choices":[{"delta":{"reasoning_content":""}}]}"#;
        assert!(matches!(parse_sse_line(empty), Ok(SseLine::Skip)));
        // Content present alongside reasoning: content is the answer, wins.
        let both =
            r#"data: {"choices":[{"delta":{"content":"answer","reasoning_content":"think"}}]}"#;
        assert!(matches!(parse_sse_line(both), Ok(SseLine::Token(s)) if s == "answer"));
    }

    /// Build a reasoning-carrying SSE event (the LM Studio / DeepSeek shape:
    /// null content, a `reasoning_content` fragment).
    fn sse_reasoning(fragment: &str) -> String {
        format!(
            "data: {}\n\n",
            serde_json::json!({"choices": [{"delta": {"content": null, "reasoning_content": fragment}}]})
        )
    }

    #[tokio::test]
    async fn reasoning_deltas_forward_to_sink_and_stay_out_of_answer_text() {
        // A thinking model: two reasoning fragments, then the real answer. The
        // reasoning must reach on_reasoning verbatim and NEVER enter text or the
        // token count — the fix for the blank-newlines-in-the-answer symptom.
        let parts = vec![
            sse_reasoning("Let me "),
            sse_reasoning("consider.\n"),
            sse_token("Final answer"),
            "data: [DONE]\n\n".to_string(),
        ];
        let endpoint = spawn_raw_server(chunked_200(&parts, true)).await;
        let client = OpenAiClient::new(&endpoint);
        let tokens = Mutex::new(String::new());
        let reasoning = Mutex::new(String::new());
        let outcome = client
            .stream_chat_reasoning(
                &req(vec![ChatMessage::user("hi")]),
                &|t| tokens.lock().unwrap().push_str(t),
                &|r| reasoning.lock().unwrap().push_str(r),
            )
            .await
            .unwrap();

        assert_eq!(
            outcome.text, "Final answer",
            "reasoning must not leak into the answer"
        );
        assert_eq!(
            outcome.token_count, 1,
            "reasoning deltas are not answer tokens"
        );
        assert_eq!(*tokens.lock().unwrap(), "Final answer");
        assert_eq!(
            *reasoning.lock().unwrap(),
            "Let me consider.\n",
            "every reasoning fragment must reach the Thinking… sink in order"
        );
    }

    #[tokio::test]
    async fn stream_chat_without_reasoning_sink_still_drops_reasoning_from_text() {
        // The plain stream_chat path (ingest/nudge): a model that emits reasoning
        // must still keep it out of the answer, even with no sink wired.
        let parts = vec![
            sse_reasoning("thinking hard"),
            sse_token("done"),
            "data: [DONE]\n\n".to_string(),
        ];
        let endpoint = spawn_raw_server(chunked_200(&parts, true)).await;
        let (result, seen) = run_chat(&endpoint).await;
        let outcome = result.unwrap();
        assert_eq!(outcome.text, "done");
        assert_eq!(outcome.token_count, 1);
        assert_eq!(
            seen,
            vec!["done"],
            "reasoning is never delivered as a content token"
        );
    }

    #[test]
    fn parse_rejects_malformed_json_with_detail() {
        let err = parse_sse_line("data: {nope").unwrap_err();
        assert!(err.contains("malformed"), "detail: {err}");
    }

    fn tools() -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: "memory_search".into(),
            description: "Search stored memories".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": { "query": { "type": "string" } },
                "required": ["query"]
            }),
        }]
    }

    async fn run_chat_with_tools(endpoint: &str) -> Result<StreamOutcome, LlmError> {
        let client = OpenAiClient::new(endpoint);
        let request = req(vec![ChatMessage::user("hi")]).with_tools(tools());
        client.stream_chat(&request, &|_| {}).await
    }

    #[tokio::test]
    async fn tool_call_arguments_accumulate_across_deltas() {
        // The unsafe-assumption case from the plan: id/name on the first
        // delta, arguments split into fragments across several deltas.
        let parts = vec![
            sse_tool_delta(0, Some("call_abc"), Some("memory_search"), None),
            sse_tool_delta(0, None, None, Some(r#"{"query""#)),
            sse_tool_delta(0, None, None, Some(r#":"morning"#)),
            sse_tool_delta(0, None, None, Some(r#" work"}"#)),
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n".to_string(),
            "data: [DONE]\n\n".to_string(),
        ];
        let endpoint = spawn_raw_server(chunked_200(&parts, true)).await;
        let outcome = run_chat_with_tools(&endpoint).await.unwrap();
        assert_eq!(outcome.text, "");
        assert_eq!(outcome.tool_calls.len(), 1);
        let call = &outcome.tool_calls[0];
        assert_eq!(call.id, "call_abc");
        assert_eq!(call.name, "memory_search");
        assert_eq!(call.arguments, r#"{"query":"morning work"}"#);
    }

    #[tokio::test]
    async fn parallel_tool_calls_accumulate_by_index_in_order() {
        // Interleaved deltas for two calls must reassemble independently and
        // come back in index order.
        let parts = vec![
            sse_tool_delta(
                0,
                Some("call_a"),
                Some("memory_search"),
                Some(r#"{"query":"#),
            ),
            sse_tool_delta(
                1,
                Some("call_b"),
                Some("memory_search"),
                Some(r#"{"query":"#),
            ),
            sse_tool_delta(0, None, None, Some(r#""first"}"#)),
            sse_tool_delta(1, None, None, Some(r#""second"}"#)),
            "data: [DONE]\n\n".to_string(),
        ];
        let endpoint = spawn_raw_server(chunked_200(&parts, true)).await;
        let outcome = run_chat_with_tools(&endpoint).await.unwrap();
        assert_eq!(outcome.tool_calls.len(), 2);
        assert_eq!(outcome.tool_calls[0].id, "call_a");
        assert_eq!(outcome.tool_calls[0].arguments, r#"{"query":"first"}"#);
        assert_eq!(outcome.tool_calls[1].id, "call_b");
        assert_eq!(outcome.tool_calls[1].arguments, r#"{"query":"second"}"#);
    }

    #[tokio::test]
    async fn tool_call_without_id_synthesizes_index_based_id() {
        // Some OpenAI-compatible servers omit the id; the synthesized id only
        // has to be self-consistent across our echo/result round-trip.
        let parts = vec![
            sse_tool_delta(0, None, Some("memory_search"), Some("{}")),
            "data: [DONE]\n\n".to_string(),
        ];
        let endpoint = spawn_raw_server(chunked_200(&parts, true)).await;
        let outcome = run_chat_with_tools(&endpoint).await.unwrap();
        assert_eq!(outcome.tool_calls[0].id, "call_0");
        assert_eq!(outcome.tool_calls[0].name, "memory_search");
    }

    #[tokio::test]
    async fn tool_call_without_name_interrupts_with_detail() {
        // A call we could never dispatch is a corrupt stream, not a guess.
        let parts = vec![
            sse_tool_delta(0, Some("call_x"), None, Some(r#"{"query":"x"}"#)),
            "data: [DONE]\n\n".to_string(),
        ];
        let endpoint = spawn_raw_server(chunked_200(&parts, true)).await;
        let err = run_chat_with_tools(&endpoint).await.unwrap_err();
        assert_eq!(err.kind(), "interrupted");
        assert!(
            err.to_string().contains("function name"),
            "detail must name the cause: {err}"
        );
    }

    #[tokio::test]
    async fn content_tokens_and_tool_calls_coexist_in_outcome() {
        // Some models narrate before calling: both surfaces must survive.
        let parts = vec![
            sse_token("Let me check. "),
            sse_tool_delta(
                0,
                Some("call_1"),
                Some("memory_search"),
                Some(r#"{"query":"x"}"#),
            ),
            "data: [DONE]\n\n".to_string(),
        ];
        let endpoint = spawn_raw_server(chunked_200(&parts, true)).await;
        let outcome = run_chat_with_tools(&endpoint).await.unwrap();
        assert_eq!(outcome.text, "Let me check. ");
        assert_eq!(outcome.token_count, 1);
        assert_eq!(outcome.tool_calls.len(), 1);
    }

    #[tokio::test]
    async fn usage_chunk_lands_on_the_outcome_and_requests_opt_in() {
        // The include_usage final chunk (empty choices) → real spend on the
        // outcome; and every streaming request asks for it.
        let usage_chunk = format!(
            "data: {}\n\n",
            serde_json::json!({"choices": [], "usage": {
                "prompt_tokens": 6377, "completion_tokens": 256, "total_tokens": 6633
            }})
        );
        let parts = vec![sse_token("hi"), usage_chunk, "data: [DONE]\n\n".to_string()];
        let (endpoint, captured) = spawn_capturing_server(chunked_200(&parts, true)).await;
        let (result, _) = run_chat(&endpoint).await;
        let outcome = result.unwrap();
        assert_eq!(outcome.prompt_tokens, Some(6377));
        assert_eq!(outcome.completion_tokens, Some(256));
        let body = String::from_utf8_lossy(&captured.lock().unwrap()).to_string();
        assert!(
            body.contains("include_usage"),
            "requests must opt into usage reporting: {body}"
        );
    }

    #[tokio::test]
    async fn plain_chat_outcome_has_no_tool_calls() {
        let parts = vec![sse_token("hi"), "data: [DONE]\n\n".to_string()];
        let endpoint = spawn_raw_server(chunked_200(&parts, true)).await;
        let (result, _) = run_chat(&endpoint).await;
        assert!(result.unwrap().tool_calls.is_empty());
    }

    #[tokio::test]
    async fn http_400_naming_tools_on_tools_request_maps_to_tools_unsupported() {
        let body = r#"{"error":"This model does not support tool use."}"#;
        let endpoint = spawn_raw_server(plain_response("400 Bad Request", body)).await;
        let err = run_chat_with_tools(&endpoint).await.unwrap_err();
        assert_eq!(err.kind(), "tools-unsupported");
        assert_eq!(err.endpoint(), endpoint);
        assert!(
            matches!(&err, LlmError::ToolsUnsupported { detail, .. } if detail.contains("tool use"))
        );
    }

    #[tokio::test]
    async fn http_400_template_error_on_tools_request_maps_to_tools_unsupported() {
        // The gemma signature: the chat template itself fails to render the
        // tools array. The user must see "this model can't use tools", not
        // "no model loaded" (a model IS loaded — it just can't do this).
        let body = r#"{"error":"Error rendering prompt with jinja template: \"Unknown test: sequence\"."}"#;
        let endpoint = spawn_raw_server(plain_response("400 Bad Request", body)).await;
        let err = run_chat_with_tools(&endpoint).await.unwrap_err();
        assert_eq!(err.kind(), "tools-unsupported");
    }

    #[tokio::test]
    async fn http_400_with_unrelated_body_on_tools_request_stays_no_model() {
        let body = r#"{"error":"No model loaded"}"#;
        let endpoint = spawn_raw_server(plain_response("400 Bad Request", body)).await;
        let err = run_chat_with_tools(&endpoint).await.unwrap_err();
        assert_eq!(err.kind(), "no-model");
    }

    #[tokio::test]
    async fn http_400_naming_tools_without_tools_in_request_stays_no_model() {
        // The classification requires a tools-carrying request: a plain
        // request can never be tools-unsupported.
        let body = r#"{"error":"tool something"}"#;
        let endpoint = spawn_raw_server(plain_response("400 Bad Request", body)).await;
        let (result, _) = run_chat(&endpoint).await;
        assert_eq!(result.unwrap_err().kind(), "no-model");
    }

    #[test]
    fn parse_empty_tool_calls_array_is_skip() {
        let line = r#"data: {"choices":[{"delta":{"content":null,"tool_calls":[]}}]}"#;
        assert!(matches!(parse_sse_line(line), Ok(SseLine::Skip)));
    }

    #[tokio::test]
    async fn request_json_carries_model_when_pinned() {
        let parts = vec![sse_token("ok"), "data: [DONE]\n\n".to_string()];
        let (endpoint, captured) = spawn_capturing_server(chunked_200(&parts, true)).await;
        let client = OpenAiClient::new(&endpoint).with_model("thin-model-1b");
        client
            .stream_chat(&req(vec![ChatMessage::user("hi")]), &|_| {})
            .await
            .unwrap();
        let body = captured_body_json(&captured);
        assert_eq!(body["model"], "thin-model-1b");
        assert_eq!(body["stream"], true);
        assert_eq!(body["messages"][0]["content"], "hi");
    }

    #[tokio::test]
    async fn request_json_omits_model_key_when_unpinned() {
        // Single-model fallback: no "model" key at all, so LM Studio serves
        // whatever is loaded.
        let parts = vec![sse_token("ok"), "data: [DONE]\n\n".to_string()];
        let (endpoint, captured) = spawn_capturing_server(chunked_200(&parts, true)).await;
        let client = OpenAiClient::new(&endpoint);
        client
            .stream_chat(&req(vec![ChatMessage::user("hi")]), &|_| {})
            .await
            .unwrap();
        let body = captured_body_json(&captured);
        assert!(
            !body.as_object().unwrap().contains_key("model"),
            "unpinned client must omit the model key entirely: {body}"
        );
    }

    #[tokio::test]
    async fn api_key_rides_as_bearer_authorization_header() {
        let parts = vec![sse_token("ok"), "data: [DONE]\n\n".to_string()];
        let (endpoint, captured) = spawn_capturing_server(chunked_200(&parts, true)).await;
        let client = OpenAiClient::new(&endpoint).with_api_key("sk-test-bearer-123");
        client
            .stream_chat(&req(vec![ChatMessage::user("hi")]), &|_| {})
            .await
            .unwrap();
        let raw = captured.lock().unwrap().clone();
        let headers = String::from_utf8_lossy(&raw).to_ascii_lowercase();
        assert!(
            headers.contains("authorization: bearer sk-test-bearer-123"),
            "keyed client must send the bearer header"
        );
    }

    #[tokio::test]
    async fn keyless_client_sends_no_authorization_header() {
        // The loopback LM Studio lanes stay keyless — no stray auth header
        // may appear on the local wire.
        let parts = vec![sse_token("ok"), "data: [DONE]\n\n".to_string()];
        let (endpoint, captured) = spawn_capturing_server(chunked_200(&parts, true)).await;
        let client = OpenAiClient::new(&endpoint);
        client
            .stream_chat(&req(vec![ChatMessage::user("hi")]), &|_| {})
            .await
            .unwrap();
        let raw = captured.lock().unwrap().clone();
        let headers = String::from_utf8_lossy(&raw).to_ascii_lowercase();
        assert!(
            !headers.contains("authorization:"),
            "keyless client must not send auth"
        );
    }

    #[tokio::test]
    async fn request_json_carries_vision_content_parts_for_attachments() {
        // The captured request body is the OpenAI vision contract: with an
        // attachment, content is the multipart array, not a string.
        let parts = vec![sse_token("ok"), "data: [DONE]\n\n".to_string()];
        let (endpoint, captured) = spawn_capturing_server(chunked_200(&parts, true)).await;
        let client = OpenAiClient::new(&endpoint);
        let msg = ChatMessage::user("what is on my screen?").with_attachments(vec![
            crate::llm::Attachment {
                base64_png: "QUJD".into(),
            },
        ]);
        client.stream_chat(&req(vec![msg]), &|_| {}).await.unwrap();

        let body = captured_body_json(&captured);
        let content = &body["messages"][0]["content"];
        assert!(
            content.is_array(),
            "attachment content must be a parts array: {body}"
        );
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[0]["text"], "what is on my screen?");
        assert_eq!(content[1]["type"], "image_url");
        assert_eq!(content[1]["image_url"]["url"], "data:image/png;base64,QUJD");
    }

    #[tokio::test]
    async fn request_json_keeps_string_content_without_attachments() {
        // Regression guard for the custom Serialize: the plain path must
        // stay the exact pre-S04 wire shape.
        let parts = vec![sse_token("ok"), "data: [DONE]\n\n".to_string()];
        let (endpoint, captured) = spawn_capturing_server(chunked_200(&parts, true)).await;
        let client = OpenAiClient::new(&endpoint);
        client
            .stream_chat(&req(vec![ChatMessage::user("hi")]), &|_| {})
            .await
            .unwrap();
        let body = captured_body_json(&captured);
        assert_eq!(body["messages"][0]["content"], "hi");
    }

    #[tokio::test]
    async fn request_json_carries_tools_array_when_tools_present() {
        // The S03 tool contract: tools serialize into the body in the OpenAI
        // function envelope, alongside the untouched messages/stream keys.
        let parts = vec![sse_token("ok"), "data: [DONE]\n\n".to_string()];
        let (endpoint, captured) = spawn_capturing_server(chunked_200(&parts, true)).await;
        let client = OpenAiClient::new(&endpoint);
        let request = req(vec![ChatMessage::user("hi")]).with_tools(vec![ToolDefinition {
            name: "memory_search".into(),
            description: "Search stored memories".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": { "query": { "type": "string" } },
                "required": ["query"]
            }),
        }]);
        client.stream_chat(&request, &|_| {}).await.unwrap();

        let body = captured_body_json(&captured);
        let tools = body["tools"]
            .as_array()
            .expect("body must carry a tools array");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["type"], "function");
        assert_eq!(tools[0]["function"]["name"], "memory_search");
        assert_eq!(tools[0]["function"]["parameters"]["required"][0], "query");
        assert_eq!(body["stream"], true);
        assert_eq!(body["messages"][0]["content"], "hi");
    }

    #[tokio::test]
    async fn request_json_omits_tools_key_without_tools() {
        // Wire-compat pin: a tools-free request is byte-for-byte the pre-S03
        // shape — no tools key at all.
        let parts = vec![sse_token("ok"), "data: [DONE]\n\n".to_string()];
        let (endpoint, captured) = spawn_capturing_server(chunked_200(&parts, true)).await;
        let client = OpenAiClient::new(&endpoint);
        client
            .stream_chat(&req(vec![ChatMessage::user("hi")]), &|_| {})
            .await
            .unwrap();
        let body = captured_body_json(&captured);
        assert!(
            !body.as_object().unwrap().contains_key("tools"),
            "tools-free request must omit the tools key entirely: {body}"
        );
    }

    #[tokio::test]
    async fn request_json_carries_tool_round_trip_messages() {
        // The T03 follow-up request shape: assistant tool_calls echo plus a
        // tool-role result message, serialized in the OpenAI snake_case form.
        let parts = vec![sse_token("ok"), "data: [DONE]\n\n".to_string()];
        let (endpoint, captured) = spawn_capturing_server(chunked_200(&parts, true)).await;
        let client = OpenAiClient::new(&endpoint);
        let request = req(vec![
            ChatMessage::user("what was I working on?"),
            ChatMessage::assistant_tool_calls(
                "",
                vec![crate::llm::ToolCall {
                    id: "call_1".into(),
                    name: "memory_search".into(),
                    arguments: r#"{"query":"morning"}"#.into(),
                }],
            ),
            ChatMessage::tool_result("call_1", r#"{"results":[]}"#),
        ]);
        client.stream_chat(&request, &|_| {}).await.unwrap();

        let body = captured_body_json(&captured);
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs[1]["role"], "assistant");
        assert_eq!(msgs[1]["tool_calls"][0]["id"], "call_1");
        assert_eq!(
            msgs[1]["tool_calls"][0]["function"]["name"],
            "memory_search"
        );
        assert_eq!(msgs[2]["role"], "tool");
        assert_eq!(msgs[2]["tool_call_id"], "call_1");
        assert_eq!(msgs[2]["content"], r#"{"results":[]}"#);
    }

    #[test]
    fn with_model_pins_and_exposes_the_model_id() {
        let client = OpenAiClient::new("http://x:1").with_model("heavy-7b");
        assert_eq!(client.model(), Some("heavy-7b"));
        assert_eq!(OpenAiClient::new("http://x:1").model(), None);
    }

    #[test]
    fn endpoint_trailing_slash_is_normalized() {
        let client = OpenAiClient::new("http://x:1/");
        assert_eq!(LlmClient::endpoint(&client), "http://x:1");
    }

    #[tokio::test]
    async fn list_models_returns_served_model_ids_in_order() {
        let body = r#"{"object":"list","data":[{"id":"qwen2.5-7b"},{"id":"llava-1.6"}]}"#;
        let endpoint = spawn_raw_server(plain_response("200 OK", body)).await;
        let models = OpenAiClient::new(&endpoint).list_models().await.unwrap();
        assert_eq!(models, vec!["qwen2.5-7b", "llava-1.6"]);
    }

    #[tokio::test]
    async fn list_models_empty_data_array_is_ok_and_empty() {
        let endpoint = spawn_raw_server(plain_response("200 OK", r#"{"data":[]}"#)).await;
        assert!(OpenAiClient::new(&endpoint)
            .list_models()
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn list_models_maps_refused_connection_to_offline_naming_endpoint() {
        let endpoint = refused_endpoint().await;
        let err = OpenAiClient::new(&endpoint)
            .list_models()
            .await
            .unwrap_err();
        assert_eq!(err.kind(), "offline");
        assert_eq!(err.endpoint(), endpoint);
    }

    #[tokio::test]
    async fn list_models_maps_http_error_to_offline_with_status_detail() {
        let endpoint = spawn_raw_server(plain_response("500 Internal Server Error", "boom")).await;
        let err = OpenAiClient::new(&endpoint)
            .list_models()
            .await
            .unwrap_err();
        assert_eq!(err.kind(), "offline");
        assert!(matches!(&err, LlmError::Offline { detail, .. } if detail.contains("500")));
    }

    #[tokio::test]
    async fn list_models_maps_malformed_body_to_offline_with_detail() {
        let endpoint = spawn_raw_server(plain_response("200 OK", "not json at all")).await;
        let err = OpenAiClient::new(&endpoint)
            .list_models()
            .await
            .unwrap_err();
        assert_eq!(err.kind(), "offline");
        assert!(matches!(&err, LlmError::Offline { detail, .. } if detail.contains("malformed")));
    }

    #[tokio::test]
    async fn list_models_maps_missing_data_array_to_offline() {
        let endpoint = spawn_raw_server(plain_response("200 OK", r#"{"models":[]}"#)).await;
        let err = OpenAiClient::new(&endpoint)
            .list_models()
            .await
            .unwrap_err();
        assert_eq!(err.kind(), "offline");
        assert!(matches!(&err, LlmError::Offline { detail, .. } if detail.contains("data array")));
    }

    #[test]
    fn default_endpoint_matches_project_constant() {
        // Single definition site: the settings override / THIRD_EYE_ENDPOINT
        // fall back to this constant (S05).
        assert_eq!(DEFAULT_ENDPOINT, "http://localhost:1234");
        assert_eq!(
            LlmClient::endpoint(&OpenAiClient::default_endpoint()),
            DEFAULT_ENDPOINT
        );
    }
}
