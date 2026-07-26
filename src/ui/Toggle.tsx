import type { ButtonHTMLAttributes } from "react";
import "./ui.css";
import "./controls.css";

export interface ToggleProps
  extends Omit<ButtonHTMLAttributes<HTMLButtonElement>, "onChange" | "onClick"> {
  on: boolean;
  onChange: (on: boolean) => void;
  /** Visible label to the right of the switch; omit for externally-labeled rows. */
  label?: string;
  /** Accessible name when no visible label is given. */
  ariaLabel?: string;
  disabled?: boolean;
}

/** The 38×22 pill switch (Auto-routing, Watching is on). Controlled only.
 *  Extra button attributes (data-* hooks, ids) pass through. */
export function Toggle({ on, onChange, label, ariaLabel, disabled, ...rest }: ToggleProps) {
  return (
    <button
      type="button"
      role="switch"
      aria-checked={on}
      aria-label={label ? undefined : ariaLabel}
      className="te-toggle"
      data-on={on || undefined}
      disabled={disabled}
      onClick={() => onChange(!on)}
      {...rest}
    >
      <span className="te-toggle__track" aria-hidden="true">
        <span className="te-toggle__knob" />
      </span>
      {label && <span className="te-toggle__label">{label}</span>}
    </button>
  );
}
