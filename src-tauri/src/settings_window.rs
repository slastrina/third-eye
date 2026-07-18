//! Settings window: the app's second surface, opened from the tray (S07).
//!
//! Declared hidden in tauri.conf.json (label "settings", rendering the
//! `?view=settings` branch of the same vite bundle) and converted at setup
//! into the app's second nonactivating NSPanel on macOS. Under
//! `ActivationPolicy::Accessory` a plain window would activate the app on
//! click, so the panel conversion is mandatory there — same rationale as the
//! overlay. Unlike the overlay it sits at PanelLevel::Floating (a utility
//! surface, not an always-on-top HUD), moves to the active Space when shown,
//! and is borderless: closing is in-page (button or Escape) via the
//! `hide_settings_window` IPC command.

use tauri::AppHandle;

/// Label of the settings window declared in tauri.conf.json.
pub const SETTINGS_WINDOW_LABEL: &str = "settings";

#[cfg(not(target_os = "macos"))]
mod platform {
    //! Non-macOS backend: plain window show/hide. Compile-correctness only —
    //! behavioral parity off-macOS follows the overlay fallback's posture.

    use tauri::{AppHandle, Manager, WebviewWindow};

    use super::SETTINGS_WINDOW_LABEL;

    pub fn init(_app: &AppHandle) -> Result<(), String> {
        log::debug!("settings: plain-window fallback backend active (non-macOS)");
        Ok(())
    }

    pub fn show(app: &AppHandle) -> Result<(), String> {
        let window = window(app)?;
        window
            .show()
            .map_err(|e| format!("settings show failed: {e}"))?;
        window
            .set_focus()
            .map_err(|e| format!("settings focus failed: {e}"))
    }

    pub fn hide(app: &AppHandle) -> Result<(), String> {
        window(app)?
            .hide()
            .map_err(|e| format!("settings hide failed: {e}"))
    }

    fn window(app: &AppHandle) -> Result<WebviewWindow, String> {
        app.get_webview_window(SETTINGS_WINDOW_LABEL)
            .ok_or_else(|| format!("settings window '{SETTINGS_WINDOW_LABEL}' not found"))
    }
}

#[cfg(target_os = "macos")]
mod platform {
    //! macOS backend: nonactivating NSPanel conversion via tauri-nspanel.
    //! Panel operations are not internally main-thread-safe, so every
    //! show/hide/key call goes through `run_on_main_thread` (MEM010).

    use tauri::{AppHandle, Manager};
    use tauri_nspanel::{
        tauri_panel, CollectionBehavior, ManagerExt, PanelLevel, StyleMask, WebviewWindowExt,
    };

    use super::SETTINGS_WINDOW_LABEL;

    tauri_panel! {
        panel!(SettingsPanel {
            config: {
                can_become_key_window: true,
                can_become_main_window: false,
                is_floating_panel: true
            }
        })
    }

    /// Convert the settings window to a nonactivating NSPanel. Must run on
    /// the main thread (Tauri setup hook). Fails loudly — a failed conversion
    /// means a plain window would activate the app when shown, breaking the
    /// Accessory contract.
    pub fn init(app: &AppHandle) -> Result<(), String> {
        let window = app
            .get_webview_window(SETTINGS_WINDOW_LABEL)
            .ok_or_else(|| format!("settings window '{SETTINGS_WINDOW_LABEL}' not found"))?;
        let panel = window
            .to_panel::<SettingsPanel>()
            .map_err(|e| format!("settings NSPanel conversion failed: {e}"))?;

        // Nonactivating: the panel can become key (accept clicks and typing)
        // without activating the app.
        panel.set_style_mask(StyleMask::empty().nonactivating_panel().into());
        // Floating: above normal windows but below the overlay's MainMenu+1 —
        // the overlay must stay on top if both are visible.
        panel.set_level(PanelLevel::Floating.value());
        // Come to whatever Space the user is on when opened from the tray;
        // coexist with fullscreen apps; stay out of Cmd+Tab.
        panel.set_collection_behavior(
            CollectionBehavior::new()
                .move_to_active_space()
                .full_screen_auxiliary()
                .ignores_cycle()
                .into(),
        );
        // The app never activates, so never auto-hide on deactivate.
        panel.set_hides_on_deactivate(false);

        log::debug!(
            "settings: converted to nonactivating NSPanel (level=floating, active-space, fullscreen-auxiliary)"
        );
        Ok(())
    }

    /// Show and make key in one step: unlike the overlay's idle/focused
    /// split, the settings window is always interactive when visible. The
    /// nonactivating style mask keeps the app inactive throughout.
    pub fn show(app: &AppHandle) -> Result<(), String> {
        let panel = panel(app)?;
        app.run_on_main_thread(move || {
            panel.order_front_regardless();
            panel.make_key_window();
        })
        .map_err(|e| format!("main-thread dispatch for settings show failed: {e}"))
    }

    /// Order the panel out. Focus returns to the prior app automatically
    /// because the app was never activated (MEM014).
    pub fn hide(app: &AppHandle) -> Result<(), String> {
        let panel = panel(app)?;
        app.run_on_main_thread(move || panel.hide())
            .map_err(|e| format!("main-thread dispatch for settings hide failed: {e}"))
    }

    fn panel(app: &AppHandle) -> Result<tauri_nspanel::PanelHandle<tauri::Wry>, String> {
        app.get_webview_panel(SETTINGS_WINDOW_LABEL)
            .map_err(|e| format!("settings panel '{SETTINGS_WINDOW_LABEL}' not registered: {e:?}"))
    }
}

/// One-time platform setup: NSPanel conversion on macOS, no-op elsewhere.
/// Must be called from the Tauri setup hook (main thread), beside
/// `overlay::init_platform`.
pub fn init(app: &AppHandle) -> Result<(), String> {
    platform::init(app)
}

/// Show the settings window. `via` names the entry point (tray vs ipc) so
/// the shown/hidden log pairs identify who summoned the panel.
pub fn show(app: &AppHandle, via: &str) -> Result<(), String> {
    platform::show(app)?;
    log::info!("settings: panel shown (via {via})");
    Ok(())
}

/// Hide the settings window. `via` names the entry point (close button and
/// Escape both arrive as ipc).
pub fn hide(app: &AppHandle, via: &str) -> Result<(), String> {
    platform::hide(app)?;
    log::info!("settings: panel hidden (via {via})");
    Ok(())
}

/// IPC: show the settings window (parity surface for tests/tooling; the tray
/// calls [`show`] directly).
#[tauri::command]
pub fn show_settings_window(app: AppHandle) -> Result<(), String> {
    show(&app, "ipc").map_err(|e| {
        log::error!("settings: show failed: {e}");
        e
    })
}

/// IPC: hide the settings window — the in-page close button and Escape both
/// land here.
#[tauri::command]
pub fn hide_settings_window(app: AppHandle) -> Result<(), String> {
    hide(&app, "ipc").map_err(|e| {
        log::error!("settings: hide failed: {e}");
        e
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The window this module manages must exist in tauri.conf.json with the
    /// exact contract T03 depends on: created hidden (never flashes at
    /// launch), borderless (in-page close), and rendering the settings view
    /// of the shared bundle.
    #[test]
    fn settings_window_is_declared_hidden_and_borderless_in_config() {
        let conf: serde_json::Value =
            serde_json::from_str(include_str!("../tauri.conf.json")).unwrap();
        let windows = conf["app"]["windows"].as_array().expect("windows array");
        let win = windows
            .iter()
            .find(|w| w["label"] == SETTINGS_WINDOW_LABEL)
            .expect("settings window declared in tauri.conf.json");
        assert_eq!(win["visible"], false, "must be created hidden");
        assert_eq!(win["decorations"], false, "must be borderless");
        assert_eq!(win["url"], "index.html?view=settings", "must render the settings view");
        assert_eq!(win["focus"], false, "must not request focus at creation");
    }

    /// Q7: the settings window must not adopt the overlay's label — the two
    /// surfaces are distinct windows with distinct platform state.
    #[test]
    fn settings_label_is_distinct_from_the_overlay() {
        assert_ne!(SETTINGS_WINDOW_LABEL, crate::OVERLAY_WINDOW_LABEL);
    }
}
