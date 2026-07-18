//! Non-macOS overlay backend: plain Tauri window show/hide/focus.
//!
//! Compile-correctness only for S01 — behavioral parity (focus handling,
//! click-through) on Windows/Linux is S06's job. Note the plain path DOES
//! activate the app on focus; that is acceptable off-macOS for now.

use tauri::{AppHandle, Manager, WebviewWindow};

use crate::OVERLAY_WINDOW_LABEL;

/// No conversion needed for the plain-window path.
pub fn init(_app: &AppHandle) -> Result<(), String> {
    log::debug!("overlay: plain-window fallback backend active (non-macOS)");
    Ok(())
}

pub fn show(app: &AppHandle) -> Result<(), String> {
    window(app)?
        .show()
        .map_err(|e| format!("overlay show failed: {e}"))
}

pub fn hide(app: &AppHandle) -> Result<(), String> {
    window(app)?
        .hide()
        .map_err(|e| format!("overlay hide failed: {e}"))
}

pub fn focus(app: &AppHandle) -> Result<(), String> {
    window(app)?
        .set_focus()
        .map_err(|e| format!("overlay focus failed: {e}"))
}

/// Same click-through contract as the macOS backend: idle overlay ignores
/// cursor events, focused overlay accepts them.
pub fn set_click_through(app: &AppHandle, ignore: bool) -> Result<(), String> {
    window(app)?
        .set_ignore_cursor_events(ignore)
        .map_err(|e| format!("set_ignore_cursor_events({ignore}) failed: {e}"))
}

fn window(app: &AppHandle) -> Result<WebviewWindow, String> {
    app.get_webview_window(OVERLAY_WINDOW_LABEL)
        .ok_or_else(|| format!("overlay window '{OVERLAY_WINDOW_LABEL}' not found"))
}
