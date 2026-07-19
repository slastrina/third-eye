//! LLM client boundary: the trait seam behind all chat traffic.
//!
//! [`LlmClient`] is the abstraction S03's [`router::ModelRouter`] and M003's
//! privacy guard wrap — nothing outside this module may talk HTTP to a model
//! endpoint directly. R006 (failure visibility) is enforced structurally:
//! every failure a client can hit maps to a typed [`LlmError`] variant that
//! names the endpoint, so no caller can ever be stuck with an anonymous
//! "something went wrong" or, worse, a silent hang.
//!
//! [`ChatMessage`] is the composition model; S04 extends it (attachments)
//! rather than replacing it.

pub mod commands;
pub mod guard;
pub mod openai;
pub mod router;
pub mod toolloop;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// Message author role, serialized lowercase to match the OpenAI wire format
/// (`system` / `user` / `assistant` / `tool`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    /// A tool-result turn: the executed output of one requested tool call,
    /// riding back to the model with the `tool_call_id` it answers.
    Tool,
}

/// One image riding a chat message: base64-encoded PNG bytes without the
/// `data:` prefix — exactly the `base64Png` field of a `CapturedFrame` from
/// the `capture_screen` command, echoed back over IPC by the frontend.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Attachment {
    pub base64_png: String,
}

impl Attachment {
    /// The OpenAI vision URL form: `data:image/png;base64,...`.
    pub fn data_url(&self) -> String {
        format!("data:image/png;base64,{}", self.base64_png)
    }
}

/// One tool the model may call, in the shape the request body needs.
/// Serializes to the OpenAI tool definition envelope
/// `{"type":"function","function":{"name","description","parameters"}}`;
/// `parameters` is a JSON Schema object.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

impl Serialize for ToolDefinition {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serde_json::json!({
            "type": "function",
            "function": {
                "name": self.name,
                "description": self.description,
                "parameters": self.parameters,
            }
        })
        .serialize(serializer)
    }
}

/// One complete tool call requested by the model. `arguments` is the raw
/// JSON string exactly as the model produced it (the OpenAI wire carries it
/// as a string, not an object) — the dispatcher parses and validates it.
///
/// Serialized camelCase: this shape rides in `llm://tool-call` events (T03).
/// The OpenAI wire form (`{"id","type":"function","function":{...}}`) is
/// produced by [`ChatMessage`]'s serializer, not here.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

/// One chat turn. Serializes directly into the OpenAI `messages` array.
///
/// The IPC contract stays additive: `attachments` defaults to empty on
/// deserialize, so frontend messages without the field parse unchanged. The
/// custom [`Serialize`] keeps the wire format backward compatible too —
/// plain string `content` when there are no attachments (the S02/S03 shape),
/// and the OpenAI vision multipart content array
/// `[{type:"text"},{type:"image_url",...}]` when there are.
///
/// Tool turns are additive the same way: `tool_calls` (the assistant echo of
/// requested calls) and `tool_call_id` (a tool-result turn) serialize in the
/// OpenAI snake_case wire form only when present — plain messages are
/// byte-for-byte the pre-S03 shape.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ChatMessage {
    pub role: Role,
    pub content: String,
    #[serde(default)]
    pub attachments: Vec<Attachment>,
    /// Assistant turn only: the tool calls the model requested, echoed back
    /// per the OpenAI protocol so the follow-up request is self-consistent.
    #[serde(default)]
    pub tool_calls: Vec<ToolCall>,
    /// Tool turn only: the id of the call this result answers.
    #[serde(default)]
    pub tool_call_id: Option<String>,
}

impl ChatMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self::plain(Role::System, content)
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self::plain(Role::User, content)
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self::plain(Role::Assistant, content)
    }

    /// The assistant turn echoing the model's requested tool calls back to
    /// it — the first half of the OpenAI tool round-trip. `content` is the
    /// text (often empty) that accompanied the request.
    pub fn assistant_tool_calls(content: impl Into<String>, tool_calls: Vec<ToolCall>) -> Self {
        Self { tool_calls, ..Self::plain(Role::Assistant, content) }
    }

    /// A tool-result turn answering `tool_call_id` — the second half of the
    /// OpenAI tool round-trip. `content` is the tool's output for the model.
    pub fn tool_result(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self { tool_call_id: Some(tool_call_id.into()), ..Self::plain(Role::Tool, content) }
    }

    pub fn with_attachments(mut self, attachments: Vec<Attachment>) -> Self {
        self.attachments = attachments;
        self
    }

    fn plain(role: Role, content: impl Into<String>) -> Self {
        Self {
            role,
            content: content.into(),
            attachments: Vec::new(),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }
}

impl Serialize for ChatMessage {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut msg = serializer.serialize_struct("ChatMessage", 2)?;
        msg.serialize_field("role", &self.role)?;
        if self.attachments.is_empty() {
            msg.serialize_field("content", &self.content)?;
        } else {
            let mut parts =
                vec![serde_json::json!({ "type": "text", "text": self.content })];
            parts.extend(self.attachments.iter().map(|att| {
                serde_json::json!({
                    "type": "image_url",
                    "image_url": { "url": att.data_url() }
                })
            }));
            msg.serialize_field("content", &parts)?;
        }
        if !self.tool_calls.is_empty() {
            // The OpenAI assistant-echo form; `arguments` stays the raw
            // string the model produced.
            let calls: Vec<serde_json::Value> = self
                .tool_calls
                .iter()
                .map(|c| {
                    serde_json::json!({
                        "id": c.id,
                        "type": "function",
                        "function": { "name": c.name, "arguments": c.arguments }
                    })
                })
                .collect();
            msg.serialize_field("tool_calls", &calls)?;
        }
        if let Some(id) = &self.tool_call_id {
            msg.serialize_field("tool_call_id", id)?;
        }
        msg.end()
    }
}

/// One chat completion request: the message history plus the tools the model
/// may call. The per-request seam [`LlmClient::stream_chat`] consumes —
/// clients serialize `tools` into the body exactly when non-empty, so a
/// tools-free request is byte-for-byte the pre-S03 wire shape.
#[derive(Debug, Clone, PartialEq)]
pub struct ChatRequest {
    pub messages: Vec<ChatMessage>,
    pub tools: Vec<ToolDefinition>,
}

impl ChatRequest {
    /// A plain request with no tools — the S02 chat/ingest shape.
    pub fn new(messages: Vec<ChatMessage>) -> Self {
        Self { messages, tools: Vec::new() }
    }

    pub fn with_tools(mut self, tools: Vec<ToolDefinition>) -> Self {
        self.tools = tools;
        self
    }
}

/// The full chat failure taxonomy (R006). Serialized with a `kind` tag
/// (`offline` / `no-model` / `interrupted`) and camelCase fields — this JSON
/// shape is the error half of the IPC contract with the UI.
///
/// Every variant carries the endpoint that was tried, so error surfaces can
/// always name it. `Interrupted` carries the text streamed before the drop —
/// the UI preserves it on screen rather than discarding the user's partial
/// answer.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case", rename_all_fields = "camelCase")]
pub enum LlmError {
    /// The endpoint could not be reached (connection refused, timeout, DNS)
    /// or answered with a server error before any token arrived.
    Offline { endpoint: String, detail: String },
    /// The endpoint is up but rejected the request (HTTP 4xx) — typically
    /// LM Studio running with no model loaded.
    NoModel { endpoint: String, detail: String },
    /// The endpoint rejected a tools-carrying request because the loaded
    /// model does not support tool calling (HTTP 4xx whose body names
    /// tools). Distinct from `NoModel` so the UI can say "this model can't
    /// search memory" instead of the misleading "no model loaded".
    ToolsUnsupported { endpoint: String, detail: String },
    /// The stream died after tokens started arriving. `partial_text` holds
    /// everything streamed before the drop.
    Interrupted { endpoint: String, partial_text: String, detail: String },
    /// The privacy guard refused to send this request to a non-loopback
    /// endpoint (R016 fail closed). Carries the endpoint and a kebab-case
    /// machine-readable reason only — never any request text.
    GuardBlocked { endpoint: String, reason: guard::GuardBlockReason },
}

impl LlmError {
    /// Stable machine-readable name, mirroring the serde `kind` tag. Used in
    /// error logs so grep for `offline` / `no-model` / `interrupted` works.
    pub fn kind(&self) -> &'static str {
        match self {
            LlmError::Offline { .. } => "offline",
            LlmError::NoModel { .. } => "no-model",
            LlmError::ToolsUnsupported { .. } => "tools-unsupported",
            LlmError::Interrupted { .. } => "interrupted",
            LlmError::GuardBlocked { .. } => "guard-blocked",
        }
    }

    /// The endpoint the failed request was sent to.
    pub fn endpoint(&self) -> &str {
        match self {
            LlmError::Offline { endpoint, .. }
            | LlmError::NoModel { endpoint, .. }
            | LlmError::ToolsUnsupported { endpoint, .. }
            | LlmError::Interrupted { endpoint, .. }
            | LlmError::GuardBlocked { endpoint, .. } => endpoint,
        }
    }

    /// Text streamed before an interruption, if any.
    pub fn partial_text(&self) -> Option<&str> {
        match self {
            LlmError::Interrupted { partial_text, .. } => Some(partial_text),
            _ => None,
        }
    }
}

impl std::fmt::Display for LlmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LlmError::Offline { endpoint, detail } => {
                write!(f, "local AI offline: {endpoint} unreachable ({detail})")
            }
            LlmError::NoModel { endpoint, detail } => {
                write!(f, "no model available at {endpoint} ({detail})")
            }
            LlmError::ToolsUnsupported { endpoint, detail } => {
                write!(f, "model at {endpoint} does not support tool calling ({detail})")
            }
            LlmError::Interrupted { endpoint, partial_text, detail } => write!(
                f,
                "stream from {endpoint} interrupted after {} chars ({detail})",
                partial_text.chars().count()
            ),
            LlmError::GuardBlocked { endpoint, reason } => {
                write!(f, "blocked by privacy guard: request to {endpoint} not sent ({reason})")
            }
        }
    }
}

impl std::error::Error for LlmError {}

/// Successful stream result: the full accumulated text, how many token
/// deltas produced it (observability: logged at stream end), and any tool
/// calls the model requested. A non-empty `tool_calls` means the model
/// stopped to call tools — the dispatch loop (T03) executes them and issues
/// the follow-up request; T02's SSE accumulation populates the field.
#[derive(Debug, Clone, PartialEq)]
pub struct StreamOutcome {
    pub text: String,
    pub token_count: usize,
    pub tool_calls: Vec<ToolCall>,
}

/// Queryable online/offline surface: `{ online, endpoint }`. Returned by the
/// `llm_health` command (T02) and reused by the UI backoff probe and the
/// future tray status (S05).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LlmHealth {
    pub online: bool,
    pub endpoint: String,
}

/// Per-token callback invoked as deltas arrive. `Fn` (not `FnMut`) so a
/// `&dyn` reference can be shared with the streaming loop; collect state
/// behind a `Mutex` or channel.
pub type TokenSink<'a> = &'a (dyn Fn(&str) + Send + Sync);

/// The chat seam. Object-safe (`Arc<dyn LlmClient>`) so S03's router and
/// M003's privacy guard can wrap any implementation without knowing its
/// transport.
#[async_trait]
pub trait LlmClient: Send + Sync {
    /// The endpoint this client targets, for logs and error surfaces.
    fn endpoint(&self) -> &str;

    /// Stream one chat completion. `on_token` fires for every content delta
    /// in arrival order; the returned outcome holds the accumulated text and
    /// any tool calls the model requested (`request.tools` advertises what
    /// it may call). Never hangs silently: every failure path resolves to an
    /// [`LlmError`].
    async fn stream_chat(
        &self,
        request: &ChatRequest,
        on_token: TokenSink<'_>,
    ) -> Result<StreamOutcome, LlmError>;

    /// Cheap liveness probe (never errors — offline is a value, not a fault).
    async fn health(&self) -> LlmHealth;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// Minimal in-memory client proving the trait is implementable and
    /// object-safe — the same shape S03's router mock will take.
    struct MockClient {
        fail_with: Option<LlmError>,
    }

    #[async_trait]
    impl LlmClient for MockClient {
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
            for token in ["mock ", "reply"] {
                on_token(token);
            }
            Ok(StreamOutcome { text: "mock reply".into(), token_count: 2, tool_calls: Vec::new() })
        }

        async fn health(&self) -> LlmHealth {
            LlmHealth { online: self.fail_with.is_none(), endpoint: self.endpoint().into() }
        }
    }

    #[tokio::test]
    async fn trait_is_object_safe_and_streams_through_dyn() {
        let client: Arc<dyn LlmClient> = Arc::new(MockClient { fail_with: None });
        let seen = Mutex::new(String::new());
        let outcome = client
            .stream_chat(&ChatRequest::new(vec![ChatMessage::user("hi")]), &|t| {
                seen.lock().unwrap().push_str(t)
            })
            .await
            .unwrap();
        assert_eq!(outcome.text, "mock reply");
        assert_eq!(outcome.token_count, 2);
        assert!(outcome.tool_calls.is_empty());
        assert_eq!(*seen.lock().unwrap(), "mock reply");
    }

    #[tokio::test]
    async fn errors_propagate_through_dyn_with_partial_text() {
        let client: Arc<dyn LlmClient> = Arc::new(MockClient {
            fail_with: Some(LlmError::Interrupted {
                endpoint: "http://mock.invalid".into(),
                partial_text: "half an ans".into(),
                detail: "connection reset".into(),
            }),
        });
        let err = client
            .stream_chat(&ChatRequest::new(vec![ChatMessage::user("hi")]), &|_| {})
            .await
            .unwrap_err();
        assert_eq!(err.kind(), "interrupted");
        assert_eq!(err.partial_text(), Some("half an ans"));
        assert_eq!(err.endpoint(), "http://mock.invalid");
    }

    #[test]
    fn chat_messages_serialize_to_openai_wire_format() {
        let msgs =
            vec![ChatMessage::system("be brief"), ChatMessage::user("hi"), ChatMessage::assistant("yo")];
        let v = serde_json::to_value(&msgs).unwrap();
        assert_eq!(v[0]["role"], "system");
        assert_eq!(v[1]["role"], "user");
        assert_eq!(v[2]["role"], "assistant");
        assert_eq!(v[1]["content"], "hi");
    }

    #[test]
    fn message_without_attachments_keeps_plain_string_content() {
        // The pre-S04 wire shape, byte-for-byte: no attachments key, string
        // content — every S02/S03 consumer stays green.
        let v = serde_json::to_value(ChatMessage::user("hi")).unwrap();
        assert!(v["content"].is_string());
        assert_eq!(v["content"], "hi");
        assert!(!v.as_object().unwrap().contains_key("attachments"));
    }

    #[test]
    fn message_with_attachment_serializes_vision_content_parts() {
        let msg = ChatMessage::user("what is on my screen?")
            .with_attachments(vec![Attachment { base64_png: "QUJD".into() }]);
        let v = serde_json::to_value(&msg).unwrap();
        assert_eq!(v["role"], "user");
        let content = v["content"].as_array().expect("content must be a parts array");
        assert_eq!(content.len(), 2);
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[0]["text"], "what is on my screen?");
        assert_eq!(content[1]["type"], "image_url");
        assert_eq!(content[1]["image_url"]["url"], "data:image/png;base64,QUJD");
    }

    #[test]
    fn multiple_attachments_each_get_an_image_part() {
        let msg = ChatMessage::user("compare these").with_attachments(vec![
            Attachment { base64_png: "QQ==".into() },
            Attachment { base64_png: "Qg==".into() },
        ]);
        let v = serde_json::to_value(&msg).unwrap();
        let content = v["content"].as_array().unwrap();
        assert_eq!(content.len(), 3);
        assert_eq!(content[1]["image_url"]["url"], "data:image/png;base64,QQ==");
        assert_eq!(content[2]["image_url"]["url"], "data:image/png;base64,Qg==");
    }

    #[test]
    fn deserialize_without_attachments_field_is_additive() {
        // Frontend messages predating S04 (and history turns) carry no
        // attachments key — they must parse to an empty vec, not error.
        let msg: ChatMessage =
            serde_json::from_str(r#"{"role":"user","content":"hi"}"#).unwrap();
        assert_eq!(msg, ChatMessage::user("hi"));
        assert!(msg.attachments.is_empty());
    }

    #[test]
    fn deserialize_with_camel_case_attachments_from_ipc() {
        let msg: ChatMessage = serde_json::from_str(
            r#"{"role":"user","content":"look","attachments":[{"base64Png":"QUJD"}]}"#,
        )
        .unwrap();
        assert_eq!(msg.attachments.len(), 1);
        assert_eq!(msg.attachments[0].base64_png, "QUJD");
        assert_eq!(msg.attachments[0].data_url(), "data:image/png;base64,QUJD");
    }

    #[test]
    fn error_json_shape_is_the_ipc_contract() {
        // The UI matches on `kind` and reads camelCase fields; a change here
        // is a breaking IPC change and must be coordinated with src/chat.ts.
        let offline = LlmError::Offline {
            endpoint: "http://192.168.182.224:1234".into(),
            detail: "connection refused".into(),
        };
        let v = serde_json::to_value(&offline).unwrap();
        assert_eq!(v["kind"], "offline");
        assert_eq!(v["endpoint"], "http://192.168.182.224:1234");
        assert_eq!(v["detail"], "connection refused");

        let no_model = LlmError::NoModel { endpoint: "e".into(), detail: "d".into() };
        assert_eq!(serde_json::to_value(&no_model).unwrap()["kind"], "no-model");

        let tools = LlmError::ToolsUnsupported { endpoint: "e".into(), detail: "d".into() };
        assert_eq!(serde_json::to_value(&tools).unwrap()["kind"], "tools-unsupported");

        let interrupted = LlmError::Interrupted {
            endpoint: "e".into(),
            partial_text: "partial".into(),
            detail: "d".into(),
        };
        let v = serde_json::to_value(&interrupted).unwrap();
        assert_eq!(v["kind"], "interrupted");
        assert_eq!(v["partialText"], "partial");

        let blocked = LlmError::GuardBlocked {
            endpoint: "http://192.0.2.1:9".into(),
            reason: guard::GuardBlockReason::LowConfidence,
        };
        let v = serde_json::to_value(&blocked).unwrap();
        assert_eq!(v["kind"], "guard-blocked");
        assert_eq!(v["endpoint"], "http://192.0.2.1:9");
        assert_eq!(v["reason"], "low-confidence");
    }

    #[test]
    fn guard_blocked_error_names_endpoint_kind_and_reason() {
        let err = LlmError::GuardBlocked {
            endpoint: "http://192.0.2.1:9".into(),
            reason: guard::GuardBlockReason::AttachmentUnredactable,
        };
        assert_eq!(err.kind(), "guard-blocked");
        assert_eq!(err.endpoint(), "http://192.0.2.1:9");
        let msg = err.to_string();
        assert!(msg.contains("privacy guard"), "guard missing: {msg}");
        assert!(msg.contains("http://192.0.2.1:9"), "endpoint missing: {msg}");
        assert!(msg.contains("attachment-unredactable"), "reason missing: {msg}");
    }

    #[test]
    fn error_display_names_endpoint_and_failure_type() {
        let err = LlmError::Offline {
            endpoint: "http://192.168.182.224:1234".into(),
            detail: "connection refused".into(),
        };
        let msg = err.to_string();
        assert!(msg.contains("http://192.168.182.224:1234"), "endpoint missing: {msg}");
        assert!(msg.contains("offline"), "failure type missing: {msg}");
    }

    #[test]
    fn tools_unsupported_error_names_endpoint_and_kind() {
        let err = LlmError::ToolsUnsupported {
            endpoint: "http://192.168.182.224:1234".into(),
            detail: "model does not support tools".into(),
        };
        assert_eq!(err.kind(), "tools-unsupported");
        assert_eq!(err.endpoint(), "http://192.168.182.224:1234");
        let msg = err.to_string();
        assert!(msg.contains("http://192.168.182.224:1234"), "endpoint missing: {msg}");
        assert!(msg.contains("tool calling"), "failure type missing: {msg}");
    }

    #[test]
    fn health_serializes_camel_case() {
        let h = LlmHealth { online: false, endpoint: "http://x:1".into() };
        let v = serde_json::to_value(&h).unwrap();
        assert_eq!(v["online"], false);
        assert_eq!(v["endpoint"], "http://x:1");
    }

    #[test]
    fn tool_definition_serializes_openai_function_envelope() {
        let def = ToolDefinition {
            name: "memory_search".into(),
            description: "Search stored memories".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": { "query": { "type": "string" } },
                "required": ["query"]
            }),
        };
        let v = serde_json::to_value(&def).unwrap();
        assert_eq!(v["type"], "function");
        assert_eq!(v["function"]["name"], "memory_search");
        assert_eq!(v["function"]["description"], "Search stored memories");
        assert_eq!(v["function"]["parameters"]["type"], "object");
        assert_eq!(v["function"]["parameters"]["required"][0], "query");
    }

    #[test]
    fn assistant_tool_calls_message_serializes_openai_echo_form() {
        // The first half of the tool round-trip: assistant message carrying
        // tool_calls in the OpenAI envelope, arguments as the raw string.
        let msg = ChatMessage::assistant_tool_calls(
            "",
            vec![ToolCall {
                id: "call_abc".into(),
                name: "memory_search".into(),
                arguments: r#"{"query":"morning work"}"#.into(),
            }],
        );
        let v = serde_json::to_value(&msg).unwrap();
        assert_eq!(v["role"], "assistant");
        assert_eq!(v["content"], "");
        let calls = v["tool_calls"].as_array().expect("tool_calls must be an array");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["id"], "call_abc");
        assert_eq!(calls[0]["type"], "function");
        assert_eq!(calls[0]["function"]["name"], "memory_search");
        assert_eq!(calls[0]["function"]["arguments"], r#"{"query":"morning work"}"#);
        assert!(!v.as_object().unwrap().contains_key("tool_call_id"));
    }

    #[test]
    fn tool_result_message_serializes_tool_role_with_call_id() {
        // The second half of the round-trip: role "tool" + tool_call_id.
        let msg = ChatMessage::tool_result("call_abc", r#"{"results":[]}"#);
        let v = serde_json::to_value(&msg).unwrap();
        assert_eq!(v["role"], "tool");
        assert_eq!(v["content"], r#"{"results":[]}"#);
        assert_eq!(v["tool_call_id"], "call_abc");
        assert!(!v.as_object().unwrap().contains_key("tool_calls"));
    }

    #[test]
    fn plain_message_wire_shape_has_no_tool_keys() {
        // Regression pin: the pre-S03 wire shape is untouched — no tool_calls
        // or tool_call_id keys on ordinary messages.
        let v = serde_json::to_value(ChatMessage::user("hi")).unwrap();
        let obj = v.as_object().unwrap();
        assert!(!obj.contains_key("tool_calls"));
        assert!(!obj.contains_key("tool_call_id"));
        assert_eq!(obj.len(), 2, "plain message must carry exactly role+content: {v}");
    }

    #[test]
    fn tool_call_serializes_camel_case_for_events() {
        // llm://tool-call events (T03) carry this shape over IPC.
        let call = ToolCall {
            id: "call_1".into(),
            name: "memory_search".into(),
            arguments: r#"{"query":"x"}"#.into(),
        };
        let v = serde_json::to_value(&call).unwrap();
        assert_eq!(v["id"], "call_1");
        assert_eq!(v["name"], "memory_search");
        assert_eq!(v["arguments"], r#"{"query":"x"}"#);
    }

    #[test]
    fn chat_request_new_has_no_tools_and_with_tools_attaches_them() {
        let req = ChatRequest::new(vec![ChatMessage::user("hi")]);
        assert!(req.tools.is_empty());
        assert_eq!(req.messages.len(), 1);

        let req = req.with_tools(vec![ToolDefinition {
            name: "memory_search".into(),
            description: "d".into(),
            parameters: serde_json::json!({"type": "object"}),
        }]);
        assert_eq!(req.tools.len(), 1);
        assert_eq!(req.tools[0].name, "memory_search");
    }

    #[test]
    fn deserialize_plain_message_defaults_tool_fields_empty() {
        // Additive IPC contract: frontend messages carry no tool keys.
        let msg: ChatMessage =
            serde_json::from_str(r#"{"role":"user","content":"hi"}"#).unwrap();
        assert!(msg.tool_calls.is_empty());
        assert_eq!(msg.tool_call_id, None);
    }
}
