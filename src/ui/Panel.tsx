import type { HTMLAttributes } from "react";
import "./chrome.css";

export interface PanelProps extends HTMLAttributes<HTMLDivElement> {
  /** `glass` is the translucent blurred chrome (palette, tray panel, HUD);
   *  `solid` the opaque navy window body (memory, settings). */
  variant?: "glass" | "solid";
  /** Accent border: green for the summon palette, amber while acting. */
  accent?: "none" | "green" | "amber";
}

/** Dark panel chrome: rounded, hairline border, deep shadow. */
export function Panel({ variant = "glass", accent = "none", className, ...rest }: PanelProps) {
  const classes = ["te-panel", `te-panel--${variant}`, accent !== "none" && `te-panel--${accent}`, className]
    .filter(Boolean)
    .join(" ");
  return <div className={classes} {...rest} />;
}
