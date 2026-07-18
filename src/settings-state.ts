// UI side of the settings-window IPC surface (S07): the `list_models`,
// `set_lane_model`, `set_privacy_mode`, `hotkey_status`, `autostart_status`,
// and `hide_settings_window` commands, plus the pure `settingsReducer` behind
// the settings view. Chat-shared shapes (ModelInfo, PrivacyStatus, LlmError,
// the broadcast subscriptions) live in src/chat.ts — one contract, imported
// here, so the two windows cannot drift.
//
// The reducer is pure so offline states, lane-pin rejections, and persist
// failures are unit-testable without a Tauri runtime (src/settings.test.ts);
// Settings.tsx is only glue. (Named settings-state.ts, not settings.ts: on
// macOS's case-insensitive filesystem a src/settings.ts would collide with
// Settings.tsx and hijack the `./Settings` import — .ts wins resolution.)

import { invoke } from "@tauri-apps/api/core";
import type { LlmError, ModelInfo, PrivacyStatus } from "./chat";

/** Global-hotkey health `{ shortcut, registered, error }` — the serde
 *  camelCase serialization of Rust's HotkeyStatus (read-only here). */
export interface HotkeyStatus {
  shortcut: string;
  registered: boolean;
  error: string | null;
}

/** Launch-at-login health `{ enabled, error }` — the serde camelCase
 *  serialization of Rust's AutostartStatus (read-only here). */
export interface AutostartStatus {
  enabled: boolean;
  error: string | null;
}

/** List the model ids the LM Studio endpoint actually serves. Rejects with
 *  the kind-tagged LlmError (`offline`) on any transport/protocol failure —
 *  for the pickers, can't-list and endpoint-down are the same state. */
export function listModels(): Promise<string[]> {
  return invoke<string[]>("list_models");
}

/** Re-pin a lane's model and persist it (S07). `null` pins "endpoint
 *  default" (explicit unpin — survives restart too). Resolves to the updated
 *  routing state; rejects with a string naming the lane or the failed
 *  persist path, leaving routing unchanged backend-side. */
export function setLaneModel(lane: string, model: string | null): Promise<ModelInfo> {
  return invoke<ModelInfo>("set_lane_model", { lane, model });
}

/** Toggle privacy mode. Never rejects backend-side: a persist failure comes
 *  back as data on the resulting status (same contract as `set_autostart`). */
export function setPrivacyMode(enable: boolean): Promise<PrivacyStatus> {
  return invoke<PrivacyStatus>("set_privacy_mode", { enable });
}

/** Read-only global-hotkey health for the status section. */
export function hotkeyStatus(): Promise<HotkeyStatus> {
  return invoke<HotkeyStatus>("hotkey_status");
}

/** Read-only launch-at-login health for the status section. */
export function autostartStatus(): Promise<AutostartStatus> {
  return invoke<AutostartStatus>("autostart_status");
}

/** Hide the settings panel (Escape / in-page close). Rejection outside a
 *  Tauri runtime is absorbed by the caller — the view must stay renderable
 *  in a plain browser. */
export function hideSettingsWindow(): Promise<void> {
  return invoke("hide_settings_window");
}

// ---------------------------------------------------------------------------
// Settings view state machine (pure)
// ---------------------------------------------------------------------------

/** A typed backend failure from `list_models`, or an IPC-level failure where
 *  the invoke itself rejected (plain browser / vite dev — no Tauri). */
export type ModelsError = LlmError | { kind: "ipc"; detail: string };

/** Normalize a `list_models` rejection: kind-tagged errors pass through;
 *  anything else (IPC string, Error) becomes "ipc". */
export function toModelsError(err: unknown): ModelsError {
  if (typeof err === "object" && err !== null && "kind" in err) {
    return err as LlmError;
  }
  return { kind: "ipc", detail: String(err) };
}

export interface SettingsState {
  /** Routing snapshot feeding the pickers' current values; null until the
   *  mount-time `model_info` resolves (or forever, outside Tauri). */
  modelInfo: ModelInfo | null;
  /** Model ids the endpoint serves; null until the first fetch resolves.
   *  Kept through a failed refresh — a stale list beats an empty picker. */
  models: string[] | null;
  /** True while a `list_models` fetch is in flight (refresh affordance). */
  modelsLoading: boolean;
  /** Why the model list is unavailable; `offline` names the endpoint. */
  modelsError: ModelsError | null;
  /** Last rejected lane pin, naming the lane (routing stays unchanged
   *  backend-side, so the pickers keep showing the real state). */
  laneError: string | null;
  /** Privacy toggle state; `error` carries a persist failure. Null until
   *  the mount-time query resolves (toggle renders unavailable). */
  privacy: PrivacyStatus | null;
  hotkey: HotkeyStatus | null;
  autostart: AutostartStatus | null;
}

export const initialSettingsState: SettingsState = {
  modelInfo: null,
  models: null,
  modelsLoading: false,
  modelsError: null,
  laneError: null,
  privacy: null,
  hotkey: null,
  autostart: null,
};

export type SettingsAction =
  | { type: "models-loading" }
  | { type: "models-loaded"; models: string[] }
  | { type: "models-error"; error: ModelsError }
  | { type: "model-info"; info: ModelInfo }
  | { type: "lane-error"; lane: string; detail: string }
  | { type: "privacy"; status: PrivacyStatus }
  | { type: "hotkey"; status: HotkeyStatus }
  | { type: "autostart"; status: AutostartStatus };

export function settingsReducer(state: SettingsState, action: SettingsAction): SettingsState {
  switch (action.type) {
    case "models-loading":
      return { ...state, modelsLoading: true, modelsError: null };
    case "models-loaded":
      return { ...state, models: action.models, modelsLoading: false, modelsError: null };
    case "models-error":
      // The stale `models` list survives so the pickers keep rendering the
      // current pins; the error banner names why refresh failed.
      return { ...state, modelsLoading: false, modelsError: action.error };
    case "model-info":
      // Mount-time query, set_lane_model responses, and the llm://model-info
      // broadcast all land here — the backend snapshot is authoritative. A
      // successful update supersedes any stale lane rejection.
      return { ...state, modelInfo: action.info, laneError: null };
    case "lane-error":
      return { ...state, laneError: `${action.lane}: ${action.detail}` };
    case "privacy":
      // Mount-time query, set_privacy_mode responses, and the
      // capture://privacy broadcast (tray toggles included) land here.
      return { ...state, privacy: action.status };
    case "hotkey":
      return { ...state, hotkey: action.status };
    case "autostart":
      return { ...state, autostart: action.status };
  }
}

// ---------------------------------------------------------------------------
// Picker + copy helpers (pure)
// ---------------------------------------------------------------------------

/** Options for a lane picker: the fetched model list, with the lane's
 *  current pin prepended when the endpoint no longer lists it — the select
 *  must always show the truth, even for a model that went away. The
 *  "endpoint default" (null) option is rendered separately by the UI. */
export function laneOptions(models: string[] | null, currentModelId: string | null): string[] {
  const list = models ?? [];
  if (currentModelId !== null && !list.includes(currentModelId)) {
    return [currentModelId, ...list];
  }
  return list;
}

/** Short human title for a failed model-list fetch. */
export function modelsErrorTitle(error: ModelsError): string {
  switch (error.kind) {
    case "offline":
      return "Can't reach the model endpoint";
    case "no-model":
      return "No model loaded";
    case "interrupted":
      return "Model list fetch interrupted";
    case "ipc":
      return "Model list unavailable";
  }
}

/** Detail line naming the endpoint that was tried (R006). */
export function modelsErrorDetail(error: ModelsError): string {
  return error.kind === "ipc" ? error.detail : `${error.endpoint} — ${error.detail}`;
}
