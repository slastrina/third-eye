// UI side of the overlay IPC surface defined in src-tauri/src/overlay/mod.rs.
// The event name and state strings are the contract; keep them in sync with
// STATE_CHANGED_EVENT and OverlayState's kebab-case serialization.

import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

export type OverlayState = "hidden" | "visible-idle" | "visible-focused";

export const STATE_CHANGED_EVENT = "overlay://state-changed";

/** Subscribe to overlay state pushes from Rust. Resolves to an unlisten fn. */
export function onOverlayStateChanged(
  callback: (state: OverlayState) => void,
): Promise<UnlistenFn> {
  return listen<OverlayState>(STATE_CHANGED_EVENT, (event) => callback(event.payload));
}

export function showOverlay(): Promise<OverlayState> {
  return invoke<OverlayState>("show_overlay");
}

export function hideOverlay(): Promise<OverlayState> {
  return invoke<OverlayState>("hide_overlay");
}

export function focusOverlay(): Promise<OverlayState> {
  return invoke<OverlayState>("focus_overlay");
}
