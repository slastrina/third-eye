//! Managed HID input state: the [`InputState`] holder the composite executor's
//! `InputTool` (S01/T05) draws its backend from, and the surface S03's
//! Settings/arming later queries.
//!
//! This is the input twin of [`crate::capture::commands::CaptureState`], but
//! deliberately thinner: S01 ships only the managed-state holder and the
//! platform cfg-select ([`InputState::with_platform_backend`]). No Tauri IPC
//! commands live here yet — S03 adds the `input_permission_status` /
//! `open_input_settings` surface once the off-by-default arming gate exists.
//! Keeping the state holder ahead of the commands lets T05 mount the executor
//! against a real backend without waiting on the S03 IPC surface.

use std::collections::HashSet;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, State};

use super::{ActionKind, InputControl, InputError, InputPermission};

/// HID arm-state broadcast (S03, D038): mutation responses only reach the
/// calling window, so every arm/disarm also emits the resulting
/// [`HidArmedStatus`] app-wide — the overlay/tray affordance stays truthful
/// when the settings window (or a future tray path) flips the arming toggle.
/// Mirrors [`crate::capture::commands::PRIVACY_EVENT`].
pub const HID_STATE_EVENT: &str = "hid://state";

/// Deep link to System Settings → Privacy & Security → Accessibility — the
/// arming walkthrough's "Open System Settings" action. Accessibility is a
/// *separate* TCC entitlement from Screen Recording (which
/// [`crate::capture::commands::SCREEN_RECORDING_SETTINGS_URL`] targets), so
/// the pane anchor differs.
pub const ACCESSIBILITY_SETTINGS_URL: &str =
    "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility";

/// Shared HID run-state: off by default (disarmed, D038/R019). One holder is
/// shared — an `Arc` clone — between the managed [`InputState`] and the
/// composite executor's `InputTool` ([`crate::llm::toolloop::InputTool`]), so a
/// single mode mutation (the S03/S04 applier) is reflected both in the tool set
/// the model is advertised (`definitions()` withholds the tool when disarmed)
/// and in the `execute()` refusal gate, with no re-mount. Structural inertness
/// lives here: a stored [`HidRunMode`], not a UI hint.
///
/// S04 widens the stored value from a boolean to the three-way [`HidRunMode`]
/// (`Off`/`Ask`/`AutoRun`) the user picks in Settings — the `ApprovalGate`
/// snapshots [`Self::mode`] to gate each action, while `armed()` (mode ≠ `Off`)
/// keeps driving the S03 structural tool-advertise/refuse gate unchanged.
#[derive(Debug)]
pub struct HidArmState {
    /// The current run mode, stored as its `u8` discriminant so it can live in a
    /// lock-free atomic beside the S01–S03 `armed()` reads on every action.
    mode: AtomicU8,
}

impl HidArmState {
    /// A holder in the given run mode.
    pub fn with_mode(mode: HidRunMode) -> Self {
        Self {
            mode: AtomicU8::new(mode as u8),
        }
    }

    /// A holder starting armed (→ `Ask`) or disarmed (→ `Off`). Back-compat with
    /// the S03 boolean surface: an armed machine defaults to inline prompting.
    pub fn new(armed: bool) -> Self {
        Self::with_mode(if armed {
            HidRunMode::Ask
        } else {
            HidRunMode::Off
        })
    }

    /// The safe default: disarmed (`Off`). HID is never armed without an explicit,
    /// persisted, permission-gated Settings choice (D038).
    pub fn disarmed() -> Self {
        Self::with_mode(HidRunMode::Off)
    }

    /// The current run mode. The `ApprovalGate` snapshots this per run (S04).
    pub fn mode(&self) -> HidRunMode {
        HidRunMode::from_u8(self.mode.load(Ordering::SeqCst))
    }

    /// Whether HID is currently armed — any non-`Off` mode. The `InputTool` reads
    /// this on every `definitions()` and `execute()` — the S03 structural gate.
    pub fn armed(&self) -> bool {
        self.mode() != HidRunMode::Off
    }

    /// Set the run mode. The single writer is the applier (`apply_hid_run_mode`),
    /// which owns persist + permission preflight + rollback + broadcast around
    /// this store.
    pub fn set_mode(&self, mode: HidRunMode) {
        self.mode.store(mode as u8, Ordering::SeqCst);
    }

    /// Back-compat boolean writer: arm → `Ask`, disarm → `Off`. Retained for the
    /// S03 `set_hid_armed` applier path.
    pub fn set_armed(&self, armed: bool) {
        self.set_mode(if armed {
            HidRunMode::Ask
        } else {
            HidRunMode::Off
        });
    }
}

/// Managed input state: the platform backend behind the [`InputControl`] seam,
/// so the composite executor (and tests) never name a concrete backend, plus
/// the shared [`HidArmState`] the executor's `InputTool` gates on.
pub struct InputState {
    backend: Arc<dyn InputControl>,
    arm: Arc<HidArmState>,
    /// The most recent arm/disarm failure — a refused arm (permission-denied)
    /// or a persist failure (input-failed) — kept queryable on the status so a
    /// mutation that could not complete stays visible after the fact (R007),
    /// mirroring [`crate::capture::PrivacyState`]'s `last_error`.
    last_error: Mutex<Option<InputError>>,
}

impl InputState {
    /// State bound to `backend`, disarmed (HID off) by default — the only safe
    /// startup posture for a capability that can click and type anywhere
    /// (D038). The S03 applier arms it from the persisted `hidEnabled` choice
    /// after a permission preflight.
    pub fn new(backend: Arc<dyn InputControl>) -> Self {
        Self {
            backend,
            arm: Arc::new(HidArmState::disarmed()),
            last_error: Mutex::new(None),
        }
    }

    /// State bound to this platform's live backend: the enigo-backed
    /// [`super::macos::MacosInput`] on macOS, the typed-unsupported
    /// [`super::fallback::FallbackInput`] everywhere else. Mirrors
    /// [`crate::capture::commands::CaptureState::with_platform_backend`].
    pub fn with_platform_backend() -> Self {
        #[cfg(target_os = "macos")]
        {
            Self::new(Arc::new(super::macos::MacosInput))
        }
        #[cfg(not(target_os = "macos"))]
        {
            Self::new(Arc::new(super::fallback::FallbackInput))
        }
    }

    /// The backend handle for the composite executor's `InputTool` (T05): a
    /// cheap `Arc` clone so the tool can perform actions without holding a
    /// borrow on managed state across an `.await`.
    pub fn backend(&self) -> Arc<dyn InputControl> {
        self.backend.clone()
    }

    /// The shared arm-state handle for the composite executor's `InputTool`: a
    /// cheap `Arc` clone so the tool's `definitions()`/`execute()` gate reads
    /// the same flag the S03 applier mutates. Arming from any surface reflects
    /// in the model's advertised tools with no re-mount.
    pub fn arm_state(&self) -> Arc<HidArmState> {
        self.arm.clone()
    }

    /// Whether HID is currently armed — convenience over [`Self::arm_state`] for
    /// the S03 applier and `hid_armed_status`.
    pub fn armed(&self) -> bool {
        self.arm.armed()
    }

    /// The current HID run mode (S04) — convenience over [`Self::arm_state`] for
    /// the applier and the chat run's `ApprovalGate` snapshot.
    pub fn mode(&self) -> HidRunMode {
        self.arm.mode()
    }

    /// Current Accessibility permission state — health-as-value, never an error,
    /// never a prompt. The surface S03's arming/Settings queries.
    pub fn permission(&self) -> InputPermission {
        self.backend.permission()
    }

    /// Trigger the OS Accessibility prompt through the backend and return the
    /// resulting permission value — the first-run onboarding entry point.
    /// Requesting the grant here does NOT arm HID: it only pre-grants the OS
    /// permission so a later Settings arm is a one-click flip, not a grant
    /// round-trip. The arm state is untouched (still `Off`, D038/R019). On an
    /// unsupported platform the backend returns `false` and the value stays
    /// `supported: false`, so the UI can present it truthfully.
    pub fn request_permission(&self) -> InputPermission {
        // Only ask where a prompt can appear; off macOS the fallback returns
        // false without side effects.
        if self.backend.permission().supported {
            self.backend.request_permission();
        }
        self.backend.permission()
    }

    /// Record (or clear) the most recent arm/disarm failure — the applier calls
    /// this on every mutation so a refused arm or persist failure stays
    /// queryable on the status (R007). Mirrors `PrivacyState::record_error`.
    pub fn record_error(&self, error: Option<InputError>) {
        *self.last_error.lock().unwrap() = error;
    }

    /// Current arming status as health-as-value: `{ armed, permission, error }`.
    /// Never errors, safe to poll (the `hid_armed_status` command). `permission`
    /// is read live from the backend so the walkthrough reflects a grant/revoke
    /// that happened out of band.
    pub fn status(&self) -> HidArmedStatus {
        HidArmedStatus {
            armed: self.armed(),
            mode: self.mode(),
            permission: self.permission(),
            error: self.last_error.lock().unwrap().clone(),
        }
    }
}

/// Queryable HID arming state: `{ armed, permission, error }` — the
/// health-as-value shape the `hid_armed_status` command returns and the
/// `hid://state` broadcast carries (R007), the input twin of
/// [`crate::capture::PrivacyStatus`]. `error` is a typed [`InputError`] so a
/// refused arm serializes with its `kind` tag and the Settings walkthrough can
/// match on `kind == "permission-denied"`.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HidArmedStatus {
    pub armed: bool,
    /// The active run mode (S04): `off`/`ask`/`auto-run`. `armed` is `mode !=
    /// off`, kept for the S03 boolean surface; the Settings selector reads
    /// `mode`. Rides the `hid://state` broadcast so every window renders the
    /// same three-way choice.
    pub mode: HidRunMode,
    pub permission: InputPermission,
    pub error: Option<InputError>,
}

/// Testable core of the applier's state decision (no Tauri, no store): given the
/// desired arm state, the previous arm state, the live permission, and a persist
/// function, return the `(armed, error)` the applier will store and broadcast.
///
/// Two gates, in order: (1) arming requires a real Accessibility grant — an
/// ungranted arm is refused with a typed `permission-denied` and the machine
/// stays disarmed (D038); the toggle never claims armed without a real grant.
/// (2) On a persist failure the in-memory flag rolls back to `previous` so an
/// unpersisted arming choice can never silently revert on restart (hotkey
/// precedent), and the persist error is surfaced typed. Disarming (`desired ==
/// false`) skips the permission gate — turning HID off must always succeed.
fn resolve_arm(
    desired: bool,
    previous: bool,
    permission: InputPermission,
    persist: impl FnOnce(bool) -> Result<(), String>,
) -> (bool, Option<InputError>) {
    if desired && !permission.granted {
        return (
            false,
            Some(InputError::PermissionDenied {
                detail: "Accessibility not granted; enable Third Eye in System Settings → \
                         Privacy & Security → Accessibility to arm HID"
                    .into(),
            }),
        );
    }
    match persist(desired) {
        Ok(()) => (desired, None),
        Err(e) => (previous, Some(InputError::InputFailed { detail: e })),
    }
}

/// Testable core of the S04 run-mode applier's decision (no Tauri, no store) —
/// the three-way twin of [`resolve_arm`]. Given the desired [`HidRunMode`], the
/// previous mode, the live permission, and a persist function, return the
/// `(mode, error)` the applier will store and broadcast.
///
/// Same two gates as [`resolve_arm`], generalized to the mode: (1) selecting any
/// *active* mode (`Ask` or `AutoRun`) requires a real Accessibility grant — an
/// ungranted select is refused with a typed `permission-denied` and the machine
/// stays `Off` (D038); the selector never claims an armed mode without a real
/// grant. (2) On a persist failure the in-memory mode rolls back to `previous`
/// so an unpersisted choice can never silently revert on restart, and the error
/// is surfaced typed. Selecting `Off` skips the permission gate — turning HID
/// off must always succeed, even on a machine whose grant was revoked out of
/// band.
fn resolve_run_mode(
    desired: HidRunMode,
    previous: HidRunMode,
    permission: InputPermission,
    persist: impl FnOnce(HidRunMode) -> Result<(), String>,
) -> (HidRunMode, Option<InputError>) {
    if desired != HidRunMode::Off && !permission.granted {
        return (
            HidRunMode::Off,
            Some(InputError::PermissionDenied {
                detail: "Accessibility not granted; enable Third Eye in System Settings → \
                         Privacy & Security → Accessibility to arm HID"
                    .into(),
            }),
        );
    }
    match persist(desired) {
        Ok(()) => (desired, None),
        Err(e) => (previous, Some(InputError::InputFailed { detail: e })),
    }
}

/// How HID input actions are gated (S04). The run-mode the user picks in
/// Settings, replacing S03's boolean arm toggle: `Off` stays structurally inert
/// (the S03 disabled refusal, D038), `Ask` prompts inline in the overlay before
/// each not-yet-whitelisted action kind, `AutoRun` performs every action without
/// prompting. Serialized kebab-case (`off` / `ask` / `auto-run`) — the exact
/// strings `config.rs` persists and `src/chat.ts` matches on. `Off` is the
/// `Default` so a missing/garbage persisted value maps to the safe inert state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HidRunMode {
    /// HID off: structurally inert, no input synthesized (S03/D038).
    #[default]
    Off,
    /// Prompt inline before each HID action kind not yet whitelisted this session.
    Ask,
    /// Perform every HID action without prompting.
    AutoRun,
}

impl HidRunMode {
    /// Decode a mode from the `u8` discriminant [`HidArmState`] stores. Any
    /// out-of-range byte maps to the safe inert default (`Off`) — the same
    /// fail-closed posture as a garbage persisted value.
    fn from_u8(v: u8) -> Self {
        match v {
            1 => HidRunMode::Ask,
            2 => HidRunMode::AutoRun,
            _ => HidRunMode::Off,
        }
    }
}

/// The decision the pure approval resolver returns for one pending HID action —
/// the S04 twin of [`resolve_arm`]'s `(armed, error)`: a value the gate acts on,
/// never a side effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalDecision {
    /// HID is `Off`: the gate refuses with the S03 [`InputError::disabled`]
    /// structural refusal before the backend is touched (D038).
    Refuse,
    /// Perform the action without prompting — `AutoRun`, or `Ask` with the
    /// action's kind already granted for this session.
    Perform,
    /// `Ask` and the action's kind is not yet whitelisted: prompt the user inline
    /// in the overlay (Allow once / Always allow this kind / Deny).
    Prompt,
}

/// The session-scoped by-kind approval whitelist (S04): the set of
/// [`ActionKind`]s the user has chosen "Always allow this kind" for. Grants are
/// session-only — [`Self::clear`] empties it on run/session end so an allow never
/// outlives the run that granted it (R023: nothing about a session's actions is
/// persisted). Mutated ONLY by [`Self::allow`] (the "Always allow this kind"
/// verdict); an "Allow once" verdict performs without touching the set.
#[derive(Debug, Default)]
pub struct SessionWhitelist {
    kinds: HashSet<ActionKind>,
}

impl SessionWhitelist {
    /// An empty whitelist — the start-of-session posture: every kind prompts.
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether `kind` has been granted for this session (an `Ask`-mode action of
    /// this kind performs without a prompt).
    pub fn contains(&self, kind: ActionKind) -> bool {
        self.kinds.contains(&kind)
    }

    /// Grant `kind` for the rest of this session — the "Always allow this kind"
    /// verdict. Idempotent; the only mutation that adds to the set.
    pub fn allow(&mut self, kind: ActionKind) {
        self.kinds.insert(kind);
    }

    /// Empty the whitelist — called on run/session end so a grant never outlives
    /// its session (R023). After this every kind prompts again.
    pub fn clear(&mut self) {
        self.kinds.clear();
    }

    /// Whether no kind is currently granted.
    pub fn is_empty(&self) -> bool {
        self.kinds.is_empty()
    }
}

/// Pure approval resolver (S04): given the current [`HidRunMode`], the pending
/// action's [`ActionKind`], and the session whitelist, decide whether to
/// [`ApprovalDecision::Refuse`], [`ApprovalDecision::Perform`], or
/// [`ApprovalDecision::Prompt`]. Tauri-free and side-effect-free — the twin of
/// [`resolve_arm`] — so every mode × whitelisted/not transition is unit-testable
/// without a Tauri app. The gate layer (T03) owns the effects (emit the prompt,
/// mutate the whitelist on "Always allow", touch the backend on `Perform`).
///
/// `Off` maps to `Refuse` unconditionally — the whitelist cannot un-inert a
/// disarmed machine (D038). `AutoRun` maps to `Perform` unconditionally. `Ask`
/// consults the whitelist: a granted kind performs, an ungranted kind prompts.
pub fn resolve_approval(
    mode: HidRunMode,
    kind: ActionKind,
    whitelist: &SessionWhitelist,
) -> ApprovalDecision {
    match mode {
        HidRunMode::Off => ApprovalDecision::Refuse,
        HidRunMode::AutoRun => ApprovalDecision::Perform,
        HidRunMode::Ask => {
            if whitelist.contains(kind) {
                ApprovalDecision::Perform
            } else {
                ApprovalDecision::Prompt
            }
        }
    }
}

/// The one shared HID arming applier (S03, D038): every arm/disarm mutation —
/// the `set_hid_armed` IPC (`via = "ipc"`) and any future tray path — funnels
/// through here so they cannot drift (privacy-mode precedent MEM049/MEM053,
/// hotkey precedent MEM044). Owns the whole mutation: AX-permission preflight
/// (never persists armed without a real grant), persist to settings.json with
/// rollback on failure, and the app-wide `hid://state` broadcast. Always
/// returns (and broadcasts) the resulting [`HidArmedStatus`]; a refused arm or
/// persist failure is data on the status, never a thrown error.
pub fn apply_hid_armed(app: &AppHandle, desired: bool, via: &str) -> HidArmedStatus {
    let state = app.state::<InputState>();
    let previous = state.armed();
    let permission = state.permission();
    let (armed, error) = resolve_arm(desired, previous, permission, |d| {
        crate::config::save_hid_enabled(app, d)
    });

    state.arm_state().set_armed(armed);
    state.record_error(error.clone());
    match &error {
        None => log::info!(
            "input: HID {} (via {via})",
            if armed { "armed" } else { "disarmed" }
        ),
        Some(err) => {
            log::error!(
                "input: arm={desired} refused/failed (via {via}): {} ({err})",
                err.kind()
            )
        }
    }

    let status = state.status();
    if let Err(e) = app.emit(HID_STATE_EVENT, status.clone()) {
        log::warn!("input: hid state broadcast failed: {e}");
    }
    status
}

/// The one shared HID run-mode applier (S04, D038): the three-way successor to
/// [`apply_hid_armed`]. Every mode mutation — the `set_hid_run_mode` IPC and any
/// future path — funnels through here so they cannot drift. Owns the whole
/// mutation: AX-permission preflight (never persists an active mode without a
/// real grant), persist to settings.json with rollback on failure, and the
/// app-wide `hid://state` broadcast (now carrying the mode). Always returns (and
/// broadcasts) the resulting [`HidArmedStatus`]; a refused select or persist
/// failure is data on the status, never a thrown error.
pub fn apply_hid_run_mode(app: &AppHandle, desired: HidRunMode, via: &str) -> HidArmedStatus {
    let state = app.state::<InputState>();
    let previous = state.mode();
    let permission = state.permission();
    let (mode, error) = resolve_run_mode(desired, previous, permission, |m| {
        crate::config::save_hid_run_mode(app, m)
    });

    state.arm_state().set_mode(mode);
    state.record_error(error.clone());
    match &error {
        None => log::info!("input: HID run mode = {mode:?} (via {via})"),
        Some(err) => {
            log::error!(
                "input: mode={desired:?} refused/failed (via {via}): {} ({err})",
                err.kind()
            )
        }
    }

    let status = state.status();
    if let Err(e) = app.emit(HID_STATE_EVENT, status.clone()) {
        log::warn!("input: hid state broadcast failed: {e}");
    }
    status
}

/// Apply the persisted HID arming choice at startup (called from `setup()`,
/// after privacy — mirrors `apply_persisted_privacy_mode`). In-memory only: no
/// re-save, no broadcast — nothing is listening yet. The AX gate still applies:
/// a machine whose Accessibility grant was revoked since arming comes up
/// disarmed, so the persisted choice can never re-arm HID without a live grant
/// (D038). An absent key keeps the default (off); load failures are logged
/// inside `config`, never fatal.
pub fn apply_persisted_hid_armed(app: &AppHandle) {
    if let Some(persisted) = crate::config::load_hid_run_mode(app) {
        let state = app.state::<InputState>();
        // The AX gate still applies: a machine whose grant was revoked since the
        // choice was persisted comes up Off, so a persisted active mode can never
        // re-arm HID without a live grant (D038).
        let mode = if persisted != HidRunMode::Off && !state.permission().granted {
            HidRunMode::Off
        } else {
            persisted
        };
        state.arm_state().set_mode(mode);
        log::info!(
            "input: applied persisted HID run mode (persisted={persisted:?}, mode={mode:?})"
        );
    }
}

/// Arm or disarm HID from the UI (S03). Returns the resulting [`HidArmedStatus`]
/// instead of erroring — a refused arm (permission-denied) or persist failure is
/// data the caller renders, same contract as `set_privacy_mode`.
#[tauri::command]
pub fn set_hid_armed(app: AppHandle, arm: bool) -> HidArmedStatus {
    apply_hid_armed(&app, arm, "ipc")
}

/// Select the HID run mode from the UI (S04) — the three-way successor to
/// `set_hid_armed`. Returns the resulting [`HidArmedStatus`] instead of erroring:
/// a refused select (permission-denied) or persist failure is data the caller
/// renders, same contract as `set_hid_armed`/`set_privacy_mode`.
#[tauri::command]
pub fn set_hid_run_mode(app: AppHandle, mode: HidRunMode) -> HidArmedStatus {
    apply_hid_run_mode(&app, mode, "ipc")
}

/// Current HID arming state — health-as-value beside `privacy_status` (R007): a
/// value at any time, never an error, safe for the UI to poll while the
/// walkthrough waits for the user to grant Accessibility.
#[tauri::command]
pub fn hid_armed_status(state: State<'_, InputState>) -> HidArmedStatus {
    let status = state.status();
    log::debug!(
        "input: hid armed status armed={} granted={} supported={} error={:?}",
        status.armed,
        status.permission.granted,
        status.permission.supported,
        status.error.as_ref().map(|e| e.kind())
    );
    status
}

/// Open the macOS Accessibility privacy pane — the arming walkthrough's "Open
/// System Settings" action. Typed `unsupported` off macOS.
#[tauri::command]
pub fn open_input_settings() -> Result<(), InputError> {
    open_input_settings_impl()
}

fn open_input_settings_impl() -> Result<(), InputError> {
    #[cfg(target_os = "macos")]
    {
        log::info!("input: opening Accessibility settings pane");
        std::process::Command::new("open")
            .arg(ACCESSIBILITY_SETTINGS_URL)
            .spawn()
            .map(|_| ())
            .map_err(|e| {
                let err = InputError::InputFailed {
                    detail: format!("failed to open System Settings: {e}"),
                };
                log::error!("input: {} ({err})", err.kind());
                err
            })
    }
    #[cfg(not(target_os = "macos"))]
    {
        let err = InputError::unsupported_here();
        log::error!("input: {} ({err})", err.kind());
        Err(err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::{
        ActionReport, InputAction, InputControl, InputError, InputPermission, MouseButton,
    };
    use async_trait::async_trait;
    use std::sync::Mutex;

    /// Minimal scriptable backend: records the last action so delegation
    /// through the managed state can be asserted without touching real HID, plus
    /// whether the OS prompt was requested (the onboarding flow's contract).
    struct ScriptedInput {
        permission: InputPermission,
        last: Mutex<Option<InputAction>>,
        prompt_requested: std::sync::atomic::AtomicBool,
    }

    #[async_trait]
    impl InputControl for ScriptedInput {
        fn permission(&self) -> InputPermission {
            self.permission
        }

        fn request_permission(&self) -> bool {
            self.prompt_requested
                .store(true, std::sync::atomic::Ordering::SeqCst);
            self.permission.granted
        }

        async fn perform(&self, action: InputAction) -> Result<ActionReport, InputError> {
            *self.last.lock().unwrap() = Some(action);
            Ok(ActionReport::default())
        }
    }

    fn state_with(permission: InputPermission) -> (InputState, Arc<ScriptedInput>) {
        let backend = Arc::new(ScriptedInput {
            permission,
            last: Mutex::new(None),
            prompt_requested: std::sync::atomic::AtomicBool::new(false),
        });
        (InputState::new(backend.clone()), backend)
    }

    #[test]
    fn permission_is_a_backend_passthrough_value() {
        let (state, _) = state_with(InputPermission {
            granted: false,
            supported: true,
        });
        assert_eq!(
            state.permission(),
            InputPermission {
                granted: false,
                supported: true
            }
        );
    }

    #[tokio::test]
    async fn backend_handle_reaches_the_same_backend() {
        let (state, backend) = state_with(InputPermission {
            granted: true,
            supported: true,
        });
        // The Arc the executor will hold must dispatch to the state's backend.
        state
            .backend()
            .perform(InputAction::click(MouseButton::Left))
            .await
            .unwrap();
        assert_eq!(
            *backend.last.lock().unwrap(),
            Some(InputAction::click(MouseButton::Left)),
            "backend() must return a handle to the state's own backend"
        );
    }

    #[test]
    fn hid_arm_state_defaults_off_and_flips() {
        // Off is the safe default (D038); the applier is the single writer.
        let arm = HidArmState::disarmed();
        assert!(!arm.armed(), "disarmed is the only safe startup posture");
        arm.set_armed(true);
        assert!(arm.armed());
        arm.set_armed(false);
        assert!(!arm.armed());
    }

    #[test]
    fn request_permission_prompts_and_never_arms_hid() {
        // First-run onboarding: requesting the grant prompts the backend and
        // reports the live permission, but must NOT arm HID — arming stays the
        // explicit Settings choice (D038/R019). The machine stays disarmed even
        // when the grant is present.
        use std::sync::atomic::Ordering;
        let (state, backend) = state_with(InputPermission {
            granted: true,
            supported: true,
        });
        assert!(!state.armed(), "precondition: disarmed by default");
        let result = state.request_permission();
        assert!(
            backend.prompt_requested.load(Ordering::SeqCst),
            "a supported backend must be prompted during onboarding"
        );
        assert_eq!(
            result,
            InputPermission {
                granted: true,
                supported: true
            }
        );
        assert!(
            !state.armed(),
            "requesting the Accessibility grant must never arm HID (D038)"
        );
    }

    #[test]
    fn input_state_is_disarmed_by_default() {
        // A freshly constructed managed state must never advertise armed — HID
        // is armed only by the explicit persisted Settings choice.
        let (state, _) = state_with(InputPermission {
            granted: true,
            supported: true,
        });
        assert!(
            !state.armed(),
            "InputState must default to disarmed regardless of permission"
        );
    }

    #[test]
    fn arm_state_handle_shares_the_same_flag() {
        // The Arc the executor's InputTool holds must observe the applier's
        // mutation through the state — one shared holder, no re-mount.
        let (state, _) = state_with(InputPermission {
            granted: true,
            supported: true,
        });
        let handle = state.arm_state();
        assert!(!handle.armed());
        handle.set_armed(true);
        assert!(
            state.armed(),
            "a mutation through the shared handle must be visible on the state"
        );
    }

    #[test]
    fn platform_backend_binding_matches_this_os() {
        // On macOS the live backend exists (supported: true); off macOS the
        // fallback reports the typed-unsupported value. Mirrors the capture
        // cross-target contract (R020).
        let state = InputState::with_platform_backend();
        assert_eq!(state.permission().supported, cfg!(target_os = "macos"));
    }

    #[test]
    fn arming_without_permission_is_refused_typed_and_stays_disarmed() {
        // D038: the toggle never claims armed without a real Accessibility
        // grant. A persist that would succeed must never be reached — the
        // preflight refuses first with a typed permission-denied.
        let mut persisted = None;
        let (armed, error) = resolve_arm(
            true,
            false,
            InputPermission {
                granted: false,
                supported: true,
            },
            |d| {
                persisted = Some(d);
                Ok(())
            },
        );
        assert!(!armed, "an ungranted arm must stay disarmed");
        assert_eq!(
            persisted, None,
            "permission preflight must refuse before persisting"
        );
        let err = error.expect("a refused arm must surface a typed error, never a silent no-op");
        assert_eq!(err.kind(), "permission-denied");
        // The walkthrough matches on the serialized kind tag over IPC.
        assert_eq!(
            serde_json::to_value(&err).unwrap()["kind"],
            "permission-denied"
        );
    }

    #[test]
    fn arming_with_permission_persists_and_clears_error() {
        let mut persisted = None;
        let (armed, error) = resolve_arm(
            true,
            false,
            InputPermission {
                granted: true,
                supported: true,
            },
            |d| {
                persisted = Some(d);
                Ok(())
            },
        );
        assert!(armed);
        assert_eq!(
            persisted,
            Some(true),
            "a granted arm must persist the choice"
        );
        assert!(error.is_none());
    }

    #[test]
    fn persist_failure_rolls_back_to_previous_and_surfaces_typed_error() {
        // An unpersisted arming choice must never silently revert on restart:
        // the in-memory flag rolls back to `previous` and the error is kept.
        let (armed, error) = resolve_arm(
            true,
            false,
            InputPermission {
                granted: true,
                supported: true,
            },
            |_| Err("disk full at /settings.json".to_string()),
        );
        assert!(
            !armed,
            "persist failure must roll back to the previous arm state"
        );
        let err = error.expect("a persist failure must be surfaced, not swallowed");
        assert_eq!(err.kind(), "input-failed");
        assert_eq!(
            serde_json::to_value(&err).unwrap()["detail"],
            "disk full at /settings.json"
        );
    }

    #[test]
    fn disarming_skips_the_permission_gate() {
        // Turning HID off must always succeed — even on a machine with no
        // Accessibility grant (e.g. it was revoked out of band).
        let mut persisted = None;
        let (armed, error) = resolve_arm(
            false,
            true,
            InputPermission {
                granted: false,
                supported: true,
            },
            |d| {
                persisted = Some(d);
                Ok(())
            },
        );
        assert!(!armed);
        assert_eq!(
            persisted,
            Some(false),
            "disarm must persist off regardless of permission"
        );
        assert!(error.is_none(), "disarming is never a permission failure");
    }

    #[test]
    fn status_is_health_as_value_reflecting_arm_permission_and_error() {
        let (state, _) = state_with(InputPermission {
            granted: true,
            supported: true,
        });
        let status = state.status();
        assert!(!status.armed, "status defaults to disarmed");
        assert_eq!(
            status.permission,
            InputPermission {
                granted: true,
                supported: true
            }
        );
        assert!(status.error.is_none());

        state.arm_state().set_armed(true);
        state.record_error(Some(InputError::disabled()));
        let status = state.status();
        assert!(status.armed);
        assert_eq!(status.error.as_ref().unwrap().kind(), "disabled");

        // Serializes camelCase with the nested permission value the UI reads.
        let v = serde_json::to_value(&status).unwrap();
        assert_eq!(v["armed"], true);
        assert_eq!(v["permission"]["granted"], true);
        assert_eq!(v["error"]["kind"], "disabled");

        // record_error(None) clears it — health-as-value never sticks stale.
        state.record_error(None);
        assert!(state.status().error.is_none());
    }

    #[test]
    fn hid_state_event_name_is_the_ipc_contract() {
        // src/chat.ts (T05) subscribes on this exact string.
        assert_eq!(HID_STATE_EVENT, "hid://state");
    }

    #[test]
    fn settings_deep_link_targets_the_accessibility_pane() {
        // The walkthrough contract: the Accessibility pane, not Screen Recording.
        assert!(ACCESSIBILITY_SETTINGS_URL.starts_with("x-apple.systempreferences:"));
        assert!(ACCESSIBILITY_SETTINGS_URL.ends_with("Privacy_Accessibility"));
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn open_input_settings_off_macos_is_typed_unsupported() {
        assert_eq!(
            open_input_settings_impl().unwrap_err().kind(),
            "unsupported"
        );
    }

    use crate::input::ActionKind;

    #[test]
    fn approval_off_refuses_every_kind_regardless_of_whitelist() {
        // Off is structurally inert (D038): the whitelist can never un-inert a
        // disarmed machine, so even a "granted" kind still refuses.
        let mut wl = SessionWhitelist::new();
        wl.allow(ActionKind::MouseClick);
        for kind in [
            ActionKind::MouseMove,
            ActionKind::MouseClick,
            ActionKind::TypeText,
            ActionKind::KeyPress,
        ] {
            assert_eq!(
                resolve_approval(HidRunMode::Off, kind, &wl),
                ApprovalDecision::Refuse,
                "Off must refuse {kind:?} even when whitelisted"
            );
        }
    }

    #[test]
    fn approval_auto_run_performs_every_kind_without_a_grant() {
        // Auto-run performs unconditionally — no prompt, no whitelist consult.
        let wl = SessionWhitelist::new();
        assert!(wl.is_empty());
        for kind in [
            ActionKind::MouseMove,
            ActionKind::MouseClick,
            ActionKind::TypeText,
            ActionKind::KeyPress,
        ] {
            assert_eq!(
                resolve_approval(HidRunMode::AutoRun, kind, &wl),
                ApprovalDecision::Perform,
                "Auto-run must perform {kind:?} without a grant"
            );
        }
    }

    #[test]
    fn approval_ask_prompts_a_new_kind_and_performs_a_whitelisted_kind() {
        // Ask is the interesting axis: an ungranted kind prompts; the same kind
        // after "Always allow" performs; an unrelated kind still prompts.
        let mut wl = SessionWhitelist::new();
        assert_eq!(
            resolve_approval(HidRunMode::Ask, ActionKind::MouseClick, &wl),
            ApprovalDecision::Prompt,
            "an ungranted kind must prompt in Ask mode"
        );

        wl.allow(ActionKind::MouseClick);
        assert_eq!(
            resolve_approval(HidRunMode::Ask, ActionKind::MouseClick, &wl),
            ApprovalDecision::Perform,
            "a whitelisted kind must perform without prompting"
        );
        // The grant is by-kind, not blanket: a different kind still prompts.
        assert_eq!(
            resolve_approval(HidRunMode::Ask, ActionKind::TypeText, &wl),
            ApprovalDecision::Prompt,
            "granting one kind must not suppress prompts for another"
        );
    }

    #[test]
    fn approval_always_allow_adds_the_kind_and_session_clear_empties_it() {
        // "Always allow this kind" adds exactly that kind; session-clear empties
        // the set so no grant outlives its run (R023).
        let mut wl = SessionWhitelist::new();
        assert!(wl.is_empty(), "a fresh session grants nothing");
        assert!(!wl.contains(ActionKind::KeyPress));

        wl.allow(ActionKind::KeyPress);
        assert!(wl.contains(ActionKind::KeyPress), "allow must add the kind");
        assert!(
            !wl.contains(ActionKind::MouseMove),
            "allow adds only the named kind"
        );

        // Idempotent — allowing the same kind twice is one grant.
        wl.allow(ActionKind::KeyPress);
        assert!(wl.contains(ActionKind::KeyPress));

        wl.clear();
        assert!(wl.is_empty(), "session-clear must empty the whitelist");
        assert!(
            !wl.contains(ActionKind::KeyPress),
            "a cleared kind prompts again"
        );
        // After clear, that kind resolves back to Prompt in Ask mode.
        assert_eq!(
            resolve_approval(HidRunMode::Ask, ActionKind::KeyPress, &wl),
            ApprovalDecision::Prompt,
        );
    }

    #[test]
    fn hid_arm_state_stores_the_three_way_mode_and_derives_armed() {
        // S04: the shared holder now stores the run mode; armed() is mode != Off,
        // keeping the S03 structural gate (InputTool advertise/refuse) intact.
        let arm = HidArmState::disarmed();
        assert_eq!(arm.mode(), HidRunMode::Off);
        assert!(!arm.armed(), "Off is the only safe startup posture");

        arm.set_mode(HidRunMode::Ask);
        assert_eq!(arm.mode(), HidRunMode::Ask);
        assert!(arm.armed(), "Ask is an armed mode");

        arm.set_mode(HidRunMode::AutoRun);
        assert_eq!(arm.mode(), HidRunMode::AutoRun);
        assert!(arm.armed(), "Auto-run is an armed mode");

        arm.set_mode(HidRunMode::Off);
        assert!(!arm.armed(), "Off disarms");

        // Back-compat boolean writer maps arm→Ask, disarm→Off.
        arm.set_armed(true);
        assert_eq!(arm.mode(), HidRunMode::Ask);
        arm.set_armed(false);
        assert_eq!(arm.mode(), HidRunMode::Off);
    }

    #[test]
    fn selecting_active_mode_without_permission_is_refused_typed_and_stays_off() {
        // D038: the selector never claims an armed mode without a real
        // Accessibility grant. Both Ask and Auto-run are refused, the persist is
        // never reached, and the machine stays Off with a typed permission-denied.
        for desired in [HidRunMode::Ask, HidRunMode::AutoRun] {
            let mut persisted = None;
            let (mode, error) = resolve_run_mode(
                desired,
                HidRunMode::Off,
                InputPermission {
                    granted: false,
                    supported: true,
                },
                |m| {
                    persisted = Some(m);
                    Ok(())
                },
            );
            assert_eq!(
                mode,
                HidRunMode::Off,
                "an ungranted {desired:?} must stay Off"
            );
            assert_eq!(
                persisted, None,
                "permission preflight must refuse before persisting"
            );
            let err = error.expect("a refused select must surface a typed error");
            assert_eq!(err.kind(), "permission-denied");
            assert_eq!(
                serde_json::to_value(&err).unwrap()["kind"],
                "permission-denied"
            );
        }
    }

    #[test]
    fn selecting_active_mode_with_permission_persists_and_clears_error() {
        for desired in [HidRunMode::Ask, HidRunMode::AutoRun] {
            let mut persisted = None;
            let (mode, error) = resolve_run_mode(
                desired,
                HidRunMode::Off,
                InputPermission {
                    granted: true,
                    supported: true,
                },
                |m| {
                    persisted = Some(m);
                    Ok(())
                },
            );
            assert_eq!(mode, desired);
            assert_eq!(
                persisted,
                Some(desired),
                "a granted select must persist the mode"
            );
            assert!(error.is_none());
        }
    }

    #[test]
    fn selecting_off_skips_the_permission_gate() {
        // Turning HID off must always succeed — even on a machine with no grant
        // (e.g. it was revoked out of band). No env fallback, no refusal.
        let mut persisted = None;
        let (mode, error) = resolve_run_mode(
            HidRunMode::Off,
            HidRunMode::Ask,
            InputPermission {
                granted: false,
                supported: true,
            },
            |m| {
                persisted = Some(m);
                Ok(())
            },
        );
        assert_eq!(mode, HidRunMode::Off);
        assert_eq!(
            persisted,
            Some(HidRunMode::Off),
            "Off must persist regardless of permission"
        );
        assert!(
            error.is_none(),
            "selecting Off is never a permission failure"
        );
    }

    #[test]
    fn run_mode_persist_failure_rolls_back_to_previous_and_surfaces_typed_error() {
        // An unpersisted mode choice must never silently revert on restart: the
        // in-memory mode rolls back to `previous` and the error is kept typed.
        let (mode, error) = resolve_run_mode(
            HidRunMode::AutoRun,
            HidRunMode::Ask,
            InputPermission {
                granted: true,
                supported: true,
            },
            |_| Err("disk full at /settings.json".to_string()),
        );
        assert_eq!(
            mode,
            HidRunMode::Ask,
            "persist failure must roll back to the previous mode"
        );
        let err = error.expect("a persist failure must be surfaced, not swallowed");
        assert_eq!(err.kind(), "input-failed");
        assert_eq!(
            serde_json::to_value(&err).unwrap()["detail"],
            "disk full at /settings.json"
        );
    }

    #[test]
    fn status_carries_the_run_mode_for_the_selector() {
        // The hid://state broadcast now carries `mode` so the Settings selector
        // renders the persisted three-way choice, not just the boolean.
        let (state, _) = state_with(InputPermission {
            granted: true,
            supported: true,
        });
        assert_eq!(
            state.status().mode,
            HidRunMode::Off,
            "status defaults to Off"
        );
        let v = serde_json::to_value(state.status()).unwrap();
        assert_eq!(v["mode"], "off");
        assert_eq!(v["armed"], false);

        state.arm_state().set_mode(HidRunMode::AutoRun);
        let status = state.status();
        assert_eq!(status.mode, HidRunMode::AutoRun);
        assert!(
            status.armed,
            "an active mode reports armed for the S03 surface"
        );
        assert_eq!(serde_json::to_value(&status).unwrap()["mode"], "auto-run");
    }

    #[test]
    fn hid_run_mode_serializes_kebab_case_and_defaults_off() {
        // The persisted/IPC strings config.rs and src/chat.ts key on, and the
        // safe default a missing/garbage value falls back to.
        assert_eq!(serde_json::to_value(HidRunMode::Off).unwrap(), "off");
        assert_eq!(serde_json::to_value(HidRunMode::Ask).unwrap(), "ask");
        assert_eq!(
            serde_json::to_value(HidRunMode::AutoRun).unwrap(),
            "auto-run"
        );
        assert_eq!(HidRunMode::default(), HidRunMode::Off);
        // Round-trips through the wire form.
        for mode in [HidRunMode::Off, HidRunMode::Ask, HidRunMode::AutoRun] {
            let v = serde_json::to_value(mode).unwrap();
            let back: HidRunMode = serde_json::from_value(v).unwrap();
            assert_eq!(back, mode);
        }
    }
}
