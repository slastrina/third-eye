import "./chrome.css";

export interface StepIndicatorProps {
  /** 0-based current step. */
  current: number;
  labels: readonly string[];
}

/** Tour progress: a dot + label per step, connected by hairlines. Completed
 *  and current steps light up green; upcoming steps stay muted. */
export function StepIndicator({ current, labels }: StepIndicatorProps) {
  return (
    <ol className="te-steps" aria-label={`Step ${current + 1} of ${labels.length}`}>
      {labels.map((label, index) => {
        const status = index < current ? "done" : index === current ? "current" : "todo";
        return (
          <li key={label} className="te-steps__step" data-status={status} aria-current={status === "current" ? "step" : undefined}>
            <span className="te-steps__dot" aria-hidden="true">
              {status === "done" ? "✓" : ""}
            </span>
            <span className="te-steps__label">{label}</span>
          </li>
        );
      })}
    </ol>
  );
}
