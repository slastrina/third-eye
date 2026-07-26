//! Continuous watcher (S01, R011): a ~5s-cadence background loop that
//! captures the primary display, extracts on-screen text on-device through
//! the [`crate::ocr::OcrEngine`] seam, and broadcasts
//! [`TextObservation`]s — extract-and-discard, so no pixel type appears
//! anywhere in this module and nothing here ever touches the PNG encoder.
//!
//! Control surface clones the privacy-mode pattern (MEM049/MEM053): one
//! shared [`WatcherState`] mutated only through
//! [`commands::apply_watcher_enabled`], persisted as `watcherEnabled` in
//! settings.json, broadcast as `watcher://state`, queryable health-as-value
//! via `watcher_status`.
//!
//! The loop itself never owns policy: every tick re-derives what to do from
//! the pure [`decide_run_state`] (privacy on → `paused-privacy`, disabled →
//! `idle`), so the gating is unit-testable without a runtime, a display, or
//! an AppHandle. Observations fan out on a `tokio::sync::broadcast` channel
//! (S02's ingestion seam) and are mirrored app-wide as
//! `watcher://observation` for the Settings diagnostics surface.

pub mod commands;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use tauri::Manager;
use tokio::sync::{broadcast, Notify};

use crate::ocr::{OcrEngine, OcrError};
use crate::privacy::{Detection, RedactionConfidence, RedactionError};

/// Seconds between watcher ticks. Fixed for S01; per milestone research a
/// heavier/adaptive cadence is a later concern, measured via the ignored
/// live test.
pub const WATCH_CADENCE_SECS: u64 = 5;

/// Broadcast channel capacity. Consumers that lag past this many
/// observations see `Lagged` and skip forward — old screen text is worthless
/// to replay, so a small bound is correct.
pub const OBSERVATION_CHANNEL_CAPACITY: usize = 64;

/// One extracted screen observation. Crosses IPC as camelCase JSON and is
/// the S02 ingestion payload. Structurally pixel-free (R011): text, the
/// frontmost app's name (when known), and a capture timestamp — nothing
/// else, ever. A serialization test pins this exact field set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TextObservation {
    /// Recognized lines joined with newlines, in Vision's reading order.
    pub text: String,
    /// Localized name of the frontmost application at capture time, when
    /// the OS would say (login windows and permission edge cases yield
    /// `None`).
    pub app_context: Option<String>,
    /// Capture wall-clock time as milliseconds since the Unix epoch.
    pub captured_at: u64,
}

/// What the loop is doing right now — the typed, visible run state
/// (kebab-case over IPC, matching every kind tag in the app).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum WatcherRunState {
    /// Watcher toggle is off; the loop sleeps without capturing.
    Idle,
    /// Watcher is on and actively capturing/recognizing each tick.
    Watching,
    /// Watcher is on but privacy mode is pausing it — visibly, as its own
    /// typed state (R027), never a silent skip.
    PausedPrivacy,
}

impl WatcherRunState {
    /// Stable machine-readable name, mirroring the serde value. Used in
    /// transition logs so grep for `paused-privacy` works.
    pub fn as_str(self) -> &'static str {
        match self {
            WatcherRunState::Idle => "idle",
            WatcherRunState::Watching => "watching",
            WatcherRunState::PausedPrivacy => "paused-privacy",
        }
    }
}

/// The pure tick-gating policy: what should the loop be doing given the two
/// toggles? Disabled wins over privacy (an off watcher is idle, not
/// paused); privacy pauses an enabled watcher before any capture happens.
/// Only `Watching` captures.
pub fn decide_run_state(enabled: bool, privacy: bool) -> WatcherRunState {
    match (enabled, privacy) {
        (false, _) => WatcherRunState::Idle,
        (true, true) => WatcherRunState::PausedPrivacy,
        (true, false) => WatcherRunState::Watching,
    }
}

/// Queryable watcher state (health-as-value, R007): returned by
/// `watcher_status`, broadcast on `watcher://state`. `last_tick_error` is
/// the most recent typed OCR failure (kept until a tick succeeds); `error`
/// carries the most recent persist failure, exactly like
/// [`crate::capture::PrivacyStatus`].
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WatcherStatus {
    pub enabled: bool,
    pub state: WatcherRunState,
    pub last_tick_error: Option<OcrError>,
    pub error: Option<String>,
}

/// The one shared watcher core: both entry points (tray check item in T04,
/// `set_watcher_enabled` IPC) mutate it through the single applier, the
/// loop reads it every tick, S02 subscribes to its observation channel.
/// Pure in-memory state — persistence and broadcasting live in the applier.
pub struct WatcherState {
    enabled: AtomicBool,
    run_state: Mutex<WatcherRunState>,
    last_tick_error: Mutex<Option<OcrError>>,
    persist_error: Mutex<Option<String>>,
    observations: broadcast::Sender<TextObservation>,
    wake: Notify,
}

impl Default for WatcherState {
    fn default() -> Self {
        let (observations, _) = broadcast::channel(OBSERVATION_CHANNEL_CAPACITY);
        Self {
            enabled: AtomicBool::new(false),
            run_state: Mutex::new(WatcherRunState::Idle),
            last_tick_error: Mutex::new(None),
            persist_error: Mutex::new(None),
            observations,
            wake: Notify::new(),
        }
    }
}

impl WatcherState {
    pub fn new() -> Self {
        Self::default()
    }

    /// The watcher starts off; persisted state is applied in `setup()`.
    pub fn enabled(&self) -> bool {
        self.enabled.load(Ordering::SeqCst)
    }

    pub fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::SeqCst);
    }

    pub fn run_state(&self) -> WatcherRunState {
        *self.run_state.lock().unwrap()
    }

    /// Move to `next`, reporting whether anything changed — the loop and
    /// applier only log/broadcast on real transitions.
    pub fn set_run_state(&self, next: WatcherRunState) -> bool {
        let mut current = self.run_state.lock().unwrap();
        let changed = *current != next;
        *current = next;
        changed
    }

    /// Record (or clear, on a successful tick) the most recent typed tick
    /// failure, reporting whether the stored value changed.
    pub fn record_tick_error(&self, error: Option<OcrError>) -> bool {
        let mut current = self.last_tick_error.lock().unwrap();
        let changed = *current != error;
        *current = error;
        changed
    }

    /// Record (or clear) the most recent persist failure.
    pub fn record_persist_error(&self, error: Option<String>) {
        *self.persist_error.lock().unwrap() = error;
    }

    /// Subscribe to the observation stream — the S02 ingestion seam. Late
    /// subscribers only see observations published after they subscribe;
    /// laggards receive `Lagged` and skip forward.
    pub fn subscribe(&self) -> broadcast::Receiver<TextObservation> {
        self.observations.subscribe()
    }

    /// Publish one observation to in-process subscribers. A send with no
    /// live receiver is normal (diagnostics closed, S02 not built yet) —
    /// ignored, never logged per-tick.
    pub fn publish(&self, observation: TextObservation) {
        let _ = self.observations.send(observation);
    }

    /// Current status as health-as-value — never an error, safe to poll.
    pub fn status(&self) -> WatcherStatus {
        WatcherStatus {
            enabled: self.enabled(),
            state: self.run_state(),
            last_tick_error: self.last_tick_error.lock().unwrap().clone(),
            error: self.persist_error.lock().unwrap().clone(),
        }
    }

    /// Wake the loop out of its cadence sleep — the applier calls this so a
    /// toggle takes effect immediately instead of up to a full tick later.
    pub fn wake(&self) {
        self.wake.notify_one();
    }

    /// Resolves when [`wake`](Self::wake) is called (or immediately, if it
    /// already was) — the loop races this against the cadence sleep.
    pub async fn woken(&self) {
        self.wake.notified().await;
    }
}

/// Everything a redacted tick yields: the broadcastable observation plus
/// the engine's detection metadata (kinds/counts and confidence only —
/// never the original text), which the tick logs and S02/S03 consume.
struct RedactedObservation {
    observation: TextObservation,
    detections: Vec<Detection>,
    confidence: RedactionConfidence,
}

/// The single observation constructor (D029 mount 1): raw OCR text passes
/// through [`crate::privacy::redact`] before a [`TextObservation`] can
/// exist, so every fan-out consumer — memory ingest, nudge classification,
/// the `watcher://observation` mirror — receives already-redacted text with
/// no per-consumer changes. Pure: unit-testable without a runtime, display,
/// or AppHandle. On `Err` the caller must drop the whole observation (fail
/// closed); this module offers no bypass path.
fn build_observation(
    text: String,
    app_context: Option<String>,
    captured_at: u64,
) -> Result<RedactedObservation, RedactionError> {
    let outcome = crate::privacy::redact(&text)?;
    Ok(RedactedObservation {
        observation: TextObservation {
            text: outcome.text,
            app_context,
            captured_at,
        },
        detections: outcome.detections,
        confidence: outcome.confidence,
    })
}

/// Render detections as `kind=count` pairs for the redaction log line —
/// kinds and counts only (the S03 counter vocabulary), never any text.
fn detection_summary(detections: &[Detection]) -> String {
    detections
        .iter()
        .map(|d| format!("{}={}", d.kind.as_str(), d.count))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Milliseconds since the Unix epoch — `TextObservation.captured_at`.
fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Localized name of the frontmost application, when the OS knows one.
/// `None` off macOS and on the edge cases (login window, no frontmost app).
fn frontmost_app_context() -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        let workspace = objc2_app_kit::NSWorkspace::sharedWorkspace();
        let app = workspace.frontmostApplication()?;
        app.localizedName().map(|name| name.to_string())
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

/// Spawn the watcher loop for the app's lifetime, bound to this platform's
/// live OCR backend. Called once from `setup()`; the loop is permanent and
/// cheap while idle (one state read per cadence), so there is no task
/// lifecycle to manage — the toggle changes what a tick does, not whether
/// the task exists.
pub fn spawn_loop(app: tauri::AppHandle) {
    let engine = crate::ocr::platform_engine(crate::ocr::OCR_MAX_DIMENSION);
    tauri::async_runtime::spawn(run_loop(app, engine));
}

/// The loop body: derive the run state, transition visibly, capture on
/// `Watching`, then sleep until the next tick or an applier wake.
async fn run_loop(app: tauri::AppHandle, engine: Arc<dyn OcrEngine>) {
    log::info!("watcher: loop started (cadence {WATCH_CADENCE_SECS}s)");
    // Held while the run state is `Watching` so the tray eye animates for
    // the whole watching span (T04) — one ActivityKind::Watcher guard across
    // ticks, not one per capture. Leaving `Watching` drops it, which rests
    // the icon through the RAII guard on every exit path.
    let mut activity: Option<crate::tray::ActivityGuard> = None;
    loop {
        let state = app.state::<WatcherState>();
        let privacy = app
            .try_state::<crate::capture::PrivacyState>()
            .map(|p| p.enabled())
            .unwrap_or(false);
        let next = decide_run_state(state.enabled(), privacy);
        transition(&app, &state, next);

        if next == WatcherRunState::Watching {
            if activity.is_none() {
                activity = Some(crate::tray::begin_activity(
                    &app,
                    crate::tray::ActivityKind::Watcher,
                ));
            }
            tick(&app, &state, engine.as_ref()).await;
        } else {
            activity = None;
        }

        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(WATCH_CADENCE_SECS)) => {}
            _ = state.woken() => {}
        }
    }
}

/// Apply one run-state transition, logging it (grep `paused-privacy` for
/// privacy pauses) and broadcasting the resulting status when it changed.
fn transition(app: &tauri::AppHandle, state: &WatcherState, next: WatcherRunState) {
    let previous = state.run_state();
    if state.set_run_state(next) {
        log::info!("watcher: state {} -> {}", previous.as_str(), next.as_str());
        commands::emit_state(app, state.status());
    }
}

/// One capture→recognize→broadcast tick. The pixels live and die inside
/// `engine.extract()`; only text reaches this frame. Failures are typed,
/// logged by kind, kept queryable on the status, and broadcast — the loop
/// itself never stops on an error (the next tick retries).
async fn tick(app: &tauri::AppHandle, state: &WatcherState, engine: &dyn OcrEngine) {
    match engine.extract().await {
        Ok(lines) => {
            if state.record_tick_error(None) {
                commands::emit_state(app, state.status());
            }
            if lines.is_empty() {
                log::debug!("watcher: tick extracted no text");
                return;
            }
            let built =
                match build_observation(lines.join("\n"), frontmost_app_context(), now_millis()) {
                    Ok(built) => built,
                    Err(err) => {
                        // Fail closed (D028): no unredacted text may leave this
                        // frame, so the whole observation is dropped. Error kind
                        // only — never the captured text.
                        log::error!(
                            "watcher: observation dropped, redaction failed ({})",
                            err.kind()
                        );
                        return;
                    }
                };
            if !built.detections.is_empty() {
                log::info!("watcher: redacted {}", detection_summary(&built.detections));
                // The shared guard counters (M003 S02) see watcher detections
                // too, so S03's privacy surface reflects every redaction site
                // — kinds and counts only, same as the log line above.
                if let Some(guard) =
                    app.try_state::<std::sync::Arc<crate::llm::guard::GuardState>>()
                {
                    guard.record_redactions(&built.detections);
                }
            }
            if built.confidence == RedactionConfidence::Low {
                log::warn!("watcher: redaction confidence low");
            }
            log::debug!(
                "watcher: tick extracted {} line(s) (app={:?})",
                lines.len(),
                built.observation.app_context
            );
            state.publish(built.observation.clone());
            commands::emit_observation(app, built.observation);
        }
        Err(err) => {
            log::error!("watcher: tick failed ({}): {err}", err.kind());
            if state.record_tick_error(Some(err)) {
                commands::emit_state(app, state.status());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decide_disabled_is_idle_regardless_of_privacy() {
        // Pure tick gating (slice must-have 1): off means idle, not paused.
        assert_eq!(decide_run_state(false, false), WatcherRunState::Idle);
        assert_eq!(decide_run_state(false, true), WatcherRunState::Idle);
    }

    #[test]
    fn decide_privacy_pauses_an_enabled_watcher_before_any_capture() {
        // R027: privacy on → no capture; the state is its own typed value.
        assert_eq!(decide_run_state(true, true), WatcherRunState::PausedPrivacy);
    }

    #[test]
    fn decide_enabled_without_privacy_watches() {
        assert_eq!(decide_run_state(true, false), WatcherRunState::Watching);
    }

    #[test]
    fn only_watching_captures() {
        // The loop keys capture on this exact comparison; every other state
        // must skip the OCR engine entirely.
        for (enabled, privacy) in [(false, false), (false, true), (true, true)] {
            assert_ne!(
                decide_run_state(enabled, privacy),
                WatcherRunState::Watching
            );
        }
    }

    #[test]
    fn run_state_serializes_kebab_case_matching_as_str() {
        for (state, name) in [
            (WatcherRunState::Idle, "idle"),
            (WatcherRunState::Watching, "watching"),
            (WatcherRunState::PausedPrivacy, "paused-privacy"),
        ] {
            assert_eq!(state.as_str(), name);
            assert_eq!(
                serde_json::to_value(state).unwrap(),
                serde_json::json!(name)
            );
        }
    }

    #[test]
    fn observation_serializes_camel_case_with_a_pixel_free_field_set() {
        // R011 structural proof at the IPC boundary: exactly these three
        // fields, camelCase, and no pixel/image/base64 field can ever ride
        // along without failing this test.
        let observation = TextObservation {
            text: "hello\nworld".into(),
            app_context: Some("Safari".into()),
            captured_at: 1_752_800_000_000,
        };
        let v = serde_json::to_value(&observation).unwrap();
        assert_eq!(v["text"], "hello\nworld");
        assert_eq!(v["appContext"], "Safari");
        assert_eq!(v["capturedAt"], 1_752_800_000_000u64);

        let keys: Vec<&String> = v.as_object().unwrap().keys().collect();
        assert_eq!(
            keys,
            ["appContext", "capturedAt", "text"],
            "unexpected field set: {keys:?}"
        );
    }

    #[test]
    fn observation_app_context_is_optional_end_to_end() {
        let observation = TextObservation {
            text: "x".into(),
            app_context: None,
            captured_at: 1,
        };
        let v = serde_json::to_value(&observation).unwrap();
        assert_eq!(v["appContext"], serde_json::Value::Null);
    }

    #[test]
    fn status_serializes_camel_case_with_typed_run_state_and_errors() {
        let status = WatcherStatus {
            enabled: true,
            state: WatcherRunState::PausedPrivacy,
            last_tick_error: Some(OcrError::PermissionDenied {
                detail: "TCC denied".into(),
            }),
            error: Some("persist failed".into()),
        };
        let v = serde_json::to_value(&status).unwrap();
        assert_eq!(v["enabled"], true);
        assert_eq!(v["state"], "paused-privacy");
        // The nested tick error keeps its kind-tagged shape — diagnostics
        // match on `kind` exactly like every other error surface.
        assert_eq!(v["lastTickError"]["kind"], "permission-denied");
        assert_eq!(v["lastTickError"]["detail"], "TCC denied");
        assert_eq!(v["error"], "persist failed");
    }

    #[test]
    fn state_defaults_off_idle_and_error_free() {
        let state = WatcherState::new();
        assert!(!state.enabled(), "watcher must default to off");
        assert_eq!(
            state.status(),
            WatcherStatus {
                enabled: false,
                state: WatcherRunState::Idle,
                last_tick_error: None,
                error: None,
            }
        );
    }

    #[test]
    fn state_toggles_and_reports_run_state_transitions() {
        let state = WatcherState::new();
        state.set_enabled(true);
        assert!(state.enabled());

        assert!(
            state.set_run_state(WatcherRunState::Watching),
            "idle -> watching changed"
        );
        assert!(
            !state.set_run_state(WatcherRunState::Watching),
            "watching -> watching did not"
        );
        assert!(state.set_run_state(WatcherRunState::PausedPrivacy));
        assert_eq!(state.run_state(), WatcherRunState::PausedPrivacy);
    }

    #[test]
    fn tick_errors_are_kept_until_a_success_clears_them() {
        let state = WatcherState::new();
        let err = OcrError::CaptureFailed {
            detail: "no display".into(),
        };
        assert!(
            state.record_tick_error(Some(err.clone())),
            "first error is a change"
        );
        assert!(
            !state.record_tick_error(Some(err)),
            "same error again is not"
        );
        assert_eq!(
            state.status().last_tick_error.unwrap().kind(),
            "capture-failed"
        );
        assert!(state.record_tick_error(None), "success clears it");
        assert_eq!(state.status().last_tick_error, None);
    }

    #[test]
    fn persist_errors_are_queryable_and_clearable() {
        let state = WatcherState::new();
        state.record_persist_error(Some("failed to persist watcherEnabled=true".into()));
        assert!(state
            .status()
            .error
            .as_deref()
            .unwrap()
            .contains("watcherEnabled"));
        state.record_persist_error(None);
        assert_eq!(state.status().error, None);
    }

    #[tokio::test]
    async fn observations_fan_out_to_subscribers() {
        let state = WatcherState::new();
        let mut rx = state.subscribe();
        let observation = TextObservation {
            text: "on screen".into(),
            app_context: Some("Terminal".into()),
            captured_at: 42,
        };
        state.publish(observation.clone());
        assert_eq!(rx.recv().await.unwrap(), observation);
    }

    #[test]
    fn publish_without_receivers_is_silent_and_safe() {
        // The diagnostics view may be closed and S02 does not exist yet —
        // a receiverless send must be a no-op, not an error path.
        let state = WatcherState::new();
        state.publish(TextObservation {
            text: "x".into(),
            app_context: None,
            captured_at: 1,
        });
    }

    #[tokio::test]
    async fn wake_short_circuits_the_cadence_wait() {
        let state = Arc::new(WatcherState::new());
        state.wake();
        // A pre-arrived wake resolves immediately; the timeout would only
        // trip if woken() lost the notification.
        tokio::time::timeout(Duration::from_secs(1), state.woken())
            .await
            .expect("woken() must resolve after wake()");
    }

    #[test]
    fn build_observation_redacts_seeded_secrets_before_the_observation_exists() {
        // D029 mount 1: the slice's three seed classes (password, Luhn-valid
        // card, prefixed API key) must already be typed placeholders in the
        // constructed observation — every fan-out consumer sees only this.
        let raw = "password: hunter2\n\
                   card 4242 4242 4242 4242\n\
                   sk-abcdefghijklmnop1234"
            .to_string();
        let built = build_observation(raw, Some("Terminal".into()), 42).unwrap();

        assert_eq!(
            built.observation.text,
            "password: [REDACTED:password]\ncard [REDACTED:card]\n[REDACTED:api-key]"
        );
        for secret in ["hunter2", "4242", "sk-abcdefghijklmnop1234"] {
            assert!(
                !built.observation.text.contains(secret),
                "seed secret {secret:?} leaked into the observation"
            );
        }
        // Non-text fields pass through the constructor untouched.
        assert_eq!(built.observation.app_context.as_deref(), Some("Terminal"));
        assert_eq!(built.observation.captured_at, 42);
        assert_eq!(built.confidence, RedactionConfidence::Confident);
        assert_eq!(
            built.detections,
            [
                Detection {
                    kind: crate::privacy::DetectionKind::Password,
                    count: 1
                },
                Detection {
                    kind: crate::privacy::DetectionKind::Card,
                    count: 1
                },
                Detection {
                    kind: crate::privacy::DetectionKind::ApiKey,
                    count: 1
                },
            ]
        );
        // The loggable metadata never carries any text, original or redacted.
        let metadata = serde_json::to_string(&built.detections).unwrap();
        assert!(!metadata.contains("hunter2") && !metadata.contains("4242"));
    }

    #[test]
    fn build_observation_leaves_innocent_text_untouched() {
        // Negative path: an ordinary dev screen must survive the mount
        // byte-identical with zero detections — redaction is not lossy noise.
        let raw = "fn main() { println!(\"hello\"); }\nBuild finished in 3.2s".to_string();
        let built = build_observation(raw.clone(), None, 7).unwrap();
        assert_eq!(built.observation.text, raw);
        assert!(built.detections.is_empty());
        assert_eq!(built.confidence, RedactionConfidence::Confident);
        assert_eq!(built.observation.app_context, None);
    }

    #[test]
    fn oversized_input_surfaces_low_confidence_through_the_mount() {
        // The S02 policy signal must survive the helper: a scan-cap-exceeding
        // input comes back Low, not silently Confident.
        let raw = "a ".repeat(40_000); // > 64 KiB scan cap
        let built = build_observation(raw, None, 1).unwrap();
        assert_eq!(built.confidence, RedactionConfidence::Low);
    }

    #[test]
    fn detection_summary_speaks_the_serde_kind_vocabulary_kinds_and_counts_only() {
        // The log-line vocabulary is the same kebab-case tag set S03 counters
        // key off — pin as_str to the serde tag, and the summary shape.
        for kind in crate::privacy::DetectionKind::ALL {
            assert_eq!(
                serde_json::to_value(kind).unwrap(),
                serde_json::json!(kind.as_str())
            );
        }
        let summary = detection_summary(&[
            Detection {
                kind: crate::privacy::DetectionKind::Password,
                count: 2,
            },
            Detection {
                kind: crate::privacy::DetectionKind::ApiKey,
                count: 1,
            },
        ]);
        assert_eq!(summary, "password=2 api-key=1");
        assert_eq!(detection_summary(&[]), "");
    }

    #[test]
    fn redaction_error_kind_is_kebab_case_for_fail_closed_drop_logs() {
        // The drop log line greps by this exact kind tag.
        let err = RedactionError::PatternCompile { detector: "card" };
        assert_eq!(err.kind(), "pattern-compile");
        assert!(!format!("{err}").contains("hunter2"));
    }

    #[test]
    fn now_millis_is_a_plausible_wall_clock() {
        // 2026-01-01 in ms — a sanity floor, not an exactness claim.
        assert!(now_millis() > 1_767_225_600_000);
    }
}
