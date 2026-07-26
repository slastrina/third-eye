import type { ReactNode } from "react";
import "./chrome.css";

export interface ToastProps {
  children: ReactNode;
  /** `fixed` pins it bottom-center of the window (the design's placement);
   *  `inline` renders in flow for embedding/stories. Default fixed. */
  placement?: "fixed" | "inline";
  /** Dot color communicates tone; defaults to the green status dot. */
  tone?: "green" | "red";
}

/** Bottom-center status pill ("Exported Q3-report-final.pdf — 6 actions · 7 s").
 *  Lifecycle (timeouts, queuing) belongs to the caller; this is presentation. */
export function Toast({ children, placement = "fixed", tone = "green" }: ToastProps) {
  return (
    <div className="te-toast" data-placement={placement} data-tone={tone} role="status">
      <span className="te-toast__dot" aria-hidden="true" />
      {children}
    </div>
  );
}
