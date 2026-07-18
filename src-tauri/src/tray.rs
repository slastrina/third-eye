//! System tray / menu-bar icon: the resident anchor for Third Eye (R009).
//!
//! T01 scope: a procedurally drawn menu-bar icon with a menu — Activate
//! Third Eye (reuses the S01 show+focus summon chain), three S07 stub
//! entries (Settings…, Configure Models…, Privacy Mode) that summon the
//! overlay and emit a `tray://notice` event the UI renders as a visible
//! transient banner, and Quit. Tray build failure is non-fatal: the caller
//! logs it and the app stays fully usable via the global hotkey (Q5).
//!
//! T02 scope: a [`TrayStatus`] (watching/sleeping) driven by a pure
//! [`ActivityCounter`] behind RAII [`ActivityGuard`]s. LLM streaming and
//! screen capture call [`begin_activity`]; the 0→1 transition starts a
//! ~500ms procedural frame-cycling animation and the 1→0 transition rests
//! the icon on the sleeping frame. Animation tasks are epoch-keyed: every
//! status transition bumps the epoch, and a task exits as soon as its epoch
//! goes stale — no handles to join, no task ever left animating a stale
//! status.

use std::sync::Mutex;

use serde::Serialize;
use tauri::{
    image::Image,
    menu::{CheckMenuItem, IsMenuItem, Menu, MenuItem, PredefinedMenuItem, Submenu},
    tray::TrayIconBuilder,
    AppHandle, Emitter, Manager, Wry,
};

use crate::autostart;
use crate::hotkey;
use crate::overlay::{self, OverlayEvent, OverlayManager, OverlayState};

/// Event emitted when an S07 stub entry is chosen. Payload: `{ feature }`.
/// The UI maps the feature id to banner copy (src/tray-notice.ts); S07
/// replaces the stubs with real surfaces using these same ids.
pub const NOTICE_EVENT: &str = "tray://notice";

/// Stable tray id — there is exactly one tray icon.
pub const TRAY_ID: &str = "third-eye-tray";

// Menu item ids: the string contract between menu construction, the
// menu-event handler, and the S07 surfaces that take over the stub ids.
pub const MENU_ID_ACTIVATE: &str = "activate";
pub const MENU_ID_AUTOSTART: &str = "launch-at-login";
pub const MENU_ID_SETTINGS: &str = "settings";
pub const MENU_ID_CONFIGURE_MODELS: &str = "configure-models";
pub const MENU_ID_PRIVACY_MODE: &str = "privacy-mode";
pub const MENU_ID_QUIT: &str = "quit";
/// Prefix of the Hotkey preset submenu item ids: `hotkey:<shortcut>` (T04).
pub const MENU_ID_HOTKEY_PREFIX: &str = "hotkey:";

/// What a menu selection should do. Pure so the id→action table is testable
/// without a running tray.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuAction {
    /// Summon the overlay through the S01 show+focus chain.
    Activate,
    /// Flip launch-at-login via tauri-plugin-autostart (T03).
    ToggleAutostart,
    /// Rebind the global hotkey to this preset shortcut (T04).
    SetHotkey(&'static str),
    /// Flip privacy mode through the shared applier (S07) — same core as
    /// the `set_privacy_mode` IPC, so the entry points cannot drift.
    TogglePrivacy,
    /// Show the settings window (S07). Settings… and Configure Models… both
    /// land here — the window is one surface for both.
    OpenSettings,
    /// Summon the overlay and emit `tray://notice` naming the stub feature.
    /// No menu entry maps here since S07 de-stubbed the tray; the plumbing
    /// stays for future transient notices.
    Notice(&'static str),
    /// Exit the app cleanly.
    Quit,
    /// Unknown ids map to no action — never a panic or a misfire (Q7).
    Ignore,
}

/// Map a menu item id to its action.
pub fn menu_action(id: &str) -> MenuAction {
    // Preset ids resolve against the static preset table, so a stale or
    // foreign `hotkey:` id can never rebind to an arbitrary string (Q7).
    if let Some(preset) = id.strip_prefix(MENU_ID_HOTKEY_PREFIX) {
        return match hotkey::HOTKEY_PRESETS.iter().find(|&&p| p == preset) {
            Some(&p) => MenuAction::SetHotkey(p),
            None => MenuAction::Ignore,
        };
    }
    match id {
        MENU_ID_ACTIVATE => MenuAction::Activate,
        MENU_ID_AUTOSTART => MenuAction::ToggleAutostart,
        MENU_ID_SETTINGS => MenuAction::OpenSettings,
        MENU_ID_CONFIGURE_MODELS => MenuAction::OpenSettings,
        MENU_ID_PRIVACY_MODE => MenuAction::TogglePrivacy,
        MENU_ID_QUIT => MenuAction::Quit,
        _ => MenuAction::Ignore,
    }
}

/// Events that take the overlay from `current` to visible-focused. Unlike the
/// hotkey's toggle, Activate always summons: the strict state machine rejects
/// redundant Show/Focus, so the chain is computed from the current state.
pub fn summon_events(current: OverlayState) -> &'static [OverlayEvent] {
    match current {
        OverlayState::Hidden => &[OverlayEvent::Show, OverlayEvent::Focus],
        OverlayState::VisibleIdle => &[OverlayEvent::Focus],
        OverlayState::VisibleFocused => &[],
    }
}

/// Payload of [`NOTICE_EVENT`].
#[derive(Debug, Clone, Serialize)]
pub struct NoticePayload {
    pub feature: String,
}

/// Side length of the procedural tray frames (RGBA, no asset pipeline).
pub const ICON_SIZE: u32 = 32;

/// Procedural sleeping frame: a closed-eyelid arc, pure white-on-transparent
/// so macOS renders it as a template image (auto light/dark). T02 adds the
/// animated watching frames beside this one.
pub fn sleeping_frame_rgba() -> Vec<u8> {
    let n = ICON_SIZE as i32;
    let mut buf = vec![0u8; (n * n * 4) as usize];
    for y in 0..n {
        for x in 0..n {
            // Pixel center in normalized [-1, 1] coordinates, y downward.
            let fx = (x as f64 + 0.5) / n as f64 * 2.0 - 1.0;
            let fy = (y as f64 + 0.5) / n as f64 * 2.0 - 1.0;
            // Closed lid: a cup-shaped arc, center dipping below the edges.
            let lid = 0.18 - 0.35 * fx * fx;
            if fx.abs() <= 0.72 && (fy - lid).abs() <= 0.14 {
                let i = ((y * n + x) * 4) as usize;
                buf[i..i + 4].copy_from_slice(&[255, 255, 255, 255]);
            }
        }
    }
    buf
}

/// Procedural privacy resting frame (S07): the closed lid crossed by a
/// diagonal "do not" slash, so idle-with-privacy is visibly distinct from
/// plain sleeping. Same template-safe white-on-transparent palette.
pub fn privacy_frame_rgba() -> Vec<u8> {
    let n = ICON_SIZE as i32;
    let mut buf = vec![0u8; (n * n * 4) as usize];
    for y in 0..n {
        for x in 0..n {
            let fx = (x as f64 + 0.5) / n as f64 * 2.0 - 1.0;
            let fy = (y as f64 + 0.5) / n as f64 * 2.0 - 1.0;
            // Same closed lid as the sleeping frame…
            let lid = 0.18 - 0.35 * fx * fx;
            let in_lid = fx.abs() <= 0.72 && (fy - lid).abs() <= 0.14;
            // …crossed by a diagonal slash: the universal "blocked" mark.
            let in_slash = (fy + fx).abs() <= 0.12 && fx.abs() <= 0.80 && fy.abs() <= 0.80;
            if in_lid || in_slash {
                let i = ((y * n + x) * 4) as usize;
                buf[i..i + 4].copy_from_slice(&[255, 255, 255, 255]);
            }
        }
    }
    buf
}

/// Pupil positions the watching eye cycles through — a slow scan left and
/// right, one step per animation tick.
const PUPIL_CYCLE: [f64; 4] = [0.0, 0.30, 0.0, -0.30];

/// Milliseconds between watching frames.
pub const FRAME_INTERVAL_MS: u64 = 500;

/// Procedural watching frame: an open-eye outline with a pupil whose
/// position depends on `tick` (period [`PUPIL_CYCLE`]`.len()`), so
/// consecutive frames differ and the icon visibly animates. Same
/// template-safe white-on-transparent palette as the sleeping frame.
pub fn watching_frame_rgba(tick: usize) -> Vec<u8> {
    let pupil_x = PUPIL_CYCLE[tick % PUPIL_CYCLE.len()];
    let n = ICON_SIZE as i32;
    let mut buf = vec![0u8; (n * n * 4) as usize];
    for y in 0..n {
        for x in 0..n {
            let fx = (x as f64 + 0.5) / n as f64 * 2.0 - 1.0;
            let fy = (y as f64 + 0.5) / n as f64 * 2.0 - 1.0;
            // Eye outline: the band between two nested lens shapes formed
            // by mirrored parabolic lids.
            let lid = 0.62 * (1.0 - (fx / 0.82) * (fx / 0.82));
            let in_outer = fx.abs() <= 0.82 && fy.abs() <= lid;
            let in_inner = fx.abs() <= 0.70 && fy.abs() <= lid - 0.16;
            // Pupil: a filled dot inside the lens, scanning with the tick.
            let dx = fx - pupil_x;
            let pupil = in_inner && dx * dx + fy * fy <= 0.20 * 0.20;
            if (in_outer && !in_inner) || pupil {
                let i = ((y * n + x) * 4) as usize;
                buf[i..i + 4].copy_from_slice(&[255, 255, 255, 255]);
            }
        }
    }
    buf
}

/// Tray status: watching while any LLM stream or screen capture is active,
/// sleeping otherwise.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TrayStatus {
    Watching,
    Sleeping,
}

impl TrayStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            TrayStatus::Watching => "watching",
            TrayStatus::Sleeping => "sleeping",
        }
    }
}

/// What kind of work is keeping the eye open — names the cause in logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivityKind {
    Stream,
    Capture,
}

impl ActivityKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ActivityKind::Stream => "stream",
            ActivityKind::Capture => "capture",
        }
    }
}

/// Outcome of one activity-counter transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusChange {
    To(TrayStatus),
    NoChange,
    /// `end` without a matching `begin` — a caller bug, guarded instead of
    /// wrapping the counter (Q7).
    Underflow,
}

/// Pure activity counter: 0→1 wakes the eye, 1→0 puts it to sleep, and
/// overlapping activities keep it watching. Pure so every transition —
/// including overlap and underflow — is testable without a tray.
#[derive(Debug, Default)]
pub struct ActivityCounter {
    count: usize,
}

impl ActivityCounter {
    pub const fn new() -> Self {
        Self { count: 0 }
    }

    pub fn count(&self) -> usize {
        self.count
    }

    pub fn begin(&mut self) -> StatusChange {
        self.count += 1;
        if self.count == 1 {
            StatusChange::To(TrayStatus::Watching)
        } else {
            StatusChange::NoChange
        }
    }

    pub fn end(&mut self) -> StatusChange {
        match self.count {
            0 => StatusChange::Underflow,
            1 => {
                self.count = 0;
                StatusChange::To(TrayStatus::Sleeping)
            }
            _ => {
                self.count -= 1;
                StatusChange::NoChange
            }
        }
    }
}

/// Managed activity state: the counter plus an animation epoch. Both status
/// transitions bump the epoch under the same lock as the counter, so an
/// animation task holding a stale epoch can never race a newer transition.
pub struct TrayActivity {
    inner: Mutex<TrayActivityInner>,
}

struct TrayActivityInner {
    counter: ActivityCounter,
    epoch: u64,
}

impl TrayActivity {
    pub fn new() -> Self {
        Self { inner: Mutex::new(TrayActivityInner { counter: ActivityCounter::new(), epoch: 0 }) }
    }

    /// Returns the transition, the activity count after it, and the epoch
    /// after it (bumped when the status changed).
    pub fn begin(&self) -> (StatusChange, usize, u64) {
        let mut inner = self.inner.lock().unwrap();
        let change = inner.counter.begin();
        if change == StatusChange::To(TrayStatus::Watching) {
            inner.epoch += 1;
        }
        (change, inner.counter.count(), inner.epoch)
    }

    /// Returns the transition and the activity count after it.
    pub fn end(&self) -> (StatusChange, usize) {
        let mut inner = self.inner.lock().unwrap();
        let change = inner.counter.end();
        if change == StatusChange::To(TrayStatus::Sleeping) {
            inner.epoch += 1;
        }
        (change, inner.counter.count())
    }

    pub fn epoch(&self) -> u64 {
        self.inner.lock().unwrap().epoch
    }

    pub fn count(&self) -> usize {
        self.inner.lock().unwrap().counter.count()
    }
}

/// RAII guard from [`begin_activity`]: ends the activity on drop — on every
/// exit path, including an aborted stream task, since aborting a tokio task
/// drops the future and runs its locals' destructors.
pub struct ActivityGuard {
    app: AppHandle,
    kind: ActivityKind,
}

impl Drop for ActivityGuard {
    fn drop(&mut self) {
        end_activity(&self.app, self.kind);
    }
}

/// Mark one activity (stream or capture) as running until the returned
/// guard drops. The 0→1 transition logs and starts the watching animation;
/// nested activities just join the count.
pub fn begin_activity(app: &AppHandle, kind: ActivityKind) -> ActivityGuard {
    match app.try_state::<TrayActivity>() {
        Some(state) => {
            let (change, activities, epoch) = state.begin();
            if change == StatusChange::To(TrayStatus::Watching) {
                log::info!(
                    "tray status: watching (activities={activities}, kind={})",
                    kind.as_str()
                );
                spawn_animation(app.clone(), epoch);
            } else {
                log::debug!(
                    "tray: activity joined (activities={activities}, kind={})",
                    kind.as_str()
                );
            }
        }
        // Only reachable if a caller outruns setup's manage() — status goes
        // untracked but the caller's real work must not be blocked.
        None => log::debug!("tray: activity state unmanaged (kind={})", kind.as_str()),
    }
    ActivityGuard { app: app.clone(), kind }
}

fn end_activity(app: &AppHandle, kind: ActivityKind) {
    let Some(state) = app.try_state::<TrayActivity>() else {
        return;
    };
    let (change, activities) = state.end();
    match change {
        StatusChange::To(TrayStatus::Sleeping) => {
            // The epoch bump already stopped the animation task; rest the
            // icon without waiting for it to notice.
            log::info!("tray status: sleeping");
            set_tray_frame(app, resting_frame(app));
        }
        StatusChange::NoChange => {
            log::debug!("tray: activity left (activities={activities}, kind={})", kind.as_str());
        }
        StatusChange::Underflow => {
            log::error!("tray: activity underflow (kind={}) — end without begin ignored", kind.as_str());
        }
        StatusChange::To(TrayStatus::Watching) => unreachable!("end() never starts watching"),
    }
}

/// True when privacy mode is on — the managed [`crate::capture::PrivacyState`]
/// is the single truth (S07); unmanaged state reads as off.
fn privacy_enabled(app: &AppHandle) -> bool {
    app.try_state::<crate::capture::PrivacyState>().map(|s| s.enabled()).unwrap_or(false)
}

/// The frame the icon rests on when no activity runs: the privacy frame
/// while privacy mode is on (a user-visible state indicator, S07), the
/// sleeping frame otherwise.
fn resting_frame(app: &AppHandle) -> Vec<u8> {
    if privacy_enabled(app) {
        privacy_frame_rgba()
    } else {
        sleeping_frame_rgba()
    }
}

/// Redraw the resting frame if the tray is idle — called by the privacy
/// applier so a toggle is visible immediately. While activities run, the
/// watching animation owns the icon and the next sleep transition picks up
/// the right resting frame on its own.
pub fn refresh_resting_frame(app: &AppHandle) {
    let idle = app.try_state::<TrayActivity>().map(|s| s.count() == 0).unwrap_or(true);
    if idle {
        set_tray_frame(app, resting_frame(app));
    }
}

/// Cycle watching frames every [`FRAME_INTERVAL_MS`] until the epoch goes
/// stale (the next status transition) or there is no tray to draw on.
fn spawn_animation(app: AppHandle, epoch: u64) {
    tauri::async_runtime::spawn(async move {
        let mut tick: usize = 0;
        loop {
            if app.try_state::<TrayActivity>().map(|s| s.epoch()) != Some(epoch) {
                break;
            }
            if !set_tray_frame(&app, watching_frame_rgba(tick)) {
                break;
            }
            tick = tick.wrapping_add(1);
            tokio::time::sleep(std::time::Duration::from_millis(FRAME_INTERVAL_MS)).await;
        }
        log::debug!("tray: watching animation stopped (epoch={epoch}, frames={tick})");
    });
}

/// Draw `rgba` on the tray icon. Returns false when there is no tray (build
/// failed at startup — non-fatal per Q5) or the icon set failed; both are
/// logged, never bubbled, because status frames are cosmetic.
fn set_tray_frame(app: &AppHandle, rgba: Vec<u8>) -> bool {
    let Some(tray) = app.tray_by_id(TRAY_ID) else {
        log::debug!("tray: no tray icon to draw status frame on");
        return false;
    };
    if let Err(e) = tray.set_icon(Some(Image::new_owned(rgba, ICON_SIZE, ICON_SIZE))) {
        log::warn!("tray: failed to set status frame: {e}");
        return false;
    }
    // Reassert template rendering: setting a new icon must not cost the
    // light/dark menu-bar legibility established at build time.
    #[cfg(target_os = "macos")]
    if let Err(e) = tray.set_icon_as_template(true) {
        log::warn!("tray: failed to re-mark icon as template: {e}");
    }
    true
}

/// Menu items whose checked state must track backend state after build
/// time. Managed as app state so the menu-event handler can resync the
/// launch-at-login check item to the real OS state after every toggle, and
/// the hotkey preset checks to whichever shortcut is actually bound.
pub struct TrayUi {
    autostart_item: CheckMenuItem<Wry>,
    hotkey_items: Vec<(&'static str, CheckMenuItem<Wry>)>,
    privacy_item: CheckMenuItem<Wry>,
}

/// Build the tray icon and menu. Errors bubble to the caller, which logs
/// them and continues — a missing tray must never take the app down (Q5).
pub fn init(app: &AppHandle) -> Result<(), String> {
    let err = |e: tauri::Error| e.to_string();

    let activate =
        MenuItem::with_id(app, MENU_ID_ACTIVATE, "Activate Third Eye", true, None::<&str>)
            .map_err(err)?;
    // Checked state comes from the OS-owned launcher entry, so it reflects
    // reality even across restarts and out-of-band changes.
    let autostart_item = CheckMenuItem::with_id(
        app,
        MENU_ID_AUTOSTART,
        "Launch at Login",
        true,
        autostart::is_enabled(app),
        None::<&str>,
    )
    .map_err(err)?;
    // Hotkey preset submenu: the active binding (post startup-fallback, so
    // the real one) is checked; choosing a preset rebinds at runtime.
    let active_shortcut = app
        .try_state::<hotkey::HotkeyState>()
        .map(|s| s.status().shortcut)
        .unwrap_or_else(|| hotkey::DEFAULT_SHORTCUT.into());
    let mut hotkey_items: Vec<(&'static str, CheckMenuItem<Wry>)> = Vec::new();
    for preset in hotkey::HOTKEY_PRESETS {
        let item = CheckMenuItem::with_id(
            app,
            format!("{MENU_ID_HOTKEY_PREFIX}{preset}"),
            hotkey::preset_label(preset),
            true,
            preset == active_shortcut,
            None::<&str>,
        )
        .map_err(err)?;
        hotkey_items.push((preset, item));
    }
    let hotkey_refs: Vec<&dyn IsMenuItem<Wry>> =
        hotkey_items.iter().map(|(_, item)| item as &dyn IsMenuItem<Wry>).collect();
    let hotkey_menu = Submenu::with_items(app, "Hotkey", true, &hotkey_refs).map_err(err)?;
    let settings =
        MenuItem::with_id(app, MENU_ID_SETTINGS, "Settings…", true, None::<&str>).map_err(err)?;
    let models =
        MenuItem::with_id(app, MENU_ID_CONFIGURE_MODELS, "Configure Models…", true, None::<&str>)
            .map_err(err)?;
    // Checked state comes from the shared PrivacyState, which setup()
    // seeded from settings.json before building the tray — so the item
    // reflects the persisted toggle across restarts (S07).
    let privacy_item = CheckMenuItem::with_id(
        app,
        MENU_ID_PRIVACY_MODE,
        "Privacy Mode",
        true,
        privacy_enabled(app),
        None::<&str>,
    )
    .map_err(err)?;
    let quit =
        MenuItem::with_id(app, MENU_ID_QUIT, "Quit Third Eye", true, None::<&str>).map_err(err)?;
    let menu = Menu::with_items(
        app,
        &[
            &activate,
            &PredefinedMenuItem::separator(app).map_err(err)?,
            &autostart_item,
            &hotkey_menu,
            &settings,
            &models,
            &privacy_item,
            &PredefinedMenuItem::separator(app).map_err(err)?,
            &quit,
        ],
    )
    .map_err(err)?;

    // The initial icon honors persisted privacy mode (applied before init).
    let builder = TrayIconBuilder::with_id(TRAY_ID)
        .icon(Image::new_owned(resting_frame(app), ICON_SIZE, ICON_SIZE))
        .tooltip("Third Eye")
        .menu(&menu)
        .show_menu_on_left_click(true)
        .on_menu_event(|app, event| on_menu_event(app, event.id().as_ref()));
    // Template rendering keeps the white-on-transparent frames legible on
    // both light and dark menu bars.
    #[cfg(target_os = "macos")]
    let builder = builder.icon_as_template(true);
    builder.build(app).map_err(err)?;

    app.manage(TrayUi { autostart_item, hotkey_items, privacy_item });

    log::info!(
        "tray: initialized (menu: {MENU_ID_ACTIVATE}, {MENU_ID_AUTOSTART}, \
         {MENU_ID_HOTKEY_PREFIX}<{} presets>, {MENU_ID_SETTINGS}, \
         {MENU_ID_CONFIGURE_MODELS}, {MENU_ID_PRIVACY_MODE}, {MENU_ID_QUIT})",
        hotkey::HOTKEY_PRESETS.len()
    );
    Ok(())
}

fn on_menu_event(app: &AppHandle, id: &str) {
    match menu_action(id) {
        MenuAction::Activate => summon(app, MENU_ID_ACTIVATE),
        MenuAction::ToggleAutostart => {
            // The outcome (including failure detail) is logged and recorded
            // on AutostartState by autostart::toggle; here only the check
            // item is resynced — the OS click already flipped it visually,
            // so a failed toggle must flip it back (Q5, never silent).
            let status = autostart::toggle(app);
            sync_autostart_check(app, status.enabled);
        }
        MenuAction::SetHotkey(preset) => {
            // rebind logs the outcome and keeps the old binding on failure;
            // the checks resync to whatever shortcut is actually active, so
            // a failed rebind visibly flips the clicked preset back (Q5).
            match app.try_state::<hotkey::HotkeyState>() {
                Some(state) => {
                    let status = hotkey::rebind(app, &state, preset);
                    sync_hotkey_checks(app, &status.shortcut);
                }
                None => log::error!("tray: hotkey state unmanaged — cannot rebind to '{preset}'"),
            }
        }
        MenuAction::TogglePrivacy => {
            // The shared applier persists (rolling back on failure), logs
            // naming this entry point, broadcasts capture://privacy, and
            // resyncs the check item + resting frame — so a failed persist
            // visibly flips the clicked item back (Q5, never silent).
            let desired = !privacy_enabled(app);
            crate::capture::commands::apply_privacy_mode(app, desired, "tray");
        }
        MenuAction::OpenSettings => {
            // show logs "settings: panel shown (via tray)" on success; a
            // failure is logged here — a settings click is never silent (Q5).
            if let Err(e) = crate::settings_window::show(app, "tray") {
                log::error!("tray: failed to open settings window: {e}");
            }
        }
        MenuAction::Notice(feature) => {
            // Summon first so the banner lands on a visible overlay, but emit
            // even if summoning failed — a stub click is never silent.
            summon(app, feature);
            match app.emit(NOTICE_EVENT, NoticePayload { feature: feature.into() }) {
                Ok(()) => log::info!("tray: emitted {NOTICE_EVENT} for '{feature}'"),
                Err(e) => log::error!("tray: failed to emit {NOTICE_EVENT} for '{feature}': {e}"),
            }
        }
        MenuAction::Quit => {
            log::info!("tray: quit selected — exiting");
            app.exit(0);
        }
        MenuAction::Ignore => log::warn!("tray: unknown menu id '{id}' — ignored"),
    }
}

/// Reflect the real OS launch-at-login state on the check item. Failure to
/// redraw is cosmetic (the status stays queryable via `autostart_status`),
/// so it is logged, never bubbled.
fn sync_autostart_check(app: &AppHandle, enabled: bool) {
    let Some(ui) = app.try_state::<TrayUi>() else {
        return;
    };
    if let Err(e) = ui.autostart_item.set_checked(enabled) {
        log::warn!("tray: failed to sync launch-at-login check state: {e}");
    }
}

/// Reflect the shared privacy-mode state on the check item. Failure to
/// redraw is cosmetic (the truth stays queryable via `privacy_status`), so
/// it is logged, never bubbled.
pub fn sync_privacy_check(app: &AppHandle, enabled: bool) {
    let Some(ui) = app.try_state::<TrayUi>() else {
        return;
    };
    if let Err(e) = ui.privacy_item.set_checked(enabled) {
        log::warn!("tray: failed to sync privacy-mode check state: {e}");
    }
}

/// Check exactly the preset matching the actually-bound shortcut (none, if
/// the active binding is a non-preset from settings.json). Failure to
/// redraw is cosmetic — the truth stays queryable via `hotkey_status` — so
/// it is logged, never bubbled.
fn sync_hotkey_checks(app: &AppHandle, active: &str) {
    let Some(ui) = app.try_state::<TrayUi>() else {
        return;
    };
    for (preset, item) in &ui.hotkey_items {
        if let Err(e) = item.set_checked(*preset == active) {
            log::warn!("tray: failed to sync hotkey check for '{preset}': {e}");
        }
    }
}

/// Drive the overlay to visible-focused via the same command surface the
/// hotkey uses; the S01 latency instrumentation in those paths is untouched.
fn summon(app: &AppHandle, cause: &str) {
    let current = app.state::<OverlayManager>().current();
    for event in summon_events(current) {
        let result = match event {
            OverlayEvent::Show => overlay::show_overlay(app.clone()),
            OverlayEvent::Focus => overlay::focus_overlay(app.clone()),
            OverlayEvent::Hide => overlay::hide_overlay(app.clone()),
        };
        if let Err(e) = result {
            log::error!("tray: summon ({cause}): {event:?} failed: {e}");
            return;
        }
    }
    log::info!("tray: summoned overlay ({cause}, from={})", current.as_str());
}

#[cfg(test)]
mod tests {
    use super::*;
    use OverlayState::*;

    #[test]
    fn every_menu_id_maps_to_its_action() {
        assert_eq!(menu_action(MENU_ID_ACTIVATE), MenuAction::Activate);
        assert_eq!(menu_action(MENU_ID_AUTOSTART), MenuAction::ToggleAutostart);
        // S07: both settings entries open the real settings window — no menu
        // id maps to the stub Notice action anymore.
        assert_eq!(menu_action(MENU_ID_SETTINGS), MenuAction::OpenSettings);
        assert_eq!(menu_action(MENU_ID_CONFIGURE_MODELS), MenuAction::OpenSettings);
        assert_eq!(menu_action(MENU_ID_PRIVACY_MODE), MenuAction::TogglePrivacy);
        assert_eq!(menu_action(MENU_ID_QUIT), MenuAction::Quit);
    }

    #[test]
    fn unknown_menu_id_maps_to_no_action() {
        // Q7: a stale or foreign menu id must never trigger a real action.
        for bad in ["", "settngs", "ACTIVATE", "quit ", "tray://notice", "launch-at-login "] {
            assert_eq!(menu_action(bad), MenuAction::Ignore, "id: {bad:?}");
        }
    }

    #[test]
    fn every_hotkey_preset_id_maps_to_its_rebind() {
        for preset in hotkey::HOTKEY_PRESETS {
            let id = format!("{MENU_ID_HOTKEY_PREFIX}{preset}");
            assert_eq!(menu_action(&id), MenuAction::SetHotkey(preset), "id: {id}");
        }
    }

    #[test]
    fn non_preset_hotkey_ids_map_to_no_action() {
        // Q7: a `hotkey:` id outside the static preset table must never
        // rebind — arbitrary strings cannot reach the registrar via menu ids.
        for bad in [
            "hotkey:",
            "hotkey:notakey",
            "hotkey:super+shift+space ",
            "hotkey:SUPER+SHIFT+SPACE",
            "hotkey:alt+space+extra",
        ] {
            assert_eq!(menu_action(bad), MenuAction::Ignore, "id: {bad:?}");
        }
    }

    #[test]
    fn summon_chain_lands_focused_from_every_state() {
        // Activate must end input-ready no matter where the overlay is, and
        // every step must be a transition the strict state machine accepts.
        for start in [Hidden, VisibleIdle, VisibleFocused] {
            let mut s = start;
            for &event in summon_events(start) {
                s = s.apply(event).unwrap_or_else(|e| panic!("from {start:?}: {e}"));
            }
            assert_eq!(s, VisibleFocused, "starting at {start:?}");
        }
    }

    #[test]
    fn summon_from_focused_is_a_no_op() {
        // Redundant Show/Focus would be rejected by the state machine; the
        // chain must not contain them when already focused.
        assert!(summon_events(VisibleFocused).is_empty());
    }

    #[test]
    fn notice_payload_serializes_feature_field() {
        let v = serde_json::to_value(NoticePayload { feature: "settings".into() }).unwrap();
        assert_eq!(v, serde_json::json!({ "feature": "settings" }));
    }

    #[test]
    fn sleeping_frame_is_a_full_rgba_buffer() {
        let buf = sleeping_frame_rgba();
        assert_eq!(buf.len(), (ICON_SIZE * ICON_SIZE * 4) as usize);
    }

    #[test]
    fn sleeping_frame_draws_only_template_safe_pixels() {
        // macOS template images use alpha only: every pixel must be either
        // fully transparent or opaque white, and both kinds must exist.
        assert_template_safe(&sleeping_frame_rgba());
    }

    /// Every pixel fully transparent or opaque white, with both present.
    fn assert_template_safe(buf: &[u8]) {
        assert_eq!(buf.len(), (ICON_SIZE * ICON_SIZE * 4) as usize);
        let (mut opaque, mut transparent) = (0usize, 0usize);
        for px in buf.chunks_exact(4) {
            match px {
                [255, 255, 255, 255] => opaque += 1,
                [0, 0, 0, 0] => transparent += 1,
                other => panic!("non-template pixel: {other:?}"),
            }
        }
        assert!(opaque > 0, "icon drew nothing");
        assert!(transparent > 0, "icon has no transparent background");
    }

    #[test]
    fn watching_frames_are_template_safe_at_every_cycle_position() {
        for tick in 0..PUPIL_CYCLE.len() {
            assert_template_safe(&watching_frame_rgba(tick));
        }
    }

    #[test]
    fn privacy_frame_is_template_safe() {
        assert_template_safe(&privacy_frame_rgba());
    }

    #[test]
    fn privacy_frame_differs_from_sleeping_and_every_watching_frame() {
        // The privacy resting state must be visually distinct (S07): a user
        // glancing at the menu bar can tell privacy-idle from plain idle
        // and from watching.
        let privacy = privacy_frame_rgba();
        assert_ne!(privacy, sleeping_frame_rgba());
        for tick in 0..PUPIL_CYCLE.len() {
            assert_ne!(privacy, watching_frame_rgba(tick), "tick {tick}");
        }
    }

    #[test]
    fn watching_frames_differ_per_tick_and_from_sleeping() {
        // The icon must visibly animate: consecutive ticks render different
        // buffers, both scan directions differ, and every watching frame
        // differs from the sleeping frame.
        assert_ne!(watching_frame_rgba(0), watching_frame_rgba(1));
        assert_ne!(watching_frame_rgba(1), watching_frame_rgba(3));
        for tick in 0..PUPIL_CYCLE.len() {
            assert_ne!(watching_frame_rgba(tick), sleeping_frame_rgba(), "tick {tick}");
        }
    }

    #[test]
    fn watching_frame_cycle_repeats_and_survives_large_ticks() {
        let period = PUPIL_CYCLE.len();
        assert_eq!(watching_frame_rgba(0), watching_frame_rgba(period));
        assert_eq!(watching_frame_rgba(3), watching_frame_rgba(3 + 2 * period));
        // A long-running animation must not misbehave at large tick values.
        assert_eq!(watching_frame_rgba(usize::MAX % period), watching_frame_rgba(usize::MAX));
    }

    #[test]
    fn tray_status_serializes_lowercase_with_matching_as_str() {
        for (status, name) in [(TrayStatus::Watching, "watching"), (TrayStatus::Sleeping, "sleeping")]
        {
            assert_eq!(status.as_str(), name);
            assert_eq!(serde_json::to_value(status).unwrap(), serde_json::json!(name));
        }
        assert_eq!(ActivityKind::Stream.as_str(), "stream");
        assert_eq!(ActivityKind::Capture.as_str(), "capture");
    }

    #[test]
    fn counter_wakes_on_first_activity_and_sleeps_on_last() {
        let mut c = ActivityCounter::new();
        assert_eq!(c.begin(), StatusChange::To(TrayStatus::Watching));
        assert_eq!(c.count(), 1);
        assert_eq!(c.end(), StatusChange::To(TrayStatus::Sleeping));
        assert_eq!(c.count(), 0);
    }

    #[test]
    fn overlapping_activities_keep_watching_until_the_last_ends() {
        // A capture during a stream must not flip the icon to sleeping when
        // it finishes first.
        let mut c = ActivityCounter::new();
        assert_eq!(c.begin(), StatusChange::To(TrayStatus::Watching));
        assert_eq!(c.begin(), StatusChange::NoChange);
        assert_eq!(c.end(), StatusChange::NoChange);
        assert_eq!(c.count(), 1);
        assert_eq!(c.end(), StatusChange::To(TrayStatus::Sleeping));
    }

    #[test]
    fn counter_underflow_is_guarded_not_wrapped() {
        // Q7: an end without a begin must not wrap to usize::MAX (which
        // would pin the icon on watching forever).
        let mut c = ActivityCounter::new();
        assert_eq!(c.end(), StatusChange::Underflow);
        assert_eq!(c.count(), 0);
        // The counter still works normally afterwards.
        assert_eq!(c.begin(), StatusChange::To(TrayStatus::Watching));
        assert_eq!(c.end(), StatusChange::To(TrayStatus::Sleeping));
    }

    #[test]
    fn activity_epoch_advances_on_every_status_transition() {
        let a = TrayActivity::new();
        let (change, count, e1) = a.begin();
        assert_eq!(change, StatusChange::To(TrayStatus::Watching));
        assert_eq!(count, 1);

        // Joining does not bump the epoch — the running animation stays valid.
        let (change, count, e2) = a.begin();
        assert_eq!(change, StatusChange::NoChange);
        assert_eq!((count, e2), (2, e1));
        assert_eq!(a.end(), (StatusChange::NoChange, 1));
        assert_eq!(a.epoch(), e1);

        // The sleep transition bumps it: an animation task spawned with e1
        // sees a stale epoch and exits.
        assert_eq!(a.end(), (StatusChange::To(TrayStatus::Sleeping), 0));
        assert_ne!(a.epoch(), e1, "sleep must invalidate the watching animation");

        // The next watch cycle gets a fresh epoch, distinct from both.
        let (_, _, e3) = a.begin();
        assert_ne!(e3, e1);
        assert!(a.count() == 1 && a.epoch() == e3);
    }

    #[test]
    fn activity_underflow_leaves_epoch_and_count_untouched() {
        let a = TrayActivity::new();
        let before = a.epoch();
        assert_eq!(a.end(), (StatusChange::Underflow, 0));
        assert_eq!(a.epoch(), before, "underflow must not invalidate anything");
        assert_eq!(a.count(), 0);
    }
}
