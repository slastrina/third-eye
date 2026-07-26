import "./chrome.css";

/** The eye's four moods (spec: tray icon, palette, HUD, tour).
 *  - `watching`: green iris, still pupil — passively observing
 *  - `thinking`: green iris, scanning pupil — palette open / working
 *  - `acting`: amber iris, scanning pupil — hands on keyboard & mouse
 *  - `closed`: lid arc only — paused, nothing is observed */
export type EyeState = "watching" | "thinking" | "acting" | "closed";

export interface EyeIconProps {
  state: EyeState;
  /** Rendered width in px; height follows the 48:32 viewBox. Default 34. */
  size?: number;
  /** Outline color. Defaults to currentColor so the surface decides. */
  stroke?: string;
}

/** The Third Eye mark. Pure SVG, animates the pupil via the shared te-scan
 *  keyframes when thinking/acting. */
export function EyeIcon({ state, size = 34, stroke = "currentColor" }: EyeIconProps) {
  const height = (size * 32) / 48;
  if (state === "closed") {
    return (
      <svg width={size} height={height} viewBox="0 0 48 32" aria-hidden="true" className="te-eye">
        <path
          d="M4 14 C13 25 35 25 44 14"
          fill="none"
          stroke={stroke}
          strokeOpacity={0.7}
          strokeWidth={3}
          strokeLinecap="round"
        />
      </svg>
    );
  }
  const iris = state === "acting" ? "var(--te-amber)" : "var(--te-green)";
  const scanning = state === "thinking" || state === "acting";
  return (
    <svg width={size} height={height} viewBox="0 0 48 32" aria-hidden="true" className="te-eye">
      <path
        d="M4 16 C13 5 35 5 44 16 C35 27 13 27 4 16 Z"
        fill="none"
        stroke={stroke}
        strokeWidth={3}
        strokeLinejoin="round"
      />
      <circle cx={24} cy={16} r={8} fill={iris} />
      <circle cx={24} cy={16} r={3.4} fill="var(--te-navy)" className={scanning ? "te-eye__pupil--scan" : undefined} />
    </svg>
  );
}
