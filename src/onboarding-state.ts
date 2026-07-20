// Pure state for the first-run onboarding panel (M006), mirroring the
// watcher-state/settings-state split: every transition lives here so the
// explainer's show/hide logic, per-permission request lifecycle, and the
// macOS-only gate are unit-testable without a Tauri runtime (onboarding-state.test.ts).
// App.tsx is only glue: it fires the IPC and dispatches the resulting snapshots.
//
// Requesting the Accessibility grant here NEVER arms HID — that is the backend
// contract (D038/R019); this module only tracks the permission request UX.

import type { CapturePermission, FirstRunStatus, InputPermission } from "./chat";

/** Per-permission request lifecycle in the panel. `idle` before the user acts,
 *  `requesting` while the OS prompt is up, `granted`/`denied` after it settles
 *  (a macOS denial or a repeat-ask that macOS suppressed both read as `denied`,
 *  routing the user to the Settings deep-link). `unsupported` off macOS. */
export type PermissionStep = "idle" | "requesting" | "granted" | "denied" | "unsupported";

export interface OnboardingViewState {
  /** Whether the onboarding panel should render. False until the mount snapshot
   *  reports `pending`, and false again the moment the user finishes/skips —
   *  the panel is one-shot per app install (governed by the persisted flag). */
  visible: boolean;
  /** The Screen Recording request lifecycle. */
  capture: PermissionStep;
  /** The Accessibility request lifecycle. */
  input: PermissionStep;
  /** A failed "mark done" persist, surfaced so the user knows onboarding may
   *  re-show next launch — never blocks dismissal (the flag grants nothing). */
  persistError: string | null;
}

export const initialOnboardingState: OnboardingViewState = {
  visible: false,
  capture: "idle",
  input: "idle",
  persistError: null,
};

/** Map a live permission value onto a step, treating an in-flight request as
 *  settled: `supported: false` → `unsupported`, granted → `granted`, else
 *  `denied`. Used to fold both the mount snapshot and a request's result. */
export function stepFor(permission: CapturePermission | InputPermission): PermissionStep {
  if (!permission.supported) return "unsupported";
  return permission.granted ? "granted" : "denied";
}

export type OnboardingAction =
  // The mount snapshot from first_run_status: decides whether to show the panel
  // and seeds each step from the already-live permission state (a grant made in
  // a prior partial run or out of band shows as granted immediately).
  | { type: "snapshot"; status: FirstRunStatus }
  // A permission request is in flight — the OS prompt is (or would be) up.
  | { type: "request-start"; which: "capture" | "input" }
  // A permission request settled with this live value.
  | { type: "request-done"; which: "capture" | "input"; permission: CapturePermission | InputPermission }
  // complete_first_run returned — dismiss the panel; a persist failure still
  // dismisses (the flag grants nothing, re-showing is harmless) but is surfaced.
  | { type: "completed"; status: FirstRunStatus };

/** Pure reducer for the onboarding panel. Only shows the panel when the backend
 *  says onboarding is pending AND the platform supports at least one of the two
 *  permissions — off a supported platform there is nothing to onboard, so the
 *  panel never appears (matching the Settings "unsupported" posture). */
export function onboardingReducer(
  state: OnboardingViewState,
  action: OnboardingAction,
): OnboardingViewState {
  switch (action.type) {
    case "snapshot": {
      const { status } = action;
      const capture = stepFor(status.capture);
      const input = stepFor(status.input);
      // Nothing to onboard if neither permission surface exists on this platform.
      const anySupported = status.capture.supported || status.input.supported;
      return {
        ...state,
        visible: status.pending && anySupported,
        capture,
        input,
        persistError: status.persistError,
      };
    }
    case "request-start":
      return { ...state, [action.which]: "requesting" };
    case "request-done":
      return { ...state, [action.which]: stepFor(action.permission) };
    case "completed":
      // Dismiss regardless of persist outcome; surface a persist failure so the
      // user understands the panel may return next launch.
      return { ...state, visible: false, persistError: action.status.persistError };
    default:
      return state;
  }
}

/** Whether every supported permission has been granted — the signal to relax the
 *  "Continue" button's copy from "grant then continue" to "all set". A step that
 *  is `unsupported` doesn't block completion (nothing to grant there). */
export function allSupportedGranted(state: OnboardingViewState): boolean {
  const settled = (step: PermissionStep) => step === "granted" || step === "unsupported";
  return settled(state.capture) && settled(state.input);
}

/** Whether onboarding is *blocked* — a required permission is not yet granted, so
 *  the user must fix it before continuing (there is no Skip past a hard block).
 *
 *  Only Screen Recording is required: it is the core on-device screen-reading
 *  loop the whole app is built around. Accessibility is deliberately NOT required
 *  — it grants HID's most-dangerous capability, which stays off-by-default
 *  (D038/R019); forcing it at first run would contradict that posture. So a
 *  missing Accessibility grant never blocks; a missing Screen Recording grant on
 *  a platform that supports it does.
 *
 *  An `unsupported` capture step (off macOS) is not a block — there is nothing to
 *  grant, so blocking would trap a user on a platform where the permission can
 *  never exist. Blocked precisely when capture is supported and not yet granted. */
export function onboardingBlocked(state: OnboardingViewState): boolean {
  // Blocked while the required grant is missing — including mid-request, so the
  // user cannot slip past by clicking Continue during the OS prompt.
  return state.capture !== "granted" && state.capture !== "unsupported";
}
