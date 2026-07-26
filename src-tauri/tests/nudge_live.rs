//! S05 closure proof (T05): the roadmap demo's classify→nudge leg at test
//! level, against a real LM Studio thin lane.
//!
//! `live_classify_and_nudge_against_lm_studio` is `#[ignore]` because it
//! needs LM Studio serving a chat model at the project-default endpoint
//! (mirrors `memory_live::live_distill_and_recall_against_lm_studio`).
//! Run it explicitly at closeout:
//!
//! ```sh
//! THIRD_EYE_THIN_MODEL=<served-chat-model-id> \
//!   cargo test --manifest-path src-tauri/Cargo.toml \
//!   --test nudge_live -- --ignored --nocapture
//! ```
//!
//! Leaving `THIRD_EYE_THIN_MODEL` unset uses an unpinned lane (LM Studio's
//! loaded default), exactly like production's `with_default_endpoint`.
//! `THIRD_EYE_ENDPOINT` overrides the endpoint — since S04 T01 it is the
//! production config seam read by `with_default_endpoint` itself, and this
//! test resolves it through the same `env_endpoint` rules (trim, trailing
//! slash, blank = default) instead of a private re-implementation.

use std::sync::Arc;

use third_eye_lib::llm::commands::env_endpoint;
use third_eye_lib::llm::guard::GuardState;
use third_eye_lib::llm::router::{ModelRouter, THIN_LANE};
use third_eye_lib::nudge::{
    classification_round, NudgePayload, NudgeState, RoundOutcome, SkipReason,
};
use third_eye_lib::overlay::OverlayState;
use third_eye_lib::watcher::TextObservation;

fn observation(text: &str, app: &str, at: u64) -> TextObservation {
    TextObservation {
        text: text.into(),
        app_context: Some(app.into()),
        captured_at: at,
    }
}

/// An interval's worth of seeded observations shaped like the prompt's own
/// nudge-worthy examples: the same build error recurring across snapshots
/// while the user searches for it — unmistakably "stuck on an error".
fn stuck_on_error_batch() -> Vec<TextObservation> {
    let snapshots: [(&str, &str); 6] = [
        (
            "error[E0502]: cannot borrow `state` as mutable because it is also \
             borrowed as immutable --> src/detector.rs:214:9 cargo build failed",
            "Terminal",
        ),
        (
            "cargo build error[E0502]: cannot borrow `state` as mutable \
             note: immutable borrow occurs here src/detector.rs:198",
            "Terminal",
        ),
        (
            "rust E0502 cannot borrow as mutable because it is also borrowed \
             as immutable - Google Search",
            "Safari",
        ),
        (
            "stackoverflow.com - Why does the Rust borrow checker reject a \
             mutable borrow after an immutable one? 47 answers",
            "Safari",
        ),
        (
            "error[E0502]: cannot borrow `state` as mutable because it is also \
             borrowed as immutable --> src/detector.rs:214:9 cargo build failed \
             again same error",
            "Terminal",
        ),
        (
            "rust borrow checker E0502 fix split borrow NLL - Google Search \
             page 2 of results",
            "Safari",
        ),
    ];
    snapshots
        .iter()
        .enumerate()
        .map(|(i, (text, app))| observation(text, app, 1_000 + i as u64 * 5_000))
        .collect()
}

/// The classify→nudge demo over seeded observations: a real thin-lane
/// round-trip through `classification_round` must yield a nudge with the
/// pixel-free payload contract intact, and — after `record_shown` stamps the
/// cooldown — the very next round must be gate-suppressed before spending
/// any tokens, proving the live rate limit.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires LM Studio serving a chat model at DEFAULT_ENDPOINT"]
async fn live_classify_and_nudge_against_lm_studio() {
    let thin_model = std::env::var("THIRD_EYE_THIN_MODEL")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let endpoint = env_endpoint(std::env::var("THIRD_EYE_ENDPOINT").ok());
    eprintln!("live endpoint: {endpoint}");
    let router = Arc::new(ModelRouter::thin_heavy(
        &endpoint,
        thin_model,
        None,
        Arc::new(GuardState::new()),
    ));
    // Sanity: the lane resolves before spending time on the round.
    router
        .lane_client(THIN_LANE)
        .expect("thin lane must resolve");

    let state = NudgeState::new();
    let batch = stuck_on_error_batch();
    let now_ms = 1_700_000_000_000i64;
    let cooldown_secs = 300u64;

    let outcome = tokio::time::timeout(
        std::time::Duration::from_secs(120),
        classification_round(
            &state,
            &router,
            &batch,
            OverlayState::Hidden,
            now_ms,
            cooldown_secs,
        ),
    )
    .await
    .expect("one live classification must finish well inside 2 minutes");

    let payload = match outcome {
        RoundOutcome::Nudge(payload) => payload,
        other => panic!(
            "a user visibly stuck on the same build error must classify as \
             nudge-worthy, got {other:?} (status: {:?})",
            state.status()
        ),
    };
    eprintln!("live nudge message: {}", payload.message);

    // The wire contract on a live payload: kind-tagged, a real one-liner,
    // grounded in the newest seeded observation, and pixel-free by field set.
    assert_eq!(payload.kind, NudgePayload::KIND);
    assert!(
        !payload.message.trim().is_empty(),
        "nudge message must be usable"
    );
    let newest = batch.last().unwrap();
    assert_eq!(
        payload.screen_text, newest.text,
        "grounded in the newest observation"
    );
    assert_eq!(payload.app_context.as_deref(), Some("Safari"));
    assert_eq!(payload.captured_at_ms, newest.captured_at as i64);
    assert!(
        payload.memory_context.is_empty(),
        "classification_round leaves memory_context for the loop's best-effort attach"
    );

    // Live rate limit: showing the nudge stamps the cooldown, so the next
    // round is suppressed by the pure gate — zero thin-lane tokens spent.
    state.record_shown(payload, now_ms);
    let next = classification_round(
        &state,
        &router,
        &batch,
        OverlayState::Hidden,
        now_ms + 1_000,
        cooldown_secs,
    )
    .await;
    assert_eq!(next, RoundOutcome::Skipped(SkipReason::CoolingDown));
    let status = state.status();
    eprintln!("live nudge status: {status:?}");
    assert!(
        status.last_error.is_none(),
        "live classification failed: {:?}",
        status.last_error
    );
    assert_eq!(status.last_nudge_at_ms, Some(now_ms));
    assert_eq!(status.suppressed.cooling_down, 1);
}
