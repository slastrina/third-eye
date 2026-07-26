//! Memory window (2026-07 redesign, surface 5): the standalone
//! Timeline / Learned / Recall surface, opened from the tray panel.
//!
//! Same posture as the settings window in every respect: declared hidden in
//! tauri.conf.json (label "memory", `?view=memory` branch of the shared
//! bundle), converted at setup into a nonactivating can-become-key NSPanel
//! (it has a filter input and the recall query — typing needs key status;
//! the nonactivating mask keeps the app from activating), borderless with
//! in-page close via the `hide_memory_window` IPC.

use tauri::AppHandle;

/// Label of the memory window declared in tauri.conf.json.
pub const MEMORY_WINDOW_LABEL: &str = "memory";

#[cfg(not(target_os = "macos"))]
mod platform {
    use tauri::{AppHandle, Manager, WebviewWindow};

    use super::MEMORY_WINDOW_LABEL;

    pub fn init(_app: &AppHandle) -> Result<(), String> {
        log::debug!("memory-window: plain-window fallback backend active (non-macOS)");
        Ok(())
    }

    pub fn show(app: &AppHandle) -> Result<(), String> {
        let window = window(app)?;
        window
            .show()
            .map_err(|e| format!("memory window show failed: {e}"))?;
        window
            .set_focus()
            .map_err(|e| format!("memory window focus failed: {e}"))
    }

    pub fn hide(app: &AppHandle) -> Result<(), String> {
        window(app)?
            .hide()
            .map_err(|e| format!("memory window hide failed: {e}"))
    }

    fn window(app: &AppHandle) -> Result<WebviewWindow, String> {
        app.get_webview_window(MEMORY_WINDOW_LABEL)
            .ok_or_else(|| format!("memory window '{MEMORY_WINDOW_LABEL}' not found"))
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use tauri::{AppHandle, Manager};
    use tauri_nspanel::{
        tauri_panel, CollectionBehavior, ManagerExt, PanelLevel, StyleMask, WebviewWindowExt,
    };

    use super::MEMORY_WINDOW_LABEL;

    tauri_panel! {
        panel!(MemoryPanel {
            config: {
                can_become_key_window: true,
                can_become_main_window: false,
                is_floating_panel: true
            }
        })
    }

    /// Convert the memory window to a nonactivating NSPanel (settings
    /// posture). Fails loudly — a plain window would activate the app.
    pub fn init(app: &AppHandle) -> Result<(), String> {
        let window = app
            .get_webview_window(MEMORY_WINDOW_LABEL)
            .ok_or_else(|| format!("memory window '{MEMORY_WINDOW_LABEL}' not found"))?;
        let panel = window
            .to_panel::<MemoryPanel>()
            .map_err(|e| format!("memory NSPanel conversion failed: {e}"))?;
        panel.set_style_mask(StyleMask::empty().nonactivating_panel().resizable().into());
        panel.set_level(PanelLevel::Floating.value());
        panel.set_collection_behavior(
            CollectionBehavior::new()
                .move_to_active_space()
                .full_screen_auxiliary()
                .ignores_cycle()
                .into(),
        );
        panel.set_hides_on_deactivate(false);
        log::debug!("memory-window: converted to nonactivating NSPanel (level=floating)");
        Ok(())
    }

    pub fn show(app: &AppHandle) -> Result<(), String> {
        let panel = panel(app)?;
        app.run_on_main_thread(move || {
            panel.order_front_regardless();
            panel.make_key_window();
        })
        .map_err(|e| format!("main-thread dispatch for memory show failed: {e}"))
    }

    pub fn hide(app: &AppHandle) -> Result<(), String> {
        let panel = panel(app)?;
        app.run_on_main_thread(move || panel.hide())
            .map_err(|e| format!("main-thread dispatch for memory hide failed: {e}"))
    }

    fn panel(app: &AppHandle) -> Result<tauri_nspanel::PanelHandle<tauri::Wry>, String> {
        app.get_webview_panel(MEMORY_WINDOW_LABEL)
            .map_err(|e| format!("memory panel '{MEMORY_WINDOW_LABEL}' not registered: {e:?}"))
    }
}

/// One-time platform setup: NSPanel conversion on macOS, no-op elsewhere.
pub fn init(app: &AppHandle) -> Result<(), String> {
    platform::init(app)
}

/// IPC: show the memory window (tray panel's Memory button).
#[tauri::command]
pub fn show_memory_window(app: AppHandle) -> Result<(), String> {
    platform::show(&app).map_err(|e| {
        log::error!("memory-window: show failed: {e}");
        e
    })
}

/// IPC: hide the memory window (in-page close button / Escape).
#[tauri::command]
pub fn hide_memory_window(app: AppHandle) -> Result<(), String> {
    platform::hide(&app).map_err(|e| {
        log::error!("memory-window: hide failed: {e}");
        e
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_window_is_declared_in_tauri_conf() {
        let conf = include_str!("../tauri.conf.json");
        assert!(conf.contains(&format!("\"label\": \"{MEMORY_WINDOW_LABEL}\"")));
    }
}
