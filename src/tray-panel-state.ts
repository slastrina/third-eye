// Pure state for the tray dropdown panel (2026-07 redesign, surface 4): the
// left-click webview anchored under the menu-bar eye. Folds the real
// backend snapshots — watcher status, memory status, latest memories — and
// the pause/resume lifecycle. No-fake-data: every field is either a backend
// value or null, and the view omits what is null (the prototype's
// "observed today 4h 12m" style demo stats have no backend and never render).
//
// Timed pause is a webview-side timer: the tray-panel window is created at
// launch and lives as long as the app, so its setTimeout re-enable is
// reliable while the app runs. After an app restart a timed pause degrades
// to a plain persisted off — the sub-line derives from `pausedUntil`, which
// does not survive restart, so the UI never claims a resume it can't keep.

import type { WatcherStatus } from "./watcher-state";
import type { MemoryStatus, MemoryRecord } from "./memory-state";

export type PauseChoice = "15m" | "1h" | "manual";

export const PAUSE_OPTIONS: readonly { value: PauseChoice; label: string }[] = [
  { value: "15m", label: "15 min" },
  { value: "1h", label: "1 hour" },
  { value: "manual", label: "Until I resume" },
];

/** Timer duration for a timed pause; null for the manual choice. */
export function pauseMs(choice: PauseChoice): number | null {
  switch (choice) {
    case "15m":
      return 15 * 60 * 1000;
    case "1h":
      return 60 * 60 * 1000;
    case "manual":
      return null;
  }
}

export interface TrayPanelViewState {
  /** Live watcher snapshot; null before the mount query lands (or outside
   *  the app), which renders the unknown posture, never a guess. */
  watching: boolean | null;
  /** Epoch ms when a timed pause auto-resumes; null when watching or on a
   *  manual pause. Set by the pause action, cleared by any status fold that
   *  reports watching (the resume landed, timed or manual). */
  pausedUntil: number | null;
  /** memory_status.count — total stored memories; null when unavailable. */
  memoriesStored: number | null;
  /** Latest stored memories (newest first) for the LATEST section. */
  latest: MemoryRecord[];
}

export const initialTrayPanelState: TrayPanelViewState = {
  watching: null,
  pausedUntil: null,
  memoriesStored: null,
  latest: [],
};

export type TrayPanelAction =
  | { type: "watcher"; status: WatcherStatus }
  | { type: "memory"; status: MemoryStatus }
  | { type: "latest"; records: MemoryRecord[] }
  // The user chose a pause option; `now` injected for testability.
  | { type: "paused"; choice: PauseChoice; now: number };

export function trayPanelReducer(
  state: TrayPanelViewState,
  action: TrayPanelAction,
): TrayPanelViewState {
  switch (action.type) {
    case "watcher": {
      const watching = action.status.enabled;
      // Any fold that reports watching clears the countdown (the resume
      // landed — by timer, by this panel, or by Settings/the menu).
      return { ...state, watching, pausedUntil: watching ? null : state.pausedUntil };
    }
    case "memory":
      return { ...state, memoriesStored: action.status.count };
    case "latest":
      return { ...state, latest: action.records };
    case "paused": {
      const ms = pauseMs(action.choice);
      return { ...state, watching: false, pausedUntil: ms === null ? null : action.now + ms };
    }
    default:
      return state;
  }
}

/** Header title — mirrors the design's Watching / Paused states plus the
 *  honest unknown posture. */
export function trayTitle(state: TrayPanelViewState): string {
  if (state.watching === null) return "Third Eye";
  return state.watching ? "Watching" : "Paused";
}

/** Header sub-line. Only claims what is known: a timed pause names its
 *  resume time, a manual pause says so, watching says on-device. */
export function traySub(state: TrayPanelViewState, now: number): string {
  if (state.watching === null) return "state unavailable";
  if (state.watching) return "observing on-device";
  if (state.pausedUntil === null) return "resumes when you say so";
  const minutes = Math.max(1, Math.round((state.pausedUntil - now) / 60000));
  return `resumes in ~${minutes} min`;
}

/** The eye's state for the header. */
export function trayEye(state: TrayPanelViewState): "watching" | "closed" {
  return state.watching === true ? "watching" : "closed";
}
