// Pure state for the first-start tour — the four-step wizard (Welcome →
// Permissions → Memory → Summon) from the 2026-07 redesign
// (specs/2026-07-26-redesign-and-first-start-tour.md, surface 1). It wraps the
// M006 permission lifecycle (onboarding-state.ts) rather than replacing it:
// every permission transition still flows through onboardingReducer, so the
// D038/R019 contract (Screen Recording required, Accessibility optional and
// never arming HID) is enforced by the same tested code as before. This module
// adds only what the wizard needs on top: step navigation, the retention
// choice, and hotkey-press completion. App.tsx stays glue.

import type { FirstRunStatus, CapturePermission, InputPermission } from "./chat";
import {
  initialOnboardingState,
  onboardingBlocked,
  onboardingReducer,
  type OnboardingAction,
  type OnboardingViewState,
} from "./onboarding-state";

/** The wizard's steps, in order. */
export const TOUR_STEPS = ["welcome", "permissions", "memory", "summon"] as const;
export type TourStep = (typeof TOUR_STEPS)[number];
export const TOUR_STEP_LABELS: readonly string[] = ["Welcome", "Permissions", "Memory", "Summon"];

/** Retention choices offered on the Memory step; the serialized values are the
 *  `memoryRetention` setting's wire contract (B2). */
export const RETENTION_OPTIONS = [
  { value: "7d", label: "7 days" },
  { value: "30d", label: "30 days" },
  { value: "90d", label: "90 days" },
  { value: "forever", label: "Forever" },
] as const;
export type Retention = (typeof RETENTION_OPTIONS)[number]["value"];

export interface TourViewState {
  /** The wrapped M006 permission lifecycle. `permissions.visible` doubles as
   *  the tour's own visibility: the same snapshot/pending rules decide both. */
  permissions: OnboardingViewState;
  /** 0-based index into TOUR_STEPS. */
  step: number;
  /** The retention selection shown on the Memory step. Seeded from the
   *  persisted setting; every change is the caller's cue to fire the set IPC. */
  retention: Retention;
}

export const initialTourState: TourViewState = {
  permissions: initialOnboardingState,
  step: 0,
  retention: "30d",
};

export type TourAction =
  // Wrapped permission-lifecycle actions, forwarded verbatim (mount snapshot,
  // request start/done, completed). `snapshot` also resets the step so a
  // re-shown tour (persist failure on a prior run) starts from Welcome.
  | { type: "permissions"; action: OnboardingAction }
  | { type: "next" }
  | { type: "back" }
  // Skip = finish now; the caller fires complete_first_run and the resulting
  // `permissions/completed` action hides the tour. No separate skip state.
  | { type: "retention"; value: Retention }
  // Seed from the persisted memoryRetention setting on mount.
  | { type: "retention-loaded"; value: Retention }
  // The global summon hotkey fired while the tour is up. Only the Summon step
  // treats it as "finish" — earlier steps ignore it so an accidental press
  // can't skip the permission gate.
  | { type: "hotkey-pressed" };

/** Whether Continue is blocked on the current step. Only the Permissions step
 *  ever blocks, and it blocks exactly per M006's onboardingBlocked (Screen
 *  Recording missing on a platform that supports it). */
export function tourBlocked(state: TourViewState): boolean {
  return TOUR_STEPS[state.step] === "permissions" && onboardingBlocked(state.permissions);
}

/** Whether *finishing* (Finish, Skip, hotkey) is blocked — step-independent,
 *  exactly M006's rule: the required Screen Recording grant is missing. This
 *  is the guard on every completion path so Skip from the Welcome step cannot
 *  persist "done" past the hard block; `tourBlocked` above only gates the
 *  Continue button on the Permissions step itself. */
export function tourFinishBlocked(state: TourViewState): boolean {
  return onboardingBlocked(state.permissions);
}

/** Whether the current step is the last one (button reads Finish). */
export function tourOnLastStep(state: TourViewState): boolean {
  return state.step === TOUR_STEPS.length - 1;
}

export function tourReducer(state: TourViewState, action: TourAction): TourViewState {
  switch (action.type) {
    case "permissions": {
      const permissions = onboardingReducer(state.permissions, action.action);
      // A fresh mount snapshot restarts the wizard at Welcome; other wrapped
      // actions (request lifecycle, completed) leave the step alone.
      const step = action.action.type === "snapshot" ? 0 : state.step;
      return { ...state, permissions, step };
    }
    case "next": {
      if (tourBlocked(state)) return state;
      if (tourOnLastStep(state)) return state; // Finish is an effect (complete_first_run), not a step change.
      return { ...state, step: state.step + 1 };
    }
    case "back":
      return { ...state, step: Math.max(0, state.step - 1) };
    case "retention":
    case "retention-loaded":
      return { ...state, retention: action.value };
    case "hotkey-pressed":
      // Handled as visibility-preserving no-op here; the *caller* treats a
      // press on the Summon step as Finish (fires complete_first_run) because
      // completion is an effect. The reducer only guards the step gate.
      return state;
    default:
      return state;
  }
}

/** Whether a hotkey press should finish the tour: only on the Summon step,
 *  and never while a required permission is still missing (can't happen —
 *  Continue gates entry to later steps — but guarded for defense in depth). */
export function hotkeyFinishesTour(state: TourViewState): boolean {
  return (
    state.permissions.visible &&
    TOUR_STEPS[state.step] === "summon" &&
    !tourFinishBlocked(state)
  );
}

/** Convenience: the tour's visibility (delegates to the wrapped lifecycle). */
export function tourVisible(state: TourViewState): boolean {
  return state.permissions.visible;
}

/** Render one hotkey token as its keycap label. macOS uses modifier symbols
 *  (`super` is the plugin's Cmd token there); elsewhere plain words. Unknown
 *  tokens pass through so a novel binding still displays something truthful. */
function keycapLabel(token: string, mac: boolean): string {
  const t = token.trim().toLowerCase();
  const mapped: Record<string, [string, string]> = {
    // token: [macos label, other-platform label]
    super: ["⌘", "Win"],
    cmd: ["⌘", "Win"],
    ctrl: ["⌃", "Ctrl"],
    control: ["⌃", "Ctrl"],
    alt: ["⌥", "Alt"],
    option: ["⌥", "Alt"],
    shift: ["⇧", "Shift"],
    space: ["space", "Space"],
  };
  const entry = mapped[t];
  if (entry) return mac ? entry[0] : entry[1];
  return token.trim();
}

/** Split a plugin shortcut string ("super+shift+space") into display keycaps
 *  for the Summon step. Empty/garbage input yields no caps — the caller shows
 *  the step without the keycap row rather than inventing a binding. */
export function shortcutKeycaps(shortcut: string, mac: boolean): string[] {
  return shortcut
    .split("+")
    .map((token) => token.trim())
    .filter((token) => token.length > 0)
    .map((token) => keycapLabel(token, mac));
}

// Re-exported so Tour.tsx has a single import surface for step rendering.
export type { OnboardingViewState, CapturePermission, InputPermission, FirstRunStatus };
