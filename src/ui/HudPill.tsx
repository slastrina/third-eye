import type { MouseEvent as ReactMouseEvent } from "react";
import { EyeIcon, type EyeState } from "./EyeIcon";
import "./hud.css";

export type HudPillTone = "acting" | "done" | "stopped";

export interface HudPillProps {
  tone: HudPillTone;
  /** Current action label / terminal message (hud-state's headline). */
  headline: string;
  /** Announced-only progress ("2 / 3"); empty hides the counter. */
  progress?: string;
  /** Present while a run is live — renders the Stop control. */
  onStop?: () => void;
  /** Mouse-down on the pill body (not its buttons): the drag handle that
   *  moves the whole HUD stack when it covers something the user needs. */
  onGrab?: (event: ReactMouseEvent) => void;
}

/** The run-status pill (top-center of the hud-pill window): amber scanning
 *  eye while Third Eye holds input, green on done, red on stopped. */
export function HudPill({ tone, headline, progress, onStop, onGrab }: HudPillProps) {
  const eye: EyeState = tone === "acting" ? "acting" : "watching";
  return (
    <div className="te-hudpill" data-tone={tone} onMouseDown={onGrab}>
      <EyeIcon state={tone === "stopped" ? "closed" : eye} size={26} stroke="#ffffff" />
      <span className="te-hudpill__headline">{headline}</span>
      {progress ? <span className="te-hudpill__count">{progress}</span> : null}
      {onStop && (
        <button type="button" className="te-hudpill__stop" onClick={onStop}>
          Stop · esc
        </button>
      )}
    </div>
  );
}
