import { describe, expect, it } from "vitest";
import type { ToolCallPayload, ToolResultPayload } from "./chat";
import {
  TRAIL_MAX_AGE_MS,
  TRAIL_MAX_POINTS,
  appendTrailPoint,
  currentEntry,
  describeCall,
  fitContains,
  ghostTarget,
  hudHeadline,
  hudProgress,
  hudReducer,
  hudVisible,
  initialHudState,
  isClickEntry,
  nextUserControl,
  settledClickRipples,
  trailOpacity,
  type HudViewState,
} from "./hud-state";

const call = (id: string, name: string, args: object | string): { type: "tool-call"; payload: ToolCallPayload } => ({
  type: "tool-call",
  payload: {
    requestId: 1,
    round: 0,
    call: { id, name, arguments: typeof args === "string" ? args : JSON.stringify(args) },
  },
});

const result = (callId: string, ok: boolean, failure: string | null = null): { type: "tool-result"; payload: ToolResultPayload } => ({
  type: "tool-result",
  payload: { requestId: 1, round: 0, callId, name: "input_action", ok, resultCount: null, mode: null, failure },
});

const live = (): HudViewState => hudReducer(initialHudState, { type: "run-state", phase: "running" });

describe("describeCall", () => {
  it("labels coordinate clicks and exposes the ghost target from the same args", () => {
    const described = describeCall("input_action", JSON.stringify({ action: "mouse-click", x: 312, y: 208, button: "left" }));
    expect(described).toEqual({ label: "click · 312, 208", input: true, target: { x: 312, y: 208 } });
  });

  it("truncates typed text and never exposes a target for typing", () => {
    const described = describeCall("input_action", JSON.stringify({ action: "type-text", text: "a".repeat(40) }));
    expect(described.label).toBe(`type · “${"a".repeat(24)}…”`);
    expect(described.target).toBeNull();
  });

  it("labels the extended vocabulary: drag, scroll, multi-click, combos", () => {
    const drag = describeCall(
      "input_action",
      JSON.stringify({ action: "mouse-drag", button: "left", fromX: 1, fromY: 2, toX: 30, toY: 40 }),
    );
    expect(drag.label).toBe("drag · 1, 2 → 30, 40");
    expect(drag.target).toEqual({ x: 30, y: 40 });
    expect(
      describeCall("input_action", JSON.stringify({ action: "scroll", deltaY: 5 })).label,
    ).toBe("scroll · down");
    expect(
      describeCall(
        "input_action",
        JSON.stringify({ action: "mouse-click", x: 1, y: 2, clicks: 2 }),
      ).label,
    ).toBe("double-click · 1, 2");
    expect(
      describeCall(
        "input_action",
        JSON.stringify({ action: "key-press", key: "c", modifiers: ["cmd"] }),
      ).label,
    ).toBe("press · cmd+c");
  });

  it("labels non-input tools and survives malformed arguments", () => {
    expect(describeCall("screen_query", "{}").label).toBe("read the screen");
    expect(describeCall("memory_search", JSON.stringify({ query: "launch logs" })).label).toBe("recall · “launch logs”");
    const malformed = describeCall("input_action", "{not json");
    expect(malformed).toEqual({ label: "input action", input: true, target: null });
    expect(describeCall("some_new_tool", "{}").label).toBe("some new tool");
  });

  it("labels the workspace file tools by file name", () => {
    expect(describeCall("read_file", JSON.stringify({ path: "/ws/src/main.rs" })).label).toBe(
      "read · main.rs",
    );
    expect(describeCall("write_file", JSON.stringify({ path: "notes/a.txt", content: "x" })).label).toBe(
      "write · a.txt",
    );
    expect(describeCall("list_dir", "{}").label).toBe("list the workspace");
    expect(describeCall("list_dir", JSON.stringify({ path: "/ws/src" })).label).toBe("list · src");
  });
});

describe("run lifecycle", () => {
  it("running starts a fresh trail; a redundant running mid-run keeps entries", () => {
    let state = live();
    state = hudReducer(state, call("c1", "screen_query", {}));
    expect(hudReducer(state, { type: "run-state", phase: "running" }).entries).toHaveLength(1);
  });

  it("idle after a trailless run never flashes an empty HUD", () => {
    const state = hudReducer(live(), { type: "run-state", phase: "idle" });
    expect(state).toEqual(initialHudState);
    expect(hudVisible(state)).toBe(false);
  });

  it("idle after a trailing run lingers as done; dismiss clears", () => {
    let state = hudReducer(live(), call("c1", "screen_query", {}));
    state = hudReducer(state, result("c1", true));
    state = hudReducer(state, { type: "run-state", phase: "idle" });
    expect(state.phase).toBe("done");
    expect(hudVisible(state)).toBe(true);
    expect(hudReducer(state, { type: "dismiss" })).toEqual(initialHudState);
  });

  it("stopped is terminal-with-message and idle stays idle", () => {
    const state = hudReducer(hudReducer(live(), call("c1", "screen_query", {})), { type: "run-state", phase: "stopped" });
    expect(state.phase).toBe("stopped");
    expect(hudHeadline(state)).toBe("Stopped — keyboard & mouse are yours");
    expect(hudReducer(initialHudState, { type: "run-state", phase: "stopped" })).toEqual(initialHudState);
  });
});

describe("trail folding", () => {
  it("calls append running entries and results settle them in place", () => {
    let state = live();
    state = hudReducer(state, call("c1", "screen_query", {}));
    state = hudReducer(state, call("c2", "input_action", { action: "mouse-click", x: 10, y: 20 }));
    state = hudReducer(state, result("c1", true));
    state = hudReducer(state, result("c2", false, "verification-failed"));
    expect(state.entries.map((entry) => entry.status)).toEqual(["ok", "failed"]);
    expect(state.entries[1].failure).toBe("verification-failed");
  });

  it("ignores calls outside a live run and results for unknown callIds", () => {
    expect(hudReducer(initialHudState, call("c1", "screen_query", {})).entries).toHaveLength(0);
    const state = live();
    expect(hudReducer(state, result("ghost", true))).toEqual(state);
  });

  it("a replayed tool-call (same callId) folds exactly once", () => {
    // StrictMode double-fires effects and a double subscription would replay
    // events; the trail must not duplicate.
    let state = hudReducer(live(), call("c1", "screen_query", {}));
    state = hudReducer(state, call("c1", "screen_query", {}));
    expect(state.entries).toHaveLength(1);
  });
});

describe("derived views", () => {
  it("current entry, ghost target, headline, and progress track the running action", () => {
    let state = live();
    expect(hudHeadline(state)).toBe("thinking…");
    state = hudReducer(state, call("c1", "input_action", { action: "mouse-click", x: 312, y: 208 }));
    expect(currentEntry(state)?.callId).toBe("c1");
    expect(ghostTarget(state)).toEqual({ x: 312, y: 208 });
    expect(hudHeadline(state)).toBe("click · 312, 208");
    expect(hudProgress(state)).toBe("1 / 1");
    state = hudReducer(state, result("c1", true));
    // Settled: no stale ghost ring, pill back to the holding line.
    expect(ghostTarget(state)).toBeNull();
    expect(hudHeadline(state)).toBe("thinking…");
  });

  it("done headline counts failures honestly", () => {
    let state = hudReducer(live(), call("c1", "input_action", { action: "key-press", key: "return" }));
    state = hudReducer(state, result("c1", false, "no-grant"));
    state = hudReducer(state, { type: "run-state", phase: "idle" });
    expect(hudHeadline(state)).toBe("Done — 1 action failed");
  });

  it("ghost target only renders while live — done/stopped leave no ring", () => {
    let state = hudReducer(live(), call("c1", "input_action", { action: "mouse-move", x: 5, y: 6 }));
    expect(ghostTarget(state)).toEqual({ x: 5, y: 6 });
    state = hudReducer(state, { type: "run-state", phase: "stopped" });
    expect(ghostTarget(state)).toBeNull();
  });
});


describe("approval mirroring in the HUD", () => {
  const request = {
    approvalId: 9,
    kind: "run-command" as const,
    summary: "Run command: curl -s ifconfig.me",
  };

  it("folds a request once, keeps it across a run start, clears on resolved", () => {
    let state = hudReducer(initialHudState, { type: "hid-approval", request });
    state = hudReducer(state, { type: "hid-approval", request });
    expect(state.hidApprovals).toHaveLength(1);
    // A (re)start of the run must not drop a parked ask.
    state = hudReducer(state, { type: "run-state", phase: "running" });
    expect(state.hidApprovals).toHaveLength(1);
    state = hudReducer(state, { type: "approval-resolved", approvalId: 9 });
    expect(state.hidApprovals).toHaveLength(0);
  });

  it("pending approvals alone make the pill visible (gated focus_app case)", async () => {
    const { hudApprovalsPending } = await import("./hud-state");
    const state = hudReducer(initialHudState, { type: "hid-approval", request });
    expect(hudApprovalsPending(state)).toBe(true);
    expect(hudApprovalsPending(initialHudState)).toBe(false);
  });

  it("resolved clears across both queues by id", () => {
    let state = hudReducer(initialHudState, { type: "hid-approval", request });
    state = hudReducer(state, {
      type: "mcp-approval",
      request: { approvalId: 10, toolName: "mcp__files__write", summary: "write x" },
    });
    state = hudReducer(state, { type: "approval-resolved", approvalId: 10 });
    expect(state.hidApprovals).toHaveLength(1);
    expect(state.mcpApprovals).toHaveLength(0);
  });
});

describe("cursor motion trail (canvas animation)", () => {
  it("folds samples, skipping idle duplicates and expiring old points", () => {
    let trail = appendTrailPoint([], { x: 10, y: 10, t: 1000 });
    trail = appendTrailPoint(trail, { x: 10, y: 10, t: 1033 });
    expect(trail).toHaveLength(1);
    trail = appendTrailPoint(trail, { x: 40, y: 22, t: 1066 });
    expect(trail).toHaveLength(2);
    // The first point ages out once TRAIL_MAX_AGE_MS passes.
    trail = appendTrailPoint(trail, { x: 60, y: 30, t: 1000 + TRAIL_MAX_AGE_MS + 1 });
    expect(trail.map((p) => p.x)).toEqual([40, 60]);
  });

  it("caps the retained points at the newest TRAIL_MAX_POINTS", () => {
    let trail: { x: number; y: number; t: number }[] = [];
    for (let i = 0; i < TRAIL_MAX_POINTS + 8; i++) {
      trail = appendTrailPoint(trail, { x: i, y: 0, t: 1000 + i });
    }
    expect(trail).toHaveLength(TRAIL_MAX_POINTS);
    expect(trail[trail.length - 1].x).toBe(TRAIL_MAX_POINTS + 7);
  });

  it("fades opacity linearly from fresh to expiry", () => {
    const p = { x: 0, y: 0, t: 1000 };
    expect(trailOpacity(p, 1000)).toBe(1);
    expect(trailOpacity(p, 1000 + TRAIL_MAX_AGE_MS / 2)).toBeCloseTo(0.5);
    expect(trailOpacity(p, 1000 + TRAIL_MAX_AGE_MS)).toBe(0);
  });
});

describe("click ripples (canvas animation)", () => {
  const clickEntry = (
    callId: string,
    status: "running" | "ok" | "failed",
    label = "click · 300, 200",
  ) => ({
    callId,
    name: "input_action",
    label,
    input: true,
    target: { x: 300, y: 200 },
    status,
    failure: null,
  });

  it("classifies single/double/triple clicks and nothing else", () => {
    expect(isClickEntry(clickEntry("a", "ok"))).toBe(true);
    expect(isClickEntry(clickEntry("a", "ok", "double-click · 3, 4"))).toBe(true);
    expect(isClickEntry(clickEntry("a", "ok", "triple-click"))).toBe(true);
    expect(isClickEntry(clickEntry("a", "ok", "move · 3, 4"))).toBe(false);
    expect(isClickEntry({ ...clickEntry("a", "ok"), name: "screen_query" })).toBe(false);
  });

  it("bursts exactly when a click settles, colored by outcome", () => {
    const prev = [clickEntry("c1", "running")];
    const settledOk = settledClickRipples(prev, [clickEntry("c1", "ok")]);
    expect(settledOk).toEqual([{ callId: "c1", x: 300, y: 200, ok: true }]);
    const settledBad = settledClickRipples(prev, [clickEntry("c1", "failed")]);
    expect(settledBad[0].ok).toBe(false);
    // Already-settled entries never re-burst (replayed events, StrictMode).
    expect(settledClickRipples([clickEntry("c1", "ok")], [clickEntry("c1", "ok")])).toEqual([]);
    // A still-running click has not settled.
    expect(settledClickRipples(prev, prev)).toEqual([]);
  });

  it("ignores settling non-click actions and targetless clicks", () => {
    const move = { ...clickEntry("m1", "running"), label: "move · 1, 2" };
    expect(settledClickRipples([move], [{ ...move, status: "ok" as const }])).toEqual([]);
    const bare = { ...clickEntry("b1", "running"), target: null };
    expect(settledClickRipples([bare], [{ ...bare, status: "ok" as const }])).toEqual([]);
  });
});

describe("canvas fit containment (follower-offset fix)", () => {
  const fit = { originX: 1512, originY: 0, width: 1920, height: 1080 };

  it("contains points on the fitted monitor and rejects the rest", () => {
    expect(fitContains(fit, 1512, 0)).toBe(true);
    expect(fitContains(fit, 3000, 500)).toBe(true);
    // Right/bottom edges are exclusive; the primary monitor is outside.
    expect(fitContains(fit, 1512 + 1920, 500)).toBe(false);
    expect(fitContains(fit, 700, 400)).toBe(false);
  });

  it("a missing or degenerate fit contains nothing — forcing a (re)fit", () => {
    expect(fitContains(null, 100, 100)).toBe(false);
    expect(fitContains({ originX: 0, originY: 0, width: 0, height: 0 }, 0, 0)).toBe(false);
  });
});

describe("user-takes-the-mouse hand-off (follower hiding)", () => {
  it("movement between actions sets the flag; Third Eye acting clears it", () => {
    // User nudges the mouse while nothing is running → user control.
    expect(nextUserControl({ x: 100, y: 100 }, { x: 140, y: 108 }, false, false)).toBe(true);
    // The flag is sticky while the pointer rests between actions.
    expect(nextUserControl({ x: 140, y: 108 }, { x: 140, y: 108 }, false, true)).toBe(true);
    // The moment an input action runs, movement is Third Eye's — flag clears.
    expect(nextUserControl({ x: 140, y: 108 }, { x: 600, y: 400 }, true, true)).toBe(false);
  });

  it("readback jitter below the threshold never claims user control", () => {
    expect(
      nextUserControl({ x: 100, y: 100 }, { x: 102, y: 99 }, false, false),
    ).toBe(false);
    // First sample (no previous) proves nothing either.
    expect(nextUserControl(null, { x: 500, y: 500 }, false, false)).toBe(false);
  });
});
