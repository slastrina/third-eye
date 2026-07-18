//! OpenAI-compatible streaming client for LM Studio.
//!
//! POSTs to `{endpoint}/v1/chat/completions` with `stream: true` and parses
//! the SSE response line-by-line, mapping every transport and protocol
//! failure onto the typed [`LlmError`] taxonomy (R006):
//!
//! - connection refused / timeout / HTTP 5xx before any token → `Offline`
//! - HTTP 4xx (LM Studio with no model loaded) → `NoModel`
//! - any failure after tokens started (drop, malformed SSE) → `Interrupted`,
//!   carrying the partial text streamed so far
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

use super::{ChatMessage, LlmClient, LlmError, LlmHealth, StreamOutcome, TokenSink};

/// The single definition site for the LM Studio endpoint. S05 makes this
/// user-configurable; until then every layer reads it from here.
pub const DEFAULT_ENDPOINT: &str = "http://192.168.182.224:1234";

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
    http: reqwest::Client,
}

impl OpenAiClient {
    pub fn new(endpoint: impl Into<String>) -> Self {
        let endpoint = endpoint.into().trim_end_matches('/').to_string();
        let http = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .build()
            .expect("reqwest client construction cannot fail with static config");
        Self { endpoint, model: None, http }
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

    /// Client against the project-default LM Studio endpoint.
    pub fn default_endpoint() -> Self {
        Self::new(DEFAULT_ENDPOINT)
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
            .http
            .get(&url)
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
            self.offline(format!("malformed /v1/models response ({e}): {}", snippet(&body)))
        })?;
        let data = value["data"].as_array().ok_or_else(|| {
            self.offline(format!("/v1/models response has no data array: {}", snippet(&body)))
        })?;
        Ok(data.iter().filter_map(|m| m["id"].as_str().map(str::to_owned)).collect())
    }

    fn offline(&self, detail: impl Into<String>) -> LlmError {
        LlmError::Offline { endpoint: self.endpoint.clone(), detail: detail.into() }
    }
}

#[async_trait]
impl LlmClient for OpenAiClient {
    fn endpoint(&self) -> &str {
        &self.endpoint
    }

    async fn stream_chat(
        &self,
        messages: &[ChatMessage],
        on_token: TokenSink<'_>,
    ) -> Result<StreamOutcome, LlmError> {
        let url = format!("{}/v1/chat/completions", self.endpoint);
        let mut body = serde_json::json!({ "messages": messages, "stream": true });
        if let Some(model) = &self.model {
            body["model"] = serde_json::json!(model);
        }
        let started = Instant::now();
        log::debug!(
            "llm: request start endpoint={} model={} messages={}",
            self.endpoint,
            self.model.as_deref().unwrap_or("default"),
            messages.len()
        );

        let resp = self
            .http
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| self.offline(e.to_string()))?;

        let status = resp.status();
        if !status.is_success() {
            let detail = format!("HTTP {status}: {}", snippet(&resp.text().await.unwrap_or_default()));
            return Err(if status.is_client_error() {
                LlmError::NoModel { endpoint: self.endpoint.clone(), detail }
            } else {
                self.offline(detail)
            });
        }

        let mut stream = resp.bytes_stream();
        let mut buf: Vec<u8> = Vec::new();
        let mut text = String::new();
        let mut token_count = 0usize;
        let mut first_token_at: Option<Instant> = None;

        let mut handle_line = |line: &str, text: &mut String, token_count: &mut usize| {
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
                if handle_line(line.trim_end_matches(['\n', '\r']), &mut text, &mut token_count)? {
                    break 'stream;
                }
            }
        }
        // A final line without a trailing newline is still data (clean EOF
        // without [DONE] is a valid successful completion).
        if !buf.is_empty() {
            let line = String::from_utf8_lossy(&buf).to_string();
            handle_line(line.trim_end_matches(['\n', '\r']), &mut text, &mut token_count)?;
        }

        log::debug!(
            "llm: stream done: {} tokens, {} chars in {:.0} ms",
            token_count,
            text.len(),
            started.elapsed().as_secs_f64() * 1000.0
        );
        Ok(StreamOutcome { text, token_count })
    }

    async fn health(&self) -> LlmHealth {
        let url = format!("{}/v1/models", self.endpoint);
        let online = match self.http.get(&url).timeout(HEALTH_TIMEOUT).send().await {
            Ok(resp) => resp.status().is_success(),
            Err(e) => {
                log::debug!("llm: health probe failed for {}: {e}", self.endpoint);
                false
            }
        };
        LlmHealth { online, endpoint: self.endpoint.clone() }
    }
}

/// One parsed SSE line.
#[derive(Debug)]
enum SseLine {
    /// A content delta to append and forward.
    Token(String),
    /// The `data: [DONE]` terminator.
    Done,
    /// Anything to ignore: blank lines, comments, role-change deltas,
    /// null/empty content.
    Skip,
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
    match value["choices"][0]["delta"]["content"].as_str() {
        Some(content) if !content.is_empty() => Ok(SseLine::Token(content.to_string())),
        // Role-change deltas, null content, finish_reason-only events.
        _ => Ok(SseLine::Skip),
    }
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
    pub(crate) async fn spawn_capturing_server(
        response: Vec<u8>,
    ) -> (String, Arc<Mutex<Vec<u8>>>) {
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
        let Some(header_end) = text.find("\r\n\r\n") else { return false };
        let content_length = text
            .lines()
            .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:")?.trim().parse::<usize>().ok())
            .unwrap_or(0);
        buf.len() >= header_end + 4 + content_length
    }

    /// The JSON body of a captured request (panics if none was captured).
    pub(crate) fn captured_body_json(captured: &Arc<Mutex<Vec<u8>>>) -> serde_json::Value {
        let raw = captured.lock().unwrap().clone();
        let text = String::from_utf8_lossy(&raw);
        let body = text.split("\r\n\r\n").nth(1).expect("captured request has no body");
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
        format!("data: {}\n\n", serde_json::json!({"choices": [{"delta": {"content": token}}]}))
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
    use std::sync::Mutex;

    async fn run_chat(endpoint: &str) -> (Result<StreamOutcome, LlmError>, Vec<String>) {
        let client = OpenAiClient::new(endpoint);
        let seen = Mutex::new(Vec::new());
        let result = client
            .stream_chat(&[ChatMessage::user("hi")], &|t| seen.lock().unwrap().push(t.to_string()))
            .await;
        (result, seen.into_inner().unwrap())
    }

    #[tokio::test]
    async fn streams_tokens_and_accumulates_text() {
        let parts = vec![sse_token("Hel"), sse_token("lo"), "data: [DONE]\n\n".to_string()];
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
        let endpoint =
            spawn_raw_server(chunked_200(&[a.to_string(), b.to_string()], true)).await;
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
        assert!(err.to_string().contains(&endpoint), "display must name endpoint: {err}");
        assert!(seen.is_empty());
    }

    #[tokio::test]
    async fn http_400_maps_to_no_model_with_body_detail() {
        let body = r#"{"error":"No model loaded"}"#;
        let endpoint = spawn_raw_server(plain_response("400 Bad Request", body)).await;
        let (result, _) = run_chat(&endpoint).await;
        let err = result.unwrap_err();
        assert_eq!(err.kind(), "no-model");
        assert!(matches!(&err, LlmError::NoModel { detail, .. } if detail.contains("No model loaded")));
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
        assert_eq!(seen, vec!["par", "tial"], "tokens before the drop must have been delivered");
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
            assert!(matches!(parse_sse_line(line), Ok(SseLine::Skip)), "line: {line:?}");
        }
    }

    #[test]
    fn parse_recognizes_done_marker() {
        assert!(matches!(parse_sse_line("data: [DONE]"), Ok(SseLine::Done)));
    }

    #[test]
    fn parse_rejects_malformed_json_with_detail() {
        let err = parse_sse_line("data: {nope").unwrap_err();
        assert!(err.contains("malformed"), "detail: {err}");
    }

    #[tokio::test]
    async fn request_json_carries_model_when_pinned() {
        let parts = vec![sse_token("ok"), "data: [DONE]\n\n".to_string()];
        let (endpoint, captured) = spawn_capturing_server(chunked_200(&parts, true)).await;
        let client = OpenAiClient::new(&endpoint).with_model("thin-model-1b");
        client.stream_chat(&[ChatMessage::user("hi")], &|_| {}).await.unwrap();
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
        client.stream_chat(&[ChatMessage::user("hi")], &|_| {}).await.unwrap();
        let body = captured_body_json(&captured);
        assert!(
            !body.as_object().unwrap().contains_key("model"),
            "unpinned client must omit the model key entirely: {body}"
        );
    }

    #[tokio::test]
    async fn request_json_carries_vision_content_parts_for_attachments() {
        // The captured request body is the OpenAI vision contract: with an
        // attachment, content is the multipart array, not a string.
        let parts = vec![sse_token("ok"), "data: [DONE]\n\n".to_string()];
        let (endpoint, captured) = spawn_capturing_server(chunked_200(&parts, true)).await;
        let client = OpenAiClient::new(&endpoint);
        let msg = ChatMessage::user("what is on my screen?")
            .with_attachments(vec![crate::llm::Attachment { base64_png: "QUJD".into() }]);
        client.stream_chat(&[msg], &|_| {}).await.unwrap();

        let body = captured_body_json(&captured);
        let content = &body["messages"][0]["content"];
        assert!(content.is_array(), "attachment content must be a parts array: {body}");
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
        client.stream_chat(&[ChatMessage::user("hi")], &|_| {}).await.unwrap();
        let body = captured_body_json(&captured);
        assert_eq!(body["messages"][0]["content"], "hi");
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
        assert!(OpenAiClient::new(&endpoint).list_models().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn list_models_maps_refused_connection_to_offline_naming_endpoint() {
        let endpoint = refused_endpoint().await;
        let err = OpenAiClient::new(&endpoint).list_models().await.unwrap_err();
        assert_eq!(err.kind(), "offline");
        assert_eq!(err.endpoint(), endpoint);
    }

    #[tokio::test]
    async fn list_models_maps_http_error_to_offline_with_status_detail() {
        let endpoint = spawn_raw_server(plain_response("500 Internal Server Error", "boom")).await;
        let err = OpenAiClient::new(&endpoint).list_models().await.unwrap_err();
        assert_eq!(err.kind(), "offline");
        assert!(matches!(&err, LlmError::Offline { detail, .. } if detail.contains("500")));
    }

    #[tokio::test]
    async fn list_models_maps_malformed_body_to_offline_with_detail() {
        let endpoint = spawn_raw_server(plain_response("200 OK", "not json at all")).await;
        let err = OpenAiClient::new(&endpoint).list_models().await.unwrap_err();
        assert_eq!(err.kind(), "offline");
        assert!(matches!(&err, LlmError::Offline { detail, .. } if detail.contains("malformed")));
    }

    #[tokio::test]
    async fn list_models_maps_missing_data_array_to_offline() {
        let endpoint = spawn_raw_server(plain_response("200 OK", r#"{"models":[]}"#)).await;
        let err = OpenAiClient::new(&endpoint).list_models().await.unwrap_err();
        assert_eq!(err.kind(), "offline");
        assert!(matches!(&err, LlmError::Offline { detail, .. } if detail.contains("data array")));
    }

    #[test]
    fn default_endpoint_matches_project_constant() {
        // Single definition site: S05 configurability replaces this constant.
        assert_eq!(DEFAULT_ENDPOINT, "http://192.168.182.224:1234");
        assert_eq!(LlmClient::endpoint(&OpenAiClient::default_endpoint()), DEFAULT_ENDPOINT);
    }
}
