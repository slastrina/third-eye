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

/// Hand global keyboard focus back to the active app (M005 follow-up). A
/// nonactivating panel that became key KEEPS key status even while another
/// app is active — the Spotlight trait this overlay borrows — so synthesized
/// keystrokes posted by the HID backend land in the overlay's own prompt
/// instead of the app `focus_app` just fronted. Called (through the
/// `KeyboardFocusYield` decorator) before every synthesized type/key action.
///
/// `resignKeyWindow` alone is only AppKit's notification hook — the window
/// server keeps routing keys to the panel. The load-bearing step is the
/// `orderOut:`: an ordered-out window cannot hold key, which forces the window
/// server to hand keyboard focus to the active app; the immediate
/// `orderFrontRegardless` restores visibility WITHOUT reclaiming key (it never
/// makes key). Both run inside one main-thread closure, before the next
/// display cycle, so the panel never visibly blinks. Click-through state is a
/// window property and survives the reorder untouched.
///
/// Synchronous by contract: the caller is about to post keystrokes and MUST
/// NOT race the handoff, so this blocks on a completion handshake with the
/// main thread (bounded — a stalled main thread returns a typed error rather
/// than hanging the tool loop). Callers therefore must be OFF the main thread
/// unless dispatch executes inline (Tauri runs the closure inline when already
/// on main, so the handshake cannot self-deadlock).
pub fn yield_key_focus(app: &AppHandle) -> Result<(), String> {
    let panel = panel(app)?;
    let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
    app.run_on_main_thread(move || {
        // A hidden panel cannot hold key — nothing to yield.
        if panel.is_visible() {
            panel.make_first_responder(None);
            panel.resign_key_window();
            panel.hide();
            panel.order_front_regardless();
            log::debug!("overlay: yielded key focus to the active app for synthesized keyboard input");
        }
        let _ = done_tx.send(());
    })
    .map_err(|e| format!("main-thread dispatch for key-focus yield failed: {e}"))?;
    done_rx
        .recv_timeout(std::time::Duration::from_millis(1000))
        .map_err(|_| "overlay key-focus yield timed out (main thread busy)".to_string())
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
