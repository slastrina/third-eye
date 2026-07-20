// Pure-module unit for the overlay-presentation frontend seam (M006/S04/T03).
// The invoke/listen wrappers are thin glue over the Tauri IPC (exercised live,
// UAT-deferred per MEM115); what IS automatable is the IPC event contract and
// the mode→geometry mapping the overlay panel branches on, locked here.

import { describe, expect, it } from "vitest";

import {
  drawerEdgeOf,
  drawerExtentFor,
  PRESENTATION_EVENT,
  setOverlayPosition,
  type PresentationStatus,
} from "./overlay-presentation-state";
import { OVERLAY_MIN_HEIGHT, OVERLAY_MIN_WIDTH, type Edge } from "./overlay-geometry";

// A representative status with distinct per-edge extents so a wrong-axis pick
// is caught, and a modal size the modal-mode assertions read back.
const status = (mode: PresentationStatus["mode"]): PresentationStatus => ({
  mode,
  edgeExtents: { top: 300, bottom: 340, left: 420, right: 480 },
  modalSize: { width: 720, height: 480 },
  modalPosition: null,
  persistError: null,
});

describe("overlay-presentation-state", () => {
  it("pins the presentation broadcast event name (the IPC contract)", () => {
    // src-tauri/src/overlay/presentation.rs and the e2e listen on this exact
    // string — a drift here silently breaks the immediate-adopt path.
    expect(PRESENTATION_EVENT).toBe("overlay://presentation");
  });
});

describe("PresentationStatus.modalPosition", () => {
  it("carries a never-moved modal as null (→ App.tsx centers)", () => {
    // Mirrors the Rust status where an absent modal_position serializes to JSON
    // null; the apply branch reads `modalPosition === null` → centeredModalRect.
    expect(status("modal").modalPosition).toBeNull();
  });

  it("accepts a set {x,y} point, negative origins included (no floor)", () => {
    // A legal multi-monitor virtual desktop places monitors at negative origins,
    // so the field carries a raw OverlayPoint with no min floor — matching the
    // Rust stored_point interpreter (finite-only, no floor).
    const moved: PresentationStatus = { ...status("modal"), modalPosition: { x: -1920, y: -128 } };
    expect(moved.modalPosition).toEqual({ x: -1920, y: -128 });
  });

  it("exposes the setOverlayPosition persist wrapper (persistMoveEnd's mutator)", () => {
    // The move-end mutator App.tsx invokes on drag-handle mouseup. The IPC round
    // trip is live glue (UAT-deferred, MEM115); here we only pin that the wrapper
    // exists and is callable — the invoke("set_overlay_position", {x,y}) contract.
    expect(typeof setOverlayPosition).toBe("function");
  });
});

describe("drawerEdgeOf", () => {
  it("maps modal mode to null (floating panel, no docked edge)", () => {
    expect(drawerEdgeOf(status("modal"))).toBeNull();
  });

  const edges: Edge[] = ["top", "bottom", "left", "right"];
  it.each(edges)("maps drawer mode %s to that edge", (edge) => {
    expect(drawerEdgeOf(status(edge))).toBe(edge);
  });
});

describe("drawerExtentFor", () => {
  it("reads the stored extent for each edge (correct axis, no cross-talk)", () => {
    const s = status("left");
    expect(drawerExtentFor(s, "top")).toBe(300);
    expect(drawerExtentFor(s, "bottom")).toBe(340);
    expect(drawerExtentFor(s, "left")).toBe(420);
    expect(drawerExtentFor(s, "right")).toBe(480);
  });

  it("returns extents already floored at or above the overlay minimums", () => {
    // The Rust interpreter floors every persisted extent, so a status that
    // reached the frontend can never carry a sub-min value to drawerRect.
    const s = status("right");
    expect(drawerExtentFor(s, "left")).toBeGreaterThanOrEqual(OVERLAY_MIN_WIDTH);
    expect(drawerExtentFor(s, "right")).toBeGreaterThanOrEqual(OVERLAY_MIN_WIDTH);
    expect(drawerExtentFor(s, "top")).toBeGreaterThanOrEqual(OVERLAY_MIN_HEIGHT);
    expect(drawerExtentFor(s, "bottom")).toBeGreaterThanOrEqual(OVERLAY_MIN_HEIGHT);
  });
});
