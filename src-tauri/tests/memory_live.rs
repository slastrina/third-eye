//! S02 closure proofs (T05).
//!
//! `memory_db_file_is_text_only` is the file-level R011/R012 pin: it drives
//! the real store surface against a real on-disk database, then inspects the
//! raw file — schema, stored value types, and bytes — to prove nothing but
//! text and metadata can land in `memory.db`.
//!
//! `live_distill_and_recall_against_lm_studio` is the roadmap demo at
//! command/test level (mirrors S01's `real_screen_extract_smoke`): `#[ignore]`
//! because it needs LM Studio serving a chat model and
//! `text-embedding-nomic-embed-text-v1.5` at the project-default endpoint.
//! Run it explicitly at closeout:
//!
//! ```sh
//! THIRD_EYE_THIN_MODEL=<served-chat-model-id> \
//!   cargo test --manifest-path src-tauri/Cargo.toml \
//!   --test memory_live -- --ignored --nocapture
//! ```
//!
//! Leaving `THIRD_EYE_THIN_MODEL` unset uses an unpinned lane (LM Studio's
//! loaded default), exactly like production's `with_default_endpoint`.

use std::path::PathBuf;
use std::sync::Arc;

use third_eye_lib::llm::guard::GuardState;
use third_eye_lib::llm::openai::DEFAULT_ENDPOINT;
use third_eye_lib::llm::router::{ModelRouter, THIN_LANE};
use third_eye_lib::llm::toolloop::{ToolCallEvent, ToolEvent, ToolResultEvent};
use third_eye_lib::llm::ToolCall;
use third_eye_lib::memory::chat_ingest::{self, ChatIngestState};
use third_eye_lib::memory::ingest::{run_loop, IngestState, BATCH_SIZE};
use third_eye_lib::memory::{
    search, MemorySource, MemoryStore, NewMemory, OpenAiEmbedder, SearchMode,
};
use third_eye_lib::watcher::{TextObservation, WatcherState};

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

/// File-level structural proof that the memory database is text-only
/// (R011/R012): after real inserts through the store surface, the on-disk
/// file holds no byte-array columns, no non-text stored values, and no
/// encoded image data.
#[test]
fn memory_db_file_is_text_only() {
    let scratch = ScratchDb::new("text-only");
    let store = MemoryStore::open(&scratch.path).expect("open file-backed store");
    for i in 0..3 {
        store
            .insert(NewMemory {
                summary: format!("closure proof memory {i}: text summaries only"),
                apps: vec!["Zed".into(), "Safari".into()],
                span_start_ms: 1_000 + i,
                span_end_ms: 2_000 + i,
                embedding: Some(vec![0.25, -0.5, 1.0]),
                source: MemorySource::Watcher,
                category: "other".into(),
                tags: Vec::new(),
                pinned: false,
                expires_at_ms: None,
            })
            .expect("insert");
    }
    // Closing the connection checkpoints the WAL into the main file, so the
    // byte scan below sees every stored page.
    drop(store);

    let conn = rusqlite::Connection::open(&scratch.path).expect("reopen raw db");

    // 1. Every column of every user table declares INTEGER or TEXT — never a
    //    byte-array type. FTS5's internal shadow tables (memories_fts_*) are
    //    SQLite bookkeeping over the same text content, not a storage
    //    surface, and are excluded the same way the schema excludes them
    //    from application writes.
    let tables: Vec<String> = conn
        .prepare(
            "SELECT name FROM sqlite_master
             WHERE type = 'table'
               AND name NOT LIKE 'sqlite_%'
               AND name NOT LIKE 'memories_fts%'",
        )
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert!(
        tables.contains(&"memories".to_string()),
        "memories table missing from {tables:?}"
    );
    for table in &tables {
        let mut stmt = conn
            .prepare(&format!("PRAGMA table_info({table})"))
            .unwrap();
        let cols: Vec<(String, String)> = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(1)?, row.get::<_, String>(2)?))
            })
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert!(!cols.is_empty(), "table {table} has no columns?");
        for (name, ty) in cols {
            assert!(
                matches!(ty.to_uppercase().as_str(), "INTEGER" | "TEXT"),
                "{table}.{name} declares disallowed type {ty:?}"
            );
        }
    }

    // 2. Every stored value really is text/integer/null — SQLite's type
    //    affinity would let a caller sneak raw bytes into a TEXT column, so
    //    check what is actually stored, not just what is declared.
    let mut stmt = conn
        .prepare(
            "SELECT DISTINCT typeof(id), typeof(summary), typeof(apps),
                    typeof(span_start_ms), typeof(span_end_ms),
                    typeof(embedding), typeof(created_at_ms),
                    typeof(updated_at_ms)
             FROM memories",
        )
        .unwrap();
    let mut rows = stmt.query([]).unwrap();
    let mut saw_rows = false;
    while let Some(row) = rows.next().unwrap() {
        saw_rows = true;
        for i in 0..8 {
            let ty: String = row.get(i).unwrap();
            assert!(
                matches!(ty.as_str(), "integer" | "text" | "null"),
                "column {i} stores a {ty} value"
            );
        }
    }
    assert!(saw_rows, "inserted rows must be visible in the raw file");
    drop(rows);
    drop(stmt);
    drop(conn);

    // 3. Byte-level scan: no PNG signature and no base64-encoded PNG prefix
    //    anywhere in the file. (The capture pipeline is PNG-only, so these
    //    two signatures are exactly what a frame leak would look like.)
    let bytes = std::fs::read(&scratch.path).expect("read raw db file");
    assert!(!bytes.is_empty());
    let png_magic: &[u8] = &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    let base64_png_prefix = b"iVBORw0KGgo";
    assert!(
        !bytes.windows(png_magic.len()).any(|w| w == png_magic),
        "raw db file contains a PNG signature"
    );
    assert!(
        !bytes
            .windows(base64_png_prefix.len())
            .any(|w| w == base64_png_prefix),
        "raw db file contains base64-encoded PNG data"
    );
}

fn observation(text: &str, app: &str, at: u64) -> TextObservation {
    TextObservation {
        text: text.into(),
        app_context: Some(app.into()),
        captured_at: at,
    }
}

/// Two clearly distinct work topics, each yielding one full ingest batch of
/// mutually non-duplicate observations — realistic OCR-shaped screen text.
fn topic_batches() -> Vec<TextObservation> {
    let baking: [&str; BATCH_SIZE] = [
        "sourdough starter day 5 feeding schedule 1:1:1 ratio rye flour bubbles doubling",
        "recipe notes sourdough bread hydration 75 percent autolyse 40 minutes",
        "bulk fermentation timing 4 hours stretch and fold every 30 minutes dough",
        "shaping the boule bench rest 20 minutes tension pull sourdough loaf",
        "banneton proofing overnight in fridge 12 hours cold retard bread dough",
        "oven preheat 250 celsius dutch oven scoring pattern wheat stalk blade",
        "bake covered 20 minutes uncovered 25 minutes crust color deep brown loaf",
        "crumb analysis open holes slight gumminess maybe underbaked next time longer",
    ];
    let kubernetes: [&str; BATCH_SIZE] = [
        "kubectl get pods -n prod CrashLoopBackOff payment-service restarts 14",
        "kubernetes cluster upgrade plan v1.29 to v1.30 control plane first then nodes",
        "helm diff upgrade payment-service chart values replicas 3 to 5 memory limit",
        "pod disruption budget minAvailable 2 rolling update maxSurge 1 deployment",
        "node pool cordon and drain sequence one node at a time workload eviction",
        "kubectl describe pod payment-service OOMKilled exit code 137 memory limit",
        "grafana dashboard p99 latency spike during rollout 800ms baseline 120ms",
        "post-upgrade validation checklist all deployments ready daemonsets healthy",
    ];
    baking
        .iter()
        .enumerate()
        .map(|(i, t)| observation(t, "Safari", 1_000 + i as u64))
        .chain(
            kubernetes
                .iter()
                .enumerate()
                .map(|(i, t)| observation(t, "Terminal", 10_000 + i as u64)),
        )
        .collect()
}

/// The roadmap demo, minus the multi-hour live screen session: synthetic
/// multi-topic observations flow through the real ingest loop (real thin-lane
/// distillation against LM Studio), and a topical search over the stored
/// summaries using real nomic embeddings recalls the right topic in semantic
/// mode.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires LM Studio serving a chat model and text-embedding-nomic-embed-text-v1.5 at DEFAULT_ENDPOINT"]
async fn live_distill_and_recall_against_lm_studio() {
    let thin_model = std::env::var("THIRD_EYE_THIN_MODEL")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let router = Arc::new(ModelRouter::thin_heavy(
        DEFAULT_ENDPOINT,
        thin_model,
        None,
        Arc::new(GuardState::new()),
    ));
    // Sanity: the lane resolves before spending time on the loop.
    router
        .lane_client(THIN_LANE)
        .expect("thin lane must resolve");

    let scratch = ScratchDb::new("live-recall");
    let store = Arc::new(MemoryStore::open(&scratch.path).expect("open store"));
    let ingest = Arc::new(IngestState::new());
    let watcher = WatcherState::new();
    let rx = watcher.subscribe();
    let task = tokio::spawn(run_loop(rx, store.clone(), ingest.clone(), router));

    for obs in topic_batches() {
        watcher.publish(obs);
    }
    // Closing the channel lets the loop drain both batches and exit; each
    // batch is one real distillation round-trip.
    drop(watcher);
    tokio::time::timeout(std::time::Duration::from_secs(300), task)
        .await
        .expect("ingest loop should finish two live distillations well inside 5 minutes")
        .expect("ingest loop must not panic");

    let status = ingest.status();
    eprintln!("live ingest status: {status:?}");
    assert!(
        status.last_error.is_none(),
        "live distillation failed: {:?}",
        status.last_error
    );
    assert!(
        status.distilled_count >= 1,
        "at least one batch must distill"
    );
    let stored = store.list(20, 0).expect("list stored memories");
    assert!(!stored.is_empty(), "distilled summaries must be stored");
    for rec in &stored {
        eprintln!("stored memory {}: {}", rec.id, rec.summary);
    }

    // Topical recall with real embeddings: a baking query must surface the
    // baking summary, in semantic mode, without any degrade.
    let embedder = OpenAiEmbedder::new(DEFAULT_ENDPOINT);
    let outcome = search(&store, &embedder, "baking sourdough bread at home", 3)
        .await
        .expect("search must not fail at the store layer");
    eprintln!(
        "search outcome: mode={:?} degrade={:?}",
        outcome.mode, outcome.degrade_reason
    );
    for rec in &outcome.results {
        eprintln!("search hit {}: {}", rec.id, rec.summary);
    }
    assert_eq!(
        outcome.mode,
        SearchMode::Semantic,
        "live embeddings must not degrade"
    );
    assert!(outcome.degrade_reason.is_none());
    let top = outcome
        .results
        .first()
        .expect("topical search must return results");
    let top_lower = top.summary.to_lowercase();
    assert!(
        ["sourdough", "bread", "baking", "dough", "loaf"]
            .iter()
            .any(|kw| top_lower.contains(kw)),
        "top hit should be the baking summary, got: {}",
        top.summary
    );
}

/// The production router shape against LM Studio, shared by the chat-ingest
/// live proofs below (same lane resolution as the app's
/// `with_default_endpoint` path).
fn live_router() -> Arc<ModelRouter> {
    let thin_model = std::env::var("THIRD_EYE_THIN_MODEL")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let router = Arc::new(ModelRouter::thin_heavy(
        DEFAULT_ENDPOINT,
        thin_model,
        None,
        Arc::new(GuardState::new()),
    ));
    // Sanity: the lane resolves before spending time on a live round-trip.
    router
        .lane_client(THIN_LANE)
        .expect("thin lane must resolve");
    router
}

/// M008 S01 (T05): the executable half of R032's restart-recall demo. A
/// realistic completed exchange — ask + verified tool outcome carrying the
/// typed search text in its arguments JSON + reply — distills on the real
/// thin lane into exactly one stored `source='chat'` memory, and
/// `memory_search` (real nomic embeddings) retrieves it by the searched
/// term. The quit/relaunch half of the demo is milestone-level UAT.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires LM Studio serving a chat model and text-embedding-nomic-embed-text-v1.5 at DEFAULT_ENDPOINT"]
async fn live_chat_exchange_distills_to_source_chat_memory_and_recalls() {
    let router = live_router();

    let events = vec![
        ToolEvent::Call(ToolCallEvent {
            request_id: 1,
            round: 0,
            call: ToolCall {
                id: "c1".into(),
                name: "type_text".into(),
                arguments: r#"{"text":"rust async traits"}"#.into(),
            },
        }),
        ToolEvent::Result(ToolResultEvent {
            request_id: 1,
            round: 0,
            call_id: "c1".into(),
            name: "type_text".into(),
            ok: true,
            result_count: None,
            mode: None,
            failure: None,
            preview: None,
        }),
    ];
    let exchange = chat_ingest::capture_exchange(
        "search for rust async traits in Chrome",
        &events,
        "I typed \"rust async traits\" into Chrome's search bar and submitted the search.",
    )
    .expect("a real exchange must capture");

    let scratch = ScratchDb::new("live-chat-recall");
    let store = Arc::new(MemoryStore::open(&scratch.path).expect("open store"));
    let state = Arc::new(ChatIngestState::new());
    tokio::time::timeout(
        std::time::Duration::from_secs(300),
        chat_ingest::ingest_exchange(state.clone(), router, store.clone(), exchange),
    )
    .await
    .expect("one live distillation must finish well inside 5 minutes");

    let status = state.status();
    eprintln!("live chat ingest status: {status:?}");
    assert!(
        status.last_error.is_none(),
        "live chat distillation failed: {:?}",
        status.last_error
    );
    assert_eq!(
        status.buffered, 0,
        "the exchange must not be retained after success"
    );

    let stored = store.list(10, 0).expect("list stored memories");
    for rec in &stored {
        eprintln!(
            "stored chat memory {} (source={:?}): {}",
            rec.id, rec.source, rec.summary
        );
    }
    assert_eq!(stored.len(), 1, "one exchange → exactly one stored memory");
    let memory = &stored[0];
    assert_eq!(memory.source, MemorySource::Chat);
    let summary_lower = memory.summary.to_lowercase();
    assert!(
        summary_lower.contains("rust") && summary_lower.contains("async"),
        "summary must mention the searched term, got: {}",
        memory.summary
    );

    // Recall by the searched term with real embeddings — the answer to
    // "what did I recently ask you to search for?" after a restart.
    let embedder = OpenAiEmbedder::new(DEFAULT_ENDPOINT);
    let outcome = search(&store, &embedder, "rust async traits", 3)
        .await
        .expect("search must not fail at the store layer");
    eprintln!(
        "chat recall outcome: mode={:?} degrade={:?}",
        outcome.mode, outcome.degrade_reason
    );
    assert_eq!(
        outcome.mode,
        SearchMode::Semantic,
        "live embeddings must not degrade"
    );
    let top = outcome
        .results
        .first()
        .expect("recall must return the chat memory");
    assert_eq!(top.id, memory.id, "top hit must be the stored chat memory");
}

/// M008 S02 (T04): the roadmap demo at test level — a multi-topic
/// conversation held past SESSION_SUMMARY_THRESHOLD distills on the real
/// thin lane into one rolling session-summary memory whose span covers the
/// session's oldest-to-newest exchange, and searching for one of the
/// conversation's themes surfaces that summary in memory_search results
/// (real nomic embeddings, semantic mode).
///
/// This answers S02's one mock-unanswerable risk: does a real 9B model
/// condense five distinct-topic exchanges into one faithful line, and does
/// that line search-retrieve semantically?
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires LM Studio serving a chat model and text-embedding-nomic-embed-text-v1.5 at DEFAULT_ENDPOINT"]
async fn live_multi_topic_session_summary_distills_and_recalls() {
    let router = live_router();

    /// A typed-search tool round (call + ok result), the realistic shape
    /// of the product's primary loop.
    fn typed_search(text: &str) -> Vec<ToolEvent> {
        vec![
            ToolEvent::Call(ToolCallEvent {
                request_id: 1,
                round: 0,
                call: ToolCall {
                    id: "c1".into(),
                    name: "type_text".into(),
                    arguments: format!(r#"{{"text":"{text}"}}"#),
                },
            }),
            ToolEvent::Result(ToolResultEvent {
                request_id: 1,
                round: 0,
                call_id: "c1".into(),
                name: "type_text".into(),
                ok: true,
                result_count: None,
                mode: None,
                failure: None,
                preview: None,
            }),
        ]
    }

    // Five clearly distinct topics — one session, SESSION_SUMMARY_THRESHOLD
    // completed exchanges. (ask, tool events, reply)
    let script: Vec<(&str, Vec<ToolEvent>, &str)> = vec![
        (
            "search for flights to Tokyo in Chrome",
            typed_search("flights to Tokyo late October"),
            "I typed \"flights to Tokyo late October\" into Chrome's search bar and submitted \
             the search.",
        ),
        (
            "check the payment-service pods on the kubernetes dashboard",
            Vec::new(),
            "I opened the Grafana kubernetes dashboard; payment-service shows 3/3 pods ready \
             after the v1.30 upgrade.",
        ),
        (
            "look up a sourdough hydration calculator",
            typed_search("sourdough hydration calculator 75 percent"),
            "I searched for a sourdough hydration calculator and opened the top result showing \
             a 75 percent hydration formula.",
        ),
        (
            "draft an email to Sam about the quarterly report in Mail",
            Vec::new(),
            "I drafted an email to Sam in Mail summarizing the quarterly report and left it \
             open for your review.",
        ),
        (
            "search for bouldering gyms near Brooklyn",
            typed_search("bouldering gyms near Brooklyn"),
            "I typed \"bouldering gyms near Brooklyn\" into Chrome and submitted the search.",
        ),
    ];
    let threshold = chat_ingest::SESSION_SUMMARY_THRESHOLD;
    assert_eq!(
        script.len(),
        threshold,
        "script must be exactly one session's worth"
    );

    let scratch = ScratchDb::new("live-session-summary");
    let store = Arc::new(MemoryStore::open(&scratch.path).expect("open store"));
    let state = Arc::new(ChatIngestState::new());

    // Capture each exchange immediately before ingesting it, so live
    // distillation latency guarantees strictly increasing captured_at_ms
    // and the summary's oldest..newest span is distinguishable from the
    // per-exchange point spans.
    let mut first_ts = None;
    let mut last_ts = 0_i64;
    for (ask, events, reply) in script {
        let exchange =
            chat_ingest::capture_exchange(ask, &events, reply).expect("a real exchange captures");
        first_ts.get_or_insert(exchange.captured_at_ms);
        last_ts = exchange.captured_at_ms;
        tokio::time::timeout(
            std::time::Duration::from_secs(300),
            chat_ingest::ingest_exchange(state.clone(), router.clone(), store.clone(), exchange),
        )
        .await
        .expect("one live distillation must finish well inside 5 minutes");
    }
    let first_ts = first_ts.expect("at least one exchange ran");
    assert!(
        last_ts > first_ts,
        "live distillation latency must separate capture timestamps ({first_ts}..{last_ts})"
    );

    let status = state.status();
    eprintln!("live session ingest status: {status:?}");
    assert!(
        status.last_error.is_none(),
        "live session distillation failed: {:?}",
        status.last_error
    );
    assert_eq!(
        status.buffered, 0,
        "no exchange may be retained after a clean session"
    );

    let stored = store.list(20, 0).expect("list stored memories");
    for rec in &stored {
        eprintln!(
            "stored memory {} (source={:?} span={}..{}): {}",
            rec.id, rec.source, rec.span_start_ms, rec.span_end_ms, rec.summary
        );
    }
    assert_eq!(
        status.ingested_count,
        stored.len() as u64,
        "ingestedCount must count stored session summaries alongside per-exchange memories"
    );

    // The rolling summary is the one memory whose span covers the session
    // oldest..newest; per-exchange memories are points in time.
    let summaries: Vec<_> = stored
        .iter()
        .filter(|r| r.span_start_ms == first_ts && r.span_end_ms == last_ts)
        .collect();
    assert_eq!(
        summaries.len(),
        1,
        "exactly one rolling session summary must span {first_ts}..{last_ts}"
    );
    let summary = summaries[0];
    assert_eq!(summary.source, MemorySource::Chat);
    eprintln!("rolling session summary: {}", summary.summary);

    // Faithful multi-topic line: the single sentence must name at least two
    // of the session's five distinct themes.
    let theme_groups: [&[&str]; 5] = [
        &["tokyo", "flight"],
        &["kubernetes", "payment", "pod", "grafana"],
        &["sourdough", "hydration", "bread"],
        &["email", "quarterly", "report", "mail"],
        &["boulder", "climbing", "gym"],
    ];
    let summary_lower = summary.summary.to_lowercase();
    let named = theme_groups
        .iter()
        .filter(|group| group.iter().any(|kw| summary_lower.contains(kw)))
        .count();
    assert!(
        named >= 2,
        "session summary must name at least two of the conversation's themes, \
         named {named}: {}",
        summary.summary
    );

    // The roadmap demo's search half: one theme's query surfaces the
    // rolling summary in memory_search results with real embeddings.
    let embedder = OpenAiEmbedder::new(DEFAULT_ENDPOINT);
    let outcome = search(&store, &embedder, "kubernetes payment service pods", 3)
        .await
        .expect("search must not fail at the store layer");
    eprintln!(
        "session recall outcome: mode={:?} degrade={:?}",
        outcome.mode, outcome.degrade_reason
    );
    for rec in &outcome.results {
        eprintln!("session recall hit {}: {}", rec.id, rec.summary);
    }
    assert_eq!(
        outcome.mode,
        SearchMode::Semantic,
        "live embeddings must not degrade"
    );
    assert!(outcome.degrade_reason.is_none());
    assert!(
        outcome.results.iter().any(|r| r.id == summary.id),
        "the rolling session summary must surface in theme search results, got ids {:?}",
        outcome.results.iter().map(|r| r.id).collect::<Vec<_>>()
    );
}

/// The other half of the one-line-or-NOTHING risk: a trivial greeting
/// exchange must store nothing. If the small thin-lane model ignores the
/// NOTHING instruction, this test's failure output records what it said
/// instead (one prompt-wording iteration is in scope; structure is not).
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires LM Studio serving a chat model at DEFAULT_ENDPOINT"]
async fn live_trivial_exchange_stores_nothing() {
    let router = live_router();

    let exchange = chat_ingest::capture_exchange("hi", &[], "Hello! How can I help you today?")
        .expect("even a trivial exchange captures; triviality is the distiller's call");

    let scratch = ScratchDb::new("live-chat-trivial");
    let store = Arc::new(MemoryStore::open(&scratch.path).expect("open store"));
    let state = Arc::new(ChatIngestState::new());
    tokio::time::timeout(
        std::time::Duration::from_secs(300),
        chat_ingest::ingest_exchange(state.clone(), router, store.clone(), exchange),
    )
    .await
    .expect("one live distillation must finish well inside 5 minutes");

    let status = state.status();
    eprintln!("live trivial ingest status: {status:?}");
    assert!(
        status.last_error.is_none(),
        "trivial distillation errored: {:?}",
        status.last_error
    );
    let stored = store.list(10, 0).expect("list stored memories");
    assert!(
        stored.is_empty(),
        "model over-summarized a greeting instead of replying NOTHING; it stored: {:?}",
        stored
            .iter()
            .map(|r| r.summary.as_str())
            .collect::<Vec<_>>()
    );
}
