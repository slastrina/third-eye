import type { ReactNode } from "react";
import "./ui.css";
import "./controls.css";

export interface RadioCardOption<T extends string> {
  value: T;
  label: ReactNode;
  /** Secondary line under the label. */
  sub?: ReactNode;
}

export interface RadioCardsProps<T extends string> {
  options: readonly RadioCardOption<T>[];
  value: T;
  onChange: (value: T) => void;
  /** Group label for assistive tech, e.g. "What is stored". */
  label: string;
}

/** Bordered selectable cards with a ring dot (storage mode, confirm mode).
 *  A real radiogroup: roving selection, arrow keys via native focus order. */
export function RadioCards<T extends string>({ options, value, onChange, label }: RadioCardsProps<T>) {
  return (
    <div role="radiogroup" aria-label={label} className="te-radiocards">
      {options.map((option) => {
        const selected = option.value === value;
        return (
          <button
            key={option.value}
            type="button"
            role="radio"
            aria-checked={selected}
            className="te-radiocard"
            data-selected={selected || undefined}
            onClick={() => onChange(option.value)}
          >
            <span className="te-radiocard__ring" aria-hidden="true" />
            <span className="te-radiocard__body">
              <span className="te-radiocard__label">{option.label}</span>
              {option.sub && <span className="te-radiocard__sub">{option.sub}</span>}
            </span>
          </button>
        );
      })}
    </div>
  );
}
