// HUD webviews (2026-07 redesign, surface 7). Two windows share this module:
// `?view=hud-pill` renders the status pill + action trail (interactive: the
// Stop button), `?view=hud-canvas` the full-monitor click-through layer with
// the ghost target ring. Both fold the SAME global llm:// broadcasts through
// hud-state; only the pill drives show_hud/hide_hud (single driver — the
// canvas is passive, hud.rs contract).
import { useEffect, useReducer, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import {
  onHidApprovalRequest,
  onHidApprovalResolved,
  onLlmToolCall,
  onLlmToolResult,
  onMcpApprovalRequest,
  onMcpApprovalResolved,
  onRunState,
  respondHidApproval,
  respondMcpApproval,
  runState,
  stopChat,
  type ApprovalVerdict,
  type McpApprovalVerdict,
} from "./chat";
import {
  appendTrailPoint,
  currentEntry,
  ghostTarget,
  hudApprovalsPending,
  hudHeadline,
  hudProgress,
  hudReducer,
  hudVisible,
  initialHudState,
  isClickEntry,
  pruneTrail,
  settledClickRipples,
  trailOpacity,
} from "./hud-state";
import type { TrailPoint, ClickRipple } from "./hud-state";
import { ActionTrail } from "./ui/ActionTrail";
import { GhostIndicator } from "./ui/GhostIndicator";
import { HudPill } from "./ui/HudPill";
import { ApprovalCard } from "./ui/ApprovalCard";
import "./hud.css";

/** How long the terminal pill (Done / Stopped) lingers before dismissing. */
const LINGER_MS = 2600;

function useHudState() {
  const [hud, dispatch] = useReducer(hudReducer, initialHudState);
  useEffect(() => {
    let cancelled = false;
    // TEST HOOK (?edge=/?tour= precedent): outside Tauri no llm:// events can
    // arrive, so `?hud=seed` replays a small scripted run through the SAME
    // reducer, letting Playwright assert the pill/trail/ghost DOM. The live
    // subscriptions below are still registered; inside the real app this
    // param is never present.
    if (new URLSearchParams(window.location.search).get("hud") === "seed") {
      dispatch({ type: "run-state", phase: "running" });
      const seedCall = (id: string, name: string, args: object) =>
        dispatch({
          type: "tool-call",
          payload: { requestId: 1, round: 0, call: { id, name, arguments: JSON.stringify(args) } },
        });
      const seedResult = (callId: string, ok: boolean, failure: string | null) =>
        dispatch({
          type: "tool-result",
          payload: { requestId: 1, round: 0, callId, name: "input_action", ok, resultCount: null, mode: null, failure },
        });
      seedCall("s1", "screen_query", {});
      seedResult("s1", true, null);
      seedCall("s2", "input_action", { action: "mouse-click", x: 220, y: 180, button: "left" });
      seedResult("s2", false, "verification-failed");
      seedCall("s3", "input_action", { action: "mouse-click", x: 226, y: 184, button: "left" });
    }
    // Mount query so a HUD webview that boots mid-run is truthful before the
    // next broadcast (same posture as the overlay's Stop control).
    runState().then(
      (payload) => {
        if (!cancelled) dispatch({ type: "run-state", phase: payload.phase });
      },
      () => {
        // Outside Tauri: stay idle — the HUD windows only exist in the app.
      },
    );
    const unlistens = [
      onRunState((payload) => dispatch({ type: "run-state", phase: payload.phase })),
      onLlmToolCall((payload) => dispatch({ type: "tool-call", payload })),
      onLlmToolResult((payload) => dispatch({ type: "tool-result", payload })),
      // Approval prompts mirror into the HUD so a hidden overlay never
      // leaves an action parked invisibly; resolutions (answered anywhere,
      // or timed out) clear the card.
      onHidApprovalRequest((request) => dispatch({ type: "hid-approval", request })),
      onMcpApprovalRequest((request) => dispatch({ type: "mcp-approval", request })),
      onHidApprovalResolved((payload) =>
        dispatch({ type: "approval-resolved", approvalId: payload.approvalId }),
      ),
      onMcpApprovalResolved((payload) =>
        dispatch({ type: "approval-resolved", approvalId: payload.approvalId }),
      ),
    ];
    unlistens.forEach((u) => {
      u.catch((err) => console.error("hud: event subscription failed:", err));
    });
    return () => {
      cancelled = true;
      unlistens.forEach((u) => u.then((f) => f()));
    };
  }, []);

  // Terminal linger: Done/Stopped stays readable briefly, then dismisses.
  useEffect(() => {
    if (hud.phase !== "done" && hud.phase !== "stopped") return;
    const timer = window.setTimeout(() => dispatch({ type: "dismiss" }), LINGER_MS);
    return () => window.clearTimeout(timer);
  }, [hud.phase]);

  return hud;
}

/** The interactive pill window. Owns show_hud/hide_hud: shown the moment the
 *  trail first carries an input action (the HUD narrates held input — a
 *  glance-only run stays out of the way), hidden when the state returns to
 *  idle. */
export function HudPillView() {
  const hud = useHudState();
  // Input transparency is the HUD's job: pill appears once the run announces
  // an input_action — or the moment ANY approval parks (a gated focus_app
  // has no input entry yet, but the user must still see the ask).
  const shouldShow =
    (hudVisible(hud) && hud.entries.some((entry) => entry.input)) || hudApprovalsPending(hud);
  const shownRef = useRef(false);
  useEffect(() => {
    if (shouldShow === shownRef.current) return;
    shownRef.current = shouldShow;
    invoke(shouldShow ? "show_hud" : "hide_hud").catch((err) =>
      console.warn(`hud: ${shouldShow ? "show" : "hide"}_hud failed:`, err),
    );
  }, [shouldShow]);

  if (!shouldShow) return null;
  const live = hud.phase === "live";
  // Fire-and-forget verdicts; the backend's resolved broadcast clears the
  // card in every window (including this one).
  const answerHid = (approvalId: number, verdict: ApprovalVerdict) => {
    respondHidApproval(approvalId, verdict).catch((err) =>
      console.warn("hud: respond_hid_approval failed:", err),
    );
  };
  const answerMcp = (approvalId: number, verdict: McpApprovalVerdict) => {
    respondMcpApproval(approvalId, verdict).catch((err) =>
      console.warn("hud: respond_mcp_approval failed:", err),
    );
  };
  return (
    <div className="hud-pill-root">
      <HudPill
        tone={hud.phase === "live" ? "acting" : hud.phase === "stopped" ? "stopped" : "done"}
        headline={hudHeadline(hud)}
        progress={live ? hudProgress(hud) : ""}
        onStop={
          live
            ? () => {
                stopChat().catch((err) => console.warn("hud: stop_chat failed:", err));
              }
            : undefined
        }
      />
      {hud.hidApprovals.map((request) => (
        <ApprovalCard
          key={request.approvalId}
          title="Third Eye wants to act"
          summary={request.summary}
          onAllowOnce={() => answerHid(request.approvalId, "allow-once")}
          onAllowAlways={() => answerHid(request.approvalId, "allow-kind")}
          onDeny={() => answerHid(request.approvalId, "deny")}
        />
      ))}
      {hud.mcpApprovals.map((request) => (
        <ApprovalCard
          key={request.approvalId}
          title={`External tool: ${request.toolName}`}
          summary={request.summary}
          onAllowOnce={() => answerMcp(request.approvalId, "allow-once")}
          onAllowAlways={() => answerMcp(request.approvalId, "allow-tool")}
          onDeny={() => answerMcp(request.approvalId, "deny")}
        />
      ))}
      <ActionTrail
        items={hud.entries.map((entry) => ({
          id: entry.callId,
          label: entry.label,
          status: entry.status,
          failure: entry.failure,
        }))}
      />
    </div>
  );
}

/** The canvas fit reply — the monitor origin to subtract from absolute
 *  screen points (hud.rs HudCanvasFit). */
interface HudCanvasFit {
  originX: number;
  originY: number;
}

/** The passive click-through canvas window: the ghost ring at the current
 *  input action's target. Multi-monitor: each new target re-fits the canvas
 *  over the monitor containing it (fit_hud_canvas) and the returned origin
 *  converts absolute screen points to window coordinates. Renders nothing
 *  between coordinate actions. */
export function HudCanvasView() {
  const hud = useHudState();
  const target = ghostTarget(hud);
  // The follower badge rides the REAL cursor while Third Eye holds input —
  // the design's ghost-cursor companion. Fed by a light cursor_position
  // poll (~30Hz) only while a live run has input activity.
  const [cursor, setCursor] = useState<{ x: number; y: number } | null>(null);
  // The motion trail: recent real-cursor samples, fading out (the design's
  // ghost-cursor streak). Folded on every poll tick with the pure helpers.
  const [trail, setTrail] = useState<TrailPoint[]>([]);
  const followerActive =
    hud.phase === "live" && hud.entries.some((entry) => entry.input);
  useEffect(() => {
    if (!followerActive) {
      setCursor(null);
      setTrail([]);
      return;
    }
    let cancelled = false;
    const timer = window.setInterval(() => {
      invoke<{ x: number; y: number } | null>("cursor_position").then(
        (point) => {
          if (cancelled) return;
          setCursor(point);
          const now = Date.now();
          setTrail((points) =>
            point ? appendTrailPoint(points, { x: point.x, y: point.y, t: now }) : pruneTrail(points, now),
          );
        },
        () => {},
      );
    }, 33);
    return () => {
      cancelled = true;
      window.clearInterval(timer);
    };
  }, [followerActive]);
  // Click ripples: burst at the exact target the moment a click settles —
  // green-lit amber for ok, failure color for a failed click. Diffed with
  // the pure helper so replayed events cannot double-burst.
  const [ripples, setRipples] = useState<ClickRipple[]>([]);
  const prevEntriesRef = useRef<typeof hud.entries>([]);
  useEffect(() => {
    const burst = settledClickRipples(prevEntriesRef.current, hud.entries);
    prevEntriesRef.current = hud.entries;
    if (burst.length === 0) return;
    setRipples((r) => [...r, ...burst]);
    const ids = burst.map((b) => b.callId);
    const timer = window.setTimeout(
      () => setRipples((r) => r.filter((x) => !ids.includes(x.callId))),
      700,
    );
    return () => window.clearTimeout(timer);
  }, [hud.entries]);
  const [origin, setOrigin] = useReducer(
    (_prev: HudCanvasFit, next: HudCanvasFit) => next,
    { originX: 0, originY: 0 },
  );
  const targetKey = target ? `${target.x},${target.y}` : null;
  useEffect(() => {
    if (!target) return;
    invoke<HudCanvasFit>("fit_hud_canvas", { x: target.x, y: target.y }).then(
      (fit) => setOrigin(fit),
      (err) => console.debug("hud: fit_hud_canvas unavailable:", err),
    );
    // Keyed on the coordinate pair — a settled/replaced action with the same
    // target must not re-fit.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [targetKey]);

  const entry = currentEntry(hud);
  if (!target && !cursor && ripples.length === 0 && trail.length === 0) return null;
  const isClick = entry !== null && isClickEntry(entry);
  const now = Date.now();
  return (
    <div className="hud-canvas-root">
      {trail.map((point) => (
        <span
          key={`${point.x},${point.y},${point.t}`}
          className="hud-trail-dot"
          style={{
            left: point.x - origin.originX,
            top: point.y - origin.originY,
            opacity: trailOpacity(point, now),
          }}
          aria-hidden="true"
        />
      ))}
      {ripples.map((ripple) => (
        <span
          key={ripple.callId}
          className="hud-click-ripple"
          data-failed={ripple.ok ? undefined : "true"}
          style={{ left: ripple.x - origin.originX, top: ripple.y - origin.originY }}
          aria-hidden="true"
        />
      ))}
      {target && (
        <GhostIndicator
          x={target.x - origin.originX}
          y={target.y - origin.originY}
          click={isClick}
        />
      )}
      {cursor && (
        <div
          className="hud-follower"
          style={{ left: cursor.x - origin.originX, top: cursor.y - origin.originY }}
          aria-hidden="true"
        >
          <span className="hud-follower-dot" />
          <span className="hud-follower-badge">
            {entry ? entry.label : "Third Eye"}
          </span>
        </div>
      )}
    </div>
  );
}
