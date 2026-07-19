// UI side of the watcher IPC surface (S01): the `set_watcher_enabled` and
// `watcher_status` commands plus the `watcher://state` and
// `watcher://observation` broadcasts, and the pure `watcherReducer` behind
// the Watch Screen diagnostics section in Settings. The shapes here mirror
// the serde camelCase serialization of Rust's WatcherStatus, OcrError, and
// TextObservation (src-tauri/src/watcher, src-tauri/src/ocr) — a change on
// either side is a breaking IPC change.
//
// The reducer is pure so status transitions, the rolling last-N snippet
// buffer, and every error state are unit-testable without a Tauri runtime
// (src/watcher-state.test.ts); Settings.tsx is only glue. (Kebab-case name
// per MEM051: a src/watcher.ts would be fine today, but pure-module
// companions follow settings-state.ts/overlay-state.ts convention.)

import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

/** Watcher-state broadcast: every toggle, run-state transition, and tick
 *  error change emits the resulting WatcherStatus app-wide. */
export const WATCHER_STATE_EVENT = "watcher://state";

/** Per-tick observation broadcast feeding the diagnostics snippet list. */
export const WATCHER_OBSERVATION_EVENT = "watcher://observation";

/** What the loop is doing right now — kebab-case over IPC, matching Rust's
 *  WatcherRunState. `paused-privacy` is its own visible state (R027). */
export type WatcherRunState = "idle" | "watching" | "paused-privacy";

/** A typed OCR tick failure — the serde kind-tagged serialization of Rust's
 *  OcrError. Consumers match on `kind`, same contract as LlmError. */
export type OcrError =
  | { kind: "permission-denied"; detail: string }
  | { kind: "capture-failed"; detail: string }
  | { kind: "recognition-failed"; detail: string }
  | { kind: "unsupported"; platform: string; detail: string };

/** Queryable watcher state (health-as-value, R007): returned by
 *  `watcher_status`, broadcast on `watcher://state`. `lastTickError` is the
 *  most recent typed OCR failure (kept until a tick succeeds); `error`
 *  carries the most recent persist failure, like PrivacyStatus. */
export interface WatcherStatus {
  enabled: boolean;
  state: WatcherRunState;
  lastTickError: OcrError | null;
  error: string | null;
}

/** One extracted screen observation — structurally pixel-free (R011):
 *  text, the frontmost app's name (when known), and a capture timestamp. */
export interface TextObservation {
  text: string;
  appContext: string | null;
  capturedAt: number;
}

/** Current watcher state (health-as-value, like `privacy_status`). */
export function watcherStatus(): Promise<WatcherStatus> {
  return invoke<WatcherStatus>("watcher_status");
}

/** Toggle the watcher. Never rejects backend-side: a persist failure comes
 *  back as data on the resulting status (same contract as
 *  `set_privacy_mode`); rejection outside a Tauri runtime is absorbed by
 *  the caller. */
export function setWatcherEnabled(enable: boolean): Promise<WatcherStatus> {
  return invoke<WatcherStatus>("set_watcher_enabled", { enable });
}

/** Subscribe to the app-wide watcher-state broadcast (`watcher://state`). */
export function onWatcherState(cb: (status: WatcherStatus) => void): Promise<UnlistenFn> {
  return listen<WatcherStatus>(WATCHER_STATE_EVENT, (e) => cb(e.payload));
}

/** Subscribe to per-tick observations (`watcher://observation`). */
export function onWatcherObservation(
  cb: (observation: TextObservation) => void,
): Promise<UnlistenFn> {
  return listen<TextObservation>(WATCHER_OBSERVATION_EVENT, (e) => cb(e.payload));
}

// ---------------------------------------------------------------------------
// Watcher diagnostics state machine (pure)
// ---------------------------------------------------------------------------

/** How many extracted snippets the diagnostics list keeps, newest first.
 *  Old screen text is worthless to scroll through — this is a live window,
 *  not a transcript (the persistent store is S02's job). */
export const MAX_SNIPPETS = 5;

export interface WatcherViewState {
  /** Live status; null until the mount-time `watcher_status` resolves (or
   *  forever, outside Tauri — the section renders unavailable). */
  status: WatcherStatus | null;
  /** Rolling last-N extracted snippets, newest first, capped at
   *  MAX_SNIPPETS. Kept across pauses/disables — the timestamps show
   *  staleness, and blanking the evidence would make the surface lie. */
  observations: TextObservation[];
}

export const initialWatcherViewState: WatcherViewState = {
  status: null,
  observations: [],
};

export type WatcherViewAction =
  | { type: "status"; status: WatcherStatus }
  | { type: "observation"; observation: TextObservation };

export function watcherReducer(
  state: WatcherViewState,
  action: WatcherViewAction,
): WatcherViewState {
  switch (action.type) {
    case "status":
      // Mount-time query, set_watcher_enabled responses, and the
      // watcher://state broadcast (tray toggles included) all land here —
      // the backend snapshot is authoritative.
      return { ...state, status: action.status };
    case "observation":
      return {
        ...state,
        observations: [action.observation, ...state.observations].slice(0, MAX_SNIPPETS),
      };
  }
}

// ---------------------------------------------------------------------------
// Copy helpers (pure)
// ---------------------------------------------------------------------------

/** Human label for the live run state. */
export function runStateLabel(state: WatcherRunState): string {
  switch (state) {
    case "idle":
      return "Off";
    case "watching":
      return "Watching";
    case "paused-privacy":
      return "Paused by Privacy Mode";
  }
}

/** Short human title for a typed tick failure. */
export function tickErrorTitle(error: OcrError): string {
  switch (error.kind) {
    case "permission-denied":
      return "Screen Recording permission needed";
    case "capture-failed":
      return "Screen capture failed";
    case "recognition-failed":
      return "Text recognition failed";
    case "unsupported":
      return "Watching isn't supported on this platform";
  }
}

/** Detail line for a typed tick failure, naming the platform when the
 *  backend did. */
export function tickErrorDetail(error: OcrError): string {
  return error.kind === "unsupported" ? `${error.platform} — ${error.detail}` : error.detail;
}

/** How many characters of a snippet the diagnostics list shows. */
export const SNIPPET_PREVIEW_CHARS = 160;

/** One-line preview of an extracted snippet: newlines collapse to a
 *  separator (Vision emits one line per recognized region) and long text is
 *  truncated with an ellipsis. */
export function snippetPreview(text: string, max: number = SNIPPET_PREVIEW_CHARS): string {
  const oneLine = text.replace(/\s*\n\s*/g, " · ").trim();
  return oneLine.length > max ? `${oneLine.slice(0, max)}…` : oneLine;
}

/** Wall-clock label for a snippet's capture time (local time of day — the
 *  list only spans the last few ticks, so the date would be noise). */
export function capturedAtLabel(capturedAt: number): string {
  return new Date(capturedAt).toLocaleTimeString();
}
