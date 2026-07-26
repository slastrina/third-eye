//! Nudge core (S05): pure eligibility policy, thin-lane prompt/parse, wire
//! payloads, and shared health state for the nudge detector.
//!
//! The core is runtime-free and cargo-testable: the gate that decides
//! whether a classification may run (evaluated BEFORE any LLM call, so a
//! disabled or cooling-down detector costs zero thin-lane tokens), the
//! prompt/parse contract mirroring `memory::ingest`'s distill pair, the
//! kind-tagged camelCase `nudge://` payload shapes, and [`NudgeState`] —
//! the health-as-value core shared by the detector loop, the hotkey summon
//! path, and the `nudge_status` IPC. The detector runtime lives at the
//! bottom of this file ([`spawn`] and the loop glue); the toggle applier and
//! IPC commands live in [`commands`].
//!
//! Failure contract: a nudge is disposable. Classification failures never
//! crash the loop and never retry a batch — the typed [`LlmError`] stays
//! queryable on [`NudgeStatus`] until a later classification succeeds.
//! Payloads are pixel-free by construction (text, app context, timestamps,
//! memory-context strings only — R011's structural guarantee).

pub mod commands;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};
use tokio::sync::broadcast::{self, error::RecvError};

use crate::llm::router::{ModelRouter, THIN_LANE};
use crate::llm::{ChatMessage, ChatRequest, LlmClient, LlmError};
use crate::overlay::OverlayState;
use crate::watcher::TextObservation;

/// Event carrying a [`NudgePayload`] when a nudge is shown.
pub const SHOW_EVENT: &str = "nudge://show";

/// Event carrying a [`DismissReason`] when the active nudge goes away.
pub const DISMISS_EVENT: &str = "nudge://dismiss";

/// Event carrying a [`NudgeStatus`] snapshot on every toggle/health change.
pub const STATE_EVENT: &str = "nudge://state";

/// Per-observation cap on text forwarded to the classifier — same bound as
/// ingest's distiller, so one dense screen cannot blow the thin context.
const SNIPPET_MAX_CHARS: usize = 1500;

/// Cap on the banner message extracted from the model reply. The banner is
/// a one-liner; a rambling model must not produce a paragraph-sized nudge.
pub const MAX_MESSAGE_CHARS: usize = 200;

// ---------------------------------------------------------------------------
// Eligibility policy
// ---------------------------------------------------------------------------

/// Why a classification round was suppressed. Each reason has its own
/// counter on [`NudgeStatus`] and its own log line — suppression is logged,
/// never silent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    /// The nudges off-switch (D019) is off.
    Disabled,
    /// The overlay is not `Hidden` — the user is already engaged with the
    /// panel (or an earlier nudge is still parked), so classifying would at
    /// best duplicate what is on screen.
    OverlayVisible,
    /// The configured cooldown since the last shown nudge has not elapsed.
    CoolingDown,
    /// No observations arrived this interval (paused/starved watcher).
    EmptyBatch,
}

impl SkipReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            SkipReason::Disabled => "disabled",
            SkipReason::OverlayVisible => "overlay-visible",
            SkipReason::CoolingDown => "cooling-down",
            SkipReason::EmptyBatch => "empty-batch",
        }
    }
}

/// The pure classification gate, evaluated BEFORE any LLM call (D019/D023:
/// a suppressed round costs zero thin-lane tokens). `Ok(())` means the
/// detector may classify this interval's batch; `Err` names the one reason
/// it must not, checked in fixed precedence order: disabled → overlay
/// visible → cooling down → empty batch.
pub fn classification_gate(
    enabled: bool,
    overlay: OverlayState,
    last_nudge_at_ms: Option<i64>,
    now_ms: i64,
    cooldown_secs: u64,
    batch_len: usize,
) -> Result<(), SkipReason> {
    if !enabled {
        return Err(SkipReason::Disabled);
    }
    if overlay != OverlayState::Hidden {
        return Err(SkipReason::OverlayVisible);
    }
    if let Some(last) = last_nudge_at_ms {
        // try_from + saturate: an absurd configured cooldown clamps to
        // "forever" instead of wrapping negative and disabling rate limiting.
        let cooldown_ms = i64::try_from(cooldown_secs)
            .unwrap_or(i64::MAX)
            .saturating_mul(1000);
        if now_ms.saturating_sub(last) < cooldown_ms {
            return Err(SkipReason::CoolingDown);
        }
    }
    if batch_len == 0 {
        return Err(SkipReason::EmptyBatch);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Prompt / parse (thin-lane contract)
// ---------------------------------------------------------------------------

/// The classification prompt: one system instruction plus the interval's
/// batch as a single user turn, each observation labeled with its app and
/// truncated to [`SNIPPET_MAX_CHARS`] (ingest's `distill_messages` shape).
pub fn classify_messages(batch: &[TextObservation]) -> Vec<ChatMessage> {
    let mut body = String::new();
    for obs in batch {
        let app = obs.app_context.as_deref().unwrap_or("unknown app");
        body.push_str(&format!("[{app}]\n"));
        body.extend(obs.text.chars().take(SNIPPET_MAX_CHARS));
        body.push_str("\n---\n");
    }
    vec![
        ChatMessage::system(
            "You watch snapshots of a user's recent screen activity and \
             decide whether a brief assistant nudge would clearly help right \
             now (an error they are stuck on, repeated searching, a tedious \
             manual task). Be conservative: most activity deserves no nudge. \
             Reply with exactly one line and nothing else: 'NO' when a nudge \
             is not clearly helpful, or 'YES: <one short friendly sentence \
             offering concrete help>'.",
        ),
        ChatMessage::user(body),
    ]
}

/// Model reply → nudge decision. Strict but marker-tolerant: the first
/// non-empty line (list markers stripped, like ingest's `parse_summaries`)
/// must start with `YES`/`NO` case-insensitively. `YES` needs a non-empty
/// message after the separator — a bare `YES` is unusable and parses as no
/// nudge, as does any reply outside the contract (conservative: garbage
/// never nudges). Messages are capped at [`MAX_MESSAGE_CHARS`].
pub fn parse_nudge_verdict(text: &str) -> Option<String> {
    let line = text
        .lines()
        .map(strip_list_marker)
        .find(|l| !l.is_empty())?;
    let lower = line.to_lowercase();
    if lower == "no" || lower.starts_with("no.") || lower.starts_with("no,") {
        return None;
    }
    if !lower.starts_with("yes") {
        return None;
    }
    let rest = line[3..]
        .trim()
        .trim_start_matches([':', ',', '-', '—', '.'])
        .trim();
    if rest.is_empty() {
        return None;
    }
    Some(rest.chars().take(MAX_MESSAGE_CHARS).collect())
}

/// Tolerate models that bullet or number their reply despite instructions:
/// `- YES: x`, `1. NO` all yield the bare line.
fn strip_list_marker(line: &str) -> &str {
    let l = line.trim().trim_start_matches(['-', '*', '•']).trim_start();
    let digits = l.chars().take_while(char::is_ascii_digit).count();
    if digits > 0 {
        if let Some(rest) = l[digits..]
            .strip_prefix('.')
            .or_else(|| l[digits..].strip_prefix(')'))
        {
            return rest.trim();
        }
    }
    l.trim_end()
}

// ---------------------------------------------------------------------------
// Wire payloads (nudge:// IPC contract)
// ---------------------------------------------------------------------------

/// The `nudge://show` payload. Kind-tagged camelCase like every event in the
/// app, and pixel-free by construction: text, app context, timestamps, and
/// memory-context strings only — no field can carry image data without
/// failing the serde test that pins this exact field set.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NudgePayload {
    /// Always [`NudgePayload::KIND`] — lets the frontend switch on kind if
    /// later slices add more overlay callouts.
    pub kind: String,
    /// The one-line banner message extracted from the classifier reply.
    pub message: String,
    /// Text of the triggering observation — the screen context a
    /// summon-from-nudge chat is grounded in.
    pub screen_text: String,
    /// Frontmost app of the triggering observation, when known.
    pub app_context: Option<String>,
    /// Capture time of the triggering observation (ms since Unix epoch).
    pub captured_at_ms: i64,
    /// Relevant memory summaries fetched at classification time; empty when
    /// the memory search degraded (a nudge never blocks on the embedder).
    pub memory_context: Vec<String>,
}

impl NudgePayload {
    pub const KIND: &'static str = "nudge";

    pub fn new(
        message: String,
        screen_text: String,
        app_context: Option<String>,
        captured_at_ms: i64,
        memory_context: Vec<String>,
    ) -> Self {
        Self {
            kind: Self::KIND.into(),
            message,
            screen_text,
            app_context,
            captured_at_ms,
            memory_context,
        }
    }
}

/// Why the active nudge went away — the `nudge://dismiss` payload
/// (kebab-case kind strings, matching every kind tag in the app).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum DismissReason {
    /// The auto-dismiss timer fired while the overlay was still idle.
    AutoTimeout,
    /// The hotkey summoned chat from the nudge — the banner yields to chat.
    Summoned,
    /// The user turned nudges off while one was showing.
    Disabled,
    /// The overlay was hidden out from under the nudge (hotkey dismiss).
    Hidden,
}

impl DismissReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            DismissReason::AutoTimeout => "auto-timeout",
            DismissReason::Summoned => "summoned",
            DismissReason::Disabled => "disabled",
            DismissReason::Hidden => "hidden",
        }
    }
}

// ---------------------------------------------------------------------------
// Shared state (health-as-value)
// ---------------------------------------------------------------------------

/// Per-reason suppression counters — the observability half of the gate:
/// every `Err` from [`classification_gate`] lands here, so "why has it
/// never nudged me" is answerable from `nudge_status` alone.
#[derive(Debug, Default)]
struct SuppressionCounters {
    disabled: AtomicU64,
    overlay_visible: AtomicU64,
    cooling_down: AtomicU64,
    empty_batch: AtomicU64,
}

/// The one shared nudge core: the detector loop (T02) mutates it, the
/// hotkey summon path reads `active()`, and `nudge_status` snapshots it.
/// Enabled defaults to on (D019 mandates the off-switch, not a default-off);
/// persistence and the single-applier live in `commands.rs` (T02).
pub struct NudgeState {
    enabled: AtomicBool,
    last_nudge_at_ms: Mutex<Option<i64>>,
    active: Mutex<Option<NudgePayload>>,
    last_error: Mutex<Option<LlmError>>,
    suppressed: SuppressionCounters,
    persist_error: Mutex<Option<String>>,
}

impl Default for NudgeState {
    fn default() -> Self {
        Self {
            enabled: AtomicBool::new(crate::config::NUDGES_ENABLED_DEFAULT),
            last_nudge_at_ms: Mutex::new(None),
            active: Mutex::new(None),
            last_error: Mutex::new(None),
            suppressed: SuppressionCounters::default(),
            persist_error: Mutex::new(None),
        }
    }
}

impl NudgeState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn enabled(&self) -> bool {
        self.enabled.load(Ordering::SeqCst)
    }

    /// Flip the in-memory toggle. Returns the previous value so the T02
    /// applier can roll back on a failed persist (watcher precedent).
    pub fn set_enabled(&self, enabled: bool) -> bool {
        self.enabled.swap(enabled, Ordering::SeqCst)
    }

    /// Whether a nudge is currently showing — the hotkey toggle's second
    /// input: pressing the hotkey on a parked nudge summons instead of
    /// dismissing.
    pub fn nudge_active(&self) -> bool {
        self.active.lock().unwrap().is_some()
    }

    /// The active nudge's payload, if any — the summon path reads it to
    /// hand chat its preload context without a new IPC round-trip.
    pub fn active(&self) -> Option<NudgePayload> {
        self.active.lock().unwrap().clone()
    }

    /// A nudge was shown: it becomes the active nudge, stamps the cooldown
    /// clock, and clears any persisted classification error (health rules:
    /// errors stay visible only until a success).
    pub fn record_shown(&self, payload: NudgePayload, at_ms: i64) {
        *self.active.lock().unwrap() = Some(payload);
        *self.last_nudge_at_ms.lock().unwrap() = Some(at_ms);
        *self.last_error.lock().unwrap() = None;
    }

    /// The active nudge went away (auto-dismiss, summon, disable, hide).
    /// Returns the payload that was active so the caller can log/forward it.
    pub fn clear_active(&self) -> Option<NudgePayload> {
        self.active.lock().unwrap().take()
    }

    /// A classification round completed without error and decided "no
    /// nudge": clears any persisted failure without touching the cooldown.
    pub fn record_no_nudge(&self) {
        *self.last_error.lock().unwrap() = None;
    }

    pub fn record_failure(&self, err: LlmError) {
        *self.last_error.lock().unwrap() = Some(err);
    }

    pub fn record_skip(&self, reason: SkipReason) {
        let counter = match reason {
            SkipReason::Disabled => &self.suppressed.disabled,
            SkipReason::OverlayVisible => &self.suppressed.overlay_visible,
            SkipReason::CoolingDown => &self.suppressed.cooling_down,
            SkipReason::EmptyBatch => &self.suppressed.empty_batch,
        };
        counter.fetch_add(1, Ordering::SeqCst);
    }

    pub fn set_persist_error(&self, error: Option<String>) {
        *self.persist_error.lock().unwrap() = error;
    }

    /// Snapshot for `nudge_status` — camelCase JSON, never an error, safe
    /// to poll.
    pub fn status(&self) -> NudgeStatus {
        NudgeStatus {
            enabled: self.enabled(),
            active: self.nudge_active(),
            last_nudge_at_ms: *self.last_nudge_at_ms.lock().unwrap(),
            last_error: self.last_error.lock().unwrap().clone(),
            suppressed: SuppressedCounts {
                disabled: self.suppressed.disabled.load(Ordering::SeqCst),
                overlay_visible: self.suppressed.overlay_visible.load(Ordering::SeqCst),
                cooling_down: self.suppressed.cooling_down.load(Ordering::SeqCst),
                empty_batch: self.suppressed.empty_batch.load(Ordering::SeqCst),
            },
            persist_error: self.persist_error.lock().unwrap().clone(),
        }
    }
}

/// Suppression counters as they cross IPC — camelCase like the rest of the
/// status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SuppressedCounts {
    pub disabled: u64,
    pub overlay_visible: u64,
    pub cooling_down: u64,
    pub empty_batch: u64,
}

/// The `nudge_status` / `nudge://state` shape. Health-as-value: never an
/// IPC error, the caller reads health out of the fields.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NudgeStatus {
    pub enabled: bool,
    pub active: bool,
    pub last_nudge_at_ms: Option<i64>,
    pub last_error: Option<LlmError>,
    pub suppressed: SuppressedCounts,
    pub persist_error: Option<String>,
}

// ---------------------------------------------------------------------------
// Detector loop (runtime)
// ---------------------------------------------------------------------------

/// Seconds between classification rounds. Each round takes the interval's
/// observation batch through the gate and spends at most one thin-lane call.
pub const DETECT_INTERVAL_SECS: u64 = 30;

/// Seconds a shown nudge stays parked before the auto-dismiss timer fires.
pub const AUTO_DISMISS_SECS: u64 = 12;

/// Bound on observations retained between rounds (drop-oldest). With the
/// watcher's ~5s cadence a 30s interval yields ~6 observations; the cap only
/// matters when rounds are suppressed back-to-back, and it keeps the
/// detector's memory flat at any observation volume (Q6).
pub const BATCH_CAP: usize = 32;

/// Memory summaries attached to a nudge payload, fetched best-effort at
/// classification time.
const MEMORY_CONTEXT_LIMIT: usize = 3;

/// Cap on the search query derived from the triggering screen text — recall
/// needs the gist, not the whole screen.
const MEMORY_QUERY_MAX_CHARS: usize = 300;

/// What one classification round decided — returned by
/// [`classification_round`] so the loop's side effects (memory fetch, show,
/// auto-dismiss arming) stay outside the testable core.
#[derive(Debug, Clone, PartialEq)]
pub enum RoundOutcome {
    /// The gate suppressed the round before any LLM call; the reason was
    /// counted on [`NudgeState`].
    Skipped(SkipReason),
    /// The model replied within contract: no nudge warranted.
    NoNudge,
    /// The classification failed; the typed error is on [`NudgeState`].
    Failed,
    /// A nudge is warranted — the payload is built from the batch's newest
    /// observation, with `memory_context` still empty (the loop attaches it
    /// best-effort before showing).
    Nudge(NudgePayload),
}

/// Retain `obs`, dropping the oldest retained observation once `cap` is
/// reached. Returns whether something was dropped (the loop logs it).
pub fn push_bounded(batch: &mut Vec<TextObservation>, obs: TextObservation, cap: usize) -> bool {
    let dropped = batch.len() >= cap;
    if dropped {
        batch.remove(0);
    }
    batch.push(obs);
    dropped
}

/// The auto-dismiss policy (must-have 2): fire only while the shown nudge is
/// still parked on an idle overlay. A summoned (focused) chat, an already
/// hidden window, or an already-cleared nudge all skip — a focused chat is
/// never hidden or cleared by auto-dismiss.
pub fn auto_dismiss_should_fire(nudge_active: bool, overlay: OverlayState) -> bool {
    nudge_active && overlay == OverlayState::VisibleIdle
}

/// One classification round: gate → thin-lane classify → verdict, mutating
/// only [`NudgeState`] (skip counters, typed errors, error-clearing NO).
/// Never panics and never retries — a nudge is disposable, so a failed or
/// suppressed round simply waits for the next interval.
pub async fn classification_round(
    state: &NudgeState,
    router: &ModelRouter,
    batch: &[TextObservation],
    overlay: OverlayState,
    now_ms: i64,
    cooldown_secs: u64,
) -> RoundOutcome {
    let last_nudge_at_ms = state.status().last_nudge_at_ms;
    if let Err(reason) = classification_gate(
        state.enabled(),
        overlay,
        last_nudge_at_ms,
        now_ms,
        cooldown_secs,
        batch.len(),
    ) {
        state.record_skip(reason);
        log::debug!("nudge: classification suppressed ({})", reason.as_str());
        return RoundOutcome::Skipped(reason);
    }
    // Snapshot per round so an S07 runtime re-pin applies to the next
    // classification without restarting the loop (ingest precedent).
    let (model, client) = match router.lane_client(THIN_LANE) {
        Ok(lane) => lane,
        Err(e) => {
            // Lane misconfiguration, not an LLM failure — logged, no typed
            // error (same posture as ingest's thin-lane-unavailable path).
            log::error!("nudge: thin lane unavailable: {e}");
            return RoundOutcome::Failed;
        }
    };
    log::info!(
        "nudge: classification start: {} observations via lane={THIN_LANE} model={model}",
        batch.len()
    );
    let messages = classify_messages(batch);
    match client
        .stream_chat(&ChatRequest::new(messages), &|_| {})
        .await
    {
        Ok(outcome) => match parse_nudge_verdict(&outcome.text) {
            Some(message) => {
                let trigger = batch.last().expect("gate rejects empty batches");
                log::info!(
                    "nudge: classification verdict: nudge ({} chars) via lane={THIN_LANE} \
                     model={model}",
                    message.len()
                );
                RoundOutcome::Nudge(NudgePayload::new(
                    message,
                    trigger.text.clone(),
                    trigger.app_context.clone(),
                    trigger.captured_at as i64,
                    Vec::new(),
                ))
            }
            None => {
                log::info!(
                    "nudge: classification verdict: no-nudge via lane={THIN_LANE} model={model}"
                );
                state.record_no_nudge();
                RoundOutcome::NoNudge
            }
        },
        Err(err) => {
            log::error!("nudge: classification failed ({}): {err}", err.kind());
            state.record_failure(err);
            RoundOutcome::Failed
        }
    }
}

/// Spawn the nudge detector for the app's lifetime, subscribed to the
/// watcher's observation broadcast (the S01 seam's second consumer). Called
/// once from `setup()` after `memory::ingest::spawn`. Like the watcher loop
/// there is no task lifecycle: the D019 toggle changes what a round does,
/// not whether the task exists.
pub fn spawn(app: &AppHandle) {
    let rx = app.state::<crate::watcher::WatcherState>().subscribe();
    let router = app.state::<crate::llm::commands::LlmState>().router();
    let cooldown_secs = crate::config::load_nudge_cooldown_secs(app);
    log::info!(
        "nudge: detector starting (interval {DETECT_INTERVAL_SECS}s, cooldown {cooldown_secs}s)"
    );
    tauri::async_runtime::spawn(run_loop(app.clone(), rx, router, cooldown_secs));
}

/// The loop body: buffer observations between interval ticks, then run one
/// classification round over the interval's batch. The batch is taken every
/// round whatever the outcome — a nudge is about *now*, so suppressed or
/// failed rounds discard their observations instead of retrying stale
/// screen text. Exits only when the observation channel closes (shutdown).
async fn run_loop(
    app: AppHandle,
    mut rx: broadcast::Receiver<TextObservation>,
    router: Arc<ModelRouter>,
    cooldown_secs: u64,
) {
    let mut batch: Vec<TextObservation> = Vec::new();
    let period = Duration::from_secs(DETECT_INTERVAL_SECS);
    let mut interval = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            received = rx.recv() => match received {
                Ok(obs) => {
                    if push_bounded(&mut batch, obs, BATCH_CAP) {
                        log::debug!(
                            "nudge: batch full ({BATCH_CAP}); dropped oldest observation"
                        );
                    }
                }
                Err(RecvError::Lagged(n)) => log::warn!(
                    "nudge: detector lagged behind the watcher; skipped {n} observations"
                ),
                Err(RecvError::Closed) => {
                    log::info!("nudge: observation channel closed; detector exiting");
                    break;
                }
            },
            _ = interval.tick() => {
                let round = std::mem::take(&mut batch);
                let state = app.state::<NudgeState>();
                let overlay = app.state::<crate::overlay::OverlayManager>().current();
                let outcome = classification_round(
                    &state, &router, &round, overlay, now_ms(), cooldown_secs,
                ).await;
                if let RoundOutcome::Nudge(mut payload) = outcome {
                    payload.memory_context =
                        fetch_memory_context(&app, &payload.screen_text).await;
                    show_nudge(&app, payload);
                }
            }
        }
    }
}

/// Best-effort memory recall for the payload's `memory_context`: any
/// unavailable store, failed search, or degraded embedder yields fewer (or
/// zero) summaries, never an error — a nudge never blocks on the embedder.
async fn fetch_memory_context(app: &AppHandle, screen_text: &str) -> Vec<String> {
    let memory = app.state::<crate::memory::MemoryState>();
    let Some(store) = memory.store() else {
        log::debug!("nudge: memory store unavailable; nudging without memory context");
        return Vec::new();
    };
    let router = app.state::<crate::llm::commands::LlmState>().router();
    let embedder = memory.embedder(router.endpoint());
    let query: String = screen_text.chars().take(MEMORY_QUERY_MAX_CHARS).collect();
    match crate::memory::search(&store, embedder.as_ref(), &query, MEMORY_CONTEXT_LIMIT).await {
        Ok(outcome) => outcome.results.into_iter().map(|r| r.summary).collect(),
        Err(e) => {
            log::warn!(
                "nudge: memory context lookup failed ({}); nudging without it: {e}",
                e.kind()
            );
            Vec::new()
        }
    }
}

/// Show the nudge: overlay to `visible-idle` (click-through and
/// nonactivating by the overlay's own cursor policy plus the Accessory
/// activation policy — structurally no focus steal), then record it active,
/// broadcast `nudge://show` + `nudge://state`, and arm the auto-dismiss
/// timer. A failed show (e.g. racing a summon that already made the window
/// visible) drops the nudge without stamping the cooldown.
fn show_nudge(app: &AppHandle, payload: NudgePayload) {
    match crate::overlay::show_overlay(app.clone()) {
        Ok(state) => {
            let nudge_state = app.state::<NudgeState>();
            nudge_state.record_shown(payload.clone(), now_ms());
            if let Err(e) = app.emit(SHOW_EVENT, &payload) {
                log::warn!("nudge: {SHOW_EVENT} broadcast failed: {e}");
            }
            commands::emit_state(app, nudge_state.status());
            log::info!(
                "nudge: shown (state={}, auto-dismiss in {AUTO_DISMISS_SECS}s)",
                state.as_str()
            );
            let app = app.clone();
            tauri::async_runtime::spawn(async move {
                tokio::time::sleep(Duration::from_secs(AUTO_DISMISS_SECS)).await;
                auto_dismiss(&app);
            });
        }
        Err(e) => {
            log::warn!("nudge: overlay show failed; dropping nudge: {e}");
        }
    }
}

/// The armed auto-dismiss: consult [`auto_dismiss_should_fire`] — only a
/// still-parked nudge on a still-idle overlay comes down. A summoned chat,
/// a manual hide, or a disable in the meantime all leave this a no-op.
fn auto_dismiss(app: &AppHandle) {
    let nudge_state = app.state::<NudgeState>();
    let overlay = app.state::<crate::overlay::OverlayManager>().current();
    if !auto_dismiss_should_fire(nudge_state.nudge_active(), overlay) {
        log::debug!(
            "nudge: auto-dismiss skipped (active={}, overlay={})",
            nudge_state.nudge_active(),
            overlay.as_str()
        );
        return;
    }
    nudge_state.clear_active();
    if let Err(e) = app.emit(DISMISS_EVENT, DismissReason::AutoTimeout) {
        log::warn!("nudge: {DISMISS_EVENT} broadcast failed: {e}");
    }
    if let Err(e) = crate::overlay::hide_overlay(app.clone()) {
        log::error!("nudge: auto-dismiss hide failed: {e}");
    }
    commands::emit_state(app, nudge_state.status());
    log::info!(
        "nudge: auto-dismissed ({})",
        DismissReason::AutoTimeout.as_str()
    );
}

/// Milliseconds since the Unix epoch — cooldown stamps and `nudge://show`.
fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::Role;
    use OverlayState::*;

    fn obs(text: &str, app: Option<&str>, at: u64) -> TextObservation {
        TextObservation {
            text: text.into(),
            app_context: app.map(Into::into),
            captured_at: at,
        }
    }

    fn payload() -> NudgePayload {
        NudgePayload::new(
            "Stuck on that borrow error? I can help.".into(),
            "error[E0502]: cannot borrow `buf` as mutable".into(),
            Some("Zed".into()),
            1_752_900_000_000,
            vec!["User debugged the ingest pipeline in Rust.".into()],
        )
    }

    // --- classification gate ---

    #[test]
    fn gate_passes_when_enabled_hidden_cooled_and_batched() {
        assert_eq!(
            classification_gate(true, Hidden, None, 1_000, 300, 3),
            Ok(())
        );
        // A prior nudge older than the cooldown does not suppress.
        assert_eq!(
            classification_gate(true, Hidden, Some(0), 300_000, 300, 1),
            Ok(()),
            "an exactly-elapsed cooldown must pass"
        );
    }

    #[test]
    fn gate_skips_when_disabled() {
        assert_eq!(
            classification_gate(false, Hidden, None, 1_000, 300, 3),
            Err(SkipReason::Disabled)
        );
    }

    #[test]
    fn gate_skips_when_overlay_is_visible() {
        // Idle (a parked nudge) and focused (an open chat) both suppress —
        // the user is already engaged with the overlay.
        for state in [VisibleIdle, VisibleFocused] {
            assert_eq!(
                classification_gate(true, state, None, 1_000, 300, 3),
                Err(SkipReason::OverlayVisible),
                "from {state:?}"
            );
        }
    }

    #[test]
    fn gate_skips_while_cooling_down() {
        assert_eq!(
            classification_gate(true, Hidden, Some(0), 299_999, 300, 3),
            Err(SkipReason::CoolingDown)
        );
    }

    #[test]
    fn gate_skips_an_empty_batch() {
        // A paused watcher publishes nothing → the detector starves — the
        // structural privacy-pause inheritance (must-have 4).
        assert_eq!(
            classification_gate(true, Hidden, None, 1_000, 300, 0),
            Err(SkipReason::EmptyBatch)
        );
    }

    #[test]
    fn gate_precedence_disabled_wins_over_everything() {
        // One reason per round: the counters must attribute a suppressed
        // round to the highest-precedence cause, deterministically.
        assert_eq!(
            classification_gate(false, VisibleFocused, Some(0), 1, 300, 0),
            Err(SkipReason::Disabled)
        );
        assert_eq!(
            classification_gate(true, VisibleIdle, Some(0), 1, 300, 0),
            Err(SkipReason::OverlayVisible)
        );
        assert_eq!(
            classification_gate(true, Hidden, Some(0), 1, 300, 0),
            Err(SkipReason::CoolingDown)
        );
    }

    #[test]
    fn gate_survives_a_huge_cooldown_without_overflow() {
        // u64::MAX seconds * 1000 must saturate, not wrap into "elapsed".
        assert_eq!(
            classification_gate(true, Hidden, Some(0), i64::MAX - 1, u64::MAX, 1),
            Err(SkipReason::CoolingDown)
        );
    }

    #[test]
    fn skip_reasons_have_stable_log_strings() {
        assert_eq!(SkipReason::Disabled.as_str(), "disabled");
        assert_eq!(SkipReason::OverlayVisible.as_str(), "overlay-visible");
        assert_eq!(SkipReason::CoolingDown.as_str(), "cooling-down");
        assert_eq!(SkipReason::EmptyBatch.as_str(), "empty-batch");
    }

    // --- prompt ---

    #[test]
    fn classify_messages_label_apps_and_truncate_long_text() {
        let long = "x".repeat(SNIPPET_MAX_CHARS * 2);
        let batch = vec![
            obs("cargo build failed", Some("Terminal"), 1),
            obs(&long, None, 2),
        ];
        let messages = classify_messages(&batch);
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, Role::System);
        assert!(
            messages[0].content.contains("YES:") && messages[0].content.contains("NO"),
            "system prompt must state the reply contract"
        );
        let body = &messages[1].content;
        assert!(body.contains("[Terminal]"), "app label missing: {body}");
        assert!(body.contains("[unknown app]"), "fallback label missing");
        assert!(body.contains("cargo build failed"));
        assert!(
            body.len() < SNIPPET_MAX_CHARS + 200,
            "long observation must be truncated (got {} chars)",
            body.len()
        );
    }

    // --- parse ---

    #[test]
    fn parse_accepts_yes_with_message_across_separators() {
        for reply in [
            "YES: Want a hand with that borrow error?",
            "yes - Want a hand with that borrow error?",
            "Yes, Want a hand with that borrow error?",
            "  YES:   Want a hand with that borrow error?  ",
            "- YES: Want a hand with that borrow error?",
            "1. YES: Want a hand with that borrow error?",
        ] {
            assert_eq!(
                parse_nudge_verdict(reply).as_deref(),
                Some("Want a hand with that borrow error?"),
                "reply: {reply:?}"
            );
        }
    }

    #[test]
    fn parse_takes_the_first_non_empty_line_only() {
        let verdict = parse_nudge_verdict("\n\nYES: First line wins.\nNO\nextra prose");
        assert_eq!(verdict.as_deref(), Some("First line wins."));
    }

    #[test]
    fn parse_treats_no_as_no_nudge() {
        for reply in ["NO", "no", "No.", "no, nothing helpful here", "* NO"] {
            assert_eq!(parse_nudge_verdict(reply), None, "reply: {reply:?}");
        }
    }

    #[test]
    fn parse_rejects_garbage_and_bare_yes_conservatively() {
        // Anything outside the contract must never nudge (Q7): a nudge on
        // a hallucinated format would interrupt the user for noise.
        for reply in [
            "",
            "   \n  ",
            "YES",
            "YES:",
            "yes -  ",
            "Maybe you need help?",
            "The user is compiling Rust code.",
            "I cannot determine whether a nudge is warranted.",
        ] {
            assert_eq!(parse_nudge_verdict(reply), None, "reply: {reply:?}");
        }
    }

    #[test]
    fn parse_caps_runaway_messages() {
        let long = format!("YES: {}", "help ".repeat(200));
        let message = parse_nudge_verdict(&long).unwrap();
        assert_eq!(message.chars().count(), MAX_MESSAGE_CHARS);
    }

    // --- wire payloads ---

    #[test]
    fn nudge_payload_serializes_camel_case_with_pinned_field_set() {
        // R011 structural proof at the IPC boundary: exactly these six
        // fields, camelCase, kind-tagged — no pixel/image field can ride
        // along without failing this test.
        let v = serde_json::to_value(payload()).unwrap();
        assert_eq!(v["kind"], "nudge");
        assert_eq!(v["message"], "Stuck on that borrow error? I can help.");
        assert_eq!(
            v["screenText"],
            "error[E0502]: cannot borrow `buf` as mutable"
        );
        assert_eq!(v["appContext"], "Zed");
        assert_eq!(v["capturedAtMs"], 1_752_900_000_000i64);
        assert_eq!(
            v["memoryContext"][0],
            "User debugged the ingest pipeline in Rust."
        );
        let mut keys: Vec<_> = v.as_object().unwrap().keys().cloned().collect();
        keys.sort();
        assert_eq!(
            keys,
            [
                "appContext",
                "capturedAtMs",
                "kind",
                "memoryContext",
                "message",
                "screenText"
            ],
            "unexpected field set: {keys:?}"
        );
    }

    #[test]
    fn dismiss_reasons_serialize_kebab_case() {
        assert_eq!(
            serde_json::to_value(DismissReason::AutoTimeout).unwrap(),
            "auto-timeout"
        );
        assert_eq!(
            serde_json::to_value(DismissReason::Summoned).unwrap(),
            "summoned"
        );
        assert_eq!(
            serde_json::to_value(DismissReason::Disabled).unwrap(),
            "disabled"
        );
        assert_eq!(
            serde_json::to_value(DismissReason::Hidden).unwrap(),
            "hidden"
        );
        assert_eq!(DismissReason::AutoTimeout.as_str(), "auto-timeout");
    }

    // --- state ---

    #[test]
    fn state_defaults_to_enabled_with_nothing_active() {
        let state = NudgeState::new();
        assert!(
            state.enabled(),
            "D019: default on — the off-switch is the feature"
        );
        assert!(!state.nudge_active());
        assert!(state.active().is_none());
    }

    #[test]
    fn set_enabled_swaps_and_returns_previous_for_rollback() {
        let state = NudgeState::new();
        assert!(state.set_enabled(false), "previous value was on");
        assert!(!state.enabled());
        assert!(!state.set_enabled(true));
        assert!(state.enabled());
    }

    #[test]
    fn record_shown_arms_active_stamps_cooldown_and_clears_error() {
        let state = NudgeState::new();
        state.record_failure(LlmError::Offline {
            endpoint: "http://x:1".into(),
            detail: "down".into(),
        });
        state.record_shown(payload(), 1234);
        assert!(state.nudge_active());
        assert_eq!(state.active().unwrap().message, payload().message);
        let status = state.status();
        assert_eq!(status.last_nudge_at_ms, Some(1234));
        assert!(
            status.last_error.is_none(),
            "a shown nudge is a success — error cleared"
        );
    }

    #[test]
    fn clear_active_returns_the_payload_once() {
        let state = NudgeState::new();
        state.record_shown(payload(), 1);
        assert_eq!(state.clear_active(), Some(payload()));
        assert_eq!(state.clear_active(), None, "second clear finds nothing");
        assert!(!state.nudge_active());
        // The cooldown clock survives the dismiss.
        assert_eq!(state.status().last_nudge_at_ms, Some(1));
    }

    #[test]
    fn no_nudge_verdict_clears_error_without_touching_cooldown() {
        let state = NudgeState::new();
        state.record_shown(payload(), 77);
        state.clear_active();
        state.record_failure(LlmError::Offline {
            endpoint: "http://x:1".into(),
            detail: "down".into(),
        });
        state.record_no_nudge();
        let status = state.status();
        assert!(status.last_error.is_none());
        assert_eq!(
            status.last_nudge_at_ms,
            Some(77),
            "a NO verdict must not reset cooldown"
        );
    }

    #[test]
    fn skips_count_per_reason() {
        let state = NudgeState::new();
        state.record_skip(SkipReason::Disabled);
        state.record_skip(SkipReason::Disabled);
        state.record_skip(SkipReason::CoolingDown);
        state.record_skip(SkipReason::OverlayVisible);
        state.record_skip(SkipReason::EmptyBatch);
        let s = state.status().suppressed;
        assert_eq!(
            (s.disabled, s.overlay_visible, s.cooling_down, s.empty_batch),
            (2, 1, 1, 1)
        );
    }

    #[test]
    fn status_serializes_camel_case() {
        // nudge_status and the S05 UI read exactly these keys; a change
        // here is a breaking IPC change.
        let state = NudgeState::new();
        state.record_skip(SkipReason::CoolingDown);
        state.record_failure(LlmError::Offline {
            endpoint: "http://x:1".into(),
            detail: "down".into(),
        });
        state.set_persist_error(Some("disk full".into()));
        let v = serde_json::to_value(state.status()).unwrap();
        assert_eq!(v["enabled"], true);
        assert_eq!(v["active"], false);
        assert!(v["lastNudgeAtMs"].is_null());
        assert_eq!(v["lastError"]["kind"], "offline");
        assert_eq!(v["suppressed"]["coolingDown"], 1);
        assert_eq!(v["suppressed"]["overlayVisible"], 0);
        assert_eq!(v["suppressed"]["disabled"], 0);
        assert_eq!(v["suppressed"]["emptyBatch"], 0);
        assert_eq!(v["persistError"], "disk full");
    }

    // --- detector: bounded batching + auto-dismiss policy ---

    #[test]
    fn push_bounded_drops_oldest_at_cap() {
        let mut batch = Vec::new();
        for i in 0..3 {
            assert!(!push_bounded(&mut batch, obs(&format!("t{i}"), None, i), 3));
        }
        assert!(
            push_bounded(&mut batch, obs("t3", None, 3), 3),
            "cap reached → drop"
        );
        assert_eq!(batch.len(), 3);
        assert_eq!(batch.first().unwrap().text, "t1", "oldest must be gone");
        assert_eq!(batch.last().unwrap().text, "t3");
    }

    #[test]
    fn auto_dismiss_fires_only_for_a_parked_nudge_on_an_idle_overlay() {
        assert!(auto_dismiss_should_fire(true, VisibleIdle));
        // A summoned chat is never hidden or cleared by auto-dismiss
        // (must-have 2), nor is an already hidden window or a cleared nudge.
        assert!(!auto_dismiss_should_fire(true, VisibleFocused));
        assert!(!auto_dismiss_should_fire(true, Hidden));
        for overlay in [Hidden, VisibleIdle, VisibleFocused] {
            assert!(
                !auto_dismiss_should_fire(false, overlay),
                "overlay {overlay:?}"
            );
        }
    }

    // --- detector: classification rounds (scripted thin lane) ---

    use crate::llm::router::Lane;
    use crate::llm::{LlmHealth, StreamOutcome, TokenSink};
    use async_trait::async_trait;
    use std::sync::atomic::AtomicUsize;

    /// Scripted lane client: fails its first `fail_first` calls with a typed
    /// offline error, then replies with `reply`. Records every call.
    struct ScriptedClient {
        reply: String,
        fail_first: usize,
        calls: AtomicUsize,
        last_messages: Mutex<Vec<ChatMessage>>,
    }

    impl ScriptedClient {
        fn ok(reply: &str) -> Arc<Self> {
            Arc::new(Self {
                reply: reply.into(),
                fail_first: 0,
                calls: AtomicUsize::new(0),
                last_messages: Mutex::new(Vec::new()),
            })
        }

        fn failing() -> Arc<Self> {
            Arc::new(Self {
                reply: String::new(),
                fail_first: usize::MAX,
                calls: AtomicUsize::new(0),
                last_messages: Mutex::new(Vec::new()),
            })
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl LlmClient for ScriptedClient {
        fn endpoint(&self) -> &str {
            "http://mock.invalid"
        }

        async fn stream_chat(
            &self,
            request: &ChatRequest,
            _on_token: TokenSink<'_>,
        ) -> Result<StreamOutcome, LlmError> {
            *self.last_messages.lock().unwrap() = request.messages.clone();
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            if call < self.fail_first {
                return Err(LlmError::Offline {
                    endpoint: self.endpoint().into(),
                    detail: "connection refused".into(),
                });
            }
            Ok(StreamOutcome {
                text: self.reply.clone(),
                token_count: 1,
                tool_calls: Vec::new(),
            })
        }

        async fn health(&self) -> LlmHealth {
            LlmHealth {
                online: true,
                endpoint: self.endpoint().into(),
            }
        }
    }

    fn thin_router(client: Arc<ScriptedClient>) -> ModelRouter {
        ModelRouter::new(vec![Lane::new(THIN_LANE, Some("thin-test".into()), client)])
    }

    fn batch_of(n: usize) -> Vec<TextObservation> {
        (0..n)
            .map(|i| obs(&format!("observation {i}"), Some("Zed"), 1000 + i as u64))
            .collect()
    }

    #[tokio::test]
    async fn round_records_the_skip_and_never_calls_the_model_when_gated() {
        let state = NudgeState::new();
        state.set_enabled(false);
        let client = ScriptedClient::ok("YES: never sent");
        let outcome = classification_round(
            &state,
            &thin_router(client.clone()),
            &batch_of(3),
            Hidden,
            1_000,
            300,
        )
        .await;
        assert_eq!(outcome, RoundOutcome::Skipped(SkipReason::Disabled));
        assert_eq!(
            client.calls(),
            0,
            "a suppressed round costs zero thin-lane tokens"
        );
        assert_eq!(state.status().suppressed.disabled, 1);
        assert!(!state.nudge_active());
    }

    #[tokio::test]
    async fn round_failure_persists_the_typed_error_and_arms_nothing() {
        let state = NudgeState::new();
        let client = ScriptedClient::failing();
        let outcome = classification_round(
            &state,
            &thin_router(client),
            &batch_of(2),
            Hidden,
            1_000,
            300,
        )
        .await;
        assert_eq!(outcome, RoundOutcome::Failed);
        assert!(!state.nudge_active(), "a failed round never shows a banner");
        let status = state.status();
        assert_eq!(
            status.last_error.as_ref().map(|e| e.kind()),
            Some("offline")
        );
        assert!(
            status.last_nudge_at_ms.is_none(),
            "failure must not stamp the cooldown"
        );
    }

    #[tokio::test]
    async fn round_no_verdict_clears_the_error_and_arms_nothing() {
        let state = NudgeState::new();
        state.record_failure(LlmError::Offline {
            endpoint: "http://x:1".into(),
            detail: "down".into(),
        });
        let outcome = classification_round(
            &state,
            &thin_router(ScriptedClient::ok("NO")),
            &batch_of(2),
            Hidden,
            1_000,
            300,
        )
        .await;
        assert_eq!(outcome, RoundOutcome::NoNudge);
        assert!(!state.nudge_active());
        let status = state.status();
        assert!(
            status.last_error.is_none(),
            "a within-contract NO is a success"
        );
        assert!(
            status.last_nudge_at_ms.is_none(),
            "a NO verdict must not stamp the cooldown"
        );
    }

    #[tokio::test]
    async fn round_yes_builds_the_payload_from_the_newest_observation() {
        let state = NudgeState::new();
        let client = ScriptedClient::ok("YES: Want a hand with that borrow error?");
        let mut batch = batch_of(2);
        batch.push(obs("error[E0502]: cannot borrow", Some("Terminal"), 2_000));
        let outcome = classification_round(
            &state,
            &thin_router(client.clone()),
            &batch,
            Hidden,
            1_000,
            300,
        )
        .await;
        let RoundOutcome::Nudge(payload) = outcome else {
            panic!("expected a nudge, got {outcome:?}");
        };
        assert_eq!(payload.message, "Want a hand with that borrow error?");
        assert_eq!(payload.screen_text, "error[E0502]: cannot borrow");
        assert_eq!(payload.app_context.as_deref(), Some("Terminal"));
        assert_eq!(payload.captured_at_ms, 2_000);
        assert!(
            payload.memory_context.is_empty(),
            "memory attaches later, best-effort"
        );
        // Showing (and the cooldown stamp) is the loop's job after the show
        // side effect succeeds — the round itself arms nothing.
        assert!(!state.nudge_active());
        assert!(state.status().last_nudge_at_ms.is_none());
        // The prompt actually carried the batch.
        let sent = client.last_messages.lock().unwrap().clone();
        assert!(sent[1].content.contains("observation 0"));
        assert!(sent[1].content.contains("[Terminal]"));
    }

    #[tokio::test]
    async fn round_stays_pinned_to_thin_while_active_lane_is_heavy() {
        use crate::llm::router::HEAVY_LANE;
        let thin = ScriptedClient::ok("NO");
        let heavy = ScriptedClient::ok("YES: wrong lane");
        let router = ModelRouter::new(vec![
            Lane::new(THIN_LANE, Some("thin-test".into()), thin.clone()),
            Lane::new(HEAVY_LANE, Some("heavy-test".into()), heavy.clone()),
        ]);
        router.set_active(HEAVY_LANE).unwrap();
        let state = NudgeState::new();
        let outcome = classification_round(&state, &router, &batch_of(2), Hidden, 1_000, 300).await;
        assert_eq!(outcome, RoundOutcome::NoNudge);
        assert_eq!(thin.calls(), 1, "classification must ride the thin lane");
        assert_eq!(
            heavy.calls(),
            0,
            "the user's active lane must never see nudge traffic"
        );
    }
}
