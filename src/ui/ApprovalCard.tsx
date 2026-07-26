// The approval card: one pending gate decision (HID action, run_command, or
// MCP tool call) awaiting the user's verdict. Shared by every surface that
// can show it — the overlay chat panel and the hud-pill window — so the
// prompt reaches the user wherever they're looking (the overlay may be
// hidden mid-run). Styling rides the app-level .approval-prompt classes
// (styles.css, loaded by every window via main.tsx).

export interface ApprovalCardProps {
  /** Card heading ("Third Eye wants to act", "External tool: …"). */
  title: string;
  /** The exact pending action — command line / action summary. */
  summary: string;
  /** Label for the session-wide grant button. */
  alwaysLabel?: string;
  onAllowOnce: () => void;
  onAllowAlways: () => void;
  /** Permanent grant (persisted; revocable in Settings). Omitted on cards
   *  whose gate has no forever semantics (MCP tools) — no button renders. */
  onAllowForever?: () => void;
  onDeny: () => void;
}

export function ApprovalCard({
  title,
  summary,
  alwaysLabel = "This session",
  onAllowOnce,
  onAllowAlways,
  onAllowForever,
  onDeny,
}: ApprovalCardProps) {
  return (
    <div className="approval-prompt" role="alertdialog" aria-label={title}>
      <div className="approval-prompt-text">
        <strong>{title}</strong>
        <span className="approval-prompt-summary">{summary}</span>
      </div>
      <div className="approval-prompt-actions">
        <button type="button" className="approval-allow" onClick={onAllowOnce}>
          Allow once
        </button>
        <button type="button" className="approval-always" onClick={onAllowAlways}>
          {alwaysLabel}
        </button>
        {onAllowForever && (
          <button
            type="button"
            className="approval-always approval-forever"
            title="Never ask again for this kind of action (change in Settings)"
            onClick={onAllowForever}
          >
            Always
          </button>
        )}
        <button type="button" className="approval-deny" onClick={onDeny}>
          Deny
        </button>
      </div>
    </div>
  );
}
