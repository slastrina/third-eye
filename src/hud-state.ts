// Pure state for the live automation HUD (2026-07 redesign, surface 7): what
// the hud-pill and hud-canvas windows render while a chat run executes tool
// calls. Folds the EXISTING event contracts — llm://run-state,
// llm://tool-call, llm://tool-result (chat.ts) — into an honest, reactive
// action trail: entries appear as the loop announces them and settle ✓/✗ from
// their results. No upfront plan is invented (the real loop is reactive; the
// prototype's future-step checklist was demo staging — no-fake-data rule).
//
// The reducer is Tauri-free; Hud.tsx is glue that subscribes and dispatches.

import { describeCall } from "./action-labels";

export { describeCall };

import type {
  HidApprovalRequest,
  McpApprovalRequest,
  RunPhase,
  ToolCallPayload,
  ToolResultPayload,
} from "./chat";

/** One announced tool call in the trail. */
export interface HudEntry {
  callId: string;
  /** Tool name ("input_action", "screen_query", "memory_search", …). */
  name: string;
  /** Human label for the pill/trail ("click · 312, 208", "read the screen"). */
  label: string;
  /** True for input_action calls — the ones that hold keyboard/mouse. */
  input: boolean;
  /** Screen-point target of a coordinate-bearing mouse action, for the
   *  canvas's ghost indicator. Same points the click itself uses — the
   *  arguments are the source, so indicator and action cannot disagree. */
  target: { x: number; y: number } | null;
  status: "running" | "ok" | "failed";
  /** Typed failure line from the result (ok: false). */
  failure: string | null;
}

/** HUD lifecycle. `done` lingers after a natural finish (the pill shows
 *  "Done" until dismissed on a timer); `stopped` after a user Stop. */
export type HudPhase = "idle" | "live" | "done" | "stopped";

export interface HudViewState {
  phase: HudPhase;
  entries: HudEntry[];
  /** Pending approval prompts, mirrored into the hud-pill window so the
   *  user sees them even when the overlay is hidden mid-run. Cleared by the
   *  backend's approval-resolved broadcast (answered anywhere / timed out). */
  hidApprovals: HidApprovalRequest[];
  mcpApprovals: McpApprovalRequest[];
}

export const initialHudState: HudViewState = {
  phase: "idle",
  entries: [],
  hidApprovals: [],
  mcpApprovals: [],
};

export type HudAction =
  | { type: "run-state"; phase: RunPhase }
  | { type: "tool-call"; payload: ToolCallPayload }
  | { type: "tool-result"; payload: ToolResultPayload }
  // The linger timer fired (or the surface unmounted) — back to idle.
  | { type: "dismiss" }
  | { type: "hid-approval"; request: HidApprovalRequest }
  | { type: "mcp-approval"; request: McpApprovalRequest }
  // The backend's resolved broadcast: answered in any window, or timed out.
  | { type: "approval-resolved"; approvalId: number };


export function hudReducer(state: HudViewState, action: HudAction): HudViewState {
  switch (action.type) {
    case "run-state":
      switch (action.phase) {
        case "running":
          // A new run starts a fresh trail; a redundant "running" mid-run
          // (mount query racing the broadcast) must not clear live entries.
          // Parked approvals carry over — they clear via approval-resolved.
          return state.phase === "live"
            ? state
            : {
                ...initialHudState,
                phase: "live",
                hidApprovals: state.hidApprovals,
                mcpApprovals: state.mcpApprovals,
              };
        case "stopped":
          return state.phase === "idle" ? state : { ...state, phase: "stopped" };
        case "idle":
          // Natural finish: linger as "done" only if the run showed anything —
          // a toolless chat answer never flashes an empty HUD.
          if (state.phase !== "live") return state;
          return state.entries.length > 0 ? { ...state, phase: "done" } : initialHudState;
      }
      return state;
    case "tool-call": {
      if (state.phase !== "live") return state;
      const { call } = action.payload;
      // Call ids are unique per run; a repeat means a replayed event (double
      // subscription, StrictMode re-fire) — folding it again would corrupt
      // the trail's counts, so it is ignored.
      if (state.entries.some((entry) => entry.callId === call.id)) return state;
      const described = describeCall(call.name, call.arguments);
      const entry: HudEntry = {
        callId: call.id,
        name: call.name,
        label: described.label,
        input: described.input,
        target: described.target,
        status: "running",
        failure: null,
      };
      return { ...state, entries: [...state.entries, entry] };
    }
    case "tool-result": {
      const { callId, ok, failure } = action.payload;
      if (!state.entries.some((entry) => entry.callId === callId)) return state;
      return {
        ...state,
        entries: state.entries.map((entry) =>
          entry.callId === callId
            ? { ...entry, status: ok ? "ok" : "failed", failure: failure ?? null }
            : entry,
        ),
      };
    }
    case "hid-approval":
      if (state.hidApprovals.some((r) => r.approvalId === action.request.approvalId)) return state;
      return { ...state, hidApprovals: [...state.hidApprovals, action.request] };
    case "mcp-approval":
      if (state.mcpApprovals.some((r) => r.approvalId === action.request.approvalId)) return state;
      return { ...state, mcpApprovals: [...state.mcpApprovals, action.request] };
    case "approval-resolved":
      return {
        ...state,
        hidApprovals: state.hidApprovals.filter((r) => r.approvalId !== action.approvalId),
        mcpApprovals: state.mcpApprovals.filter((r) => r.approvalId !== action.approvalId),
      };
    case "dismiss":
      return initialHudState;
    default:
      return state;
  }
}

/** One sampled point of the real cursor's recent path, for the canvas's
 *  motion trail (the design's ghost-cursor streak). */
export interface TrailPoint {
  x: number;
  y: number;
  /** Sample time (ms epoch) — drives the fade-out. */
  t: number;
}

/** How long a trail point stays visible. */
export const TRAIL_MAX_AGE_MS = 550;

/** Hard cap on retained trail points (a long slow glide must not grow an
 *  unbounded list between prunes). */
export const TRAIL_MAX_POINTS = 32;

/** Fold one cursor sample into the trail: identical consecutive samples are
 *  skipped (an idle cursor leaves no streak), expired points fall off, and
 *  the list is capped. Pure — the canvas calls it on every poll tick. */
export function appendTrailPoint(
  points: TrailPoint[],
  sample: TrailPoint,
): TrailPoint[] {
  const last = points[points.length - 1];
  const next =
    last && last.x === sample.x && last.y === sample.y
      ? points
      : [...points, sample];
  return pruneTrail(next, sample.t);
}

/** Drop expired points and enforce the cap (newest kept). */
export function pruneTrail(points: TrailPoint[], nowMs: number): TrailPoint[] {
  const alive = points.filter((p) => nowMs - p.t <= TRAIL_MAX_AGE_MS);
  return alive.length > TRAIL_MAX_POINTS ? alive.slice(alive.length - TRAIL_MAX_POINTS) : alive;
}

/** A trail point's opacity at `nowMs`: 1 fresh → 0 at expiry. */
export function trailOpacity(point: TrailPoint, nowMs: number): number {
  const age = nowMs - point.t;
  if (age <= 0) return 1;
  if (age >= TRAIL_MAX_AGE_MS) return 0;
  return 1 - age / TRAIL_MAX_AGE_MS;
}

/** Whether an entry is a mouse click (single/double/triple) — the actions
 *  that burst a ripple at their target when they settle. */
export function isClickEntry(entry: HudEntry): boolean {
  return entry.name === "input_action" && /^(double-|triple-)?click/.test(entry.label);
}

/** One click-ripple burst parked on the canvas until its animation ends. */
export interface ClickRipple {
  callId: string;
  x: number;
  y: number;
  /** Failed clicks ripple in the failure color — honesty in the animation. */
  ok: boolean;
}

/** Diff two entry lists and return the clicks that JUST settled (running →
 *  ok/failed) with a known target — each becomes one ripple burst. Pure so
 *  the double-fire risk (StrictMode, replayed events) is testable. */
export function settledClickRipples(
  prev: HudEntry[],
  next: HudEntry[],
): ClickRipple[] {
  const wasRunning = new Set(
    prev.filter((e) => e.status === "running").map((e) => e.callId),
  );
  return next
    .filter(
      (e) =>
        e.status !== "running" &&
        wasRunning.has(e.callId) &&
        isClickEntry(e) &&
        e.target !== null,
    )
    .map((e) => ({
      callId: e.callId,
      x: e.target!.x,
      y: e.target!.y,
      ok: e.status === "ok",
    }));
}

/** The hud-canvas fit: the monitor the canvas window currently covers, in
 *  logical screen points (hud.rs HudCanvasFit, camelCase). */
export interface CanvasFit {
  originX: number;
  originY: number;
  width: number;
  height: number;
}

/** Cursor movement bigger than this between ~33ms samples while NO input
 *  action is running means a human hand is on the mouse. Small enough to
 *  catch a gentle nudge, big enough to ignore readback jitter. */
export const USER_CONTROL_THRESHOLD_PX = 3;

/** Fold one cursor sample into the "the user took the mouse" flag:
 *  - while an input action is RUNNING, movement is Third Eye's own
 *    synthesis — the flag clears (Third Eye visibly holds the pointer);
 *  - between actions, movement beyond the jitter threshold sets it —
 *    the follower dot/annotation hide until Third Eye acts again.
 *  Pure so the hand-off policy is testable without a mouse. */
export function nextUserControl(
  prev: { x: number; y: number } | null,
  next: { x: number; y: number },
  actionRunning: boolean,
  current: boolean,
): boolean {
  if (actionRunning) return false;
  if (
    prev !== null &&
    (Math.abs(next.x - prev.x) > USER_CONTROL_THRESHOLD_PX ||
      Math.abs(next.y - prev.y) > USER_CONTROL_THRESHOLD_PX)
  ) {
    return true;
  }
  return current;
}

/** Whether a global screen point is inside the fitted monitor. A null fit
 *  (nothing fit yet this mount) or a degenerate one (the backend's
 *  no-monitor fallback) contains nothing — the caller must (re)fit before
 *  rendering global coordinates against its origin. This is the
 *  follower-offset fix: the badge used to subtract a stale/zero origin
 *  whenever a run had no coordinate target (approval prompts). */
export function fitContains(fit: CanvasFit | null, x: number, y: number): boolean {
  if (fit === null || fit.width <= 0 || fit.height <= 0) return false;
  return (
    x >= fit.originX &&
    x < fit.originX + fit.width &&
    y >= fit.originY &&
    y < fit.originY + fit.height
  );
}

/** Whether any approval is parked — the pill must surface these even when
 *  no input action has been announced yet (e.g. a gated focus_app). */
export function hudApprovalsPending(state: HudViewState): boolean {
  return state.hidApprovals.length > 0 || state.mcpApprovals.length > 0;
}

/** Whether the pill window has anything truthful to show. */
export function hudVisible(state: HudViewState): boolean {
  return state.phase === "live" || ((state.phase === "done" || state.phase === "stopped") && state.entries.length > 0);
}

/** The entry currently executing, if any (drives the pill label + ghost). */
export function currentEntry(state: HudViewState): HudEntry | null {
  if (state.phase !== "live") return null;
  for (let i = state.entries.length - 1; i >= 0; i--) {
    if (state.entries[i].status === "running") return state.entries[i];
  }
  return null;
}

/** The ghost-indicator target: only while live, only for the action being
 *  executed right now — a settled click leaves no stale ring behind. */
export function ghostTarget(state: HudViewState): { x: number; y: number } | null {
  return currentEntry(state)?.target ?? null;
}

/** The pill's headline. Live: the current action (or a holding line between
 *  calls); done/stopped: the terminal message. */
export function hudHeadline(state: HudViewState): string {
  switch (state.phase) {
    case "live":
      return currentEntry(state)?.label ?? "thinking…";
    case "done": {
      const failed = state.entries.filter((entry) => entry.status === "failed").length;
      return failed > 0 ? `Done — ${failed} action${failed === 1 ? "" : "s"} failed` : "Done";
    }
    case "stopped":
      return "Stopped — keyboard & mouse are yours";
    case "idle":
      return "";
  }
}

/** Progress fraction text for the pill ("3 / 4" equivalent): settled/total.
 *  Total is only what has been announced — no invented future steps. */
export function hudProgress(state: HudViewState): string {
  if (state.entries.length === 0) return "";
  const settled = state.entries.filter((entry) => entry.status !== "running").length;
  return `${Math.min(settled + 1, state.entries.length)} / ${state.entries.length}`;
}
