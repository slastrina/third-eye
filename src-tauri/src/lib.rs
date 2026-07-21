use tauri::Manager;

pub mod appfocus;
#[cfg(desktop)]
pub mod autostart;
pub mod capture;
#[cfg(desktop)]
pub mod cloud;
#[cfg(desktop)]
pub mod config;
#[cfg(desktop)]
pub mod hotkey;
pub mod input;
pub mod llm;
#[cfg(desktop)]
pub mod memory;
#[cfg(desktop)]
pub mod nudge;
pub mod ocr;
#[cfg(desktop)]
pub mod onboarding;
pub mod overlay;
pub mod screenquery;
pub mod privacy;
pub mod settings_window;
#[cfg(desktop)]
pub mod tray;
#[cfg(desktop)]
pub mod watcher;

/// Label of the pre-existing overlay window declared in tauri.conf.json.
/// The window is created hidden at launch so summoning it later (T02/T03)
/// never pays window-creation cost on the hotkey path.
pub const OVERLAY_WINDOW_LABEL: &str = "overlay";

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Logs go to stderr by default. When THIRD_EYE_LOG_FILE names a path, tee
    // them to that file instead — a durable diagnostic seam that survives app
    // relaunches (a `npm run tauri dev` started from a terminal otherwise sends
    // stderr to that terminal, unreadable by an out-of-band observer). Opt-in:
    // an unset var keeps the plain stderr behavior for normal runs.
    {
        let mut builder =
            env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("debug"));
        if let Ok(path) = std::env::var("THIRD_EYE_LOG_FILE") {
            match std::fs::OpenOptions::new().create(true).append(true).open(&path) {
                Ok(file) => {
                    builder.target(env_logger::Target::Pipe(Box::new(file)));
                }
                Err(e) => {
                    eprintln!("third-eye: cannot open THIRD_EYE_LOG_FILE {path:?}: {e} — logging to stderr");
                }
            }
        }
        builder.init();
    }

    // Privacy-guard telemetry (M003 S02): one shared GuardState behind every
    // guarded lane client and the guarded embedder, incremented by the
    // watcher's redaction site, and managed so S03's IPC surface can read it.
    let guard = std::sync::Arc::new(llm::guard::GuardState::new());

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
    // Continuous watcher (M002 S01): one shared toggle/status core serving
    // the tray check item (T04) and the set_watcher_enabled IPC; the loop
    // task reads it every tick. Persisted state applied in setup().
    #[cfg(desktop)]
    let builder = builder.manage(watcher::WatcherState::new());
    // Memory core (M002 S02): one managed state holding the store handle
    // (installed in setup() once the app data dir is known) and the
    // ingestion status surface — memory_status and the ingest loop share it.
    #[cfg(desktop)]
    let builder = builder.manage(memory::MemoryState::new(guard.clone()));
    // Nudge core (M002 S05): managed before any command or the hotkey
    // handler can run — the nudge-aware toggle reads it on every press.
    // The detector loop and persisted-toggle apply run from setup().
    #[cfg(desktop)]
    let builder = builder.manage(nudge::NudgeState::new());
    // Cloud keystore (M004 S02): key bytes live in the OS credential store;
    // the managed state only ever serializes presence booleans outbound.
    #[cfg(desktop)]
    let builder = builder.manage(cloud::commands::CloudKeysState::new());
    // Cloud opt-in gate (M004 S03): defaults OFF so the local-only default is
    // untouched; the single guarded construction choke point reads it before
    // any remote client can exist. Persisted state applied in setup(); the
    // Settings toggle UX arrives in S04.
    #[cfg(desktop)]
    let builder = builder.manage(cloud::optin::CloudOptIn::new());
    // Heavy-lane cloud provider selection (M004 S04): persisted + readable so
    // the Settings surface can render the choice; live routing lands in S05.
    #[cfg(desktop)]
    let builder = builder.manage(cloud::optin::CloudHeavyProvider::new());

    // Persisted overlay presentation (M006 S04): the mode + per-edge extents +
    // modal size the overlay webview applies. Defaults to modal at the default
    // size; the persisted shape is restored in setup(). Owned in Rust so the
    // ACL-less Settings webview can drive it via IPC while only the overlay
    // webview applies geometry (D040/MEM148).
    #[cfg(desktop)]
    let builder = builder.manage(overlay::presentation::OverlayPresentationState::new());

    builder
        .manage(overlay::OverlayManager::new())
        .manage(guard.clone())
        .manage(llm::commands::LlmState::with_default_endpoint(guard))
        .manage(capture::commands::CaptureState::with_platform_backend())
        // HID input (M005/S01): the managed InputControl backend the composite
        // executor's InputTool draws from — enigo-backed on macOS, typed
        // unsupported elsewhere. Advertised unconditionally in S01; the
        // off-by-default arming gate lands in S03.
        .manage(input::commands::InputState::with_platform_backend())
        // HID approval gate (M005/S04): the session-scoped by-kind whitelist and
        // the pending-verdict registry the ApprovalGate consults and the
        // respond_hid_approval command delivers into. Managed once, cloned into
        // every chat run's gate.
        .manage(std::sync::Arc::new(llm::commands::ApprovalState::new()))
        // Screen query (M005/S02): the managed ScreenQuery backend the composite
        // executor's ScreenQueryTool draws from — Vision-backed on macOS, typed
        // unsupported elsewhere. Advertised unconditionally alongside input_action;
        // returns transient on-screen coordinates that never reach the store (R011).
        .manage(screenquery::commands::ScreenQueryState::with_platform_backend())
        // App focus (M005): the managed AppFocus backend the composite executor's
        // FocusAppTool draws from — NSWorkspace-backed on macOS, typed unsupported
        // elsewhere. Activation needs no TCC; the tool is gated as a HID-class
        // action (ActionKind::FocusApp) through the same ApprovalGate as input.
        .manage(appfocus::commands::AppFocusState::with_platform_backend())
        // External MCP tools (M007 S02): the managed holder a chat run mounts an
        // McpExecutor from when an already-serving MCP client peer has been
        // injected. Empty by default — the mount logs its absence and the run
        // proceeds with only the built-in tools; the settings-driven spawn that
        // injects a peer is S04.
        .manage(llm::mcp::McpState::new())
        // Remote MCP server auth (M007 S05, R018): the keychain-backed store for
        // a remote HTTP/SSE server's bearer token. Only presence crosses IPC; the
        // token bytes flow inbound once and back out only to the http connect path
        // through the crate-internal get_token. Managed beside CloudKeysState.
        .manage(llm::commands::McpAuthState::new())
        // Privacy mode (S07): one shared toggle core serving the tray check
        // item and the set_privacy_mode IPC; persisted state applied in
        // setup() before the tray builds.
        .manage(capture::PrivacyState::new())
        .invoke_handler(tauri::generate_handler![
            overlay::show_overlay,
            overlay::hide_overlay,
            overlay::focus_overlay,
            overlay::presentation::set_overlay_presentation,
            overlay::presentation::set_overlay_extent,
            overlay::presentation::set_overlay_position,
            overlay::presentation::overlay_presentation,
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
            llm::commands::guard_status,
            llm::commands::respond_hid_approval,
            llm::commands::stop_chat,
            llm::commands::run_state,
            llm::commands::set_mcp_run_mode,
            llm::commands::mcp_status,
            llm::commands::respond_mcp_approval,
            llm::commands::mcp_servers,
            llm::commands::set_mcp_servers,
            llm::commands::set_mcp_auth,
            llm::commands::delete_mcp_auth,
            llm::commands::mcp_auth_status,
            capture::commands::capture_screen,
            capture::commands::capture_permission_status,
            capture::commands::open_capture_settings,
            capture::commands::set_privacy_mode,
            capture::commands::privacy_status,
            input::commands::set_hid_armed,
            input::commands::set_hid_run_mode,
            input::commands::hid_armed_status,
            input::commands::open_input_settings,
            watcher::commands::set_watcher_enabled,
            watcher::commands::watcher_status,
            memory::commands::memory_search,
            memory::commands::memory_list,
            memory::commands::memory_update,
            memory::commands::memory_delete,
            memory::commands::memory_wipe,
            memory::commands::memory_status,
            nudge::commands::set_nudges_enabled,
            nudge::commands::nudge_status,
            settings_window::show_settings_window,
            settings_window::hide_settings_window,
            cloud::commands::set_cloud_api_key,
            cloud::commands::delete_cloud_api_key,
            cloud::commands::cloud_key_status,
            cloud::optin::set_cloud_optin,
            cloud::optin::cloud_optin_status,
            cloud::optin::set_cloud_heavy_provider,
            cloud::optin::cloud_heavy_provider,
            // First-run onboarding (M006): the overlay's first-launch explainer
            // requests the OS permissions with context, then marks onboarding
            // done so it never shows again. Requesting Accessibility here does
            // not arm HID (D038/R019).
            onboarding::first_run_status,
            onboarding::request_capture_permission,
            onboarding::request_input_permission,
            onboarding::complete_first_run
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

            // Persisted HID arming (M005 S03, D038): applied after privacy so
            // the arming choice survives restart. The AX gate re-checks the
            // live Accessibility grant inside the applier — a revoked grant
            // comes up disarmed, so the persisted choice can never re-arm HID
            // without a real permission.
            #[cfg(desktop)]
            input::commands::apply_persisted_hid_armed(app.handle());

            // Persisted watcher toggle (S01) follows the same contract:
            // applied after privacy (the loop's gating input), before the
            // tray builds so the T04 check item reflects it.
            #[cfg(desktop)]
            watcher::commands::apply_persisted_watcher_enabled(app.handle());

            // Persisted nudges toggle (S05, D019): same in-memory-only
            // startup contract, applied before the detector spawns so its
            // first round already sees the user's choice.
            #[cfg(desktop)]
            nudge::commands::apply_persisted_nudges_enabled(app.handle());

            // Persisted cloud opt-in (M004 S03): a present settings.json key
            // restores the user's choice; absent keeps the safe default (off).
            // In-memory only — the construction choke point reads it live.
            #[cfg(desktop)]
            cloud::optin::apply_persisted_cloud_opt_in(app.handle());

            // Persisted heavy-lane provider selection (M004 S04): a present
            // settings.json key restores the choice; absent/garbage keeps the
            // safe default (unselected). In-memory only — no re-save, nothing
            // listening yet; S05 wires it into the running heavy lane.
            #[cfg(desktop)]
            cloud::optin::apply_persisted_cloud_heavy_provider(app.handle());

            // Persisted overlay presentation (M006 S04): a present settings.json
            // key restores the overlay shape; absent keeps the safe default
            // (modal), garbage is repaired field-by-field in config so it can
            // never adopt an off-screen shape. In-memory only — no broadcast
            // yet; the overlay webview reads it via `overlay_presentation` on
            // mount and applies the geometry itself (the ACL split).
            #[cfg(desktop)]
            overlay::presentation::apply_persisted_overlay_presentation(app.handle());

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

            // Persisted MCP run mode (M007 S04, R016): a present settings.json
            // key restores the user's Off/Ask/Auto-run choice; absent/garbage
            // keeps the fail-closed default (Off, inert — no external tool runs
            // without an explicit choice). In-memory only — no re-save, no
            // broadcast; the already-mounted gate reads it live through McpState,
            // and the T03 spawn launch task reads the enabled server list next.
            #[cfg(desktop)]
            llm::commands::apply_persisted_mcp_run_mode(app.handle());

            // External MCP server spawn (M007 S04 T03): read the enabled server
            // list and, if any, spawn its child + bounded handshake and inject the
            // peer into McpState so the already-mounted gate's Some(peer) branch
            // lights up and the agent sees the server's tools. Async (the npx
            // first-run handshake can take up to 2 min) so it joins the
            // async_runtime::spawn family below rather than blocking setup(). A
            // spawn/handshake failure degrades to crashed health — the app keeps
            // running; the child is cancelled cleanly on exit (the RunEvent hook).
            #[cfg(desktop)]
            llm::mcp_spawn::launch_on_startup(app.handle().clone());

            // Heavy-lane cloud routing (M004 S05): evaluated AFTER the local
            // lane pins are settled so the revert path has the right local
            // fallback, and after the persisted opt-in + provider are restored.
            // Opt-in on + a provider + a stored key routes the heavy lane to the
            // guarded cloud client; every other case (the default) leaves it
            // local. Fail-safe — a build failure logs and stays local.
            #[cfg(desktop)]
            cloud::routing::apply_cloud_routing(app.handle());

            // Privacy-guard notifier (M003 S03): install the privacy://state
            // emitter on the shared GuardState before the watcher loop
            // spawns, so every mutation site — guarded forward, guard block,
            // watcher redaction — broadcasts from its first occurrence.
            llm::commands::install_guard_notifier(app.handle());

            // The watcher loop runs for the app's lifetime; the toggle
            // changes what a tick does, not whether the task exists. Spawned
            // after the tray so watching-state animation (T04) has its
            // target from the first tick.
            #[cfg(desktop)]
            watcher::spawn_loop(app.handle().clone());

            // Memory ingestion (S02): opens app_data_dir/memory.db and
            // consumes the watcher's observation broadcast, distilling
            // batches via the thin lane. Spawned right after the watcher
            // loop; any observation published before the subscription is
            // live is missed by design (worthless to replay) and every
            // failure path disables ingestion visibly, never fatally.
            #[cfg(desktop)]
            memory::ingest::spawn(app.handle());

            // Nudge detector (S05): the observation broadcast's second
            // consumer. Batches on a fixed interval, classifies via the
            // thin lane behind the pure gate, and shows the click-through
            // idle nudge — every failure path logs and waits for the next
            // round, never fatal.
            #[cfg(desktop)]
            nudge::spawn(app.handle());

            // First-run onboarding (M006): if the user has not been onboarded,
            // summon the overlay focused so the one-time explainer is visible
            // and clickable without the user first pressing the hotkey. Focused
            // (not idle) because idle overlays are click-through — the grant
            // buttons must accept clicks. Non-fatal: a show failure is logged
            // and the app still runs; the panel then appears on first summon.
            #[cfg(desktop)]
            if onboarding::should_show_on_launch(app.handle()) {
                if let Err(e) = overlay::show_overlay(app.handle().clone()) {
                    log::warn!("onboarding: could not show overlay for first run: {e}");
                } else if let Err(e) = overlay::focus_overlay(app.handle().clone()) {
                    log::warn!("onboarding: could not focus overlay for first run: {e}");
                } else {
                    log::info!("onboarding: showed overlay for first-run explainer");
                }
            }

            log::debug!(
                "overlay window ready (hidden at launch, visible={:?})",
                overlay_window.is_visible()
            );
            Ok(())
        })
        // Build (not the terminal `.run`) so the event loop callback below can
        // hook RunEvent::Exit for clean MCP child shutdown (M007 S04 T03).
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app_handle, event| {
            // Clean MCP child shutdown on app exit (R020, no unix/windows-only
            // kill): cancel the spawned server's RunningService via its portable
            // cancellation token so the service loop stops and the child
            // terminates. Best-effort and exactly-once (the handle is taken); no
            // child spawned → no handle → a no-op.
            #[cfg(desktop)]
            if let tauri::RunEvent::Exit = event {
                if let Some(token) =
                    app_handle.state::<llm::mcp::McpState>().take_shutdown_handle()
                {
                    token.cancel();
                    log::info!("llm: MCP server child cancelled cleanly on app exit");
                }
            }
            #[cfg(not(desktop))]
            let _ = (app_handle, event);
        });
}
