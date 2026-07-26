import type { ButtonHTMLAttributes } from "react";
import "./ui.css";

export type ButtonVariant = "primary" | "outline" | "accent" | "danger" | "ghost";

export interface ButtonProps extends ButtonHTMLAttributes<HTMLButtonElement> {
  variant?: ButtonVariant;
}

/** Pill button. `primary` is the solid-green CTA (Continue, Resume watching);
 *  `outline` the neutral secondary (Back, Memory); `accent` the green-tinted
 *  emphasis (Summon ⌥␣); `danger` the red stop (Stop · esc); `ghost` the bare
 *  text action (Skip tour). Surface adaptation comes from the `.te-light`
 *  scope, not props. */
export function Button({ variant = "outline", className, type, ...rest }: ButtonProps) {
  const classes = ["te-btn", `te-btn--${variant}`, className].filter(Boolean).join(" ");
  return <button type={type ?? "button"} className={classes} {...rest} />;
}
