// Settings window view — the ?view=settings branch of the shared bundle,
// rendered inside the borderless nonactivating settings panel: two lane
// pickers fed by the live LM Studio model list, the privacy-mode toggle, and
// read-only hotkey/autostart status. All state transitions live in the pure
// settingsReducer (src/settings.ts); this component is only glue.
//
// Outside a Tauri runtime (vite dev, Playwright) every invoke rejects and is
// absorbed into named unavailable states — the view must stay renderable in
// a plain browser, never crash.

import { useEffect, useReducer } from "react";
import { modelInfo, onModelInfoBroadcast, onPrivacyChanged, privacyStatus } from "./chat";
import {
  autostartStatus,
  hideSettingsWindow,
  hotkeyStatus,
  initialSettingsState,
  laneOptions,
  listModels,
  modelsErrorDetail,
  modelsErrorTitle,
  setLaneModel,
  setPrivacyMode,
  settingsReducer,
  toModelsError,
} from "./settings-state";

/** Sentinel select value for "no pin" — a real model id is never empty. */
const DEFAULT_OPTION = "";

function Settings() {
  const [state, dispatch] = useReducer(settingsReducer, initialSettingsState);

  const refreshModels = () => {
    dispatch({ type: "models-loading" });
    listModels().then(
      (models) => dispatch({ type: "models-loaded", models }),
      (err) => dispatch({ type: "models-error", error: toModelsError(err) }),
    );
  };

  // Mount-time snapshots. Each query fails independently: a dead endpoint
  // must not blank the privacy toggle, and vice versa.
  useEffect(() => {
    refreshModels();
    modelInfo().then(
      (info) => dispatch({ type: "model-info", info }),
      (err) => console.debug("settings: model_info unavailable:", err),
    );
    privacyStatus().then(
      (status) => dispatch({ type: "privacy", status }),
      (err) => console.debug("settings: privacy_status unavailable:", err),
    );
    hotkeyStatus().then(
      (status) => dispatch({ type: "hotkey", status }),
      (err) => console.debug("settings: hotkey_status unavailable:", err),
    );
    autostartStatus().then(
      (status) => dispatch({ type: "autostart", status }),
      (err) => console.debug("settings: autostart_status unavailable:", err),
    );
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // Cross-window sync: a tray privacy toggle or an overlay lane override
  // must update this window too, not just the one that asked.
  useEffect(() => {
    const unlistens = [
      onModelInfoBroadcast((info) => dispatch({ type: "model-info", info })),
      onPrivacyChanged((status) => dispatch({ type: "privacy", status })),
    ];
    return () => {
      unlistens.forEach((u) => u.then((f) => f()));
    };
  }, []);

  // In-page close: the window is borderless, so the button and Escape are
  // the only ways out. Rejection outside Tauri is absorbed.
  const close = () => {
    hideSettingsWindow().catch((err) =>
      console.debug("settings: hide unavailable:", err),
    );
  };

  useEffect(() => {
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key === "Escape") close();
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, []);

  const pinLane = (lane: string, value: string) => {
    const model = value === DEFAULT_OPTION ? null : value;
    setLaneModel(lane, model).then(
      (info) => dispatch({ type: "model-info", info }),
      // Rejection means routing is unchanged backend-side — surface it and
      // keep rendering the real state.
      (err) => dispatch({ type: "lane-error", lane, detail: String(err) }),
    );
  };

  const togglePrivacy = (enable: boolean) => {
    setPrivacyMode(enable).then(
      // Never rejects backend-side; a persist failure rides status.error.
      (status) => dispatch({ type: "privacy", status }),
      (err) => console.debug("settings: set_privacy_mode unavailable:", err),
    );
  };

  const lanes = state.modelInfo?.lanes ?? null;

  return (
    <div className="settings-root">
      <div className="settings-panel">
        <header className="settings-header">
          <h1 className="settings-title">Third Eye Settings</h1>
          <button
            type="button"
            className="settings-close"
            aria-label="Close settings"
            onClick={close}
          >
            ×
          </button>
        </header>

        <section className="settings-section" aria-labelledby="settings-models-heading">
          <div className="settings-section-header">
            <h2 id="settings-models-heading" className="settings-section-title">
              Models
            </h2>
            <button
              type="button"
              className="settings-refresh"
              disabled={state.modelsLoading}
              onClick={refreshModels}
            >
              {state.modelsLoading ? "Refreshing…" : "Refresh"}
            </button>
          </div>
          {state.modelsError && (
            <div className="settings-error" role="alert">
              <strong>{modelsErrorTitle(state.modelsError)}</strong>
              <span>{modelsErrorDetail(state.modelsError)}</span>
            </div>
          )}
          {lanes ? (
            lanes.map((lane) => (
              <label key={lane.name} className="settings-row">
                <span className="settings-row-label">{lane.name} lane</span>
                <select
                  className="settings-select"
                  aria-label={`${lane.name} lane model`}
                  value={lane.modelId ?? DEFAULT_OPTION}
                  onChange={(event) => pinLane(lane.name, event.target.value)}
                >
                  <option value={DEFAULT_OPTION}>endpoint default model</option>
                  {laneOptions(state.models, lane.modelId).map((model) => (
                    <option key={model} value={model}>
                      {model}
                    </option>
                  ))}
                </select>
              </label>
            ))
          ) : (
            <p className="settings-unavailable">
              Model routing is unavailable outside the app.
            </p>
          )}
          {state.laneError && (
            <div className="settings-error" role="alert">
              <strong>Model change rejected</strong>
              <span>{state.laneError}</span>
            </div>
          )}
        </section>

        <section className="settings-section" aria-labelledby="settings-privacy-heading">
          <h2 id="settings-privacy-heading" className="settings-section-title">
            Privacy
          </h2>
          <label className="settings-row">
            <span className="settings-row-label">Privacy Mode</span>
            <input
              type="checkbox"
              className="settings-toggle"
              aria-label="Privacy Mode"
              disabled={state.privacy === null}
              checked={state.privacy?.enabled ?? false}
              onChange={(event) => togglePrivacy(event.target.checked)}
            />
          </label>
          <p className="settings-hint">
            Blocks all screen capture while on. Chat keeps working.
          </p>
          {state.privacy === null && (
            <p className="settings-unavailable">
              Privacy state is unavailable outside the app.
            </p>
          )}
          {state.privacy?.error && (
            <div className="settings-error" role="alert">
              <strong>Privacy Mode couldn't be saved</strong>
              <span>{state.privacy.error}</span>
            </div>
          )}
        </section>

        <section className="settings-section" aria-labelledby="settings-status-heading">
          <h2 id="settings-status-heading" className="settings-section-title">
            Status
          </h2>
          <div className="settings-status-row">
            <span className="settings-row-label">Hotkey</span>
            <span className="settings-status-value">
              {state.hotkey
                ? `${state.hotkey.shortcut} — ${state.hotkey.registered ? "registered" : "not registered"}`
                : "unavailable"}
            </span>
          </div>
          {state.hotkey?.error && (
            <div className="settings-error" role="alert">
              <span>{state.hotkey.error}</span>
            </div>
          )}
          <div className="settings-status-row">
            <span className="settings-row-label">Launch at login</span>
            <span className="settings-status-value">
              {state.autostart ? (state.autostart.enabled ? "on" : "off") : "unavailable"}
            </span>
          </div>
          {state.autostart?.error && (
            <div className="settings-error" role="alert">
              <span>{state.autostart.error}</span>
            </div>
          )}
        </section>
      </div>
    </div>
  );
}

export default Settings;
