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

import type {
  BuildInfo,
  NudgeHistoryEntry, ToolTogglesStatus,
  WorkspaceStatus,
  BridgeStatus,
  LmModelRow } from "./chat";
import { useEffect, useReducer, useState } from "react";
import {
  bannerDetail,
  bannerTitle,
  hidArmedStatus,
  modelInfo,
  nudgeStatus,
  onHidStateChanged,
  onModelInfoBroadcast,
  onNudgeState,
  commandsStatus,
  inventorySearch,
  inventoryStatus,
  memoryRetention,
  refreshInventory,
  onPrivacyChanged,
  openInputSettings,
  privacyStatus,
  setCommandsAllowlist,
  allowlistEntryAlwaysAsks,
  laneHealth,
  type LaneHealth,
  logPath,
  revealLog,
  chromeJsStatus,
  type ChromeJsStatus,
  setCommandsEnabled,
  setHidRunMode,
  setMemoryRetention,
  approvedActionKinds,
  buildInfo,
  removeApprovedActionKind,
  setNudgeAutoDismiss,
  setNudgeCooldown,
  nudgeHistory,
  setNudgesEnabled,
  setToolEnabled,
  toolTogglesStatus,
  setWorkspaceRoots,
  workspaceRoots,
  bridgeStatus,
  listModelsDetailed,
  verboseStatus,
  setVerboseStatus,
  type CommandsStatus,
  type HidRunMode,
  type InventoryEntry,
  type InventoryStatus,
  type InputError,
  type NudgeStatus,
} from "./chat";
import {
  CLOUD_PROVIDERS,
  cloudHeavyProvider,
  cloudCoderProvider,
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
  setCloudCoderProvider,
  setCloudOptin,
  type CloudProvider,
} from "./cloud-state";
import {
  MCP_RUN_MODE_OPTIONS,
  MCP_TRANSPORT_OPTIONS,
  deleteMcpAuth,
  initialMcpViewState,
  isMcpAuthError,
  mcpAuthRef,
  mcpAuthStatus,
  mcpHealthLine,
  mcpModeShowsAutoRunWarning,
  mcpReducer,
  mcpServers,
  mcpStatus,
  onMcpStateChanged,
  parseServersJson,
  serversToJson,
  setMcpAuth,
  setMcpRunMode,
  setMcpServers,
  type McpAuthError,
  type McpJsonError,
  type McpRunMode,
  type McpServerConfig,
  type McpTransport,
} from "./mcp-state";
import {
  SECTION_GROUPS,
  filterGroups,
  sectionFromSearch,
  type SectionId,
} from "./settings-nav";
import { OVERLAY_MIN_HEIGHT, OVERLAY_MIN_WIDTH, type Edge } from "./overlay-geometry";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { Toggle } from "./ui/Toggle";
import { ChoiceChips } from "./ui/Chip";

/** Closed nudge-tuning choice sets — mirror COOLDOWN_CHOICES_SECS /
 *  AUTO_DISMISS_CHOICES_SECS in src-tauri/src/nudge/commands.rs. */
const NUDGE_COOLDOWN_OPTIONS = [
  { value: "60", label: "1 min" },
  { value: "300", label: "5 min" },
  { value: "900", label: "15 min" },
  { value: "3600", label: "1 hour" },
] as const;
const NUDGE_DISMISS_OPTIONS = [
  { value: "8", label: "8 s" },
  { value: "12", label: "12 s" },
  { value: "20", label: "20 s" },
  { value: "30", label: "30 s" },
] as const;
import { RETENTION_OPTIONS, type Retention } from "./tour-state";
import {
  drawerEdgeOf,
  drawerExtentFor,
  onOverlayPresentation,
  overlayPresentation,
  setOverlayExtent,
  setOverlayPresentation,
  type PresentationMode,
  type PresentationStatus,
} from "./overlay-presentation-state";
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
  setChatMemoryEnabled,
  sourceLabel,
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
  integrationsStatus,
  installCli,
  removeCli,
  installFinderAction,
  removeFinderAction,
  type IntegrationsStatus,
  endpointDraftFor,
  HID_RUN_MODE_OPTIONS,
  hidModeShowsAutoRunWarning,
  hideSettingsWindow,
  hotkeyStatus,
  setHotkey,
  setAutostart,
  HOTKEY_PRESETS,
  initialSettingsState,
  isLoopbackEndpoint,
  laneOptions,
  listModels,
  llmEndpointStatus,
  modelsErrorDetail,
  modelsErrorTitle,
  setLaneModel,
  setLlmEndpoint,
  setPrivacyMode,
  settingsReducer,
  toModelsError,
  type EndpointStatus,
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

/** Overlay presentation choices for the mode select: a free modal window or a
 *  drawer docked flush against one of the four display edges. Mirrors the Rust
 *  PresentationMode serde tags (a drawer edge reuses the `Edge` union). */
const PRESENTATION_MODE_OPTIONS: { value: PresentationMode; label: string }[] = [
  { value: "modal", label: "Modal (floating)" },
  { value: "top", label: "Drawer — top edge" },
  { value: "bottom", label: "Drawer — bottom edge" },
  { value: "left", label: "Drawer — left edge" },
  { value: "right", label: "Drawer — right edge" },
];

/** The overlay minimum for a drawer edge's variable axis: width for the
 *  left/right edges, height for top/bottom. Used as the extent input's `min`
 *  (the backend also floors any sub-min value safe, so this is only guidance). */
function extentMinFor(edge: Edge): number {
  return edge === "left" || edge === "right" ? OVERLAY_MIN_WIDTH : OVERLAY_MIN_HEIGHT;
}

/** Human title for a refused HID arm / persist failure (R007 — every failure
 *  is typed and visible, never a silent no-op). `permission-denied` drives the
 *  walkthrough rather than this banner, so it is rendered separately. */
function hidErrorTitle(error: InputError): string {
  switch (error.kind) {
    case "permission-denied":
      return "Accessibility permission needed";
    case "disabled":
      return "Input Control is disarmed";
    case "unsupported":
      return "Input Control isn't supported here";
    case "input-failed":
      return "Input Control couldn't be saved";
  }
}

function Settings() {
  const [state, dispatch] = useReducer(settingsReducer, initialSettingsState);
  const [watcher, dispatchWatcher] = useReducer(watcherReducer, initialWatcherViewState);
  const [memory, dispatchMemory] = useReducer(memoryReducer, initialMemoryViewState);
  const [cloud, dispatchCloud] = useReducer(cloudReducer, initialCloudViewState);
  const [mcp, dispatchMcp] = useReducer(mcpReducer, initialMcpViewState);
  // PyCharm-style navigation: one page shown at a time, picked from the
  // grouped sidebar. The initial page honors the ?section= deep link (used by
  // Playwright and any future "open settings at X" caller); an unknown value
  // falls back to the default page. Pure view state — all backend snapshots
  // and subscriptions live above this in the component, so switching pages
  // never drops live state.
  const [active, setActive] = useState<SectionId>(() =>
    sectionFromSearch(window.location.search),
  );
  const [navFilter, setNavFilter] = useState("");
  // The MCP "Edit as JSON" mode: the draft is seeded from the authoritative
  // persisted list on entry and exists only while the editor is open — the
  // structured list view always renders backend truth. A refused draft rides
  // `error` in the banner (never a silent no-op); bearer tokens are keychain
  // refs in this JSON, never token material (R018).
  const [mcpJson, setMcpJson] = useState<{
    open: boolean;
    draft: string;
    error: McpJsonError | null;
  }>({ open: false, draft: "", error: null });
  // The add-server draft lives in local component state — never in the reducer,
  // which only ever holds the backend-authoritative persisted list. Cleared the
  // instant a server is handed to set_mcp_servers.
  const [serverDraft, setServerDraft] = useState<{
    id: string;
    transport: McpTransport;
    command: string;
    args: string;
    url: string;
    token: string;
  }>({
    id: "",
    transport: "stdio",
    command: "",
    args: "",
    url: "",
    token: "",
  });
  // Per-authRef presence of a stored bearer token — booleans only, backend
  // authoritative (never the token). Refreshed from mcp_auth_status whenever the
  // http server list changes. The write-only token drafts live keyed by server
  // id, never in a reducer, so an entered token can never round-trip back into
  // rendered view state (the never-echo property); cleared the instant handed to
  // set_mcp_auth. A typed store failure rides mcpAuthError.
  const [mcpAuthPresent, setMcpAuthPresent] = useState<Record<string, boolean>>({});
  const [mcpTokenDrafts, setMcpTokenDrafts] = useState<Record<string, string>>({});
  const [mcpAuthError, setMcpAuthError] = useState<McpAuthError | null>(null);
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
  const [toolToggles, setToolToggles] = useState<ToolTogglesStatus | null>(null);
  const [nudgeLog, setNudgeLog] = useState<NudgeHistoryEntry[] | null>(null);
  const [approvedKinds, setApprovedKinds] = useState<string[] | null>(null);
  const [build, setBuild] = useState<BuildInfo | null>(null);
  const [workspaces, setWorkspaces] = useState<WorkspaceStatus | null>(null);
  const [bridge, setBridge] = useState<BridgeStatus | null>(null);
  const [modelRows, setModelRows] = useState<LmModelRow[]>([]);
  const [verbose, setVerbose] = useState<boolean | null>(null);
  const [hotkeyDraft, setHotkeyDraft] = useState("");
  const [integrations, setIntegrations] = useState<IntegrationsStatus | null>(null);
  const applyHotkey = (shortcut: string) => {
    setHotkey(shortcut).then(
      (status) => dispatch({ type: "hotkey", status }),
      (err) => console.debug("settings: set_hotkey unavailable:", err),
    );
  };
  const [newRoot, setNewRoot] = useState("");
  // Memory retention (tour Memory step's setting, mirrored here). Optimistic
  // select, then the backend's effective value folds back — a rejected value
  // or persist failure lands the truthful state, never a lying chip.
  const [retention, setRetention] = useState<Retention>("30d");
  const [retentionError, setRetentionError] = useState<string | null>(null);
  // Terminal-commands gate (computer-control I2): a single authoritative
  // backend snapshot, like nudges.
  const [commands, setCommands] = useState<CommandsStatus | null>(null);
  const [allowlistDraft, setAllowlistDraft] = useState("");
  // Programs pane (computer-control I1 surface): cache health, an on-demand
  // rescan, and a search over the cached inventory.
  const [inventory, setInventory] = useState<InventoryStatus | null>(null);
  const [inventoryBusy, setInventoryBusy] = useState(false);
  const [programQuery, setProgramQuery] = useState("");
  const [programs, setPrograms] = useState<InventoryEntry[] | null>(null);
  // Guard telemetry is likewise a single authoritative backend snapshot
  // (mount query and privacy://state broadcast land the same shape); all
  // display logic lives in pure privacy-state.ts helpers.
  const [guard, setGuard] = useState<GuardTelemetry | null>(null);
  // Overlay presentation is a single authoritative backend snapshot too (mount
  // query, mode/extent command response, and the overlay://presentation
  // broadcast all land the same PresentationStatus shape). This webview holds
  // core:default only — no window-geometry ACLs — so it never applies geometry;
  // it invokes the mutators and lets the overlay webview do the apply. Every
  // handler dispatches the status the command returns / the broadcast pushes,
  // never optimistically mutating (MEM082/MEM027), so the two windows can't drift.
  const [presentation, setPresentation] = useState<PresentationStatus | null>(null);

  // The endpoint input draft lives in local state (typing must not round-trip
  // through the backend); the authoritative EndpointStatus lives in the
  // reducer. Seeded from every status that arrives (mount query and save
  // responses) so the input always restarts from the persisted truth.
  const [endpointDraft, setEndpointDraft] = useState("");
  const applyEndpointStatus = (status: EndpointStatus) => {
    dispatch({ type: "endpoint", status });
    setEndpointDraft(endpointDraftFor(status));
  };

  const refreshModels = () => {
    dispatch({ type: "models-loading" });
    listModels().then(
      (models) => dispatch({ type: "models-loaded", models }),
      (err) => dispatch({ type: "models-error", error: toModelsError(err) }),
    );
  };

  // Persist the endpoint draft; a blank input is an explicit reset (null),
  // same as the Reset button. The backend validates and returns the updated
  // status — restartRequired then drives the "restart to apply" hint.
  const saveEndpoint = (draft: string | null) => {
    const trimmed = draft?.trim() ?? "";
    setLlmEndpoint(trimmed === "" ? null : trimmed).then(applyEndpointStatus, (err) =>
      dispatch({ type: "endpoint-error", detail: String(err) }),
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
    llmEndpointStatus().then(applyEndpointStatus, (err) =>
      console.debug("settings: llm_endpoint_status unavailable:", err),
    );
    privacyStatus().then(
      (status) => dispatch({ type: "privacy", status }),
      (err) => console.debug("settings: privacy_status unavailable:", err),
    );
    // Health-as-value: safe to poll, never rejects backend-side. The MEM115
    // fallback if the hid://state subscription can't attach.
    hidArmedStatus().then(
      (status) => dispatch({ type: "hid", status }),
      (err) => console.debug("settings: hid_armed_status unavailable:", err),
    );
    hotkeyStatus().then(
      (status) => dispatch({ type: "hotkey", status }),
      (err) => console.debug("settings: hotkey_status unavailable:", err),
    );
    autostartStatus().then(
      (status) => dispatch({ type: "autostart", status }),
      (err) => console.debug("settings: autostart_status unavailable:", err),
    );
    integrationsStatus().then(
      (status) => setIntegrations(status),
      (err) => console.debug("settings: integrations_status unavailable:", err),
    );
    watcherStatus().then(
      (status) => dispatchWatcher({ type: "status", status }),
      (err) => console.debug("settings: watcher_status unavailable:", err),
    );
    nudgeStatus().then(
      (status) => setNudges(status),
      (err) => console.debug("settings: nudge_status unavailable:", err),
    );
    toolTogglesStatus().then(
      (status) => setToolToggles(status),
      (err) => console.debug("settings: tool_toggles_status unavailable:", err),
    );
    nudgeHistory().then(
      (entries) => setNudgeLog(entries),
      (err) => console.debug("settings: nudge_history unavailable:", err),
    );
    approvedActionKinds().then(
      (kinds) => setApprovedKinds(kinds),
      (err) => console.debug("settings: approved_action_kinds unavailable:", err),
    );
    buildInfo().then(
      (info) => setBuild(info),
      (err) => console.debug("settings: build_info unavailable:", err),
    );
    bridgeStatus().then(
      (status) => setBridge(status),
      (err) => console.debug("settings: bridge_status unavailable:", err),
    );
    listModelsDetailed().then(
      (rows) => setModelRows(rows),
      (err) => console.debug("settings: list_models_detailed unavailable:", err),
    );
    verboseStatus().then(
      (status) => setVerbose(status.enabled),
      (err) => console.debug("settings: verbose_status unavailable:", err),
    );
    workspaceRoots().then(
      (status) => setWorkspaces(status),
      (err) => console.debug("settings: workspace_roots unavailable:", err),
    );
    memoryRetention().then(
      (status) => setRetention(status.retention as Retention),
      (err) => console.debug("settings: memory_retention unavailable:", err),
    );
    commandsStatus().then(
      (status) => setCommands(status),
      (err) => console.debug("settings: commands_status unavailable:", err),
    );
    inventoryStatus().then(
      (status) => setInventory(status),
      (err) => console.debug("settings: inventory_status unavailable:", err),
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
    cloudCoderProvider().then(
      (status) => dispatchCloud({ type: "coder", status }),
      (err) => console.debug("settings: cloud_coder_provider unavailable:", err),
    );
    // MCP host health + server list — health-as-value, safe to poll (R007). The
    // mcp://state subscription is the live path; these are the boot snapshot.
    mcpStatus().then(
      (status) => dispatchMcp({ type: "health", status }),
      (err) => console.debug("settings: mcp_status unavailable:", err),
    );
    mcpServers().then(
      (status) => dispatchMcp({ type: "servers", status }),
      (err) => console.debug("settings: mcp_servers unavailable:", err),
    );
    // Health-as-value beside the overlay geometry (R007): a PresentationStatus
    // at any time, never an error. Rejects only outside a Tauri runtime, where
    // the presentation stays null and the section shows its unavailable note.
    overlayPresentation().then(
      (status) => setPresentation(status),
      (err) => console.debug("settings: overlay_presentation unavailable:", err),
    );
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // Cross-window sync: a tray privacy toggle or an overlay lane override
  // must update this window too, not just the one that asked.
  useEffect(() => {
    const unlistens = [
      onModelInfoBroadcast((info) => dispatch({ type: "model-info", info })),
      onPrivacyChanged((status) => dispatch({ type: "privacy", status })),
      // HID arming truth flows one way, backend → UI: an arm/disarm from any
      // surface (this window or a future tray path) broadcasts the resulting
      // HidArmedStatus as hid://state.
      onHidStateChanged((status) => dispatch({ type: "hid", status })),
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
      // MCP host health flows one way, backend → UI: a run-mode change from any
      // window OR a spawn/ready/crash lifecycle transition broadcasts the
      // resulting McpHealthStatus as mcp://state.
      onMcpStateChanged((status) => dispatchMcp({ type: "health", status })),
      // Overlay presentation truth flows one way, backend → UI: a mode/extent
      // change from this window OR a live resize on the overlay webview
      // broadcasts the resulting PresentationStatus as overlay://presentation.
      onOverlayPresentation((status) => setPresentation(status)),
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

  // Token presence for each remote (http) server's authRef — health-as-value,
  // booleans only (R018). Re-queried whenever the persisted list changes so a
  // freshly-added http server, a removed one, or a token set/cleared elsewhere
  // stays truthful. Outside a Tauri runtime mcp_auth_status rejects and the row
  // simply shows "Not stored".
  useEffect(() => {
    const servers = mcp.servers;
    if (servers === null) return;
    let cancelled = false;
    for (const server of servers) {
      if (server.transport !== "http" || !server.authRef) continue;
      const authRef = server.authRef;
      mcpAuthStatus(authRef).then(
        (status) => {
          if (!cancelled) setMcpAuthPresent((prev) => ({ ...prev, [authRef]: status.present }));
        },
        (err) => console.debug("settings: mcp_auth_status unavailable:", err),
      );
    }
    return () => {
      cancelled = true;
    };
  }, [mcp.servers]);

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

  // Lane health (review item 5): re-checked whenever the lane list changes
  // (a pin, a refresh) so the warning under a select is never stale.
  const [health, setHealth] = useState<LaneHealth[]>([]);
  useEffect(() => {
    laneHealth().then(setHealth, () => setHealth([]));
  }, [state.modelInfo]);
  // The always-on log's location (review item 1) — null outside the app.
  const [log, setLog] = useState<string | null>(null);
  useEffect(() => {
    logPath().then(setLog, () => setLog(null));
  }, []);
  // Chrome page scripting (system tools S3): a one-time manual toggle in
  // Chrome; shown with a recheck so the user can confirm it took.
  const [chromeJs, setChromeJs] = useState<ChromeJsStatus | null>(null);
  const recheckChromeJs = () => chromeJsStatus().then(setChromeJs, () => setChromeJs(null));
  useEffect(() => {
    recheckChromeJs();
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

  const toggleChatMemory = (enable: boolean) => {
    setChatMemoryEnabled(enable).then(
      // Never rejects backend-side; a persist failure rides status.error
      // and a rolled-back toggle comes back as the authoritative snapshot.
      (status) => dispatchMemory({ type: "chat-memory", status }),
      (err) => console.debug("settings: set_chat_memory_enabled unavailable:", err),
    );
  };

  const chooseRetention = (value: Retention) => {
    setRetention(value);
    setMemoryRetention(value).then(
      (status) => {
        // The backend's effective value wins (rejected value / persist
        // failure comes back as data — the chips must not lie).
        setRetention(status.retention as Retention);
        setRetentionError(status.error);
      },
      (err) => console.debug("settings: set_memory_retention unavailable:", err),
    );
  };

  const toggleCommands = (enable: boolean) => {
    setCommandsEnabled(enable).then(
      // Never rejects backend-side; a persist failure rides status.error and
      // the rolled-back value comes back as the authoritative snapshot.
      (status) => setCommands(status),
      (err) => console.debug("settings: set_commands_enabled unavailable:", err),
    );
  };

  const updateAllowlist = (entries: string[]) => {
    setCommandsAllowlist(entries).then(
      (status) => setCommands(status),
      (err) => console.debug("settings: set_commands_allowlist unavailable:", err),
    );
  };

  const addAllowlistEntry = () => {
    const entry = allowlistDraft.trim();
    if (!entry || commands === null) return;
    setAllowlistDraft("");
    updateAllowlist([...commands.allowlist, entry]);
  };

  const rescanInventory = () => {
    setInventoryBusy(true);
    refreshInventory().then(
      (status) => {
        setInventoryBusy(false);
        setInventory(status);
      },
      (err) => {
        setInventoryBusy(false);
        console.debug("settings: refresh_inventory unavailable:", err);
      },
    );
  };

  const searchPrograms = (query: string) => {
    setProgramQuery(query);
    if (query.trim().length === 0) {
      setPrograms(null);
      return;
    }
    inventorySearch(query, 50).then(
      (entries) => setPrograms(entries),
      (err) => console.debug("settings: inventory_search unavailable:", err),
    );
  };

  const selectHidMode = (mode: HidRunMode) => {
    setHidRunMode(mode).then(
      // Never rejects backend-side; a refused select (permission-denied) or
      // persist failure rides status.error, and a rolled-back mode comes back
      // as the authoritative snapshot (R007 — always a visible outcome).
      (status) => dispatch({ type: "hid", status }),
      (err) => console.debug("settings: set_hid_run_mode unavailable:", err),
    );
  };

  const openAccessibilitySettings = () => {
    console.debug("hid: opening Accessibility settings from walkthrough");
    openInputSettings().catch((err) =>
      console.debug("settings: open_input_settings unavailable:", err),
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

  const toggleTool = (name: string, enable: boolean) => {
    setToolEnabled(name, enable).then(
      // Never rejects backend-side; a persist failure rides persistError and
      // a rolled-back toggle comes back as the authoritative snapshot.
      (status) => setToolToggles(status),
      (err) => console.debug("settings: set_tool_enabled unavailable:", err),
    );
  };

  const chooseNudgeCooldown = (value: string) => {
    setNudgeCooldown(Number(value)).then(
      (status) => setNudges(status),
      (err) => console.debug("settings: set_nudge_cooldown unavailable:", err),
    );
  };

  const chooseNudgeAutoDismiss = (value: string) => {
    setNudgeAutoDismiss(Number(value)).then(
      (status) => setNudges(status),
      (err) => console.debug("settings: set_nudge_auto_dismiss unavailable:", err),
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

  const selectCoderProvider = (provider: CloudProvider | null) => {
    setCloudCoderProvider(provider).then(
      (status) => dispatchCloud({ type: "coder", status }),
      (err) => console.debug("settings: set_cloud_coder_provider unavailable:", err),
    );
  };
  const selectHeavyProvider = (provider: CloudProvider | null) => {
    setCloudHeavyProvider(provider).then(
      // Never rejects backend-side; a persist failure rides persistError.
      (status) => dispatchCloud({ type: "heavy", status }),
      (err) => console.debug("settings: set_cloud_heavy_provider unavailable:", err),
    );
  };

  const selectMcpMode = (mode: McpRunMode) => {
    setMcpRunMode(mode).then(
      // Never rejects backend-side; a persist failure rides lastError on the
      // returned McpHealthStatus (rolled back), the set_hid_run_mode contract.
      (status) => dispatchMcp({ type: "health", status }),
      (err) => console.debug("settings: set_mcp_run_mode unavailable:", err),
    );
  };

  // Persist the whole list on every add/remove — set_mcp_servers is the single
  // write path and returns the authoritative persisted list (or the previous
  // one plus persistError on failure), so the reducer stays backend-truthful.
  const persistServers = (servers: McpServerConfig[]) => {
    setMcpServers(servers).then(
      (status) => dispatchMcp({ type: "servers", status }),
      (err) => console.debug("settings: set_mcp_servers unavailable:", err),
    );
  };

  const openMcpJson = () => {
    setMcpJson({ open: true, draft: serversToJson(mcp.servers ?? []), error: null });
  };

  const closeMcpJson = () => {
    setMcpJson({ open: false, draft: "", error: null });
  };

  // Apply the JSON draft: validate, clear keychain tokens for any remote
  // server the edit removed (a dangling secret for a deleted server is a leak
  // — the same R018 property removeServer holds), then persist the whole list
  // through the single write path.
  const applyMcpJson = () => {
    const result = parseServersJson(mcpJson.draft);
    if ("error" in result) {
      setMcpJson((prev) => ({ ...prev, error: result.error }));
      return;
    }
    const nextIds = new Set(result.servers.map((s) => s.id));
    for (const server of mcp.servers ?? []) {
      if (server.transport === "http" && server.authRef && !nextIds.has(server.id)) {
        const authRef = server.authRef;
        deleteMcpAuth(authRef).then(
          (status) => setMcpAuthPresent((prev) => ({ ...prev, [authRef]: status.present })),
          (err) => console.debug("settings: delete_mcp_auth unavailable:", err),
        );
      }
    }
    persistServers(result.servers);
    closeMcpJson();
  };

  // Whether the current add-server draft is complete enough to persist: a stdio
  // server needs an id + command; an http server needs an id + url (the token is
  // optional — an unauthenticated remote server is valid).
  const draftReady = (() => {
    if (serverDraft.id.trim().length === 0) return false;
    return serverDraft.transport === "http"
      ? serverDraft.url.trim().length > 0
      : serverDraft.command.trim().length > 0;
  })();

  const addServer = () => {
    const id = serverDraft.id.trim();
    if (id.length === 0) return;
    const existing = mcp.servers ?? [];
    let entry: McpServerConfig;
    if (serverDraft.transport === "http") {
      const url = serverDraft.url.trim();
      if (url.length === 0) return;
      const token = serverDraft.token.trim();
      // The token (if any) rides the keychain, never settings.json (R018): the
      // config carries only the non-secret authRef account key. authRef is set
      // whenever a token is entered so the connect path knows where to read it.
      const authRef = token.length > 0 ? mcpAuthRef(id) : undefined;
      entry = { id, command: "", args: [], enabled: true, transport: "http", url, authRef };
      if (token.length > 0 && authRef) submitMcpAuth(authRef, token);
    } else {
      const command = serverDraft.command.trim();
      if (command.length === 0) return;
      // Split args on whitespace; empty tokens dropped. A blank args box → no args.
      const args =
        serverDraft.args.trim().length === 0 ? [] : serverDraft.args.trim().split(/\s+/);
      entry = { id, command, args, enabled: true, transport: "stdio" };
    }
    // Replace an entry with the same id rather than duplicating the key.
    const next = [...existing.filter((s) => s.id !== id), entry];
    persistServers(next);
    setServerDraft({ id: "", transport: "stdio", command: "", args: "", url: "", token: "" });
  };

  const removeServer = (id: string) => {
    const server = (mcp.servers ?? []).find((s) => s.id === id);
    // Removing an http server clears its stored bearer token too — a dangling
    // keychain secret for a server the user deleted is a leak (R018).
    if (server?.transport === "http" && server.authRef) {
      const authRef = server.authRef;
      deleteMcpAuth(authRef).then(
        (status) => setMcpAuthPresent((prev) => ({ ...prev, [authRef]: status.present })),
        (err) => console.debug("settings: delete_mcp_auth unavailable:", err),
      );
    }
    persistServers((mcp.servers ?? []).filter((s) => s.id !== id));
  };

  // Store a bearer token for a remote server's authRef — the one legitimate
  // inbound crossing of token material (R018). Presence comes straight back so
  // the row renders truth without a second query; a typed store failure rides
  // mcpAuthError (never silence). Clears the write-only draft on success.
  const submitMcpAuth = (authRef: string, token: string) => {
    setMcpAuth(authRef, token).then(
      (status) => {
        setMcpAuthPresent((prev) => ({ ...prev, [authRef]: status.present }));
        setMcpAuthError(null);
      },
      (err) => {
        if (isMcpAuthError(err)) setMcpAuthError(err);
        else console.debug("settings: set_mcp_auth unavailable:", err);
      },
    );
  };

  const submitServerToken = (id: string) => {
    const token = (mcpTokenDrafts[id] ?? "").trim();
    if (token.length === 0) return;
    submitMcpAuth(mcpAuthRef(id), token);
    setMcpTokenDrafts((prev) => ({ ...prev, [id]: "" }));
  };

  const removeServerToken = (id: string) => {
    const authRef = mcpAuthRef(id);
    deleteMcpAuth(authRef).then(
      (status) => {
        setMcpAuthPresent((prev) => ({ ...prev, [authRef]: status.present }));
        setMcpAuthError(null);
      },
      (err) => {
        if (isMcpAuthError(err)) setMcpAuthError(err);
        else console.debug("settings: delete_mcp_auth unavailable:", err);
      },
    );
  };

  const toggleServerEnabled = (id: string, enabled: boolean) => {
    persistServers((mcp.servers ?? []).map((s) => (s.id === id ? { ...s, enabled } : s)));
  };

  const selectPresentationMode = (mode: PresentationMode) => {
    setOverlayPresentation(mode).then(
      // Never rejects backend-side; a persist failure rides persistError and the
      // authoritative status (which the overlay webview applies) comes straight
      // back — this window never touches window geometry (the ACL split).
      (status) => setPresentation(status),
      (err) => console.debug("settings: set_overlay_presentation unavailable:", err),
    );
  };

  const submitExtent = (mode: PresentationMode, raw: string) => {
    const value = Number(raw);
    // Ignore an unparseable field; a valid-but-sub-min value is floored safe by
    // the backend, so it is passed through rather than clamped here.
    if (!Number.isFinite(value)) return;
    // Both axes carry the same value: the backend selects the active drawer
    // edge's relevant axis (width for left/right, height for top/bottom).
    setOverlayExtent(mode, value, value).then(
      (status) => setPresentation(status),
      (err) => console.debug("settings: set_overlay_extent unavailable:", err),
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
  // The active drawer edge (null in modal mode) drives whether the extent input
  // shows and which axis minimum it uses. Its stored extent seeds the input.
  const presentationEdge: Edge | null = presentation ? drawerEdgeOf(presentation) : null;
  const presentationExtent =
    presentation && presentationEdge ? drawerExtentFor(presentation, presentationEdge) : 0;

  // Drag the borderless window by its header (overlay drag-handle idiom).
  // Interactive children (close button) keep their clicks; outside a Tauri
  // runtime getCurrentWindow throws synchronously — absorbed.
  const startWindowDrag = (event: React.MouseEvent) => {
    if ((event.target as HTMLElement).closest("button, input, select, a")) return;
    try {
      getCurrentWindow()
        .startDragging()
        .catch((err) => console.debug("settings: drag no-op:", err));
    } catch (err) {
      console.debug("settings: drag unavailable:", err);
    }
  };

  return (
    <div className="settings-root">
      <div className="settings-panel">
        <header className="settings-header" onMouseDown={startWindowDrag}>
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

        <div className="settings-body">
        <nav className="settings-nav" aria-label="Settings sections">
          <input
            type="search"
            className="settings-nav-search"
            aria-label="Search settings"
            placeholder="Search settings"
            value={navFilter}
            onChange={(event) => setNavFilter(event.target.value)}
          />
          {filterGroups(SECTION_GROUPS, navFilter).map((group) => (
            <div key={group.title} className="settings-nav-group">
              <span className="settings-nav-group-title">{group.title}</span>
              {group.entries.map((entry) => (
                <button
                  key={entry.id}
                  type="button"
                  className="settings-nav-item"
                  data-section={entry.id}
                  aria-current={active === entry.id ? "page" : undefined}
                  onClick={() => setActive(entry.id)}
                >
                  {entry.label}
                </button>
              ))}
            </div>
          ))}
        </nav>
        <main className="settings-page">

        {active === "models" && (
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
          {state.endpoint ? (
            <>
              <div className="settings-row">
                <span className="settings-row-label">Endpoint</span>
                <input
                  type="text"
                  className="settings-key-input"
                  aria-label="Local model endpoint"
                  data-endpoint-input
                  autoComplete="off"
                  spellCheck={false}
                  placeholder={state.endpoint.fallback}
                  value={endpointDraft}
                  onChange={(event) => setEndpointDraft(event.target.value)}
                />
                <button
                  type="button"
                  className="settings-refresh"
                  data-endpoint-save
                  onClick={() => saveEndpoint(endpointDraft)}
                >
                  Save
                </button>
                <button
                  type="button"
                  className="settings-refresh"
                  data-endpoint-reset
                  disabled={state.endpoint.configured === null}
                  onClick={() => saveEndpoint(null)}
                >
                  Reset
                </button>
              </div>
              <p className="settings-hint">
                URL of an OpenAI-compatible server (LM Studio, Ollama, …). Save a blank
                field or press Reset to return to the default.
              </p>
              {state.endpoint.restartRequired && (
                <p className="settings-hint" data-endpoint-restart role="status">
                  Saved — restart Third Eye to apply. This run is still using{" "}
                  {state.endpoint.active}.
                </p>
              )}
              {!isLoopbackEndpoint(state.endpoint.configured ?? state.endpoint.fallback) && (
                <p className="settings-hint" data-endpoint-external>
                  This endpoint is not on this machine: the privacy guard treats it as
                  external — text is redacted and screenshots are never sent.
                </p>
              )}
            </>
          ) : (
            <p className="settings-unavailable">
              Endpoint configuration is unavailable outside the app.
            </p>
          )}
          {state.endpointError && (
            <div className="settings-error" role="alert" data-endpoint-error>
              <strong>Endpoint change rejected</strong>
              <span>{state.endpointError}</span>
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
                {health.find((h) => h.lane === lane.name)?.warning && (
                  <span className="settings-lane-warning" data-lane-warning={lane.name} role="alert">
                    ⚠ {health.find((h) => h.lane === lane.name)?.warning}
                  </span>
                )}
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
          {modelRows.length > 0 && (
            <>
              <h3 className="guard-subheading">Served models</h3>
              <ul className="settings-approved-kinds" data-model-rows>
                {modelRows.map((row) => (
                  <li key={row.id}>
                    <span>
                      {row.id}
                      <span className="settings-model-badges">
                        {" "}
                        [{row.state || "unknown"}]
                        {row.toolUse ? " [tools ✓]" : " [⚠ no tool support]"}
                        {row.quantization ? ` ${row.quantization}` : ""}
                        {row.maxContextLength
                          ? ` · ${Math.round(row.maxContextLength / 1024)}k ctx`
                          : ""}
                      </span>
                    </span>
                  </li>
                ))}
              </ul>
              <p className="settings-hint">
                From LM Studio's native API. A model without tool support cannot
                drive Third Eye's tools — every chat with it will fail.
              </p>
            </>
          )}
          {verbose !== null && (
            <>
              <label className="settings-row">
                <span className="settings-row-label">Verbose status</span>
                <input
                  type="checkbox"
                  data-verbose-toggle
                  checked={verbose}
                  onChange={(event) => {
                    const enable = event.target.checked;
                    setVerbose(enable);
                    setVerboseStatus(enable).then(
                      (status) => setVerbose(status.enabled),
                      (err) =>
                        console.debug("settings: set_verbose_status unavailable:", err),
                    );
                  }}
                />
              </label>
              <p className="settings-hint">
                Off: plain feedback while the model works ("Loading the model…",
                "Reading your request…"). On: detailed telemetry — model id, wait
                timers, and LM Studio's raw load state.
              </p>
            </>
          )}
        </section>
        )}

        {active === "privacy" && (
        <section className="settings-section" aria-labelledby="settings-privacy-heading">
          <h2 id="settings-privacy-heading" className="settings-section-title">
            Privacy
          </h2>
          <label className="settings-row">
            <span className="settings-row-label">Privacy Mode</span>
            <Toggle
              ariaLabel="Privacy Mode"
              disabled={state.privacy === null}
              on={state.privacy?.enabled ?? false}
              onChange={(next) => togglePrivacy(next)}
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
        )}

        {active === "input" && (
        <section className="settings-section" aria-labelledby="settings-hid-heading">
          <h2 id="settings-hid-heading" className="settings-section-title">
            Input Control
          </h2>
          <label className="settings-row">
            <span className="settings-row-label">Input Control (HID)</span>
            <select
              className="settings-select"
              aria-label="Input Control mode"
              data-hid-mode={state.hid?.mode ?? "off"}
              // Inert when state hasn't loaded (outside the app) or the
              // platform has no HID backend (FallbackInput, supported=false).
              disabled={state.hid === null || !state.hid.permission.supported}
              value={state.hid?.mode ?? "off"}
              onChange={(event) => selectHidMode(event.target.value as HidRunMode)}
            >
              {HID_RUN_MODE_OPTIONS.map((option) => (
                <option key={option.value} value={option.value}>
                  {option.label}
                </option>
              ))}
            </select>
          </label>
          <p className="settings-hint">
            Off by default. When armed, Third Eye can move the pointer, click,
            and type on your behalf. Ask prompts before each action; Auto-run
            performs them without asking. Needs macOS Accessibility permission;
            switching to Off reverts to fully inert.
          </p>
          <label className="settings-row">
            <span className="settings-row-label">Terminal commands</span>
            <Toggle
              ariaLabel="Terminal commands"
              disabled={commands === null}
              on={commands?.enabled ?? false}
              onChange={(next) => toggleCommands(next)}
            />
          </label>
          <p className="settings-hint">
            Off by default. When on, Third Eye can run shell commands for you —
            every command is shown and asked about before it runs, and its
            output appears in the chat.
          </p>
          {commands?.error && (
            <div className="settings-error" role="alert">
              <strong>Terminal commands couldn't be saved</strong>
              <span>{commands.error}</span>
            </div>
          )}
          {commands !== null && (
            <div className="allowlist-editor">
              <span className="settings-row-label">Always-allowed commands</span>
              <p className="settings-hint">
                Commands matching an entry run without asking — an entry covers
                itself and anything after it separated by a space ("ls" covers
                "ls -la", never "lsof"). Every run still shows in the chat and
                the on-screen HUD.
              </p>
              {commands.allowlist.map((entry) => (
                <div key={entry} className="allowlist-row">
                  <code className="allowlist-entry">{entry}</code>
                  {allowlistEntryAlwaysAsks(entry, commands.dangerousVerbs) && (
                    <span
                      className="allowlist-danger"
                      data-allowlist-danger="true"
                      title="Dangerous verb: Third Eye always asks before running this, allowlisted or not"
                    >
                      ⚠ always asks
                    </span>
                  )}
                  <button
                    type="button"
                    className="memory-delete"
                    aria-label={`Remove ${entry} from the allowlist`}
                    onClick={() =>
                      updateAllowlist(commands.allowlist.filter((e) => e !== entry))
                    }
                  >
                    ✕
                  </button>
                </div>
              ))}
              <form
                className="allowlist-add"
                onSubmit={(event) => {
                  event.preventDefault();
                  addAllowlistEntry();
                }}
              >
                <input
                  className="settings-nav-search"
                  type="text"
                  placeholder='Add a command, e.g. "date" or "curl -s ifconfig.me"'
                  aria-label="Add allowed command"
                  value={allowlistDraft}
                  onChange={(event) => setAllowlistDraft(event.target.value)}
                />
                <button type="button" className="chat-retry" onClick={addAllowlistEntry}>
                  Add
                </button>
              </form>
            </div>
          )}
          {state.hid && hidModeShowsAutoRunWarning(state.hid.mode) && (
            // Auto-run performs every HID action without a prompt — the most
            // dangerous posture, so it is called out explicitly (R007).
            <div className="settings-warning" role="alert" data-hid-autorun-warning>
              <strong>Auto-run dangerously allows all input</strong>
              <span>
                Third Eye will click and type on your behalf with no prompt for
                each action. Only use this for a task you are actively watching.
              </span>
            </div>
          )}
          {state.hid === null && (
            <p className="settings-unavailable">
              Input Control state is unavailable outside the app.
            </p>
          )}
          {state.hid && !state.hid.permission.supported && (
            <p className="settings-unavailable">
              Input Control isn't supported on this platform.
            </p>
          )}
          {state.hid &&
            state.hid.permission.supported &&
            !state.hid.permission.granted && (
              // R007: guidance, never silence. Shown whenever Accessibility is
              // ungranted — arming is refused until the user grants it.
              <div className="capture-walkthrough" role="alert">
                <strong>Accessibility permission needed</strong>
                <ol className="capture-walkthrough-steps">
                  <li>
                    Open System Settings below — it lands on Privacy &amp;
                    Security → Accessibility.
                  </li>
                  <li>Turn on Third Eye in the list (macOS may ask to relaunch the app).</li>
                  <li>Come back and switch Input Control on.</li>
                </ol>
                <div className="capture-walkthrough-actions">
                  <button
                    type="button"
                    className="chat-retry"
                    onClick={openAccessibilitySettings}
                  >
                    Open System Settings
                  </button>
                </div>
              </div>
            )}
          {state.hid?.error && state.hid.error.kind !== "permission-denied" && (
            // permission-denied is rendered as the walkthrough above; any other
            // typed failure (persist, unsupported) surfaces as a banner.
            <div className="settings-error" role="alert">
              <strong>{hidErrorTitle(state.hid.error)}</strong>
              <span>{state.hid.error.detail}</span>
            </div>
          )}
          {approvedKinds !== null && approvedKinds.length > 0 && (
            <div className="settings-row settings-row--stack">
              <span className="settings-row-label">
                Always allowed
                <span className="settings-row-sub">
                  Granted with “Always” on an approval — never prompts again.
                  Remove one to make it ask next time.
                </span>
              </span>
              <ul className="settings-approved-kinds">
                {approvedKinds.map((kind) => (
                  <li key={kind}>
                    <span>{kind.replaceAll("-", " ")}</span>
                    <button
                      type="button"
                      aria-label={`Remove permanent approval for ${kind}`}
                      onClick={() =>
                        removeApprovedActionKind(kind).then(
                          (kinds) => setApprovedKinds(kinds),
                          (err) =>
                            console.debug("settings: remove_approved_action_kind failed:", err),
                        )
                      }
                    >
                      ✕
                    </button>
                  </li>
                ))}
              </ul>
            </div>
          )}
        </section>
        )}

        {active === "watcher" && (
        <section className="settings-section" aria-labelledby="settings-watcher-heading">
          <h2 id="settings-watcher-heading" className="settings-section-title">
            Watch Screen
          </h2>
          <label className="settings-row">
            <span className="settings-row-label">Watch Screen</span>
            <Toggle
              ariaLabel="Watch Screen"
              disabled={watcher.status === null}
              on={watcher.status?.enabled ?? false}
              onChange={(next) => toggleWatcher(next)}
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
        )}

        {active === "overlay" && (
        <section className="settings-section" aria-labelledby="settings-overlay-heading">
          <h2 id="settings-overlay-heading" className="settings-section-title">
            Overlay
          </h2>
          <label className="settings-row">
            <span className="settings-row-label">Presentation</span>
            <select
              className="settings-select"
              aria-label="Overlay presentation mode"
              data-overlay-mode={presentation?.mode ?? "modal"}
              // Inert until the snapshot loads (outside the app the invoke
              // rejects and presentation stays null).
              disabled={presentation === null}
              value={presentation?.mode ?? "modal"}
              onChange={(event) =>
                selectPresentationMode(event.target.value as PresentationMode)
              }
            >
              {PRESENTATION_MODE_OPTIONS.map((option) => (
                <option key={option.value} value={option.value}>
                  {option.label}
                </option>
              ))}
            </select>
          </label>
          <p className="settings-hint">
            Modal floats free; a drawer docks flush to a screen edge. The overlay
            adopts the change immediately and restores the same shape after quit
            and relaunch.
          </p>
          {presentation === null && (
            <p className="settings-unavailable">
              Overlay presentation is unavailable outside the app.
            </p>
          )}
          {presentation && presentationEdge && (
            <>
              <label className="settings-row">
                <span className="settings-row-label">Drawer size (px)</span>
                <input
                  type="number"
                  className="settings-select"
                  aria-label="Drawer extent"
                  data-overlay-extent
                  min={extentMinFor(presentationEdge)}
                  step={10}
                  // Remounts (re-seeding defaultValue) whenever the authoritative
                  // extent changes — the input never carries drifting local state;
                  // the broadcast/response is the source of truth (MEM082/MEM027).
                  key={`${presentationEdge}-${presentationExtent}`}
                  defaultValue={presentationExtent}
                  onBlur={(event) => submitExtent(presentation.mode, event.target.value)}
                  onKeyDown={(event) => {
                    if (event.key === "Enter") event.currentTarget.blur();
                  }}
                />
              </label>
              <p className="settings-hint">
                Width for a left/right drawer, height for top/bottom. Values below
                the overlay minimum are floored to a sane on-screen size.
              </p>
            </>
          )}
          {presentation && !presentationEdge && (
            <p className="settings-hint">
              Modal size follows the overlay window — drag its corner grip to
              resize.
            </p>
          )}
          {presentation?.persistError && (
            <div className="settings-error" role="alert">
              <strong>Overlay presentation couldn't be saved</strong>
              <span>{presentation.persistError}</span>
            </div>
          )}
        </section>
        )}

        {active === "nudges" && (
        <section className="settings-section" aria-labelledby="settings-nudges-heading">
          <h2 id="settings-nudges-heading" className="settings-section-title">
            Nudges
          </h2>
          <label className="settings-row">
            <span className="settings-row-label">Nudges</span>
            <Toggle
              ariaLabel="Nudges"
              disabled={nudges === null}
              on={nudges?.enabled ?? false}
              onChange={(next) => toggleNudges(next)}
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
          {nudges !== null && (
            <>
              <div className="settings-row settings-row--stack">
                <span className="settings-row-label">At most one nudge every</span>
                <ChoiceChips
                  label="At most one nudge every"
                  options={NUDGE_COOLDOWN_OPTIONS}
                  value={String(nudges.cooldownSecs)}
                  onChange={chooseNudgeCooldown}
                />
              </div>
              <div className="settings-row settings-row--stack">
                <span className="settings-row-label">Banner stays for</span>
                <ChoiceChips
                  label="Banner stays for"
                  options={NUDGE_DISMISS_OPTIONS}
                  value={String(nudges.autoDismissSecs)}
                  onChange={chooseNudgeAutoDismiss}
                />
              </div>
            </>
          )}
          {nudgeLog !== null && nudgeLog.length > 0 && (
            <div className="settings-row settings-row--stack">
              <span className="settings-row-label">Recent nudges</span>
              <ul className="settings-nudge-log">
                {nudgeLog.map((entry) => (
                  <li key={entry.shownAtMs} className="settings-nudge-log-item">
                    <span className="settings-nudge-log-message">{entry.message}</span>
                    <span className="settings-nudge-log-meta">
                      {new Date(entry.shownAtMs).toLocaleString()}
                      {entry.appContext ? ` · ${entry.appContext}` : ""}
                      {entry.dismissReason ? ` · ${entry.dismissReason}` : " · showing"}
                    </span>
                  </li>
                ))}
              </ul>
            </div>
          )}
        </section>
        )}


        {active === "integrations" && (
        <section className="settings-section" aria-labelledby="settings-integrations-heading">
          <h2 id="settings-integrations-heading" className="settings-section-title">
            Integrations
          </h2>
          <p className="settings-hint">
            Optional ways into Third Eye from the rest of macOS. Everything here
            is installed and removed only from this pane, touching exactly the
            paths shown.
          </p>
          {integrations === null && (
            <p className="settings-unavailable">
              Integration state is unavailable outside the app.
            </p>
          )}
          {integrations?.error && (
            <div className="settings-error" role="alert">
              <strong>Integration change failed</strong>
              <span>{integrations.error}</span>
            </div>
          )}
          {chromeJs !== null && (
            <>
              <h3 className="guard-subheading">Chrome pages</h3>
              <div className="settings-row">
                <span className="settings-row-label">Page scripting</span>
                <span data-chrome-js={chromeJs.jsEnabled === null ? "unknown" : chromeJs.jsEnabled ? "on" : "off"}>
                  {chromeJs.jsEnabled ? "on ✓" : chromeJs.jsEnabled === false ? "off" : "unknown"} — {chromeJs.detail}
                </span>
                <button type="button" className="settings-refresh" onClick={recheckChromeJs}>
                  Recheck
                </button>
              </div>
            </>
          )}
          {integrations !== null && (
            <>
              <h3 className="guard-subheading">Terminal command</h3>
              <div className="settings-row">
                <span className="settings-row-label">thirdeye CLI</span>
                <span data-cli-state>
                  {integrations.cliInstalled ?? "not installed"}
                </span>
                {integrations.cliInstalled ? (
                  <button
                    type="button"
                    className="settings-refresh"
                    onClick={() => removeCli().then(setIntegrations, () => undefined)}
                  >
                    Remove
                  </button>
                ) : (
                  <button
                    type="button"
                    className="settings-refresh"
                    disabled={integrations.cliBundled === null}
                    onClick={() => installCli().then(setIntegrations, () => undefined)}
                  >
                    Install
                  </button>
                )}
              </div>
              <p className="settings-hint">
                `thirdeye .` adds the current folder as a workspace and opens the
                overlay; `thirdeye ask "…"` streams an answer in the terminal;
                `thirdeye tui` is a terminal chat. If it lands in ~/.local/bin,
                make sure that's on your PATH.
                {integrations.cliBundled === null &&
                  " (This build has no bundled CLI — install-app builds do.)"}
              </p>
              <h3 className="guard-subheading">Finder</h3>
              <div className="settings-row">
                <span className="settings-row-label">Quick Action</span>
                <span data-finder-state>
                  {integrations.finderInstalled ? "installed" : "not installed"}
                </span>
                {integrations.finderInstalled ? (
                  <button
                    type="button"
                    className="settings-refresh"
                    onClick={() => removeFinderAction().then(setIntegrations, () => undefined)}
                  >
                    Remove
                  </button>
                ) : (
                  <button
                    type="button"
                    className="settings-refresh"
                    onClick={() => installFinderAction().then(setIntegrations, () => undefined)}
                  >
                    Install
                  </button>
                )}
              </div>
              <p className="settings-hint">
                Adds "Work here with Third Eye" to a folder's right-click Quick
                Actions in Finder: it opens the overlay with that folder as a
                workspace. Uses the CLI, so install that first.
              </p>
            </>
          )}
        </section>
        )}

        {active === "workspaces" && (
        <section className="settings-section" aria-labelledby="settings-workspaces-heading">
          <h2 id="settings-workspaces-heading" className="settings-section-title">
            Workspaces
          </h2>
          <p className="settings-hint">
            The agent's working directories — the first is the ACTIVE one
            (relative paths, default cwd). The agent can work anywhere, but
            writes and commands outside directories you've approved prompt
            you each time (tmp never prompts); with none set it asks you to
            pick a folder when needed.
          </p>
          {workspaces === null && (
            <p className="settings-unavailable">
              Workspace state is unavailable outside the app.
            </p>
          )}
          {workspaces?.persistError && (
            <div className="settings-error" role="alert">
              <strong>Workspaces couldn't be saved</strong>
              <span>{workspaces.persistError}</span>
            </div>
          )}
          {workspaces !== null && (
            <>
              <ul className="settings-approved-kinds">
                {workspaces.roots.map((root) => (
                  <li key={root}>
                    <span>{root}</span>
                    <button
                      type="button"
                      aria-label={`Remove workspace ${root}`}
                      onClick={() =>
                        setWorkspaceRoots(workspaces.roots.filter((r) => r !== root)).then(
                          (status) => setWorkspaces(status),
                          (err) =>
                            console.debug("settings: set_workspace_roots failed:", err),
                        )
                      }
                    >
                      ✕
                    </button>
                  </li>
                ))}
              </ul>
              <form
                className="settings-workspace-add"
                onSubmit={(event) => {
                  event.preventDefault();
                  const root = newRoot.trim();
                  if (!root) return;
                  setWorkspaceRoots([...workspaces.roots, root]).then(
                    (status) => {
                      setWorkspaces(status);
                      setNewRoot("");
                    },
                    (err) => console.debug("settings: set_workspace_roots failed:", err),
                  );
                }}
              >
                <input
                  type="text"
                  placeholder="/Users/you/code/project (absolute path)"
                  aria-label="Add workspace folder"
                  value={newRoot}
                  onChange={(event) => setNewRoot(event.target.value)}
                />
                <button type="submit">Add</button>
              </form>
            </>
          )}
          {bridge !== null && (
            <>
              <h3 className="guard-subheading">VS Code</h3>
              <div className="settings-row">
                <span className="settings-row-label">Bridge</span>
                <span data-bridge-state>
                  {bridge.running
                    ? `listening on 127.0.0.1:${bridge.port} · ${bridge.connected} connected`
                    : "off"}
                </span>
              </div>
              <div className="settings-row">
                <span className="settings-row-label">VS Code</span>
                <span data-vscode-detected>
                  {bridge.vscodeDetected ? "detected" : "not detected"}
                </span>
              </div>
              <p className="settings-hint">
                The Third Eye extension shows files, diffs, and run status live
                in VS Code while the agent codes. Build and install it with
                `make vsix` then `code --install-extension` — it finds this app
                automatically via the bridge.
              </p>
            </>
          )}
        </section>
        )}

        {active === "tools" && (
        <section className="settings-section" aria-labelledby="settings-tools-heading">
          <h2 id="settings-tools-heading" className="settings-section-title">
            Tools
          </h2>
          <p className="settings-hint">
            Every built-in tool the assistant can use. A tool turned off here
            is never offered to the model and refuses to run — some tools also
            have their own gates (Input Control's HID mode, the Terminal
            commands switch), and both must be on.
          </p>
          {toolToggles === null && (
            <p className="settings-unavailable">
              Tool state is unavailable outside the app.
            </p>
          )}
          {toolToggles?.persistError && (
            <div className="settings-error" role="alert">
              <strong>Tool setting couldn't be saved</strong>
              <span>{toolToggles.persistError}</span>
            </div>
          )}
          {toolToggles?.tools.map((tool) => (
            <label className="settings-row" key={tool.name}>
              <span className="settings-row-label">
                {tool.label}
                <span className="settings-row-sub">{tool.description}</span>
              </span>
              <Toggle
                ariaLabel={`${tool.label} tool`}
                on={tool.enabled}
                onChange={(next) => toggleTool(tool.name, next)}
              />
            </label>
          ))}
        </section>
        )}

        {active === "cloud" && (
        <section className="settings-section" aria-labelledby="settings-cloud-heading">
          <h2 id="settings-cloud-heading" className="settings-section-title">
            Cloud Providers
          </h2>
          <label className="settings-row">
            <span className="settings-row-label">Use cloud providers</span>
            <Toggle
              ariaLabel="Use cloud providers"
              disabled={cloud.optin === null}
              on={cloud.optin?.enabled ?? false}
              onChange={(next) => toggleCloudOptin(next)}
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
                Which provider the heavy lane targets while cloud is on;
                none keeps it on the local model.
              </p>
              {cloud.heavy?.persistError && (
                <div className="settings-error" role="alert">
                  <strong>Provider choice couldn't be saved</strong>
                  <span>{cloud.heavy.persistError}</span>
                </div>
              )}
              <label className="settings-row">
                <span className="settings-row-label">Coder lane provider</span>
                <select
                  className="settings-select"
                  aria-label="Coder lane cloud provider"
                  data-cloud-coder-provider
                  value={cloud.coder?.provider ?? DEFAULT_OPTION}
                  onChange={(event) =>
                    selectCoderProvider(
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
                Which provider the coder lane targets while cloud is on;
                none keeps coding on the local model.
              </p>
              {cloud.coder?.persistError && (
                <div className="settings-error" role="alert">
                  <strong>Provider choice couldn't be saved</strong>
                  <span>{cloud.coder.persistError}</span>
                </div>
              )}
            </>
          )}
        </section>
        )}

        {active === "mcp" && (
        <section className="settings-section" aria-labelledby="settings-mcp-heading">
          <div className="settings-section-header">
            <h2 id="settings-mcp-heading" className="settings-section-title">
              MCP Servers
            </h2>
            <button
              type="button"
              className="settings-refresh"
              data-mcp-json-edit
              // No authoritative list to edit outside the app — the button
              // stays inert rather than seeding an editor from a null list.
              disabled={mcp.servers === null}
              onClick={mcpJson.open ? closeMcpJson : openMcpJson}
            >
              {mcpJson.open ? "Back to list" : "Edit as JSON"}
            </button>
          </div>
          <label className="settings-row">
            <span className="settings-row-label">External tools mode</span>
            <select
              className="settings-select"
              aria-label="External MCP tools mode"
              data-mcp-mode={mcp.health?.mode ?? "off"}
              // Inert until the mount-time mcp_status resolves (outside the app
              // it stays null and the section shows its unavailable note).
              disabled={mcp.health === null}
              value={mcp.health?.mode ?? "off"}
              onChange={(event) => selectMcpMode(event.target.value as McpRunMode)}
            >
              {MCP_RUN_MODE_OPTIONS.map((option) => (
                <option key={option.value} value={option.value}>
                  {option.label}
                </option>
              ))}
            </select>
          </label>
          <p className="settings-hint">
            Off by default. When on, Third Eye can call tools from external MCP
            servers you add below. Ask prompts before each new tool; Auto-run runs
            them without asking. Servers are spawned at startup — add or change one
            here and restart to apply.
          </p>
          {mcp.health && mcpModeShowsAutoRunWarning(mcp.health.mode) && (
            // Auto-run runs every external tool without a prompt — the most
            // permissive posture, called out explicitly (R007).
            <div className="settings-warning" role="alert" data-mcp-autorun-warning>
              <strong>Auto-run runs every external tool without asking</strong>
              <span>
                Third Eye will call any tool an enabled server advertises with no
                prompt. Only use this with servers you trust.
              </span>
            </div>
          )}
          {mcp.health && (
            <div className="settings-status-row">
              <span className="settings-row-label">Status</span>
              <span className="settings-status-value" data-mcp-phase={mcp.health.phase}>
                {mcpHealthLine(mcp.health)}
              </span>
            </div>
          )}
          {mcp.health === null && (
            <p className="settings-unavailable">
              MCP state is unavailable outside the app.
            </p>
          )}
          {mcpJson.open ? (
            <>
              <textarea
                className="settings-json-input"
                aria-label="MCP servers JSON"
                data-mcp-json-input
                spellCheck={false}
                value={mcpJson.draft}
                onChange={(event) =>
                  setMcpJson((prev) => ({ ...prev, draft: event.target.value }))
                }
              />
              {mcpJson.error && (
                <div className="settings-error" role="alert" data-mcp-json-error>
                  <strong>JSON rejected</strong>
                  <span>{mcpJson.error.detail}</span>
                </div>
              )}
              <div className="settings-row settings-json-actions">
                <button
                  type="button"
                  className="settings-refresh"
                  data-mcp-json-apply
                  onClick={applyMcpJson}
                >
                  Apply
                </button>
                <button
                  type="button"
                  className="settings-refresh"
                  data-mcp-json-cancel
                  onClick={closeMcpJson}
                >
                  Cancel
                </button>
              </div>
              <p className="settings-hint">
                This is the exact list persisted in settings.json. Bearer tokens
                never appear here — a remote server carries only its keychain
                reference (authRef). Apply validates before saving; removing a
                remote server also clears its stored token.
              </p>
            </>
          ) : (
            <>
          {mcp.servers?.map((server) => (
            <div
              key={server.id}
              className="cloud-provider"
              data-mcp-server={server.id}
              data-mcp-server-transport={server.transport}
            >
              <div className="settings-status-row">
                <span className="settings-row-label">{server.id}</span>
                <span className="settings-status-value">
                  {server.transport === "http"
                    ? (server.url ?? "")
                    : `${server.command}${server.args.length > 0 ? ` ${server.args.join(" ")}` : ""}`}
                </span>
              </div>
              {server.transport === "http" && (
                // Write-only bearer token, presence-shown — the MCP twin of the
                // cloud key row (R018): the token crosses only inbound to the
                // keychain, and the field only ever reflects "Stored / Not stored".
                <div className="settings-row">
                  <span
                    className="settings-status-value"
                    data-mcp-auth-present={mcpAuthPresent[mcpAuthRef(server.id)] ? "true" : "false"}
                  >
                    {mcpAuthPresent[mcpAuthRef(server.id)] ? "Token stored" : "No token"}
                  </span>
                  <input
                    type="password"
                    className="settings-key-input"
                    aria-label={`Bearer token for ${server.id}`}
                    data-mcp-server-token={server.id}
                    placeholder={
                      mcpAuthPresent[mcpAuthRef(server.id)] ? "Replace token" : "Paste bearer token"
                    }
                    value={mcpTokenDrafts[server.id] ?? ""}
                    onChange={(event) =>
                      setMcpTokenDrafts((prev) => ({ ...prev, [server.id]: event.target.value }))
                    }
                  />
                  <button
                    type="button"
                    className="settings-refresh"
                    data-mcp-server-token-save={server.id}
                    disabled={(mcpTokenDrafts[server.id] ?? "").trim().length === 0}
                    onClick={() => submitServerToken(server.id)}
                  >
                    Save
                  </button>
                  {mcpAuthPresent[mcpAuthRef(server.id)] && (
                    <button
                      type="button"
                      className="memory-delete"
                      data-mcp-server-token-delete={server.id}
                      aria-label={`Remove token for ${server.id}`}
                      onClick={() => removeServerToken(server.id)}
                    >
                      Remove token
                    </button>
                  )}
                </div>
              )}
              <div className="settings-row">
                <span className="settings-row-label">
                  <Toggle
                    ariaLabel={`Enable ${server.id}`}
                    data-mcp-server-enabled={server.id}
                    on={server.enabled}
                    onChange={(next) => toggleServerEnabled(server.id, next)}
                  />
                  Enabled
                </span>
                <button
                  type="button"
                  className="memory-delete"
                  data-mcp-server-delete={server.id}
                  aria-label={`Remove ${server.id}`}
                  onClick={() => removeServer(server.id)}
                >
                  Remove
                </button>
              </div>
            </div>
          ))}
          {mcpAuthError && (
            <div className="settings-error" role="alert" data-mcp-auth-error>
              <strong>Token couldn't be saved</strong>
              <span>{mcpAuthError.detail}</span>
            </div>
          )}
          <div className="settings-row">
            <input
              type="text"
              className="settings-key-input"
              aria-label="Server id"
              data-mcp-draft-id
              placeholder="id (e.g. weather)"
              value={serverDraft.id}
              onChange={(event) =>
                setServerDraft((draft) => ({ ...draft, id: event.target.value }))
              }
            />
            <select
              className="settings-select"
              aria-label="Server transport"
              data-mcp-draft-transport={serverDraft.transport}
              value={serverDraft.transport}
              onChange={(event) =>
                setServerDraft((draft) => ({
                  ...draft,
                  transport: event.target.value as McpTransport,
                }))
              }
            >
              {MCP_TRANSPORT_OPTIONS.map((option) => (
                <option key={option.value} value={option.value}>
                  {option.label}
                </option>
              ))}
            </select>
            {serverDraft.transport === "http" ? (
              <>
                <input
                  type="text"
                  className="settings-key-input"
                  aria-label="Server URL"
                  data-mcp-draft-url
                  placeholder="url (e.g. https://mcp.example.com/sse)"
                  value={serverDraft.url}
                  onChange={(event) =>
                    setServerDraft((draft) => ({ ...draft, url: event.target.value }))
                  }
                />
                <input
                  type="password"
                  className="settings-key-input"
                  aria-label="Server bearer token"
                  data-mcp-draft-token
                  placeholder="bearer token (optional)"
                  value={serverDraft.token}
                  onChange={(event) =>
                    setServerDraft((draft) => ({ ...draft, token: event.target.value }))
                  }
                />
              </>
            ) : (
              <>
                <input
                  type="text"
                  className="settings-key-input"
                  aria-label="Server command"
                  data-mcp-draft-command
                  placeholder="command (e.g. npx)"
                  value={serverDraft.command}
                  onChange={(event) =>
                    setServerDraft((draft) => ({ ...draft, command: event.target.value }))
                  }
                />
                <input
                  type="text"
                  className="settings-key-input"
                  aria-label="Server args"
                  data-mcp-draft-args
                  placeholder="args (space-separated)"
                  value={serverDraft.args}
                  onChange={(event) =>
                    setServerDraft((draft) => ({ ...draft, args: event.target.value }))
                  }
                />
              </>
            )}
            <button
              type="button"
              className="settings-refresh"
              data-mcp-server-add
              disabled={mcp.servers === null || !draftReady}
              onClick={addServer}
            >
              Add
            </button>
          </div>
          <p className="settings-hint">
            Remote (HTTP/SSE) servers connect to a URL with an optional bearer
            token stored in your OS keychain — never in settings.json.
          </p>
            </>
          )}
          {mcp.persistError && (
            <div className="settings-error" role="alert">
              <strong>Server list couldn't be saved</strong>
              <span>{mcp.persistError}</span>
            </div>
          )}
        </section>
        )}

        {active === "memory" && (
        <section className="settings-section" aria-labelledby="settings-memory-heading">
          <h2 id="settings-memory-heading" className="settings-section-title">
            Memory
          </h2>
          <label className="settings-row">
            <span className="settings-row-label">Chat memory</span>
            <Toggle
              ariaLabel="Chat memory"
              disabled={memory.status === null && memory.chatMemory === null}
              on={
                // A set response is fresher than the mount snapshot, so it wins.
                memory.chatMemory?.enabled ?? memory.status?.chatIngest.enabled ?? false
              }
              onChange={(next) => toggleChatMemory(next)}
            />
          </label>
          {memory.chatMemory?.error && (
            <div className="settings-error" role="alert">
              <strong>Chat memory couldn't be saved</strong>
              <span>{memory.chatMemory.error}</span>
            </div>
          )}
          <div className="settings-row settings-row--stack">
            <span className="settings-row-label">Keep memory for</span>
            <ChoiceChips
              label="Keep memory for"
              options={RETENTION_OPTIONS}
              value={retention}
              onChange={chooseRetention}
            />
          </div>
          <p className="settings-hint">
            Enforced daily: un-pinned memories older than this window are
            pruned. Pinned memories and per-memory expiries (set in the
            memory window) are unaffected.
          </p>
          {retentionError && (
            <div className="settings-error" role="alert">
              <strong>Retention couldn't be saved</strong>
              <span>{retentionError}</span>
            </div>
          )}
          {memory.status && (
            <p className="settings-hint">
              Chat memories stored: {memory.status.chatIngest.ingestedCount}
              {memory.status.chatIngest.buffered > 0
                ? ` · awaiting retry: ${memory.status.chatIngest.buffered}`
                : ""}
              {memory.status.chatIngest.lastError && (
                <span className="memory-edit-error">
                  {" "}
                  · last error: {memory.status.chatIngest.lastError.kind}
                </span>
              )}
            </p>
          )}
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
                        {sourceLabel(r.source)} · {appsLabel(r.apps)} ·{" "}
                        {spanLabel(r.spanStartMs, r.spanEndMs)}
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
        )}

        {active === "programs" && (
        <section className="settings-section" aria-labelledby="settings-programs-heading">
          <h2 id="settings-programs-heading" className="settings-section-title">
            Programs
          </h2>
          <p className="settings-hint">
            What Third Eye knows is installed on this machine — GUI apps and
            terminal tools, scanned daily. The assistant checks this cache
            before claiming something is (or isn't) installed.
          </p>
          {inventory === null ? (
            <p className="settings-unavailable">
              Inventory is unavailable outside the app.
            </p>
          ) : (
            <>
              <div className="settings-row">
                <span className="settings-row-label">
                  {inventory.apps} apps · {inventory.tools} terminal tools
                  {inventory.lastRefreshMs !== null && (
                    <span className="settings-hint-inline">
                      {" "}
                      · scanned {new Date(inventory.lastRefreshMs).toLocaleString()}
                    </span>
                  )}
                </span>
                <button
                  type="button"
                  className="chat-retry"
                  disabled={inventoryBusy}
                  onClick={rescanInventory}
                >
                  {inventoryBusy ? "Scanning…" : "Rescan now"}
                </button>
              </div>
              {inventory.error && (
                <div className="settings-error" role="alert">
                  <span>{inventory.error}</span>
                </div>
              )}
              <input
                className="settings-nav-search"
                type="text"
                placeholder="Search installed programs…"
                aria-label="Search installed programs"
                value={programQuery}
                onChange={(event) => searchPrograms(event.target.value)}
              />
              {programs !== null &&
                (programs.length === 0 ? (
                  <p className="settings-hint">No installed program matches.</p>
                ) : (
                  <div className="programs-list">
                    {programs.map((entry) => (
                      <div key={entry.path} className="programs-row">
                        <span
                          className="programs-kind"
                          data-kind={entry.kind}
                        >
                          {entry.kind === "app" ? "app" : "cli"}
                        </span>
                        <span className="programs-name">{entry.name}</span>
                        <span className="programs-path" title={entry.path}>
                          {entry.path}
                        </span>
                      </div>
                    ))}
                  </div>
                ))}
            </>
          )}
        </section>
        )}

                {active === "status" && (
        <section className="settings-section" aria-labelledby="settings-status-heading">
          <h2 id="settings-status-heading" className="settings-section-title">
            Status
          </h2>
          {build !== null && (
            <div className="settings-status-row">
              <span className="settings-row-label">Build</span>
              <span className="settings-status-value">
                v{build.version} · {build.gitHash} ·{" "}
                {new Date(build.builtAtMs).toLocaleString()}
              </span>
            </div>
          )}
          {log && (
            <div className="settings-status-row">
              <span className="settings-row-label">Log</span>
              <span className="settings-status-value" data-log-path>
                <code title={log}>{log.replace(/^\/Users\/[^/]+/, "~")}</code>{" "}
                <button
                  type="button"
                  className="chat-retry"
                  onClick={() => revealLog().catch(() => undefined)}
                >
                  Reveal
                </button>
              </span>
            </div>
          )}
          <div className="settings-status-row">
            <span className="settings-row-label">Hotkey</span>
            <span className="settings-status-value">
              {state.hotkey
                ? `${state.hotkey.shortcut} — ${state.hotkey.registered ? "registered" : "not registered"}`
                : "unavailable"}
            </span>
          </div>
          {state.hotkey && (
            <>
              <div className="settings-row">
                <span className="settings-row-label">Change hotkey</span>
                <select
                  className="settings-select"
                  aria-label="Global hotkey"
                  data-hotkey-select
                  value={
                    (HOTKEY_PRESETS as readonly string[]).includes(state.hotkey.shortcut)
                      ? state.hotkey.shortcut
                      : "custom"
                  }
                  onChange={(event) => {
                    if (event.target.value === "custom") return;
                    applyHotkey(event.target.value);
                  }}
                >
                  {HOTKEY_PRESETS.map((preset) => (
                    <option key={preset} value={preset}>
                      {preset}
                    </option>
                  ))}
                  <option value="custom">custom…</option>
                </select>
              </div>
              <form
                className="settings-workspace-add"
                onSubmit={(event) => {
                  event.preventDefault();
                  const shortcut = hotkeyDraft.trim();
                  if (shortcut) applyHotkey(shortcut);
                }}
              >
                <input
                  type="text"
                  aria-label="Custom hotkey accelerator"
                  placeholder="e.g. super+shift+space, alt+e, ctrl+alt+t"
                  value={hotkeyDraft}
                  onChange={(event) => setHotkeyDraft(event.target.value)}
                />
                <button type="submit">Apply</button>
              </form>
              <p className="settings-hint">
                Modifiers: super (⌘), ctrl, alt, shift — joined with +. A
                conflicting or invalid shortcut keeps the current one and shows
                the error below.
              </p>
            </>
          )}
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
          {state.autostart && (
            <label className="settings-row">
              <span className="settings-row-label">Start Third Eye at login</span>
              <input
                type="checkbox"
                data-autostart-toggle
                checked={state.autostart.enabled}
                onChange={(event) => {
                  setAutostart(event.target.checked).then(
                    (status) => dispatch({ type: "autostart", status }),
                    (err) => console.debug("settings: set_autostart unavailable:", err),
                  );
                }}
              />
            </label>
          )}
          {state.autostart?.error && (
            <div className="settings-error" role="alert">
              <span>{state.autostart.error}</span>
            </div>
          )}
        </section>
        )}

        </main>
        </div>
      </div>
    </div>
  );
}

export default Settings;
