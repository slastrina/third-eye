// HUD webviews (2026-07 redesign, surface 7). Two windows share this module:
// `?view=hud-pill` renders the status pill + action trail (interactive: the
// Stop button), `?view=hud-canvas` the full-monitor click-through layer with
// the ghost target ring. Both fold the SAME global llm:// broadcasts through
// hud-state; only the pill drives show_hud/hide_hud (single driver — the
// canvas is passive, hud.rs contract).
import { useEffect, useReducer, useRef } from "react";
import { invoke } from "@tauri-apps/api/core";
import {
  onLlmToolCall,
  onLlmToolResult,
  onRunState,
  runState,
  stopChat,
} from "./chat";
import {
  currentEntry,
  ghostTarget,
  hudHeadline,
  hudProgress,
  hudReducer,
  hudVisible,
  initialHudState,
} from "./hud-state";
import { ActionTrail } from "./ui/ActionTrail";
import { GhostIndicator } from "./ui/GhostIndicator";
import { HudPill } from "./ui/HudPill";
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
  // an input_action; earlier non-input calls are already in the trail then.
  const shouldShow = hudVisible(hud) && hud.entries.some((entry) => entry.input);
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

  if (!target) return null;
  const entry = currentEntry(hud);
  const isClick = entry?.name === "input_action" && entry.label.startsWith("click");
  return (
    <div className="hud-canvas-root">
      <GhostIndicator
        x={target.x - origin.originX}
        y={target.y - origin.originY}
        click={isClick}
      />
    </div>
  );
}
