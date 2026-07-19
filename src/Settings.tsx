// Settings window view — the ?view=settings branch of the shared bundle,
// rendered inside the borderless nonactivating settings panel: two lane
// pickers fed by the live LM Studio model list, the privacy-mode toggle, the
// Watch Screen toggle with its live diagnostics (run state + last extracted
// snippets), the Memory section (browse/edit/delete/wipe over the S02 IPC
// surface plus ingest health), and read-only hotkey/autostart status. All
// state transitions live in the pure settingsReducer (src/settings-state.ts),
// watcherReducer (src/watcher-state.ts), and memoryReducer
// (src/memory-state.ts); this component is only glue.
//
// Outside a Tauri runtime (vite dev, Playwright) every invoke rejects and is
// absorbed into named unavailable states — the view must stay renderable in
// a plain browser, never crash.

import { useEffect, useReducer, useState } from "react";
import {
  bannerDetail,
  bannerTitle,
  modelInfo,
  nudgeStatus,
  onModelInfoBroadcast,
  onNudgeState,
  onPrivacyChanged,
  privacyStatus,
  setNudgesEnabled,
  type NudgeStatus,
} from "./chat";
import {
  CLOUD_PROVIDERS,
  cloudHeavyProvider,
  cloudKeyStatus,
  cloudOptinStatus,
  cloudReducer,
  deleteCloudApiKey,
  initialCloudViewState,
  isCloudKeyError,
  keyErrorTitle,
  keyPresent,
  onCloudOptin,
  setCloudApiKey,
  setCloudHeavyProvider,
  setCloudOptin,
  type CloudProvider,
} from "./cloud-state";
import {
  MEMORY_EMPTY_HINT,
  MEMORY_PAGE_SIZE,
  MEMORY_UNAVAILABLE_MESSAGE,
  appsLabel,
  canGoNext,
  canGoPrev,
  initialMemoryViewState,
  isMemoryError,
  lastDistillLabel,
  memoryDelete,
  memoryErrorMessage,
  memoryList,
  memoryReducer,
  memoryStatus,
  memoryTimeLabel,
  memoryUpdate,
  memoryWipe,
  spanLabel,
} from "./memory-state";
import {
  blockReasonLabel,
  GUARD_UNAVAILABLE_MESSAGE,
  guardStatus,
  onPrivacyState,
  redactionRows,
  type GuardTelemetry,
} from "./privacy-state";
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
import {
  capturedAtLabel,
  initialWatcherViewState,
  onWatcherObservation,
  onWatcherState,
  runStateLabel,
  setWatcherEnabled,
  snippetPreview,
  tickErrorDetail,
  tickErrorTitle,
  watcherReducer,
  watcherStatus,
} from "./watcher-state";

/** Sentinel select value for "no pin" — a real model id is never empty. */
const DEFAULT_OPTION = "";

function Settings() {
  const [state, dispatch] = useReducer(settingsReducer, initialSettingsState);
  const [watcher, dispatchWatcher] = useReducer(watcherReducer, initialWatcherViewState);
  const [memory, dispatchMemory] = useReducer(memoryReducer, initialMemoryViewState);
  const [cloud, dispatchCloud] = useReducer(cloudReducer, initialCloudViewState);
  // The masked key drafts live only in local component state, keyed by
  // provider — never in the reducer, so an entered key can never round-trip
  // back into rendered view state (the never-echo property). Cleared the
  // instant it is handed to the backend.
  const [keyDrafts, setKeyDrafts] = useState<Record<CloudProvider, string>>({
    openai: "",
    anthropic: "",
  });
  // Nudge status is a single authoritative backend snapshot (mount query,
  // toggle response, nudge://state broadcast all land the same shape), so a
  // plain state cell suffices — no transitions to keep pure.
  const [nudges, setNudges] = useState<NudgeStatus | null>(null);
  // Guard telemetry is likewise a single authoritative backend snapshot
  // (mount query and privacy://state broadcast land the same shape); all
  // display logic lives in pure privacy-state.ts helpers.
  const [guard, setGuard] = useState<GuardTelemetry | null>(null);

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
    watcherStatus().then(
      (status) => dispatchWatcher({ type: "status", status }),
      (err) => console.debug("settings: watcher_status unavailable:", err),
    );
    nudgeStatus().then(
      (status) => setNudges(status),
      (err) => console.debug("settings: nudge_status unavailable:", err),
    );
    guardStatus().then(
      (telemetry) => setGuard(telemetry),
      (err) => console.debug("settings: guard_status unavailable:", err),
    );
    cloudOptinStatus().then(
      (status) => dispatchCloud({ type: "optin", status }),
      (err) => console.debug("settings: cloud_optin_status unavailable:", err),
    );
    cloudKeyStatus().then(
      (status) => dispatchCloud({ type: "keys", status }),
      // A CloudKeyError means the store itself failed; anything else is the
      // no-runtime case. Either way the key rows stay renderable.
      (err) => {
        if (isCloudKeyError(err)) dispatchCloud({ type: "key-error", error: err });
        else console.debug("settings: cloud_key_status unavailable:", err);
      },
    );
    cloudHeavyProvider().then(
      (status) => dispatchCloud({ type: "heavy", status }),
      (err) => console.debug("settings: cloud_heavy_provider unavailable:", err),
    );
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // Cross-window sync: a tray privacy toggle or an overlay lane override
  // must update this window too, not just the one that asked.
  useEffect(() => {
    const unlistens = [
      onModelInfoBroadcast((info) => dispatch({ type: "model-info", info })),
      onPrivacyChanged((status) => dispatch({ type: "privacy", status })),
      // Watcher truth flows one way, backend → UI: a tray toggle, a privacy
      // pause, and a tick error all arrive as watcher://state; extracted
      // snippets ride watcher://observation.
      onWatcherState((status) => dispatchWatcher({ type: "status", status })),
      onWatcherObservation((observation) =>
        dispatchWatcher({ type: "observation", observation }),
      ),
      // Nudge truth flows one way too: a toggle from any surface broadcasts
      // the resulting status as nudge://state.
      onNudgeState((status) => setNudges(status)),
      // Guard truth flows one way, backend → UI: every guard mutation
      // (external forward redaction, guard block, watcher increment)
      // arrives as a fresh privacy://state snapshot.
      onPrivacyState((telemetry) => setGuard(telemetry)),
      // Cloud opt-in truth flows one way too: a toggle from any window
      // broadcasts the resulting status as cloud://optin.
      onCloudOptin((status) => dispatchCloud({ type: "optin", status })),
    ];
    // MEM115: a capability/ACL denial rejects listen() inside the real app —
    // without this catch the rejection is unhandled and every live surface
    // silently freezes at its boot-time snapshot.
    unlistens.forEach((u) => {
      u.catch((err) => console.error("settings: event subscription failed:", err));
    });
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

  const toggleWatcher = (enable: boolean) => {
    setWatcherEnabled(enable).then(
      // Never rejects backend-side; a persist failure rides status.error
      // and a rolled-back toggle comes back as the authoritative snapshot.
      (status) => dispatchWatcher({ type: "status", status }),
      (err) => console.debug("settings: set_watcher_enabled unavailable:", err),
    );
  };

  const toggleNudges = (enable: boolean) => {
    setNudgesEnabled(enable).then(
      // Never rejects backend-side; a persist failure rides persistError and
      // a rolled-back toggle comes back as the authoritative snapshot.
      (status) => setNudges(status),
      (err) => console.debug("settings: set_nudges_enabled unavailable:", err),
    );
  };

  const toggleCloudOptin = (enable: boolean) => {
    setCloudOptin(enable).then(
      // Never rejects backend-side; a persist failure rides persistError and
      // a rolled-back toggle comes back as the authoritative snapshot.
      (status) => dispatchCloud({ type: "optin", status }),
      (err) => console.debug("settings: set_cloud_optin unavailable:", err),
    );
  };

  const submitKey = (provider: CloudProvider) => {
    const key = keyDrafts[provider];
    if (key.trim().length === 0) return;
    // Clear the draft immediately: the key is on its way to the OS store and
    // must never linger in a rendered field.
    setKeyDrafts((drafts) => ({ ...drafts, [provider]: "" }));
    setCloudApiKey(provider, key).then(
      (status) => dispatchCloud({ type: "keys", status }),
      (err) => {
        if (isCloudKeyError(err)) dispatchCloud({ type: "key-error", error: err });
        else console.debug("settings: set_cloud_api_key unavailable:", err);
      },
    );
  };

  const removeKey = (provider: CloudProvider) => {
    deleteCloudApiKey(provider).then(
      (status) => dispatchCloud({ type: "keys", status }),
      (err) => {
        if (isCloudKeyError(err)) dispatchCloud({ type: "key-error", error: err });
        else console.debug("settings: delete_cloud_api_key unavailable:", err);
      },
    );
  };

  const selectHeavyProvider = (provider: CloudProvider | null) => {
    setCloudHeavyProvider(provider).then(
      // Never rejects backend-side; a persist failure rides persistError.
      (status) => dispatchCloud({ type: "heavy", status }),
      (err) => console.debug("settings: set_cloud_heavy_provider unavailable:", err),
    );
  };

  // Memory list + status fetch. refreshToken is the pure refetch signal
  // (bumped by the reducer on mutations, staleness, and empty-page clamps);
  // offset changes on page turns. A non-MemoryError rejection means no
  // Tauri runtime — collapse to the named unavailable state. memory_status
  // never rejects backend-side, so its rejection is only the no-runtime case.
  useEffect(() => {
    let cancelled = false;
    memoryList(MEMORY_PAGE_SIZE, memory.offset).then(
      (records) => {
        if (!cancelled) dispatchMemory({ type: "list", records, offset: memory.offset });
      },
      (err) => {
        if (cancelled) return;
        if (isMemoryError(err)) dispatchMemory({ type: "list-failed", error: err });
        else dispatchMemory({ type: "unavailable" });
      },
    );
    memoryStatus().then(
      (status) => {
        if (!cancelled) dispatchMemory({ type: "status", status });
      },
      (err) => console.debug("settings: memory_status unavailable:", err),
    );
    return () => {
      cancelled = true;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [memory.offset, memory.refreshToken]);

  // Inline-edit persistence: the reducer flips edit.saving to true after
  // validating the draft; that flip is the signal to fire memory_update.
  // The reducer freezes the draft while saving, so reading edit here is safe.
  const memoryEditSaving = memory.edit?.saving ?? false;
  useEffect(() => {
    const edit = memory.edit;
    if (!edit || !edit.saving) return;
    memoryUpdate(edit.id, edit.draft.trim()).then(
      (record) => dispatchMemory({ type: "edit-saved", record }),
      (err) => {
        if (isMemoryError(err)) dispatchMemory({ type: "edit-failed", error: err });
        else dispatchMemory({ type: "unavailable" });
      },
    );
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [memoryEditSaving]);

  // Second step of the two-step delete confirm (the first step only arms
  // confirmDelete in the reducer).
  const performDelete = (id: number) => {
    memoryDelete(id).then(
      () => dispatchMemory({ type: "deleted", id }),
      (err) => {
        if (isMemoryError(err)) dispatchMemory({ type: "delete-failed", error: err });
        else dispatchMemory({ type: "unavailable" });
      },
    );
  };

  // Second step of the two-step wipe confirm.
  const performWipe = () => {
    memoryWipe().then(
      (removed) => dispatchMemory({ type: "wiped", removed }),
      (err) => {
        if (isMemoryError(err)) dispatchMemory({ type: "wipe-failed", error: err });
        else dispatchMemory({ type: "unavailable" });
      },
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

          <div className="guard-subsection" aria-labelledby="settings-guard-heading">
            <h3 id="settings-guard-heading" className="guard-subheading">
              Privacy Guard
            </h3>
            <p className="settings-hint">
              Redacts secrets before anything leaves this Mac, and blocks the
              request when it can't. Counts only — the text itself is never
              kept.
            </p>
            {guard === null ? (
              <p className="settings-unavailable">{GUARD_UNAVAILABLE_MESSAGE}</p>
            ) : (
              <>
                <div className="settings-status-row">
                  <span className="settings-row-label">Guard</span>
                  <span className="settings-status-value" data-guard-active="true">
                    Active
                  </span>
                </div>
                {redactionRows(guard).map((row) => (
                  <div
                    key={row.kind}
                    className="settings-status-row"
                    data-guard-kind={row.kind}
                  >
                    <span className="settings-row-label">{row.label} redacted</span>
                    <span className="settings-status-value">{row.count}</span>
                  </div>
                ))}
                <div className="settings-status-row" data-guard-blocked>
                  <span className="settings-row-label">Requests blocked</span>
                  <span className="settings-status-value">{guard.blockedCount}</span>
                </div>
                {guard.lastBlockReason && (
                  <div className="settings-status-row" data-guard-last-block>
                    <span className="settings-row-label">Last block</span>
                    <span className="settings-status-value">
                      {blockReasonLabel(guard.lastBlockReason)}
                    </span>
                  </div>
                )}
                {guard.lastError && (
                  <div className="settings-error" role="alert">
                    <strong>{bannerTitle(guard.lastError)}</strong>
                    <span>{bannerDetail(guard.lastError)}</span>
                  </div>
                )}
              </>
            )}
          </div>
        </section>

        <section className="settings-section" aria-labelledby="settings-watcher-heading">
          <h2 id="settings-watcher-heading" className="settings-section-title">
            Watch Screen
          </h2>
          <label className="settings-row">
            <span className="settings-row-label">Watch Screen</span>
            <input
              type="checkbox"
              className="settings-toggle"
              aria-label="Watch Screen"
              disabled={watcher.status === null}
              checked={watcher.status?.enabled ?? false}
              onChange={(event) => toggleWatcher(event.target.checked)}
            />
          </label>
          <p className="settings-hint">
            Reads on-screen text every few seconds, on-device. No image is
            ever saved.
          </p>
          {watcher.status === null && (
            <p className="settings-unavailable">
              Watcher state is unavailable outside the app.
            </p>
          )}
          {watcher.status?.error && (
            <div className="settings-error" role="alert">
              <strong>Watch Screen couldn't be saved</strong>
              <span>{watcher.status.error}</span>
            </div>
          )}
          {watcher.status && (
            <div className="settings-status-row">
              <span className="settings-row-label">Status</span>
              <span
                className="settings-status-value"
                data-watcher-state={watcher.status.state}
              >
                {runStateLabel(watcher.status.state)}
              </span>
            </div>
          )}
          {watcher.status?.lastTickError && (
            <div className="settings-error" role="alert">
              <strong>{tickErrorTitle(watcher.status.lastTickError)}</strong>
              <span>{tickErrorDetail(watcher.status.lastTickError)}</span>
            </div>
          )}
          {watcher.observations.length > 0 ? (
            <ul className="watcher-snippets" aria-label="Recent extracted text">
              {watcher.observations.map((o) => (
                <li key={o.capturedAt} className="watcher-snippet">
                  <span className="watcher-snippet-meta">
                    {capturedAtLabel(o.capturedAt)}
                    {o.appContext ? ` — ${o.appContext}` : ""}
                  </span>
                  <span className="watcher-snippet-text">{snippetPreview(o.text)}</span>
                </li>
              ))}
            </ul>
          ) : (
            watcher.status?.state === "watching" && (
              <p className="settings-hint">Watching — no text extracted yet.</p>
            )
          )}
        </section>

        <section className="settings-section" aria-labelledby="settings-nudges-heading">
          <h2 id="settings-nudges-heading" className="settings-section-title">
            Nudges
          </h2>
          <label className="settings-row">
            <span className="settings-row-label">Nudges</span>
            <input
              type="checkbox"
              className="settings-toggle"
              aria-label="Nudges"
              disabled={nudges === null}
              checked={nudges?.enabled ?? false}
              onChange={(event) => toggleNudges(event.target.checked)}
            />
          </label>
          <p className="settings-hint">
            Occasionally shows a small overlay hint when the watched screen
            looks like Third Eye can help. Needs Watch Screen on; never steals
            focus.
          </p>
          {nudges === null && (
            <p className="settings-unavailable">
              Nudge state is unavailable outside the app.
            </p>
          )}
          {nudges?.persistError && (
            <div className="settings-error" role="alert">
              <strong>Nudges couldn't be saved</strong>
              <span>{nudges.persistError}</span>
            </div>
          )}
          {nudges?.lastError && (
            <div className="settings-error" role="alert">
              <strong>{bannerTitle(nudges.lastError)}</strong>
              <span>{bannerDetail(nudges.lastError)}</span>
            </div>
          )}
        </section>

        <section className="settings-section" aria-labelledby="settings-cloud-heading">
          <h2 id="settings-cloud-heading" className="settings-section-title">
            Cloud Providers
          </h2>
          <label className="settings-row">
            <span className="settings-row-label">Use cloud providers</span>
            <input
              type="checkbox"
              className="settings-toggle"
              aria-label="Use cloud providers"
              disabled={cloud.optin === null}
              checked={cloud.optin?.enabled ?? false}
              onChange={(event) => toggleCloudOptin(event.target.checked)}
            />
          </label>
          <p className="settings-hint">
            Off by default — everything runs on-device. Turn this on to let the
            heavy lane use a remote provider with your own API key. Turning it
            off reverts to local-only.
          </p>
          {cloud.optin === null && (
            <p className="settings-unavailable">
              Cloud state is unavailable outside the app.
            </p>
          )}
          {cloud.optin?.persistError && (
            <div className="settings-error" role="alert">
              <strong>Cloud opt-in couldn't be saved</strong>
              <span>{cloud.optin.persistError}</span>
            </div>
          )}
          {cloud.optin?.enabled && (
            <>
              {CLOUD_PROVIDERS.map((p) => (
                <div key={p.id} className="cloud-provider" data-cloud-provider={p.id}>
                  <div className="settings-status-row">
                    <span className="settings-row-label">{p.label} API key</span>
                    <span
                      className="settings-status-value"
                      data-cloud-key-present={keyPresent(cloud.keys, p.id)}
                    >
                      {keyPresent(cloud.keys, p.id) ? "Stored" : "Not stored"}
                    </span>
                  </div>
                  <div className="settings-row">
                    <input
                      type="password"
                      className="settings-key-input"
                      aria-label={`${p.label} API key`}
                      data-cloud-key-input={p.id}
                      autoComplete="off"
                      placeholder={keyPresent(cloud.keys, p.id) ? "Replace stored key" : "Paste API key"}
                      value={keyDrafts[p.id]}
                      onChange={(event) =>
                        setKeyDrafts((drafts) => ({ ...drafts, [p.id]: event.target.value }))
                      }
                    />
                    <button
                      type="button"
                      className="settings-refresh"
                      data-cloud-key-save={p.id}
                      disabled={keyDrafts[p.id].trim().length === 0}
                      onClick={() => submitKey(p.id)}
                    >
                      Save
                    </button>
                    {keyPresent(cloud.keys, p.id) && (
                      <button
                        type="button"
                        className="memory-delete"
                        data-cloud-key-delete={p.id}
                        aria-label={`Delete ${p.label} API key`}
                        onClick={() => removeKey(p.id)}
                      >
                        Delete
                      </button>
                    )}
                  </div>
                </div>
              ))}
              {cloud.keyError && (
                <div className="settings-error" role="alert">
                  <strong>{keyErrorTitle(cloud.keyError)}</strong>
                  <span>{cloud.keyError.detail}</span>
                </div>
              )}
              <label className="settings-row">
                <span className="settings-row-label">Heavy lane provider</span>
                <select
                  className="settings-select"
                  aria-label="Heavy lane cloud provider"
                  data-cloud-heavy-provider
                  value={cloud.heavy?.provider ?? DEFAULT_OPTION}
                  onChange={(event) =>
                    selectHeavyProvider(
                      event.target.value === DEFAULT_OPTION
                        ? null
                        : (event.target.value as CloudProvider),
                    )
                  }
                >
                  <option value={DEFAULT_OPTION}>none (local only)</option>
                  {CLOUD_PROVIDERS.map((p) => (
                    <option key={p.id} value={p.id}>
                      {p.label}
                    </option>
                  ))}
                </select>
              </label>
              <p className="settings-hint">
                Which provider the heavy lane targets. Routing lands in a later
                update — this only remembers the choice for now.
              </p>
              {cloud.heavy?.persistError && (
                <div className="settings-error" role="alert">
                  <strong>Provider choice couldn't be saved</strong>
                  <span>{cloud.heavy.persistError}</span>
                </div>
              )}
            </>
          )}
        </section>

        <section className="settings-section" aria-labelledby="settings-memory-heading">
          <h2 id="settings-memory-heading" className="settings-section-title">
            Memory
          </h2>
          {memory.availability === "unavailable" ? (
            <p className="settings-unavailable">{MEMORY_UNAVAILABLE_MESSAGE}</p>
          ) : (
            <>
              {memory.status && !memory.status.available && (
                <div className="settings-error" role="alert">
                  <strong>Memory store is unavailable</strong>
                  {memory.status.storeError && (
                    <span>{memoryErrorMessage(memory.status.storeError)}</span>
                  )}
                </div>
              )}
              {memory.status?.available && memory.status.storeError && (
                <div className="settings-error" role="alert">
                  <strong>Memory count failed</strong>
                  <span>{memoryErrorMessage(memory.status.storeError)}</span>
                </div>
              )}
              {memory.banner && (
                <div className="settings-error" role="alert">
                  <span>{memory.banner}</span>
                  <button
                    type="button"
                    className="settings-dismiss"
                    onClick={() => dispatchMemory({ type: "dismiss-banner" })}
                  >
                    Dismiss
                  </button>
                </div>
              )}
              {memory.notice && (
                <div className="settings-notice" role="status">
                  <span>{memory.notice}</span>
                  <button
                    type="button"
                    className="settings-dismiss"
                    onClick={() => dispatchMemory({ type: "dismiss-notice" })}
                  >
                    Dismiss
                  </button>
                </div>
              )}
              {memory.status?.available && (
                <>
                  <div className="settings-status-row">
                    <span className="settings-row-label">Stored memories</span>
                    <span className="settings-status-value">
                      {memory.status.count ?? "unknown"}
                    </span>
                  </div>
                  <div className="settings-status-row">
                    <span className="settings-row-label">Ingest</span>
                    <span className="settings-status-value">
                      {memory.status.ingest.buffered} buffered ·{" "}
                      {memory.status.ingest.distilledCount} distilled
                    </span>
                  </div>
                  <div className="settings-status-row">
                    <span className="settings-row-label">Last distill</span>
                    <span className="settings-status-value">
                      {lastDistillLabel(memory.status.ingest.lastDistillAtMs)}
                    </span>
                  </div>
                  {memory.status.ingest.lastError && (
                    <div className="settings-error" role="alert">
                      <strong>{bannerTitle(memory.status.ingest.lastError)}</strong>
                      <span>{bannerDetail(memory.status.ingest.lastError)}</span>
                    </div>
                  )}
                </>
              )}
              {memory.loading ? (
                <p className="settings-hint">Loading memories…</p>
              ) : memory.records.length === 0 ? (
                <p className="settings-hint">{MEMORY_EMPTY_HINT}</p>
              ) : (
                <ul className="memory-list" aria-label="Stored memories">
                  {memory.records.map((r) => (
                    <li key={r.id} className="memory-row" data-memory-id={r.id}>
                      {memory.edit?.id === r.id ? (
                        <div className="memory-edit">
                          <textarea
                            className="memory-edit-input"
                            aria-label="Edit memory summary"
                            value={memory.edit.draft}
                            disabled={memory.edit.saving}
                            onChange={(event) =>
                              dispatchMemory({
                                type: "edit-draft",
                                draft: event.target.value,
                              })
                            }
                          />
                          {memory.edit.error && (
                            <p className="memory-edit-error" role="alert">
                              {memory.edit.error}
                            </p>
                          )}
                          <div className="memory-row-actions">
                            <button
                              type="button"
                              className="settings-refresh"
                              disabled={memory.edit.saving}
                              onClick={() => dispatchMemory({ type: "save-edit" })}
                            >
                              {memory.edit.saving ? "Saving…" : "Save"}
                            </button>
                            <button
                              type="button"
                              className="settings-refresh"
                              disabled={memory.edit.saving}
                              onClick={() => dispatchMemory({ type: "cancel-edit" })}
                            >
                              Cancel
                            </button>
                          </div>
                        </div>
                      ) : (
                        <button
                          type="button"
                          className="memory-summary"
                          title="Click to edit"
                          onClick={() =>
                            dispatchMemory({ type: "begin-edit", id: r.id })
                          }
                        >
                          {r.summary}
                        </button>
                      )}
                      <span className="memory-meta">
                        {appsLabel(r.apps)} · {spanLabel(r.spanStartMs, r.spanEndMs)}
                      </span>
                      <span className="memory-meta">
                        Created {memoryTimeLabel(r.createdAtMs)}
                        {r.updatedAtMs !== r.createdAtMs
                          ? ` · Updated ${memoryTimeLabel(r.updatedAtMs)}`
                          : ""}
                      </span>
                      <div className="memory-row-actions">
                        {memory.confirmDelete === r.id ? (
                          <>
                            <button
                              type="button"
                              className="memory-delete-confirm"
                              onClick={() => performDelete(r.id)}
                            >
                              Confirm delete
                            </button>
                            <button
                              type="button"
                              className="settings-refresh"
                              onClick={() => dispatchMemory({ type: "cancel-delete" })}
                            >
                              Cancel
                            </button>
                          </>
                        ) : (
                          <button
                            type="button"
                            className="memory-delete"
                            aria-label={`Delete memory ${r.id}`}
                            onClick={() =>
                              dispatchMemory({ type: "request-delete", id: r.id })
                            }
                          >
                            Delete
                          </button>
                        )}
                      </div>
                    </li>
                  ))}
                </ul>
              )}
              {(canGoPrev(memory) || canGoNext(memory)) && (
                <div className="memory-pager">
                  <button
                    type="button"
                    className="settings-refresh"
                    disabled={!canGoPrev(memory) || memory.loading}
                    onClick={() => dispatchMemory({ type: "prev-page" })}
                  >
                    Prev
                  </button>
                  <span className="settings-status-value">
                    Page {memory.offset / MEMORY_PAGE_SIZE + 1}
                  </span>
                  <button
                    type="button"
                    className="settings-refresh"
                    disabled={!canGoNext(memory) || memory.loading}
                    onClick={() => dispatchMemory({ type: "next-page" })}
                  >
                    Next
                  </button>
                </div>
              )}
              {memory.records.length > 0 && (
                <div className="memory-wipe-row">
                  {memory.confirmWipe ? (
                    <>
                      <span className="memory-wipe-warning">
                        Delete all stored memories? This can't be undone.
                      </span>
                      <button
                        type="button"
                        className="memory-delete-confirm"
                        onClick={performWipe}
                      >
                        Confirm wipe
                      </button>
                      <button
                        type="button"
                        className="settings-refresh"
                        onClick={() => dispatchMemory({ type: "cancel-wipe" })}
                      >
                        Cancel
                      </button>
                    </>
                  ) : (
                    <button
                      type="button"
                      className="memory-delete"
                      onClick={() => dispatchMemory({ type: "request-wipe" })}
                    >
                      Wipe all memories
                    </button>
                  )}
                </div>
              )}
            </>
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
