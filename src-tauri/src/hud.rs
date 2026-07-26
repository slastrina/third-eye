//! Live automation HUD windows (2026-07 redesign, surface 7): while a chat
//! run executes HID actions the user sees what Third Eye is doing (hud-pill:
//! status pill + action trail) and where (hud-canvas: full-monitor
//! click-through layer drawing the ghost target ring).
//!
//! Architecture: click-through is a per-window property, so the HUD is two
//! windows. Both are created hidden at launch (tauri.conf.json) so showing
//! them on a run never pays window-creation cost, both fold the SAME global
//! `llm://` broadcasts webview-side (hud-state.ts), and only the pill webview
//! drives `show_hud`/`hide_hud` (single driver; the canvas is passive).
//!
//! Focus posture is stricter than the overlay's: these panels can NEVER
//! become key (`can_become_key_window: false`) — a HUD that could hold key
//! status would swallow the synthesized keystrokes it is narrating
//! (MEM: nonactivating-panel-swallows-synthesized-keys). The pill's Stop
//! button still works: mouse clicks on a nonactivating panel need no key
//! status. The canvas additionally ignores cursor events entirely, so the
//! ghost ring can never intercept the real click it annotates.
//!
//! Esc kill-switch: while a run is live AND HID is armed, a global Escape
//! shortcut is registered that fires the same cooperative stop as the Stop
//! button / `stop_chat` (guardrails: "Esc stops instantly"). Outside that
//! window Escape is never grabbed — swallowing every app's Escape during
//! plain text runs would be hostile. Stopping does NOT flip the user's
//! standing Input Control (armed) setting: the loop terminating is what
//! returns the keyboard/mouse; the setting is the user's own (D038).

use tauri::{AppHandle, Manager};
use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut, ShortcutState};

/// Labels of the pre-declared HUD windows (tauri.conf.json).
pub const HUD_PILL_LABEL: &str = "hud-pill";
pub const HUD_CANVAS_LABEL: &str = "hud-canvas";

/// Pure: whether the global-Escape kill-switch should be registered. Only a
/// live run that could be holding the user's input warrants grabbing a key
/// every other app also uses.
pub fn esc_guard_wanted(run_live: bool, hid_armed: bool) -> bool {
    run_live && hid_armed
}

/// The Escape shortcut string (tauri-plugin-global-shortcut syntax).
const ESC_SHORTCUT: &str = "Escape";

/// Logical size of the pill window (fits pill + trail; transparent slack).
const PILL_WIDTH: f64 = 600.0;
const PILL_HEIGHT: f64 = 360.0;
/// Logical offset of the pill from the monitor's top edge.
const PILL_TOP: f64 = 56.0;

#[cfg(target_os = "macos")]
mod platform {
    use super::{HUD_CANVAS_LABEL, HUD_PILL_LABEL};
    use tauri::{AppHandle, Manager};
    use tauri_nspanel::{
        tauri_panel, CollectionBehavior, ManagerExt, PanelLevel, StyleMask, WebviewWindowExt,
    };

    tauri_panel! {
        panel!(HudPanel {
            config: {
                // Never key, never main: the HUD narrates input, it must be
                // structurally unable to receive it (stricter than the
                // overlay, which does take typing when summoned).
                can_become_key_window: false,
                can_become_main_window: false,
                is_floating_panel: true
            }
        })
    }

    /// Convert one window to a nonactivating, never-key NSPanel. `level`
    /// stacks windows (pill above canvas; tray panel reuses this too).
    pub fn convert(app: &AppHandle, label: &str, level: i64) -> Result<(), String> {
        let window = app
            .get_webview_window(label)
            .ok_or_else(|| format!("hud window '{label}' not found"))?;
        let panel = window
            .to_panel::<HudPanel>()
            .map_err(|e| format!("hud '{label}' NSPanel conversion failed: {e}"))?;
        panel.set_style_mask(StyleMask::empty().nonactivating_panel().into());
        panel.set_level(level);
        panel.set_collection_behavior(
            CollectionBehavior::new()
                .can_join_all_spaces()
                .full_screen_auxiliary()
                .ignores_cycle()
                .into(),
        );
        panel.set_hides_on_deactivate(false);
        Ok(())
    }

    pub fn init(app: &AppHandle) -> Result<(), String> {
        // Canvas below pill; both above the overlay's main-menu+1 so the HUD
        // reads over everything while a run is live.
        convert(app, HUD_CANVAS_LABEL, PanelLevel::MainMenu.value() + 2)?;
        convert(app, HUD_PILL_LABEL, PanelLevel::MainMenu.value() + 3)?;
        log::debug!("hud: converted pill+canvas to never-key nonactivating NSPanels");
        Ok(())
    }

    pub fn show(app: &AppHandle, label: &str) -> Result<(), String> {
        let panel = app
            .get_webview_panel(label)
            .map_err(|e| format!("hud panel '{label}' not found: {e:?}"))?;
        app.run_on_main_thread(move || panel.order_front_regardless())
            .map_err(|e| format!("hud '{label}' show dispatch failed: {e}"))
    }

    pub fn hide(app: &AppHandle, label: &str) -> Result<(), String> {
        let panel = app
            .get_webview_panel(label)
            .map_err(|e| format!("hud panel '{label}' not found: {e:?}"))?;
        app.run_on_main_thread(move || panel.hide())
            .map_err(|e| format!("hud '{label}' hide dispatch failed: {e}"))
    }
}

#[cfg(not(target_os = "macos"))]
mod platform {
    use tauri::{AppHandle, Manager};

    /// Plain-window fallback: `focus: false` + always-on-top come from
    /// tauri.conf.json; show/hide map to the window operations.
    pub fn init(_app: &AppHandle) -> Result<(), String> {
        Ok(())
    }

    /// No-op off macOS — plain windows need no conversion.
    pub fn convert(_app: &AppHandle, _label: &str, _level: i64) -> Result<(), String> {
        Ok(())
    }

    pub fn show(app: &AppHandle, label: &str) -> Result<(), String> {
        app.get_webview_window(label)
            .ok_or_else(|| format!("hud window '{label}' not found"))?
            .show()
            .map_err(|e| format!("hud '{label}' show failed: {e}"))
    }

    pub fn hide(app: &AppHandle, label: &str) -> Result<(), String> {
        app.get_webview_window(label)
            .ok_or_else(|| format!("hud window '{label}' not found"))?
            .hide()
            .map_err(|e| format!("hud '{label}' hide failed: {e}"))
    }
}

/// Convert an arbitrary pre-declared window into a nonactivating never-key
/// panel (no-op off macOS) — shared with the tray panel, which has the same
/// "buttons only, must never activate the app" posture as the HUD.
pub fn convert_never_key_panel(
    app: &AppHandle,
    label: &str,
    level_above_main_menu: i64,
) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    let level = tauri_nspanel::PanelLevel::MainMenu.value() + level_above_main_menu;
    #[cfg(not(target_os = "macos"))]
    let level = level_above_main_menu;
    platform::convert(app, label, level)
}

/// Order an arbitrary converted panel front (plain show off macOS). Never
/// takes key or activates the app.
pub fn panel_show(app: &AppHandle, label: &str) -> Result<(), String> {
    platform::show(app, label)
}

/// Order an arbitrary converted panel out (plain hide off macOS).
pub fn panel_hide(app: &AppHandle, label: &str) -> Result<(), String> {
    platform::hide(app, label)
}

/// One-time platform setup, from the Tauri setup hook: panel conversion,
/// permanent canvas click-through, and geometry (canvas covers the primary
/// monitor — v1 limitation, documented in the spec; pill sits top-center).
pub fn init(app: &AppHandle) -> Result<(), String> {
    platform::init(app)?;

    let canvas = app
        .get_webview_window(HUD_CANVAS_LABEL)
        .ok_or_else(|| format!("hud window '{HUD_CANVAS_LABEL}' missing from tauri.conf.json"))?;
    canvas
        .set_ignore_cursor_events(true)
        .map_err(|e| format!("hud canvas set_ignore_cursor_events failed: {e}"))?;
    // The NSPanel conversion re-enables the native window shadow (the
    // config's shadow:false applied to the pre-conversion window). A shadow
    // drawn around a transparent window's content union renders as a grey
    // blob behind the disjoint pill/card/trail — kill it post-conversion.
    let _ = canvas.set_shadow(false);

    let pill = app
        .get_webview_window(HUD_PILL_LABEL)
        .ok_or_else(|| format!("hud window '{HUD_PILL_LABEL}' missing from tauri.conf.json"))?;
    let _ = pill.set_shadow(false);

    // Geometry from the primary monitor. A missing monitor readout leaves the
    // conf.json defaults — the HUD still works, just not perfectly fitted.
    match app.primary_monitor() {
        Ok(Some(monitor)) => {
            let scale = monitor.scale_factor();
            let size = monitor.size().to_logical::<f64>(scale);
            let position = monitor.position().to_logical::<f64>(scale);
            canvas
                .set_position(tauri::LogicalPosition::new(position.x, position.y))
                .and_then(|()| canvas.set_size(tauri::LogicalSize::new(size.width, size.height)))
                .map_err(|e| format!("hud canvas geometry failed: {e}"))?;
            pill.set_position(tauri::LogicalPosition::new(
                position.x + (size.width - PILL_WIDTH) / 2.0,
                position.y + PILL_TOP,
            ))
            .and_then(|()| pill.set_size(tauri::LogicalSize::new(PILL_WIDTH, PILL_HEIGHT)))
            .map_err(|e| format!("hud pill geometry failed: {e}"))?;
        }
        Ok(None) => log::warn!("hud: no primary monitor reported; keeping conf.json geometry"),
        Err(e) => log::warn!("hud: primary monitor query failed ({e}); keeping conf.json geometry"),
    }
    Ok(())
}

/// One monitor's logical rectangle, for [`monitor_index_containing`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MonitorRect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// Pure: index of the monitor whose logical rect contains the point. Edges
/// are inclusive on the origin side (a point at x == origin is on that
/// monitor); `None` when no monitor contains it (off-screen coordinate).
pub fn monitor_index_containing(monitors: &[MonitorRect], x: f64, y: f64) -> Option<usize> {
    monitors
        .iter()
        .position(|m| x >= m.x && x < m.x + m.width && y >= m.y && y < m.y + m.height)
}

/// The canvas fit applied for a ghost target — the webview subtracts the
/// origin from absolute screen points to get window coordinates. Serialized
/// camelCase for the invoke response.
#[derive(Debug, Clone, Copy, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HudCanvasFit {
    pub origin_x: f64,
    pub origin_y: f64,
}

/// Move the hud-canvas over the monitor containing the logical screen point
/// (`x`, `y`) — the current input action's target — and return that
/// monitor's origin (multi-monitor follow-up: the canvas is no longer
/// pinned to the primary monitor). An off-screen point or missing monitor
/// list keeps the current geometry and reports the primary origin (0,0)
/// rather than guessing.
#[tauri::command]
pub fn fit_hud_canvas(app: AppHandle, x: f64, y: f64) -> Result<HudCanvasFit, String> {
    let monitors: Vec<(MonitorRect, f64)> = app
        .available_monitors()
        .map_err(|e| format!("available_monitors failed: {e}"))?
        .into_iter()
        .map(|monitor| {
            let scale = monitor.scale_factor();
            let size = monitor.size().to_logical::<f64>(scale);
            let position = monitor.position().to_logical::<f64>(scale);
            (
                MonitorRect {
                    x: position.x,
                    y: position.y,
                    width: size.width,
                    height: size.height,
                },
                scale,
            )
        })
        .collect();
    let rects: Vec<MonitorRect> = monitors.iter().map(|(rect, _)| *rect).collect();
    let Some(index) = monitor_index_containing(&rects, x, y) else {
        log::debug!("hud: fit target ({x}, {y}) is on no monitor; keeping current canvas fit");
        return Ok(HudCanvasFit {
            origin_x: 0.0,
            origin_y: 0.0,
        });
    };
    let rect = rects[index];
    let canvas = app
        .get_webview_window(HUD_CANVAS_LABEL)
        .ok_or_else(|| format!("hud window '{HUD_CANVAS_LABEL}' not found"))?;
    canvas
        .set_position(tauri::LogicalPosition::new(rect.x, rect.y))
        .and_then(|()| canvas.set_size(tauri::LogicalSize::new(rect.width, rect.height)))
        .map_err(|e| format!("hud canvas fit failed: {e}"))?;
    Ok(HudCanvasFit {
        origin_x: rect.x,
        origin_y: rect.y,
    })
}

/// Show both HUD windows (canvas first so the pill stacks above it on
/// platforms that order by front time). Driven by the pill webview when the
/// folded hud-state first shows input activity. Never steals focus: the
/// panels cannot become key, and the fallback windows are `focus: false`.
#[tauri::command]
pub fn show_hud(app: AppHandle) -> Result<(), String> {
    platform::show(&app, HUD_CANVAS_LABEL)?;
    platform::show(&app, HUD_PILL_LABEL)?;
    log::debug!("hud: shown");
    Ok(())
}

/// Hide both HUD windows. Driven by the pill webview on dismiss/idle.
#[tauri::command]
pub fn hide_hud(app: AppHandle) -> Result<(), String> {
    platform::hide(&app, HUD_PILL_LABEL)?;
    platform::hide(&app, HUD_CANVAS_LABEL)?;
    log::debug!("hud: hidden");
    Ok(())
}

/// Keep the global-Escape kill-switch in sync with the run lifecycle. Called
/// on every run-state broadcast (llm/commands.rs). Escape is registered only
/// while [`esc_guard_wanted`] holds; the handler fires the same cooperative
/// stop as `stop_chat`. Registration failures are logged, never fatal — the
/// pill's Stop button remains the guaranteed path.
pub fn sync_esc_guard(app: &AppHandle, run_live: bool) {
    let armed = app.state::<crate::input::commands::InputState>().armed();
    let wanted = esc_guard_wanted(run_live, armed);
    let shortcut: Shortcut = match ESC_SHORTCUT.parse() {
        Ok(s) => s,
        Err(e) => {
            log::error!("hud: '{ESC_SHORTCUT}' failed to parse: {e:?}");
            return;
        }
    };
    let registered = app.global_shortcut().is_registered(shortcut);
    if wanted && !registered {
        let result = app
            .global_shortcut()
            .on_shortcut(shortcut, |app, _shortcut, event| {
                if event.state() == ShortcutState::Pressed {
                    log::info!("hud: Escape kill-switch pressed — stopping the run");
                    crate::llm::commands::stop_run_from_esc(app);
                }
            });
        match result {
            Ok(()) => log::debug!("hud: Escape kill-switch registered (run live, HID armed)"),
            Err(e) => log::warn!("hud: Escape kill-switch registration failed: {e}"),
        }
    } else if !wanted && registered {
        match app.global_shortcut().unregister(shortcut) {
            Ok(()) => log::debug!("hud: Escape kill-switch released"),
            Err(e) => log::warn!("hud: Escape kill-switch unregister failed: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn esc_guard_only_arms_for_a_live_run_with_hid_armed() {
        // Grabbing every app's Escape is only justified while Third Eye could
        // actually be holding the user's input.
        assert!(esc_guard_wanted(true, true));
        assert!(!esc_guard_wanted(true, false));
        assert!(!esc_guard_wanted(false, true));
        assert!(!esc_guard_wanted(false, false));
    }

    #[test]
    fn monitor_index_containing_picks_the_right_monitor() {
        let monitors = [
            MonitorRect {
                x: 0.0,
                y: 0.0,
                width: 1920.0,
                height: 1080.0,
            },
            MonitorRect {
                x: 1920.0,
                y: 0.0,
                width: 1440.0,
                height: 900.0,
            },
            MonitorRect {
                x: -2560.0,
                y: -200.0,
                width: 2560.0,
                height: 1440.0,
            },
        ];
        assert_eq!(monitor_index_containing(&monitors, 500.0, 500.0), Some(0));
        // Origin edge is inclusive; the shared boundary belongs to the right monitor.
        assert_eq!(monitor_index_containing(&monitors, 1920.0, 10.0), Some(1));
        assert_eq!(monitor_index_containing(&monitors, -100.0, 300.0), Some(2));
        // Off-screen: nobody claims it.
        assert_eq!(monitor_index_containing(&monitors, 9999.0, 9999.0), None);
        assert_eq!(monitor_index_containing(&[], 0.0, 0.0), None);
    }

    #[test]
    fn hud_windows_are_declared_in_tauri_conf() {
        // The show path assumes both pre-created hidden windows exist; a
        // renamed label in tauri.conf.json must fail here, not at runtime.
        let conf = include_str!("../tauri.conf.json");
        assert!(conf.contains(&format!("\"label\": \"{HUD_PILL_LABEL}\"")));
        assert!(conf.contains(&format!("\"label\": \"{HUD_CANVAS_LABEL}\"")));
    }
}
