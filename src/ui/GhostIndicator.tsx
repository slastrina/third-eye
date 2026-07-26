import "./hud.css";

export interface GhostIndicatorProps {
  /** Screen-point position within the canvas window's coordinate space. */
  x: number;
  y: number;
  /** Pulses the click ripple (mouse-click actions). */
  click?: boolean;
  /** Badge text under the marker. */
  label?: string;
}

/** The labeled target marker the hud-canvas draws at the point an input
 *  action is aimed at — Third Eye showing its hands. Pure absolute-positioned
 *  presentation; the canvas window is click-through, so pointer-events stay
 *  off and the marker can never intercept the real click it annotates. */
export function GhostIndicator({ x, y, click = false, label = "Third Eye" }: GhostIndicatorProps) {
  return (
    <div className="te-ghost" style={{ left: x, top: y }} aria-hidden="true">
      {click && <span className="te-ghost__ripple" />}
      <span className="te-ghost__ring" />
      <span className="te-ghost__badge">{label}</span>
    </div>
  );
}
