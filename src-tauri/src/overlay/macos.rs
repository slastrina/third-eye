//! macOS overlay backend: converts the pre-existing overlay window into a
//! nonactivating NSPanel via tauri-nspanel (v2.1).
//!
//! The nonactivating style mask is what prevents focus steal: the panel can
//! become key (accept typing) without activating the app, so the previously
//! frontmost app keeps focus while the overlay is idle, and hiding the panel
//! returns focus for free — the app was never activated.

use tauri::{AppHandle, Manager};
use tauri_nspanel::{
    tauri_panel, CollectionBehavior, ManagerExt, PanelLevel, StyleMask, WebviewWindowExt,
};

use crate::OVERLAY_WINDOW_LABEL;

tauri_panel! {
    panel!(OverlayPanel {
        config: {
            can_become_key_window: true,
            can_become_main_window: false,
            is_floating_panel: true
        }
    })
}

/// Convert the overlay window to a nonactivating NSPanel. Must run on the
/// main thread (Tauri setup hook). Fails loudly — a failed conversion means
/// the plain window would steal focus on show, which is never acceptable.
pub fn init(app: &AppHandle) -> Result<(), String> {
    let window = app
        .get_webview_window(OVERLAY_WINDOW_LABEL)
        .ok_or_else(|| format!("overlay window '{OVERLAY_WINDOW_LABEL}' not found"))?;
    let panel = window
        .to_panel::<OverlayPanel>()
        .map_err(|e| format!("NSPanel conversion failed: {e}"))?;

    // Nonactivating: panel can become key without activating the app.
    panel.set_style_mask(StyleMask::empty().nonactivating_panel().into());
    // Above the main menu so the overlay wins over regular always-on-top windows.
    panel.set_level(PanelLevel::MainMenu.value() + 1);
    // Join every Space and coexist with fullscreen apps; stay out of Cmd+Tab.
    panel.set_collection_behavior(
        CollectionBehavior::new()
            .can_join_all_spaces()
            .full_screen_auxiliary()
            .ignores_cycle()
            .into(),
    );
    // The app never activates, so never auto-hide on deactivate.
    panel.set_hides_on_deactivate(false);

    log::debug!("overlay: converted to nonactivating NSPanel (level=main-menu+1, all-spaces, fullscreen-auxiliary)");
    Ok(())
}

/// Show without taking key/focus: orderFrontRegardless leaves the previously
/// frontmost app with keyboard focus (visible-idle state).
pub fn show(app: &AppHandle) -> Result<(), String> {
    let panel = panel(app)?;
    app.run_on_main_thread(move || panel.order_front_regardless())
        .map_err(|e| format!("main-thread dispatch for show failed: {e}"))
}

/// Order the panel out. Focus returns to the prior app automatically because
/// the app was never activated.
pub fn hide(app: &AppHandle) -> Result<(), String> {
    let panel = panel(app)?;
    app.run_on_main_thread(move || panel.hide())
        .map_err(|e| format!("main-thread dispatch for hide failed: {e}"))
}

/// Make the panel key so it accepts typing (visible-focused state). Because
/// of the nonactivating style mask this still does not activate the app.
pub fn focus(app: &AppHandle) -> Result<(), String> {
    let panel = panel(app)?;
    app.run_on_main_thread(move || panel.make_key_window())
        .map_err(|e| format!("main-thread dispatch for focus failed: {e}"))
}

/// Toggle click-through on the panel. Uses the Tauri window handle (the
/// panel is the same NSWindow, so `setIgnoresMouseEvents:` applies) — an
/// idle overlay must never intercept clicks meant for the app underneath.
pub fn set_click_through(app: &AppHandle, ignore: bool) -> Result<(), String> {
    app.get_webview_window(OVERLAY_WINDOW_LABEL)
        .ok_or_else(|| format!("overlay window '{OVERLAY_WINDOW_LABEL}' not found"))?
        .set_ignore_cursor_events(ignore)
        .map_err(|e| format!("set_ignore_cursor_events({ignore}) failed: {e}"))
}

fn panel(app: &AppHandle) -> Result<tauri_nspanel::PanelHandle<tauri::Wry>, String> {
    app.get_webview_panel(OVERLAY_WINDOW_LABEL)
        .map_err(|e| format!("overlay panel '{OVERLAY_WINDOW_LABEL}' not registered: {e:?}"))
}
