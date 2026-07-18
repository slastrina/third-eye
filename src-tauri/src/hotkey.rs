//! Global hotkey: toggle the overlay from anywhere, in pure Rust.
//!
//! The default shortcut (Cmd+Shift+Space on macOS via the `super` modifier)
//! is registered through tauri-plugin-global-shortcut at startup. The handler
//! runs on the main event loop, so the summon path — hotkey press to
//! window-visible — completes synchronously and its latency is measured and
//! logged on every summon (R005 target: well under 100ms).
//!
//! Registration failure (typically a shortcut conflict with another app) is
//! never silently swallowed: it is logged with the conflicting shortcut named
//! and exposed as managed [`HotkeyStatus`] state queryable by the
//! `hotkey_status` command.
//!
//! T04 makes the binding configurable: `set_hotkey` (and the tray preset
//! submenu, via [`rebind`]) validates the new shortcut, registers it,
//! persists it through tauri-plugin-store (settings.json), and only then
//! unregisters the old one — on any failure the old binding stays active
//! and the failure is typed on [`HotkeyStatus`]. On startup the persisted
//! shortcut is loaded; an invalid persisted value falls back to the default
//! with the fallback named in `HotkeyStatus.error`.

use std::sync::Mutex;
use std::time::Instant;

use serde::Serialize;
use tauri::{AppHandle, Manager};
use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut, ShortcutState};

use crate::config;
use crate::overlay::{self, OverlayEvent, OverlayManager, OverlayState};

/// Default toggle shortcut. `super` maps to Cmd on macOS, Win key elsewhere.
pub const DEFAULT_SHORTCUT: &str = "super+shift+space";

/// Preset shortcuts offered in the tray's Hotkey submenu. All must parse
/// (unit-tested); the default is always among them so the user can rebind
/// back without editing settings.json.
pub const HOTKEY_PRESETS: [&str; 4] =
    [DEFAULT_SHORTCUT, "alt+space", "ctrl+shift+space", "super+shift+k"];

/// Human-readable menu label for a shortcut: macOS modifier symbols on
/// macOS ("⌘⇧Space"), title-cased plus-joined tokens elsewhere
/// ("Super+Shift+Space").
pub fn preset_label(shortcut: &str) -> String {
    let parts: Vec<String> = shortcut.split('+').map(token_label).collect();
    if cfg!(target_os = "macos") {
        parts.concat()
    } else {
        parts.join("+")
    }
}

fn token_label(token: &str) -> String {
    #[cfg(target_os = "macos")]
    match token {
        "super" => return "⌘".into(),
        "shift" => return "⇧".into(),
        "alt" => return "⌥".into(),
        "ctrl" => return "⌃".into(),
        _ => {}
    }
    let mut chars = token.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// Registration outcome, managed as app state so a failed registration is
/// queryable by the UI (and S05's settings surface), not just a log line.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HotkeyStatus {
    pub shortcut: String,
    pub registered: bool,
    pub error: Option<String>,
}

/// Managed hotkey state: the live [`HotkeyStatus`] behind a lock so
/// `set_hotkey`/tray rebinds mutate it and `hotkey_status` reflects live
/// rebinds, not just the startup outcome.
pub struct HotkeyState(Mutex<HotkeyStatus>);

impl HotkeyState {
    pub fn status(&self) -> HotkeyStatus {
        self.0.lock().unwrap().clone()
    }
}

/// Pure toggle decision: a hotkey press summons a hidden overlay and
/// dismisses a visible one (idle or focused).
pub fn toggle_event(current: OverlayState) -> OverlayEvent {
    match current {
        OverlayState::Hidden => OverlayEvent::Show,
        OverlayState::VisibleIdle | OverlayState::VisibleFocused => OverlayEvent::Hide,
    }
}

/// Pure startup choice: use the persisted shortcut when it parses, else the
/// default — with the fallback (and the bad value) named so it lands in
/// `HotkeyStatus.error`, never a silent revert.
pub fn startup_shortcut(persisted: Option<&str>) -> (String, Option<String>) {
    match persisted {
        None => (DEFAULT_SHORTCUT.into(), None),
        Some(s) => match s.parse::<Shortcut>() {
            Ok(_) => (s.into(), None),
            Err(e) => (
                DEFAULT_SHORTCUT.into(),
                Some(format!(
                    "persisted shortcut '{s}' is invalid ({e:?}); fell back to \
                     default '{DEFAULT_SHORTCUT}'"
                )),
            ),
        },
    }
}

/// Register the startup shortcut — the persisted one when valid, else the
/// default. Always returns a [`HotkeyState`] — the caller manages it as app
/// state; a registration failure is surfaced there and logged, never fatal
/// (the app still runs, commands still work).
pub fn init(app: &AppHandle) -> HotkeyState {
    let persisted = config::load_hotkey(app);
    let (shortcut, fallback) = startup_shortcut(persisted.as_deref());
    if let Some(msg) = &fallback {
        log::error!("hotkey: {msg}");
    }
    let mut status = register(app, &shortcut);
    // The fallback is part of the queryable status even when the default
    // then registered fine — the user's configured value was not honored.
    if let Some(msg) = fallback {
        status.error = Some(match status.error.take() {
            Some(reg_err) => format!("{msg}; {reg_err}"),
            None => msg,
        });
    }
    HotkeyState(Mutex::new(status))
}

/// Parse and register `shortcut_str`, wiring the toggle handler. Returns
/// the resulting status; failures are logged and typed, never fatal.
fn register(app: &AppHandle, shortcut_str: &str) -> HotkeyStatus {
    let shortcut: Shortcut = match shortcut_str.parse() {
        Ok(s) => s,
        Err(e) => {
            let msg = format!("shortcut '{shortcut_str}' failed to parse: {e:?}");
            log::error!("hotkey: {msg}");
            return HotkeyStatus {
                shortcut: shortcut_str.into(),
                registered: false,
                error: Some(msg),
            };
        }
    };

    let result = app.global_shortcut().on_shortcut(shortcut, |app, _shortcut, event| {
        if event.state() == ShortcutState::Pressed {
            on_hotkey_pressed(app);
        }
    });

    match result {
        Ok(()) => {
            log::info!("hotkey: registered global shortcut '{shortcut_str}'");
            HotkeyStatus { shortcut: shortcut_str.into(), registered: true, error: None }
        }
        Err(e) => {
            let msg = format!(
                "global shortcut '{shortcut_str}' registration failed \
                 (likely already taken by another app): {e}"
            );
            log::error!("hotkey: {msg}");
            HotkeyStatus { shortcut: shortcut_str.into(), registered: false, error: Some(msg) }
        }
    }
}

/// Rebind the global shortcut: parse, register the new binding, persist it,
/// and only then unregister the old one. On any failure the old binding
/// stays active (a failed persist rolls the new registration back so the
/// binding can never silently revert on restart) and the error is typed on
/// the returned/managed [`HotkeyStatus`].
pub fn rebind(app: &AppHandle, state: &HotkeyState, new: &str) -> HotkeyStatus {
    let mut status = state.0.lock().unwrap();
    let old = status.clone();

    if new == old.shortcut && old.registered {
        log::info!("hotkey: rebind no-op — '{new}' is already the active shortcut");
        return old;
    }

    let new_shortcut: Shortcut = match new.parse() {
        Ok(s) => s,
        Err(e) => {
            let msg = format!(
                "rebind rejected: shortcut '{new}' failed to parse: {e:?}; \
                 keeping '{}'",
                old.shortcut
            );
            log::error!("hotkey: {msg}");
            status.error = Some(msg);
            return status.clone();
        }
    };

    let registered = app.global_shortcut().on_shortcut(new_shortcut, |app, _shortcut, event| {
        if event.state() == ShortcutState::Pressed {
            on_hotkey_pressed(app);
        }
    });
    if let Err(e) = registered {
        let msg = format!(
            "rebind failed: shortcut '{new}' registration failed (likely \
             already taken by another app): {e}; keeping '{}'",
            old.shortcut
        );
        log::error!("hotkey: {msg}");
        status.error = Some(msg);
        return status.clone();
    }

    if let Err(persist_err) = config::save_hotkey(app, new) {
        // Roll back: a live-but-unpersisted binding would silently revert
        // on the next launch, which is worse than keeping the old one.
        if let Err(e) = app.global_shortcut().unregister(new_shortcut) {
            log::warn!("hotkey: rollback unregister of '{new}' failed: {e}");
        }
        let msg = format!("rebind failed: {persist_err}; keeping '{}'", old.shortcut);
        log::error!("hotkey: {msg}");
        status.error = Some(msg);
        return status.clone();
    }

    if old.registered {
        match old.shortcut.parse::<Shortcut>() {
            Ok(old_shortcut) => {
                if let Err(e) = app.global_shortcut().unregister(old_shortcut) {
                    // Non-fatal: both bindings summon until restart; the new
                    // one is registered and persisted.
                    log::warn!(
                        "hotkey: failed to unregister old shortcut '{}': {e}",
                        old.shortcut
                    );
                }
            }
            Err(e) => log::warn!(
                "hotkey: old shortcut '{}' no longer parses ({e:?}) — skipping unregister",
                old.shortcut
            ),
        }
    }

    log::info!("hotkey: rebound '{}' → '{new}'", old.shortcut);
    *status = HotkeyStatus { shortcut: new.into(), registered: true, error: None };
    status.clone()
}

/// Toggle the overlay. Runs on the main event loop, so the platform show/hide
/// side effects execute inline and the measured delta is real hotkey-press to
/// window-visible latency.
fn on_hotkey_pressed(app: &AppHandle) {
    let pressed_at = Instant::now();
    let current = app.state::<OverlayManager>().current();
    match toggle_event(current) {
        // Summon chains Show then Focus: visible-idle is click-through, so an
        // overlay left there can never be clicked into focus (clicks fall to
        // the desktop). The hotkey must land in visible-focused — clickable,
        // panel key, input ready.
        OverlayEvent::Show => {
            let summoned = overlay::show_overlay(app.clone())
                .and_then(|_| overlay::focus_overlay(app.clone()));
            match summoned {
                Ok(state) => log::info!(
                    "hotkey: summon latency: {:.2}ms (hotkey-press to input-ready, state={})",
                    pressed_at.elapsed().as_secs_f64() * 1000.0,
                    state.as_str()
                ),
                Err(e) => log::error!("hotkey: summon failed: {e}"),
            }
        }
        OverlayEvent::Hide => match overlay::hide_overlay(app.clone()) {
            Ok(state) => log::info!(
                "hotkey: dismissed overlay in {:.2}ms (state={})",
                pressed_at.elapsed().as_secs_f64() * 1000.0,
                state.as_str()
            ),
            Err(e) => log::error!("hotkey: dismiss failed: {e}"),
        },
        OverlayEvent::Focus => unreachable!("toggle_event never yields Focus"),
    }
}

/// Expose registration state to the UI: `{ shortcut, registered, error }`.
/// Reflects live rebinds and the startup fallback, not just startup.
#[tauri::command]
pub fn hotkey_status(state: tauri::State<'_, HotkeyState>) -> HotkeyStatus {
    state.status()
}

/// Rebind the global shortcut (health-as-value IPC for S07's settings
/// surface): always returns the resulting [`HotkeyStatus`] — on failure the
/// old binding stays active and `error` says why, never an IPC error.
#[tauri::command]
pub fn set_hotkey(
    app: AppHandle,
    state: tauri::State<'_, HotkeyState>,
    shortcut: String,
) -> HotkeyStatus {
    rebind(&app, &state, &shortcut)
}

#[cfg(test)]
mod tests {
    use super::*;
    use OverlayState::*;

    #[test]
    fn hotkey_summons_when_hidden() {
        assert_eq!(toggle_event(Hidden), OverlayEvent::Show);
    }

    #[test]
    fn summon_chain_lands_in_visible_focused() {
        // The handler chains Show → Focus; both transitions must be valid and
        // end in the only state that accepts clicks and typing.
        let s = Hidden.apply(toggle_event(Hidden)).unwrap();
        let s = s.apply(OverlayEvent::Focus).unwrap();
        assert_eq!(s, VisibleFocused);
        assert!(!crate::overlay::click_through(s));
    }

    #[test]
    fn hotkey_dismisses_when_visible_idle_or_focused() {
        assert_eq!(toggle_event(VisibleIdle), OverlayEvent::Hide);
        assert_eq!(toggle_event(VisibleFocused), OverlayEvent::Hide);
    }

    #[test]
    fn toggle_events_are_valid_transitions_from_their_states() {
        // The toggle decision must never produce an event the state machine
        // rejects — otherwise a hotkey press could be silently dropped.
        for state in [Hidden, VisibleIdle, VisibleFocused] {
            assert!(state.apply(toggle_event(state)).is_ok(), "from {state:?}");
        }
    }

    #[test]
    fn default_shortcut_parses() {
        assert!(DEFAULT_SHORTCUT.parse::<Shortcut>().is_ok());
    }

    #[test]
    fn malformed_shortcut_strings_fail_to_parse() {
        // Negative surface for S05's user-configurable shortcuts.
        for bad in ["", "super+", "super+shift+notakey", "space+super"] {
            assert!(bad.parse::<Shortcut>().is_err(), "expected parse failure: {bad:?}");
        }
    }

    #[test]
    fn all_presets_parse_and_include_the_default() {
        // A preset the parser rejects would be a dead menu entry; the
        // default must be offered so the user can always rebind back.
        assert!(HOTKEY_PRESETS.contains(&DEFAULT_SHORTCUT));
        for preset in HOTKEY_PRESETS {
            assert!(preset.parse::<Shortcut>().is_ok(), "preset: {preset}");
        }
    }

    #[test]
    fn presets_are_distinct() {
        // Duplicate presets would collide on tray menu item ids.
        for (i, a) in HOTKEY_PRESETS.iter().enumerate() {
            for b in &HOTKEY_PRESETS[i + 1..] {
                assert_ne!(a, b);
            }
        }
    }

    #[test]
    fn preset_labels_are_human_readable() {
        #[cfg(target_os = "macos")]
        {
            assert_eq!(preset_label("super+shift+space"), "⌘⇧Space");
            assert_eq!(preset_label("alt+space"), "⌥Space");
            assert_eq!(preset_label("ctrl+shift+space"), "⌃⇧Space");
            assert_eq!(preset_label("super+shift+k"), "⌘⇧K");
        }
        #[cfg(not(target_os = "macos"))]
        {
            assert_eq!(preset_label("super+shift+space"), "Super+Shift+Space");
            assert_eq!(preset_label("super+shift+k"), "Super+Shift+K");
        }
    }

    #[test]
    fn startup_uses_valid_persisted_shortcut() {
        assert_eq!(startup_shortcut(Some("alt+space")), ("alt+space".into(), None));
    }

    #[test]
    fn startup_without_persisted_value_uses_default_silently() {
        // Nothing persisted is the normal first launch — not an error.
        assert_eq!(startup_shortcut(None), (DEFAULT_SHORTCUT.into(), None));
    }

    #[test]
    fn startup_falls_back_on_invalid_persisted_value_and_names_it() {
        // Q7/Q5: a corrupt settings.json must not kill the hotkey — the
        // default takes over and the error names both values.
        for bad in ["", "super+", "notakey+x", "42"] {
            let (shortcut, error) = startup_shortcut(Some(bad));
            assert_eq!(shortcut, DEFAULT_SHORTCUT, "bad value: {bad:?}");
            let error = error.expect("fallback must be named");
            assert!(error.contains(bad), "error must name the bad value: {error}");
            assert!(error.contains(DEFAULT_SHORTCUT), "error must name the fallback: {error}");
        }
    }

    #[test]
    fn hotkey_state_exposes_its_status() {
        let state = HotkeyState(Mutex::new(HotkeyStatus {
            shortcut: "alt+space".into(),
            registered: true,
            error: None,
        }));
        let status = state.status();
        assert_eq!(status.shortcut, "alt+space");
        assert!(status.registered && status.error.is_none());
    }

    #[test]
    fn hotkey_status_serializes_camel_case() {
        let status = HotkeyStatus {
            shortcut: DEFAULT_SHORTCUT.into(),
            registered: false,
            error: Some("conflict".into()),
        };
        let v = serde_json::to_value(&status).unwrap();
        assert_eq!(v["shortcut"], DEFAULT_SHORTCUT);
        assert_eq!(v["registered"], false);
        assert_eq!(v["error"], "conflict");
    }
}
