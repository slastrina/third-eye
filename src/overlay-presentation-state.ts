// UI side of the overlay-presentation IPC surface (M006/S04), defined in
// src-tauri/src/overlay/presentation.rs + src-tauri/src/config.rs. The overlay
// webview owns the geometry ACLs (setSize/setPosition/currentMonitor), so it is
// the sole applier: it reads `overlay_presentation` on mount to restore the
// persisted shape ("restores after relaunch") and subscribes to the
// `overlay://presentation` broadcast to adopt any change immediately. The
// settings webview (T04) only invokes the mutators and listens — it never moves
// the window (the ACL split).
//
// The shapes here mirror the serde camelCase serialization of Rust's
// PresentationStatus / OverlayPresentation / EdgeExtents / OverlaySizeConfig —
// a change on either side is a breaking IPC change, pinned Rust-side by
// `presentation::tests::status_is_health_as_value_camelcase`. Pure helpers
// (drawerEdgeOf / drawerExtentFor) live here so the mode→geometry mapping is
// unit-testable without a Tauri runtime (src/overlay-presentation-state.test.ts);
// App.tsx is only glue. Kebab-case module name per MEM051.

import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

import type { Edge, OverlayPoint } from "./overlay-geometry";

/** Presentation broadcast: every presentation mutation emits the resulting
 *  PresentationStatus app-wide so the overlay webview applies the new geometry
 *  and any other window (Settings) stays truthful. Pinned Rust-side by
 *  `presentation::tests::event_name_is_the_ipc_contract`. */
export const PRESENTATION_EVENT = "overlay://presentation";

/** The active presentation: a free `modal` window or a drawer docked flush
 *  against one display edge. Kebab-case over IPC, mirroring Rust's
 *  PresentationMode serde tags (a drawer edge reuses the `Edge` union). */
export type PresentationMode = "modal" | Edge;

/** Per-edge drawer extents (logical px): width for left/right, height for
 *  top/bottom. All four travel together so a mode switch restores that edge's
 *  last preferred extent (D040). */
export interface EdgeExtents {
  top: number;
  bottom: number;
  left: number;
  right: number;
}

/** A logical-pixel size for the modal (free) presentation. */
export interface OverlaySizeConfig {
  width: number;
  height: number;
}

/** Queryable presentation state (health-as-value, R007): the whole persisted
 *  record flattened (`mode` + `edgeExtents` + `modalSize`) plus a
 *  `persistError` carrying the most recent save failure so a change that could
 *  not be persisted stays visible after the fact (never an IPC rejection) —
 *  the same shape as CloudOptInStatus. */
export interface PresentationStatus {
  mode: PresentationMode;
  edgeExtents: EdgeExtents;
  modalSize: OverlaySizeConfig;
  /** The remembered modal top-left (LOGICAL px), or `null` when the modal has
   *  never been dragged — mirroring Rust's `modal_position: Option<...>` flattened
   *  onto the status (serializes as an `{x,y}` object or JSON null). `null` and an
   *  off-screen point both route the App.tsx apply branch to `centeredModalRect`;
   *  a set, on-screen point is restored via `setPosition`. Carries NO floor
   *  because a legal multi-monitor origin may be negative (`OverlayPoint`). */
  modalPosition: OverlayPoint | null;
  persistError: string | null;
}

// ---------------------------------------------------------------------------
// Invoke wrappers
// ---------------------------------------------------------------------------

/** Current presentation — health-as-value beside `overlay` state (R007): a
 *  value at any time, never an error. The overlay webview reads this on mount
 *  to restore the persisted shape. Rejects only outside a Tauri runtime, where
 *  the caller absorbs it and the panel stays in its default (modal) DOM. */
export function overlayPresentation(): Promise<PresentationStatus> {
  return invoke<PresentationStatus>("overlay_presentation");
}

/** Set the presentation mode (keeps every stored extent so the switch restores
 *  that edge's remembered size). Never rejects backend-side: a persist failure
 *  rides `persistError` on the returned status. */
export function setOverlayPresentation(mode: PresentationMode): Promise<PresentationStatus> {
  return invoke<PresentationStatus>("set_overlay_presentation", { mode });
}

/** Set the active mode's extent from a live `(width, height)` logical size —
 *  the resize-end / Settings surface. A drawer edge persists only its relevant
 *  axis (the backend floors + selects it); modal persists both. Never rejects
 *  backend-side: a persist failure rides `persistError`. */
export function setOverlayExtent(
  mode: PresentationMode,
  width: number,
  height: number,
): Promise<PresentationStatus> {
  return invoke<PresentationStatus>("set_overlay_extent", { mode, width, height });
}

/** Persist a live modal top-left `(x, y)` LOGICAL position — the move-end /
 *  drag-handle mouseup surface. Mirrors `setOverlayExtent`: the backend persists
 *  + broadcasts but NEVER moves the window (the ACL split — only the overlay
 *  webview applies geometry). No floor is applied: a legal multi-monitor origin
 *  may be negative. Never rejects backend-side: a persist failure rides
 *  `persistError` on the returned status. */
export function setOverlayPosition(x: number, y: number): Promise<PresentationStatus> {
  return invoke<PresentationStatus>("set_overlay_position", { x, y });
}

/** Subscribe to the app-wide presentation broadcast (`overlay://presentation`).
 *  Resolves to an unlisten fn. */
export function onOverlayPresentation(
  cb: (status: PresentationStatus) => void,
): Promise<UnlistenFn> {
  return listen<PresentationStatus>(PRESENTATION_EVENT, (e) => cb(e.payload));
}

// ---------------------------------------------------------------------------
// Pure mode→geometry helpers
// ---------------------------------------------------------------------------

/** The docked drawer edge for a status, or null in modal mode. This is the
 *  single source of the `drawerEdge` the overlay panel branches on — replacing
 *  the retired `?edge=` query read — so drawer vs floating DOM/geometry follows
 *  the persisted config rather than a diverging URL source. */
export function drawerEdgeOf(status: PresentationStatus): Edge | null {
  return status.mode === "modal" ? null : status.mode;
}

/** The variable extent (logical px) stored for a drawer `edge`: width for
 *  left/right, height for top/bottom. Feeds `drawerRect` when the overlay
 *  webview applies the snap. Every stored extent is already floored ≥ the
 *  overlay minimum by the Rust interpreter, so this never returns a sub-min
 *  value. */
export function drawerExtentFor(status: PresentationStatus, edge: Edge): number {
  return status.edgeExtents[edge];
}
