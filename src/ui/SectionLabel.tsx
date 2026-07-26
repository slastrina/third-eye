import type { ReactNode } from "react";
import "./ui.css";

/** Tracked-caps micro heading ("PAUSE WATCHING", "FROM YOUR MEMORY"). */
export function SectionLabel({ children }: { children: ReactNode }) {
  return <div className="te-seclabel">{children}</div>;
}
