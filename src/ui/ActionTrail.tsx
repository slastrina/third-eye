import { useEffect, useRef } from "react";
import "./hud.css";

export interface TrailItem {
  id: string;
  label: string;
  status: "running" | "ok" | "failed";
  /** Typed failure line shown under a failed action. */
  failure?: string | null;
}

/** The reactive action trail under the HUD pill: actions appear as the loop
 *  announces them (● running) and settle to ✓/✗ from their results. Honest by
 *  construction — only announced work is listed, never invented future steps. */
export function ActionTrail({ items }: { items: readonly TrailItem[] }) {
  // A progress display follows its newest line: when the list outgrows the
  // pill window's capped height, pin the scroll to the bottom on every new
  // entry (the fixed 360px window used to just CLIP the overflow).
  const listRef = useRef<HTMLOListElement | null>(null);
  useEffect(() => {
    const list = listRef.current;
    if (list) list.scrollTop = list.scrollHeight;
  }, [items.length]);
  if (items.length === 0) return null;
  return (
    <ol className="te-trail" ref={listRef}>
      {items.map((item) => (
        <li key={item.id} className="te-trail__item" data-status={item.status}>
          <span className="te-trail__mark" aria-hidden="true">
            {item.status === "ok" ? "✓" : item.status === "failed" ? "✗" : "●"}
          </span>
          <span className="te-trail__body">
            <span className="te-trail__label">{item.label}</span>
            {item.status === "failed" && item.failure ? (
              <span className="te-trail__failure">{item.failure}</span>
            ) : null}
          </span>
        </li>
      ))}
    </ol>
  );
}
