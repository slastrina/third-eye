// Reducer + helper coverage for the first-run onboarding panel (M006): the
// show/hide gate (pending AND at least one supported permission), the
// per-permission request lifecycle, and the completion dismissal. Pure — no
// Tauri runtime or DOM needed.

import { describe, expect, it } from "vitest";
import type { CapturePermission, FirstRunStatus, InputPermission } from "./chat";
import {
  allSupportedGranted,
  initialOnboardingState,
  onboardingBlocked,
  onboardingReducer,
  stepFor,
  type OnboardingViewState,
} from "./onboarding-state";

const granted: CapturePermission = { granted: true, supported: true };
const ungranted: CapturePermission = { granted: false, supported: true };
const unsupported: CapturePermission = { granted: false, supported: false };

function snapshot(overrides: Partial<FirstRunStatus> = {}): FirstRunStatus {
  return {
    pending: true,
    capture: ungranted,
    input: ungranted,
    persistError: null,
    ...overrides,
  };
}

describe("stepFor", () => {
  it("maps supported+granted to granted", () => {
    expect(stepFor(granted)).toBe("granted");
  });
  it("maps supported+ungranted to denied", () => {
    expect(stepFor(ungranted)).toBe("denied");
  });
  it("maps unsupported to unsupported regardless of granted", () => {
    expect(stepFor(unsupported)).toBe("unsupported");
    expect(stepFor({ granted: true, supported: false } as InputPermission)).toBe("unsupported");
  });
});

describe("onboardingReducer snapshot gate", () => {
  it("shows the panel when pending and a permission is supported", () => {
    const s = onboardingReducer(initialOnboardingState, { type: "snapshot", status: snapshot() });
    expect(s.visible).toBe(true);
    expect(s.capture).toBe("denied");
    expect(s.input).toBe("denied");
  });

  it("hides the panel when onboarding is not pending", () => {
    const s = onboardingReducer(initialOnboardingState, {
      type: "snapshot",
      status: snapshot({ pending: false }),
    });
    expect(s.visible).toBe(false);
  });

  it("hides the panel when no permission is supported (off macOS)", () => {
    // Nothing to onboard: both surfaces unsupported means the panel never shows,
    // even though the flag is still pending.
    const s = onboardingReducer(initialOnboardingState, {
      type: "snapshot",
      status: snapshot({ capture: unsupported, input: unsupported }),
    });
    expect(s.visible).toBe(false);
  });

  it("seeds already-granted permissions as granted so a partial prior run shows progress", () => {
    const s = onboardingReducer(initialOnboardingState, {
      type: "snapshot",
      status: snapshot({ capture: granted }),
    });
    expect(s.capture).toBe("granted");
    expect(s.input).toBe("denied");
    expect(s.visible).toBe(true);
  });

  it("carries a persisted-flag save error from the snapshot", () => {
    const s = onboardingReducer(initialOnboardingState, {
      type: "snapshot",
      status: snapshot({ persistError: "disk full" }),
    });
    expect(s.persistError).toBe("disk full");
  });
});

describe("onboardingReducer request lifecycle", () => {
  const shown: OnboardingViewState = onboardingReducer(initialOnboardingState, {
    type: "snapshot",
    status: snapshot(),
  });

  it("marks a permission requesting while the OS prompt is up", () => {
    const s = onboardingReducer(shown, { type: "request-start", which: "capture" });
    expect(s.capture).toBe("requesting");
    expect(s.input).toBe("denied"); // untouched
  });

  it("folds a granted result into granted", () => {
    const s = onboardingReducer(
      onboardingReducer(shown, { type: "request-start", which: "input" }),
      { type: "request-done", which: "input", permission: granted },
    );
    expect(s.input).toBe("granted");
  });

  it("folds a denied result (macOS suppressed a repeat prompt) into denied", () => {
    const s = onboardingReducer(shown, {
      type: "request-done",
      which: "capture",
      permission: ungranted,
    });
    expect(s.capture).toBe("denied");
  });
});

describe("onboardingReducer completion", () => {
  const shown = onboardingReducer(initialOnboardingState, {
    type: "snapshot",
    status: snapshot(),
  });

  it("dismisses the panel on completion", () => {
    const s = onboardingReducer(shown, { type: "completed", status: snapshot({ pending: false }) });
    expect(s.visible).toBe(false);
  });

  it("dismisses even when the flag failed to persist, surfacing the error", () => {
    // The flag grants nothing, so a persist failure must not trap the user in
    // the panel — dismiss, but surface that it may return next launch.
    const s = onboardingReducer(shown, {
      type: "completed",
      status: snapshot({ pending: true, persistError: "disk full" }),
    });
    expect(s.visible).toBe(false);
    expect(s.persistError).toBe("disk full");
  });
});

describe("allSupportedGranted", () => {
  const base = initialOnboardingState;
  it("is true only when every supported permission is granted", () => {
    expect(allSupportedGranted({ ...base, capture: "granted", input: "granted" })).toBe(true);
  });
  it("treats an unsupported step as not blocking", () => {
    expect(allSupportedGranted({ ...base, capture: "granted", input: "unsupported" })).toBe(true);
  });
  it("is false while a permission is still denied or requesting", () => {
    expect(allSupportedGranted({ ...base, capture: "granted", input: "denied" })).toBe(false);
    expect(allSupportedGranted({ ...base, capture: "requesting", input: "granted" })).toBe(false);
  });
});

describe("onboardingBlocked", () => {
  const base = initialOnboardingState;

  it("blocks while Screen Recording is denied, regardless of Accessibility", () => {
    // The core loop's grant is required — a denied capture blocks even if the
    // (optional, off-by-default) Accessibility grant is already on.
    expect(onboardingBlocked({ ...base, capture: "denied", input: "granted" })).toBe(true);
    expect(onboardingBlocked({ ...base, capture: "denied", input: "denied" })).toBe(true);
  });

  it("blocks while the Screen Recording request is still in flight", () => {
    // The user must not slip past by clicking Continue during the OS prompt.
    expect(onboardingBlocked({ ...base, capture: "requesting", input: "granted" })).toBe(true);
    expect(onboardingBlocked({ ...base, capture: "idle", input: "granted" })).toBe(true);
  });

  it("does NOT block on a missing Accessibility grant — it is optional (D038/R019)", () => {
    // Accessibility is HID's grant and stays off-by-default; a denied input step
    // must never trap the user, so long as capture is granted.
    expect(onboardingBlocked({ ...base, capture: "granted", input: "denied" })).toBe(false);
    expect(onboardingBlocked({ ...base, capture: "granted", input: "requesting" })).toBe(false);
  });

  it("is not blocked once Screen Recording is granted", () => {
    expect(onboardingBlocked({ ...base, capture: "granted", input: "granted" })).toBe(false);
  });

  it("is not blocked when Screen Recording is unsupported (off macOS)", () => {
    // Nothing to grant on a platform without the permission — never trap the user.
    expect(onboardingBlocked({ ...base, capture: "unsupported", input: "unsupported" })).toBe(false);
  });
});
