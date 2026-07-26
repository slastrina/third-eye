import type { ReactNode } from "react";
import "./ui.css";

export type ChipTone = "neutral" | "selected" | "amber" | "dashed";

interface ChipBaseProps {
  tone?: ChipTone;
  children: ReactNode;
  className?: string;
}

export interface ChipProps extends ChipBaseProps {
  /** Present makes the chip a clickable button; absent renders a static span. */
  onClick?: () => void;
  /** Present adds a nested ✕ remove button (never watch lists, attachments). */
  onRemove?: () => void;
  /** Accessible label for the ✕ button, e.g. "Remove 1Password". */
  removeLabel?: string;
  /** For clickable single-select chips: exposed as aria-pressed. */
  pressed?: boolean;
}

/** Pill chip: static badge, clickable option, or removable tag. */
export function Chip({
  tone = "neutral",
  children,
  className,
  onClick,
  onRemove,
  removeLabel,
  pressed,
}: ChipProps) {
  const classes = ["te-chip", tone !== "neutral" && `te-chip--${tone}`, className]
    .filter(Boolean)
    .join(" ");
  const removeBtn = onRemove && (
    <button
      type="button"
      className="te-chip__remove"
      aria-label={removeLabel ?? "Remove"}
      onClick={(event) => {
        event.stopPropagation();
        onRemove();
      }}
    >
      ✕
    </button>
  );
  if (onClick) {
    return (
      <button type="button" className={classes} onClick={onClick} aria-pressed={pressed}>
        {children}
        {removeBtn}
      </button>
    );
  }
  return (
    <span className={classes}>
      {children}
      {removeBtn}
    </span>
  );
}

export interface ChoiceChipsProps<T extends string> {
  options: readonly { value: T; label: string }[];
  value: T;
  onChange: (value: T) => void;
  /** Group label for assistive tech, e.g. "Keep memory for". */
  label: string;
}

/** Single-select chip row (memory retention, pause durations). */
export function ChoiceChips<T extends string>({ options, value, onChange, label }: ChoiceChipsProps<T>) {
  return (
    <div role="group" aria-label={label} style={{ display: "flex", gap: 8, flexWrap: "wrap" }}>
      {options.map((option) => (
        <Chip
          key={option.value}
          tone={option.value === value ? "selected" : "neutral"}
          pressed={option.value === value}
          onClick={() => onChange(option.value)}
        >
          {option.label}
        </Chip>
      ))}
    </div>
  );
}
