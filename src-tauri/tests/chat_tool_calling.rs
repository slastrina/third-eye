//! S03 closure proofs (T05): the tool-calling loop end to end.
//!
//! `tool_loop_end_to_end_against_mock_server` is the non-ignored CI proof:
//! a real [`OpenAiClient`] talks HTTP/SSE to a scripted mock server that
//! streams a `memory_search` tool call (arguments split across deltas, as
//! LM Studio really streams them), the production [`run_tool_loop`] executes
//! it against a real file-backed [`MemoryStore`], and the follow-up request
//! carries the OpenAI assistant-echo + tool-role turns with the stored
//! memory riding in the tool result. Nothing in the production path is
//! mocked — only the model endpoint is scripted.
//!
//! `tools_unsupported_rejection_is_typed_through_the_loop` pins the R006
//! degrade: a 4xx naming tools surfaces as the typed `tools-unsupported`
//! kind, never silence.
//!
//! `live_tool_calling_against_lm_studio` is the roadmap demo at command/test
//! level (mirrors S02's `live_distill_and_recall_against_lm_studio`):
//! `#[ignore]` because it needs LM Studio serving a tool-capable chat model
//! and `text-embedding-nomic-embed-text-v1.5` at the project-default
//! endpoint. Run it explicitly at closeout:
//!
//! ```sh
//! THIRD_EYE_TOOL_MODEL=<served-chat-model-id> \
//!   cargo test --manifest-path src-tauri/Cargo.toml \
//!   --test chat_tool_calling -- --ignored --nocapture
//! ```
//!
//! Leaving `THIRD_EYE_TOOL_MODEL` unset uses an unpinned request (LM
//! Studio's loaded default), exactly like production's default lane.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use third_eye_lib::llm::openai::{OpenAiClient, DEFAULT_ENDPOINT};
use third_eye_lib::llm::toolloop::{
    run_tool_loop, MemorySearchTool, ToolEvent, MEMORY_SEARCH_TOOL,
};
use third_eye_lib::llm::ChatMessage;
use third_eye_lib::memory::{Embedder, MemoryStore, NewMemory, OpenAiEmbedder, SearchMode};

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
// Scripted HTTP server: one pre-baked response per connection, in order,
// capturing every request's raw bytes. The `llm::openai::test_support`
// helpers are #[cfg(test)] (invisible to integration tests) and serve a
// single connection; the tool loop makes one HTTP request per round, so this
// server scripts a whole conversation.
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
                let Ok((mut sock, _)) = listener.accept().await else { return };
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
        let Some(header_end) = text.find("\r\n\r\n") else { return false };
        let content_length = text
            .lines()
            .find_map(|l| {
                l.to_ascii_lowercase().strip_prefix("content-length:")?.trim().parse::<usize>().ok()
            })
            .unwrap_or(0);
        buf.len() >= header_end + 4 + content_length
    }

    /// The JSON body of the i-th captured request.
    pub fn body_json(captured: &Arc<Mutex<Vec<Vec<u8>>>>, i: usize) -> serde_json::Value {
        let raw = captured.lock().unwrap()[i].clone();
        let text = String::from_utf8_lossy(&raw);
        let body = text.split("\r\n\r\n").nth(1).expect("captured request has no body");
        serde_json::from_str(body).expect("captured request body is not JSON")
    }

    pub fn sse_token(token: &str) -> String {
        format!("data: {}\n\n", serde_json::json!({"choices": [{"delta": {"content": token}}]}))
    }

    /// One streamed `delta.tool_calls` SSE event in the OpenAI shape: id and
    /// name on the first delta for an index, `arguments` string fragments on
    /// follow-ups. Omitted fields are absent from the JSON.
    pub fn sse_tool_delta(
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

    pub fn plain_response(status_line: &str, body: &str) -> Vec<u8> {
        format!(
            "HTTP/1.1 {status_line}\r\ncontent-type: application/json\r\n\
             content-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    }
}

/// An endpoint with nothing listening — bind, read the address, drop. Points
/// the search embedder at a dead port so semantic search degrades to keyword
/// mode (the S02 contract) without needing an embeddings server.
async fn refused_endpoint() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    format!("http://{addr}")
}

/// A real file-backed store holding one distinctive memory the scripted
/// conversation searches for.
fn seeded_store(scratch: &ScratchDb) -> Arc<MemoryStore> {
    let store = Arc::new(MemoryStore::open(&scratch.path).expect("open file-backed store"));
    store
        .insert(NewMemory {
            summary: "Kneading sourdough starter notes: 75 percent hydration, fed at 9am".into(),
            apps: vec!["Notes".into()],
            span_start_ms: 1_000,
            span_end_ms: 2_000,
            embedding: None,
        })
        .expect("insert seed memory");
    store
        .insert(NewMemory {
            summary: "Reviewed kubernetes pod OOMKilled restarts in the prod cluster".into(),
            apps: vec!["Terminal".into()],
            span_start_ms: 3_000,
            span_end_ms: 4_000,
            embedding: None,
        })
        .expect("insert second seed memory");
    store
}

struct Capture {
    events: Mutex<Vec<ToolEvent>>,
    tokens: Mutex<String>,
}

impl Capture {
    fn new() -> Self {
        Self { events: Mutex::new(Vec::new()), tokens: Mutex::new(String::new()) }
    }
}

/// The non-ignored CI proof: full production path from HTTP request bytes to
/// stored-memory-grounded answer. The scripted model requests memory_search
/// with arguments split across three SSE deltas; the loop executes it
/// against the real store and the follow-up request carries the OpenAI
/// round-trip turns with the actual stored summary in the tool result.
#[tokio::test(flavor = "multi_thread")]
async fn tool_loop_end_to_end_against_mock_server() {
    // Round 0: the model stops to call memory_search — id/name arrive on the
    // first delta, the arguments JSON is fragmented across two more.
    let round0 = scripted::sse_200(&[
        scripted::sse_tool_delta(0, Some("call_live_1"), Some(MEMORY_SEARCH_TOOL), None),
        scripted::sse_tool_delta(0, None, None, Some(r#"{"query":"sour"#)),
        scripted::sse_tool_delta(0, None, None, Some(r#"dough"}"#)),
        "data: [DONE]\n\n".to_string(),
    ]);
    // Round 1: having read the tool result, the model streams a text answer.
    let round1 = scripted::sse_200(&[
        scripted::sse_token("You were working on "),
        scripted::sse_token("your sourdough starter."),
        "data: [DONE]\n\n".to_string(),
    ]);
    let (endpoint, captured) = scripted::spawn(vec![round0, round1]).await;

    let scratch = ScratchDb::new("toolloop-mock");
    let store = seeded_store(&scratch);
    let embedder = Arc::new(OpenAiEmbedder::new(refused_endpoint().await));
    let executor = MemorySearchTool::new(store, embedder);

    let client = OpenAiClient::new(&endpoint);
    let capture = Capture::new();
    let outcome = run_tool_loop(
        &client,
        &executor,
        vec![ChatMessage::user("what was I working on this morning?")],
        42,
        &|t| capture.tokens.lock().unwrap().push_str(t),
        &|e| capture.events.lock().unwrap().push(e.clone()),
    )
    .await
    .expect("scripted conversation must succeed");

    // The final answer streamed through on_token and landed on the outcome.
    assert_eq!(outcome.text, "You were working on your sourdough starter.");
    assert_eq!(*capture.tokens.lock().unwrap(), outcome.text);
    assert!(outcome.tool_calls.is_empty(), "resolved loops leak no pending calls");

    // Tool phases: one Call announcing the reassembled arguments, one ok
    // Result carrying count and the keyword degrade mode — the exact payload
    // the UI's memory-consulted indicator consumes.
    let events = capture.events.lock().unwrap().clone();
    assert_eq!(events.len(), 2, "one call + one result: {events:?}");
    let ToolEvent::Call(call) = &events[0] else { panic!("first event must be Call") };
    assert_eq!(call.request_id, 42);
    assert_eq!(call.round, 0);
    assert_eq!(call.call.id, "call_live_1");
    assert_eq!(call.call.name, MEMORY_SEARCH_TOOL);
    assert_eq!(
        call.call.arguments, r#"{"query":"sourdough"}"#,
        "split argument deltas must reassemble byte-for-byte"
    );
    let ToolEvent::Result(result) = &events[1] else { panic!("second event must be Result") };
    assert!(result.ok);
    assert_eq!(result.call_id, "call_live_1");
    assert_eq!(result.name, MEMORY_SEARCH_TOOL);
    assert_eq!(result.result_count, Some(1), "keyword search must hit the sourdough memory");
    assert_eq!(result.mode, Some(SearchMode::Keyword), "dead embedder degrades to keyword");
    assert_eq!(result.failure, None);

    // Wire-level proof of both requests. Request 0: tools advertised, plain
    // user message, no tool turns.
    let req0 = scripted::body_json(&captured, 0);
    assert_eq!(req0["tools"][0]["function"]["name"], "memory_search");
    assert_eq!(req0["messages"].as_array().unwrap().len(), 1);
    assert_eq!(req0["messages"][0]["role"], "user");
    assert_eq!(req0["stream"], true);

    // Request 1: the OpenAI round-trip — assistant echo carrying the raw
    // arguments string, then the tool-role result with the stored memory.
    let req1 = scripted::body_json(&captured, 1);
    let messages = req1["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 3, "user + assistant echo + tool result: {messages:?}");
    assert_eq!(messages[1]["role"], "assistant");
    assert_eq!(messages[1]["tool_calls"][0]["id"], "call_live_1");
    assert_eq!(messages[1]["tool_calls"][0]["type"], "function");
    assert_eq!(
        messages[1]["tool_calls"][0]["function"]["arguments"],
        r#"{"query":"sourdough"}"#
    );
    assert_eq!(messages[2]["role"], "tool");
    assert_eq!(messages[2]["tool_call_id"], "call_live_1");
    let tool_content = messages[2]["content"].as_str().unwrap();
    assert!(
        tool_content.contains("sourdough starter notes"),
        "the stored memory must ride to the model: {tool_content}"
    );
    assert!(
        !tool_content.contains("kubernetes"),
        "the off-topic memory must not match a sourdough query: {tool_content}"
    );
    assert_eq!(req1["tools"][0]["function"]["name"], "memory_search", "round 1 still offers tools");
}

/// R006 negative proof through the real HTTP client: a 4xx whose body names
/// tools on a tools-carrying request is the typed `tools-unsupported` kind —
/// a visible banner upstream, never a misleading "no model" or silence.
#[tokio::test(flavor = "multi_thread")]
async fn tools_unsupported_rejection_is_typed_through_the_loop() {
    let rejection = scripted::plain_response(
        "400 Bad Request",
        r#"{"error":"this model does not support tool use"}"#,
    );
    let (endpoint, _captured) = scripted::spawn(vec![rejection]).await;

    let scratch = ScratchDb::new("toolloop-unsupported");
    let store = seeded_store(&scratch);
    let embedder = Arc::new(OpenAiEmbedder::new(refused_endpoint().await));
    let executor = MemorySearchTool::new(store, embedder);

    let client = OpenAiClient::new(&endpoint);
    let capture = Capture::new();
    let err = run_tool_loop(
        &client,
        &executor,
        vec![ChatMessage::user("what was I working on?")],
        7,
        &|t| capture.tokens.lock().unwrap().push_str(t),
        &|e| capture.events.lock().unwrap().push(e.clone()),
    )
    .await
    .expect_err("a tools rejection must fail typed");

    assert_eq!(err.kind(), "tools-unsupported");
    assert_eq!(err.endpoint(), endpoint);
    assert!(capture.events.lock().unwrap().is_empty(), "no tool executed, no events");
    assert!(capture.tokens.lock().unwrap().is_empty());
}

/// The roadmap demo live: seed a real store with embedded memories, ask the
/// real model about earlier work, and require that it answers by actually
/// calling memory_search and citing the stored content.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires LM Studio serving a tool-capable chat model and text-embedding-nomic-embed-text-v1.5 at DEFAULT_ENDPOINT"]
async fn live_tool_calling_against_lm_studio() {
    let scratch = ScratchDb::new("toolloop-live");
    let store = Arc::new(MemoryStore::open(&scratch.path).expect("open store"));
    let embedder = Arc::new(OpenAiEmbedder::new(DEFAULT_ENDPOINT));

    // Seed distinctive memories with real embeddings so semantic recall works
    // exactly as in production.
    let summaries = [
        "Debugged a tokio broadcast channel lag spike in the screen watcher loop using Zed",
        "Planned the sourdough bake: 75 percent hydration, overnight cold proof in the fridge",
    ];
    for (i, summary) in summaries.iter().enumerate() {
        let embedding = embedder
            .embed(&[summary.to_string()])
            .await
            .expect("live embedder must be up for the live test")
            .pop();
        store
            .insert(NewMemory {
                summary: summary.to_string(),
                apps: vec!["Zed".into()],
                span_start_ms: 1_000 + i as i64,
                span_end_ms: 2_000 + i as i64,
                embedding,
            })
            .expect("insert seeded memory");
    }
    let executor = MemorySearchTool::new(store, embedder);

    let model = std::env::var("THIRD_EYE_TOOL_MODEL")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let mut client = OpenAiClient::default_endpoint();
    if let Some(model) = model {
        client = client.with_model(model);
    }

    let capture = Capture::new();
    let outcome = run_tool_loop(
        &client,
        &executor,
        vec![
            ChatMessage::system(
                "You are a desktop assistant with access to the user's stored activity \
                 memories via the memory_search tool. Use it to answer questions about \
                 their earlier work, and cite what you find.",
            ),
            ChatMessage::user("what was I debugging earlier today?"),
        ],
        1,
        &|t| capture.tokens.lock().unwrap().push_str(t),
        &|e| capture.events.lock().unwrap().push(e.clone()),
    )
    .await
    .expect("live tool conversation must resolve");

    let events = capture.events.lock().unwrap().clone();
    eprintln!("live answer: {}", outcome.text);
    for e in &events {
        eprintln!("live event: {e:?}");
    }

    // The demo's substance: the model really called memory_search and at
    // least one execution succeeded against the seeded store.
    let calls: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            ToolEvent::Call(c) => Some(c),
            _ => None,
        })
        .collect();
    assert!(!calls.is_empty(), "the model must call memory_search for a memory question");
    assert!(calls.iter().all(|c| c.call.name == MEMORY_SEARCH_TOOL));
    assert!(
        events.iter().any(|e| matches!(e, ToolEvent::Result(r) if r.ok)),
        "at least one memory_search must succeed: {events:?}"
    );

    // The streamed answer is grounded in the debugging memory, not the
    // baking one.
    let answer = outcome.text.to_lowercase();
    assert!(
        ["tokio", "broadcast", "watcher", "lag"].iter().any(|kw| answer.contains(kw)),
        "answer must cite the seeded debugging memory: {}",
        outcome.text
    );
}
