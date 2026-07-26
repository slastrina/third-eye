import { describe, expect, it } from "vitest";
import type { ToolCallPayload, ToolResultPayload } from "./chat";
import {
  currentEntry,
  describeCall,
  ghostTarget,
  hudHeadline,
  hudProgress,
  hudReducer,
  hudVisible,
  initialHudState,
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

  it("labels non-input tools and survives malformed arguments", () => {
    expect(describeCall("screen_query", "{}").label).toBe("read the screen");
    expect(describeCall("memory_search", JSON.stringify({ query: "launch logs" })).label).toBe("recall · “launch logs”");
    const malformed = describeCall("input_action", "{not json");
    expect(malformed).toEqual({ label: "input action", input: true, target: null });
    expect(describeCall("some_new_tool", "{}").label).toBe("some new tool");
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
