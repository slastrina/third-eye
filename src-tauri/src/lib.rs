use tauri::Manager;

#[cfg(desktop)]
pub mod autostart;
pub mod capture;
#[cfg(desktop)]
pub mod config;
#[cfg(desktop)]
pub mod hotkey;
pub mod llm;
pub mod overlay;
pub mod settings_window;
#[cfg(desktop)]
pub mod tray;

/// Label of the pre-existing overlay window declared in tauri.conf.json.
/// The window is created hidden at launch so summoning it later (T02/T03)
/// never pays window-creation cost on the hotkey path.
pub const OVERLAY_WINDOW_LABEL: &str = "overlay";

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("debug")).init();

    let builder = tauri::Builder::default();
    #[cfg(target_os = "macos")]
    let builder = builder.plugin(tauri_nspanel::init());
    #[cfg(desktop)]
    let builder = builder.plugin(tauri_plugin_global_shortcut::Builder::new().build());
    // Launch-at-login (R010): a macOS LaunchAgent so the OS owns the state
    // and it survives restarts without app-side persistence.
    #[cfg(desktop)]
    let builder = builder
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .manage(autostart::AutostartState::default());
    // Watching/sleeping tray status: managed before any command can run so
    // the stream/capture activity guards always find their counter.
    #[cfg(desktop)]
    let builder = builder.manage(tray::TrayActivity::new());
    // settings.json (config.rs): the configurable hotkey persists here so a
    // rebind survives restart; S07 adds lane models and privacy mode.
    #[cfg(desktop)]
    let builder = builder.plugin(tauri_plugin_store::Builder::default().build());

    builder
        .manage(overlay::OverlayManager::new())
        .manage(llm::commands::LlmState::with_default_endpoint())
        .manage(capture::commands::CaptureState::with_platform_backend())
        // Privacy mode (S07): one shared toggle core serving the tray check
        // item and the set_privacy_mode IPC; persisted state applied in
        // setup() before the tray builds.
        .manage(capture::PrivacyState::new())
        .invoke_handler(tauri::generate_handler![
            overlay::show_overlay,
            overlay::hide_overlay,
            overlay::focus_overlay,
            hotkey::hotkey_status,
            hotkey::set_hotkey,
            autostart::set_autostart,
            autostart::autostart_status,
            llm::commands::chat,
            llm::commands::llm_health,
            llm::commands::set_model,
            llm::commands::set_lane_model,
            llm::commands::list_models,
            llm::commands::model_info,
            capture::commands::capture_screen,
            capture::commands::capture_permission_status,
            capture::commands::open_capture_settings,
            capture::commands::set_privacy_mode,
            capture::commands::privacy_status,
            settings_window::show_settings_window,
            settings_window::hide_settings_window
        ])
        .setup(|app| {
            // Accessory policy: no Dock icon, and the app can never become
            // the active app — the overlay must never steal frontmost status.
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);

            let overlay_window = app
                .get_webview_window(OVERLAY_WINDOW_LABEL)
                .ok_or("overlay window missing from tauri.conf.json")?;
            debug_assert!(!overlay_window.is_visible().unwrap_or(true));

            overlay::init_platform(app.handle())?;

            // Second nonactivating panel (S07): same fatal posture as the
            // overlay conversion — a settings window that activates the app
            // would break the Accessory contract.
            settings_window::init(app.handle())?;

            // Registration failure is surfaced (logged + queryable state),
            // never fatal: the app still runs and IPC commands still work.
            #[cfg(desktop)]
            app.manage(hotkey::init(app.handle()));

            // Persisted privacy mode (S07) is applied before the tray
            // builds so the check item and the initial resting frame
            // reflect it across restarts.
            #[cfg(desktop)]
            capture::commands::apply_persisted_privacy_mode(app.handle());

            // Tray build failure is likewise non-fatal (Q5): the overlay
            // stays reachable via the hotkey and IPC; the cause is logged.
            #[cfg(desktop)]
            if let Err(e) = tray::init(app.handle()) {
                log::error!("tray: build failed (non-fatal, hotkey still summons the overlay): {e}");
            }

            // Persisted lane pins (S07): a present settings.json key wins
            // over the THIRD_EYE_* env fallback the router booted with.
            #[cfg(desktop)]
            llm::commands::apply_persisted_lane_models(app.handle());

            log::debug!(
                "overlay window ready (hidden at launch, visible={:?})",
                overlay_window.is_visible()
            );
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
