import { describe, expect, it } from "vitest";
import type { FirstRunStatus } from "./chat";
import {
  RETENTION_OPTIONS,
  TOUR_STEPS,
  hotkeyFinishesTour,
  initialTourState,
  tourBlocked,
  tourOnLastStep,
  tourReducer,
  tourVisible,
  type TourAction,
  type TourViewState,
} from "./tour-state";

const status = (over: Partial<FirstRunStatus> = {}): FirstRunStatus => ({
  pending: true,
  capture: { granted: false, supported: true },
  input: { granted: false, supported: true },
  persistError: null,
  ...over,
});

const snapshot = (s: FirstRunStatus): TourAction => ({
  type: "permissions",
  action: { type: "snapshot", status: s },
});

/** A tour mid-flight: visible, capture granted, sitting on `step`. */
function tourAt(step: number): TourViewState {
  let state = tourReducer(
    initialTourState,
    snapshot(status({ capture: { granted: true, supported: true } })),
  );
  for (let i = 0; i < step; i++) state = tourReducer(state, { type: "next" });
  return state;
}

describe("visibility", () => {
  it("shows on a pending snapshot with a supported permission, starting at Welcome", () => {
    const state = tourReducer(initialTourState, snapshot(status()));
    expect(tourVisible(state)).toBe(true);
    expect(state.step).toBe(0);
  });

  it("stays hidden when onboarding is not pending", () => {
    const state = tourReducer(initialTourState, snapshot(status({ pending: false })));
    expect(tourVisible(state)).toBe(false);
  });

  it("a mid-tour re-snapshot restarts at Welcome", () => {
    const state = tourReducer(tourAt(2), snapshot(status()));
    expect(state.step).toBe(0);
  });

  it("completed hides the tour", () => {
    const state = tourReducer(tourAt(3), {
      type: "permissions",
      action: { type: "completed", status: status({ pending: false }) },
    });
    expect(tourVisible(state)).toBe(false);
  });
});

describe("step navigation", () => {
  it("next walks Welcome → Permissions → Memory → Summon", () => {
    let state = tourAt(0);
    const seen = [TOUR_STEPS[state.step]];
    for (let i = 0; i < 3; i++) {
      state = tourReducer(state, { type: "next" });
      seen.push(TOUR_STEPS[state.step]);
    }
    expect(seen).toEqual(["welcome", "permissions", "memory", "summon"]);
  });

  it("next on the last step is a no-op (Finish is an effect, not a step)", () => {
    const state = tourAt(3);
    expect(tourOnLastStep(state)).toBe(true);
    expect(tourReducer(state, { type: "next" }).step).toBe(3);
  });

  it("back steps down and clamps at Welcome", () => {
    let state = tourAt(2);
    state = tourReducer(state, { type: "back" });
    expect(state.step).toBe(1);
    state = tourReducer(state, { type: "back" });
    state = tourReducer(state, { type: "back" });
    expect(state.step).toBe(0);
  });
});

describe("permission gate (D038/R019 preserved)", () => {
  it("blocks Continue on Permissions while Screen Recording is missing", () => {
    let state = tourReducer(initialTourState, snapshot(status()));
    state = tourReducer(state, { type: "next" }); // → permissions
    expect(TOUR_STEPS[state.step]).toBe("permissions");
    expect(tourBlocked(state)).toBe(true);
    expect(tourReducer(state, { type: "next" }).step).toBe(state.step);
  });

  it("blocks while the capture request is still in flight", () => {
    let state = tourReducer(initialTourState, snapshot(status()));
    state = tourReducer(state, { type: "next" });
    state = tourReducer(state, {
      type: "permissions",
      action: { type: "request-start", which: "capture" },
    });
    expect(tourBlocked(state)).toBe(true);
  });

  it("unblocks once Screen Recording is granted; Accessibility never blocks", () => {
    let state = tourReducer(initialTourState, snapshot(status()));
    state = tourReducer(state, { type: "next" });
    state = tourReducer(state, {
      type: "permissions",
      action: {
        type: "request-done",
        which: "capture",
        permission: { granted: true, supported: true },
      },
    });
    expect(tourBlocked(state)).toBe(false);
    // input still idle/denied — Continue works anyway
    expect(TOUR_STEPS[tourReducer(state, { type: "next" }).step]).toBe("memory");
  });

  it("never blocks on a platform without capture support", () => {
    let state = tourReducer(
      initialTourState,
      snapshot(status({ capture: { granted: false, supported: false } })),
    );
    state = tourReducer(state, { type: "next" });
    expect(tourBlocked(state)).toBe(false);
  });

  it("only the Permissions step ever blocks", () => {
    expect(tourBlocked(tourAt(0))).toBe(false);
    expect(tourBlocked(tourAt(2))).toBe(false);
    expect(tourBlocked(tourAt(3))).toBe(false);
  });

  it("finishing is blocked from EVERY step while capture is missing", async () => {
    // Skip on the Welcome step must not bypass the hard block — the finish
    // guard is step-independent even though the Continue gate is not.
    const { tourFinishBlocked } = await import("./tour-state");
    const blocked = tourReducer(initialTourState, snapshot(status()));
    expect(tourFinishBlocked(blocked)).toBe(true);
    const granted = tourReducer(
      initialTourState,
      snapshot(status({ capture: { granted: true, supported: true } })),
    );
    expect(tourFinishBlocked(granted)).toBe(false);
  });
});

describe("retention", () => {
  it("defaults to 30 days and tracks selection", () => {
    expect(initialTourState.retention).toBe("30d");
    const state = tourReducer(tourAt(2), { type: "retention", value: "forever" });
    expect(state.retention).toBe("forever");
  });

  it("seeds from the persisted setting", () => {
    const state = tourReducer(initialTourState, { type: "retention-loaded", value: "90d" });
    expect(state.retention).toBe("90d");
  });

  it("offers exactly the wire-contract values", () => {
    expect(RETENTION_OPTIONS.map((o) => o.value)).toEqual(["7d", "30d", "90d", "forever"]);
  });
});

describe("shortcutKeycaps", () => {
  it("maps the default binding to macOS symbols", async () => {
    const { shortcutKeycaps } = await import("./tour-state");
    expect(shortcutKeycaps("super+shift+space", true)).toEqual(["⌘", "⇧", "space"]);
  });

  it("uses words off macOS and passes unknown tokens through", async () => {
    const { shortcutKeycaps } = await import("./tour-state");
    expect(shortcutKeycaps("super+shift+space", false)).toEqual(["Win", "Shift", "Space"]);
    expect(shortcutKeycaps("alt+F9", true)).toEqual(["⌥", "F9"]);
  });

  it("yields no caps for garbage instead of inventing a binding", async () => {
    const { shortcutKeycaps } = await import("./tour-state");
    expect(shortcutKeycaps("", true)).toEqual([]);
    expect(shortcutKeycaps("++", true)).toEqual([]);
  });
});

describe("hotkey completion", () => {
  it("finishes only on the Summon step", () => {
    expect(hotkeyFinishesTour(tourAt(0))).toBe(false);
    expect(hotkeyFinishesTour(tourAt(1))).toBe(false);
    expect(hotkeyFinishesTour(tourAt(2))).toBe(false);
    expect(hotkeyFinishesTour(tourAt(3))).toBe(true);
  });

  it("never finishes a hidden tour", () => {
    const hidden = tourReducer(tourAt(3), {
      type: "permissions",
      action: { type: "completed", status: status({ pending: false }) },
    });
    expect(hotkeyFinishesTour(hidden)).toBe(false);
  });

  it("the reducer itself treats the press as a no-op (completion is an effect)", () => {
    const state = tourAt(3);
    expect(tourReducer(state, { type: "hotkey-pressed" })).toEqual(state);
  });
});
