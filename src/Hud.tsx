// HUD webviews (2026-07 redesign, surface 7). Two windows share this module:
// `?view=hud-pill` renders the status pill + action trail (interactive: the
// Stop button), `?view=hud-canvas` the full-monitor click-through layer with
// the ghost target ring. Both fold the SAME global llm:// broadcasts through
// hud-state; only the pill drives show_hud/hide_hud (single driver — the
// canvas is passive, hud.rs contract).
import { useEffect, useReducer, useRef, useState } from "react";
import { getCurrentWindow } from "@tauri-apps/api/window";
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
  fitContains,
  ghostTarget,
  hudApprovalsPending,
  hudHeadline,
  hudProgress,
  hudReducer,
  hudVisible,
  initialHudState,
  isClickEntry,
  nextUserControl,
  pruneTrail,
  settledClickRipples,
  trailOpacity,
} from "./hud-state";
import type { TrailPoint, ClickRipple, CanvasFit } from "./hud-state";
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
    <div
      className="hud-pill-root"
      data-translucent={(live && !hudApprovalsPending(hud)) || undefined}
    >
      <HudPill
        tone={hud.phase === "live" ? "acting" : hud.phase === "stopped" ? "stopped" : "done"}
        headline={hudHeadline(hud)}
        progress={live ? hudProgress(hud) : ""}
        onGrab={(event) => {
          // Drag handle (user request 2026-08-02): the pill moves the whole
          // HUD stack when it covers something (an OS permission dialog, a
          // form). Primary button only; buttons inside keep their clicks.
          if (event.button !== 0) return;
          if ((event.target as HTMLElement).closest("button")) return;
          getCurrentWindow()
            .startDragging()
            .catch((err) => console.debug("hud: startDragging no-op:", err));
        }}
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
          onAllowForever={() => answerHid(request.approvalId, "allow-always")}
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
  // The hand-off flag: true while the USER is driving the mouse — the
  // follower dot/annotation and trail hide (annotating a pointer Third Eye
  // does not hold is noise), and reappear when Third Eye acts again.
  const [userControl, setUserControl] = useState(false);
  const userControlRef = useRef(false);
  const prevCursorRef = useRef<{ x: number; y: number } | null>(null);
  const runningRef = useRef(false);
  runningRef.current = currentEntry(hud)?.input === true;
  useEffect(() => {
    if (!followerActive) {
      setCursor(null);
      setTrail([]);
      setUserControl(false);
      userControlRef.current = false;
      prevCursorRef.current = null;
      return;
    }
    let cancelled = false;
    const timer = window.setInterval(() => {
      invoke<{ x: number; y: number } | null>("cursor_position").then(
        (point) => {
          if (cancelled) return;
          setCursor(point);
          const now = Date.now();
          if (point) {
            const drove = nextUserControl(
              prevCursorRef.current,
              point,
              runningRef.current,
              // Read through the setter to avoid a stale closure.
              userControlRef.current,
            );
            userControlRef.current = drove;
            setUserControl(drove);
            prevCursorRef.current = point;
            setTrail((points) =>
              drove ? pruneTrail(points, now) : appendTrailPoint(points, { x: point.x, y: point.y, t: now }),
            );
          } else {
            setTrail((points) => pruneTrail(points, now));
          }
        },
        () => {},
      );
    }, 33);
    return () => {
      cancelled = true;
      window.clearInterval(timer);
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
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
  // The canvas fit: null until the window has actually been positioned this
  // mount. Rendering global coordinates against a guessed origin is the
  // follower-offset bug — with no fit, nothing coordinate-bearing renders.
  const [fit, setFit] = useState<CanvasFit | null>(null);
  const fitInFlight = useRef(false);
  const requestFit = (x: number, y: number) => {
    if (fitInFlight.current) return;
    fitInFlight.current = true;
    invoke<CanvasFit>("fit_hud_canvas", { x, y }).then(
      (next) => {
        fitInFlight.current = false;
        setFit(next);
      },
      (err) => {
        fitInFlight.current = false;
        console.debug("hud: fit_hud_canvas unavailable:", err);
        // No Tauri runtime (browser/e2e) — render against a zero origin
        // covering everything rather than nothing. Inside the app the
        // invoke succeeds and real bounds replace this.
        setFit((current) =>
          current ?? { originX: 0, originY: 0, width: Number.MAX_SAFE_INTEGER, height: Number.MAX_SAFE_INTEGER },
        );
      },
    );
  };
  const targetKey = target ? `${target.x},${target.y}` : null;
  useEffect(() => {
    if (!target) return;
    requestFit(target.x, target.y);
    // Keyed on the coordinate pair — a settled/replaced action with the same
    // target must not re-fit.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [targetKey]);
  // No target to anchor on (approval prompts, keyboard-only stretches): the
  // canvas follows the CURSOR's monitor instead, so the follower badge and
  // trail always render against the real window origin.
  useEffect(() => {
    if (target || !cursor) return;
    if (!fitContains(fit, cursor.x, cursor.y)) requestFit(cursor.x, cursor.y);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [target, cursor, fit]);
  const origin = { originX: fit?.originX ?? 0, originY: fit?.originY ?? 0 };

  const entry = currentEntry(hud);
  if (!target && !cursor && ripples.length === 0 && trail.length === 0) return null;
  // Never draw global coordinates against an unknown window origin.
  if (fit === null) return null;
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
      {cursor && !userControl && (
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
