// Pure-module unit for the overlay geometry helpers (M006/S01/T03). The live
// proof — that dragging/resizing the nonactivating NSPanel never activates the
// app or voids click-through — is native and manual-only (RESEARCH Open Risks,
// MEM115); what IS automatable is the resize-direction contract and the
// size-clamp floor, locked here in the existing pure-module test idiom.

import { describe, expect, it } from "vitest";

import {
  centeredModalRect,
  clampMinSize,
  drawerRect,
  extentFromSize,
  isOnScreen,
  OVERLAY_MIN_HEIGHT,
  OVERLAY_MIN_WIDTH,
  RESIZE_GRIP_DIRECTION,
  resizeDirectionForEdge,
  type Edge,
  type MonitorBounds,
  type WorkArea,
} from "./overlay-geometry";

describe("overlay-geometry", () => {
  it("aims the resize grip at the bottom-right corner (SouthEast)", () => {
    // The literal must match Tauri's ResizeDirection so startResizeDragging
    // accepts it without a cast; a widened string would break the call site.
    expect(RESIZE_GRIP_DIRECTION).toBe("SouthEast");
  });

  it("leaves a size at or above the minimum untouched", () => {
    const size = { width: 640, height: 480 };
    expect(clampMinSize(size)).toEqual({ width: 640, height: 480 });
  });

  it("raises a sub-minimum width up to the floor, per axis", () => {
    const clamped = clampMinSize({ width: 100, height: 480 });
    expect(clamped.width).toBe(OVERLAY_MIN_WIDTH);
    expect(clamped.height).toBe(480);
  });

  it("raises a sub-minimum height up to the floor, per axis", () => {
    const clamped = clampMinSize({ width: 640, height: 10 });
    expect(clamped.width).toBe(640);
    expect(clamped.height).toBe(OVERLAY_MIN_HEIGHT);
  });

  it("clamps both axes when both are below the floor", () => {
    expect(clampMinSize({ width: 0, height: 0 })).toEqual({
      width: OVERLAY_MIN_WIDTH,
      height: OVERLAY_MIN_HEIGHT,
    });
  });

  it("treats the exact minimum as valid (boundary, not below)", () => {
    const atFloor = { width: OVERLAY_MIN_WIDTH, height: OVERLAY_MIN_HEIGHT };
    expect(clampMinSize(atFloor)).toEqual(atFloor);
  });

  it("honours a caller-supplied minimum over the defaults", () => {
    const clamped = clampMinSize(
      { width: 200, height: 200 },
      { width: 300, height: 150 },
    );
    expect(clamped).toEqual({ width: 300, height: 200 });
  });
});

describe("drawerRect", () => {
  // A primary display at origin (0,0) with a menu bar: the work area starts
  // BELOW the menu bar (y=25) and is 25px shorter than the full display.
  // scaleFactor 1 keeps physical == logical so edge anchoring is readable
  // without the conversion in play.
  const primary: WorkArea = {
    position: { x: 0, y: 25 },
    size: { width: 1440, height: 875 },
    scaleFactor: 1,
  };

  it("anchors a left drawer flush to the work-area origin at full height", () => {
    expect(drawerRect(primary, "left", 400)).toEqual({
      x: 0,
      y: 25,
      width: 400,
      height: 875,
    });
  });

  it("anchors a right drawer flush to the work-area right edge at full height", () => {
    // x = origin.x + areaWidth - width = 0 + 1440 - 400
    expect(drawerRect(primary, "right", 400)).toEqual({
      x: 1040,
      y: 25,
      width: 400,
      height: 875,
    });
  });

  it("anchors a top drawer below the menu bar at full width", () => {
    // The top drawer sits at the WORK-AREA origin (y=25), not the display top
    // (y=0) — that is the menu-bar/notch offset this slice exists to honour.
    expect(drawerRect(primary, "top", 200)).toEqual({
      x: 0,
      y: 25,
      width: 1440,
      height: 200,
    });
  });

  it("anchors a bottom drawer flush to the work-area bottom edge at full width", () => {
    // y = origin.y + areaHeight - height = 25 + 875 - 200 = 700
    expect(drawerRect(primary, "bottom", 200)).toEqual({
      x: 0,
      y: 700,
      width: 1440,
      height: 200,
    });
  });

  it("spans the full work-area HEIGHT for left/right drawers", () => {
    expect(drawerRect(primary, "left", 400).height).toBe(875);
    expect(drawerRect(primary, "right", 400).height).toBe(875);
  });

  it("spans the full work-area WIDTH for top/bottom drawers", () => {
    expect(drawerRect(primary, "top", 200).width).toBe(1440);
    expect(drawerRect(primary, "bottom", 200).width).toBe(1440);
  });

  it("anchors off a non-zero work-area origin (secondary / offset monitor)", () => {
    // A second display to the right of the primary: its work area does NOT start
    // at (0,0). A left drawer must land on THAT monitor, not at the virtual-desktop
    // origin, or the window lands off-screen (RESEARCH Constraints).
    const secondary: WorkArea = {
      position: { x: 1440, y: 25 },
      size: { width: 1920, height: 1055 },
      scaleFactor: 1,
    };
    expect(drawerRect(secondary, "left", 500)).toEqual({
      x: 1440,
      y: 25,
      width: 500,
      height: 1055,
    });
    // Right drawer on the secondary: x = 1440 + 1920 - 500 = 2860.
    expect(drawerRect(secondary, "right", 500)).toEqual({
      x: 2860,
      y: 25,
      width: 500,
      height: 1055,
    });
  });

  it("converts a Retina scaleFactor:2 work area from physical to logical once", () => {
    // Physical 2880x1800 display, menu bar at physical y=50, work area 1750 tall.
    // Everything divides by 2 into logical points; `extent` is ALREADY logical
    // and is NOT scaled. This is the pixel-vs-point boundary the slice locks.
    const retina: WorkArea = {
      position: { x: 0, y: 50 },
      size: { width: 2880, height: 1750 },
      scaleFactor: 2,
    };
    expect(drawerRect(retina, "left", 400)).toEqual({
      x: 0,
      y: 25, // 50 / 2
      width: 400, // logical extent, unscaled
      height: 875, // 1750 / 2
    });
    // Right drawer: x = (0/2) + (2880/2) - 400 = 1440 - 400 = 1040.
    expect(drawerRect(retina, "right", 400)).toEqual({
      x: 1040,
      y: 25,
      width: 400,
      height: 875,
    });
    // Bottom drawer: y = (50/2) + (1750/2) - 200 = 25 + 875 - 200 = 700.
    expect(drawerRect(retina, "bottom", 200)).toEqual({
      x: 0,
      y: 700,
      width: 1440,
      height: 200,
    });
  });

  it("floors a sub-minimum width extent on left/right drawers", () => {
    const left = drawerRect(primary, "left", 10);
    expect(left.width).toBe(OVERLAY_MIN_WIDTH);
    // Right drawer with a floored width still anchors flush to the right edge.
    const right = drawerRect(primary, "right", 10);
    expect(right.width).toBe(OVERLAY_MIN_WIDTH);
    expect(right.x).toBe(1440 - OVERLAY_MIN_WIDTH);
  });

  it("floors a sub-minimum height extent on top/bottom drawers", () => {
    const top = drawerRect(primary, "top", 10);
    expect(top.height).toBe(OVERLAY_MIN_HEIGHT);
    const bottom = drawerRect(primary, "bottom", 10);
    expect(bottom.height).toBe(OVERLAY_MIN_HEIGHT);
    // Bottom drawer with a floored height still sits flush to the bottom edge.
    expect(bottom.y).toBe(25 + 875 - OVERLAY_MIN_HEIGHT);
  });
});

describe("isOnScreen", () => {
  // A single primary display at the virtual-desktop origin, scaleFactor 1 so
  // physical == logical and the bounds read directly.
  const primary: MonitorBounds = {
    position: { x: 0, y: 0 },
    size: { width: 1440, height: 900 },
    scaleFactor: 1,
  };

  it("passes a point comfortably inside the only monitor", () => {
    expect(isOnScreen({ x: 700, y: 400 }, [primary])).toBe(true);
  });

  it("fails a point on a since-removed monitor (the SC4 off-screen half)", () => {
    // The point was saved against a second monitor to the right; that monitor is
    // unplugged, so `availableMonitors()` now returns only the primary and the
    // finite-but-off-screen point must fail → caller centers instead.
    expect(isOnScreen({ x: 2500, y: 400 }, [primary])).toBe(false);
  });

  it("passes a negative-origin point on a legal multi-monitor desktop", () => {
    // A second display LEFT of the primary sits at a negative virtual-desktop
    // origin — a legitimate arrangement the no-floor `stored_point` preserves.
    // A point on it must be recognised as on-screen, not repaired away.
    const leftOfPrimary: MonitorBounds = {
      position: { x: -1920, y: 0 },
      size: { width: 1920, height: 1080 },
      scaleFactor: 1,
    };
    expect(isOnScreen({ x: -1000, y: 500 }, [primary, leftOfPrimary])).toBe(true);
  });

  it("converts a Retina monitor's physical bounds to logical before testing", () => {
    // Physical 2880x1800 at scaleFactor 2 → logical 1440x900. A logical point at
    // (1400, 880) is inside; the same numeric value would be OUTSIDE if the
    // physical bounds were compared un-scaled, so this pins the conversion.
    const retina: MonitorBounds = {
      position: { x: 0, y: 0 },
      size: { width: 2880, height: 1800 },
      scaleFactor: 2,
    };
    expect(isOnScreen({ x: 1400, y: 880 }, [retina])).toBe(true);
    expect(isOnScreen({ x: 1500, y: 880 }, [retina])).toBe(false);
  });

  it("treats the far right/bottom edge as off-screen (half-open bounds)", () => {
    // A top-left exactly on the right/bottom edge would put the window off the
    // monitor, so the edge is exclusive; the origin corner is inclusive.
    expect(isOnScreen({ x: 0, y: 0 }, [primary])).toBe(true);
    expect(isOnScreen({ x: 1440, y: 400 }, [primary])).toBe(false);
    expect(isOnScreen({ x: 700, y: 900 }, [primary])).toBe(false);
  });

  it("reports off-screen for an empty monitor list (headless / detached)", () => {
    expect(isOnScreen({ x: 0, y: 0 }, [])).toBe(false);
  });
});

describe("centeredModalRect", () => {
  // Primary work area with a menu bar (origin y=25), scaleFactor 1 so the
  // centering math reads without the physical→logical conversion in play.
  const primary: WorkArea = {
    position: { x: 0, y: 25 },
    size: { width: 1440, height: 875 },
    scaleFactor: 1,
  };

  it("centers the size box within the work area", () => {
    // x = 0 + (1440 - 720) / 2 = 360; y = 25 + (875 - 480) / 2 = 222.5.
    expect(centeredModalRect(primary, { width: 720, height: 480 })).toEqual({
      x: 360,
      y: 222.5,
      width: 720,
      height: 480,
    });
  });

  it("anchors off a non-zero work-area origin (secondary / offset monitor)", () => {
    const secondary: WorkArea = {
      position: { x: 1440, y: 25 },
      size: { width: 1920, height: 1055 },
      scaleFactor: 1,
    };
    // x = 1440 + (1920 - 720) / 2 = 2040; y = 25 + (1055 - 480) / 2 = 312.5.
    expect(centeredModalRect(secondary, { width: 720, height: 480 })).toEqual({
      x: 2040,
      y: 312.5,
      width: 720,
      height: 480,
    });
  });

  it("converts a Retina scaleFactor:2 work area from physical to logical once", () => {
    const retina: WorkArea = {
      position: { x: 0, y: 50 },
      size: { width: 2880, height: 1750 },
      scaleFactor: 2,
    };
    // Logical area 1440x875 at origin (0,25): same result as the primary case.
    expect(centeredModalRect(retina, { width: 720, height: 480 })).toEqual({
      x: 360,
      y: 222.5,
      width: 720,
      height: 480,
    });
  });

  it("floors a sub-minimum size up to the overlay floor before centering", () => {
    const rect = centeredModalRect(primary, { width: 10, height: 10 });
    expect(rect.width).toBe(OVERLAY_MIN_WIDTH);
    expect(rect.height).toBe(OVERLAY_MIN_HEIGHT);
    // Centered with the floored dimensions, not the sub-min request.
    expect(rect.x).toBe((1440 - OVERLAY_MIN_WIDTH) / 2);
    expect(rect.y).toBe(25 + (875 - OVERLAY_MIN_HEIGHT) / 2);
  });

  it("produces an on-screen top-left the isOnScreen guard accepts", () => {
    // The fallback must itself pass the guard, or the no-position path would loop
    // through the off-screen branch. Center within a single monitor and confirm.
    const monitor: MonitorBounds = {
      position: { x: 0, y: 0 },
      size: { width: 1440, height: 900 },
      scaleFactor: 1,
    };
    const rect = centeredModalRect(
      { ...monitor, position: { x: 0, y: 0 } },
      { width: 720, height: 480 },
    );
    expect(isOnScreen({ x: rect.x, y: rect.y }, [monitor])).toBe(true);
  });
});

describe("resizeDirectionForEdge", () => {
  // The inner edge (facing the screen interior) is opposite the docked edge, so
  // the resize direction points INWARD: a left-docked drawer grows East, etc.
  // Getting this backwards makes the drag fight the anchor.
  const cases: Array<[Edge, string]> = [
    ["left", "East"],
    ["right", "West"],
    ["top", "South"],
    ["bottom", "North"],
  ];

  it.each(cases)(
    "resizes a %s drawer from its inner edge toward %s",
    (edge, direction) => {
      expect(resizeDirectionForEdge(edge)).toBe(direction);
    },
  );

  it("never returns the docked edge's own direction (grows inward, not outward)", () => {
    // A left drawer must not resize West (into its own docked edge); every edge
    // maps to the perpendicular-or-opposite inward direction.
    expect(resizeDirectionForEdge("left")).not.toBe("West");
    expect(resizeDirectionForEdge("right")).not.toBe("East");
    expect(resizeDirectionForEdge("top")).not.toBe("North");
    expect(resizeDirectionForEdge("bottom")).not.toBe("South");
  });
});

describe("extentFromSize", () => {
  it("reads WIDTH as the variable extent for left/right drawers", () => {
    const size = { width: 480, height: 900 };
    expect(extentFromSize("left", size)).toBe(480);
    expect(extentFromSize("right", size)).toBe(480);
  });

  it("reads HEIGHT as the variable extent for top/bottom drawers", () => {
    const size = { width: 1440, height: 260 };
    expect(extentFromSize("top", size)).toBe(260);
    expect(extentFromSize("bottom", size)).toBe(260);
  });

  it("floors a sub-minimum width extent for left/right drawers", () => {
    const size = { width: 10, height: 900 };
    expect(extentFromSize("left", size)).toBe(OVERLAY_MIN_WIDTH);
    expect(extentFromSize("right", size)).toBe(OVERLAY_MIN_WIDTH);
  });

  it("floors a sub-minimum height extent for top/bottom drawers", () => {
    const size = { width: 1440, height: 10 };
    expect(extentFromSize("top", size)).toBe(OVERLAY_MIN_HEIGHT);
    expect(extentFromSize("bottom", size)).toBe(OVERLAY_MIN_HEIGHT);
  });

  it("round-trips a drawerRect extent so persist-then-reapply is stable", () => {
    // S04 reads innerSize() back and stores extentFromSize(edge, size); feeding
    // a drawerRect's own dimensions back in must return the same extent so the
    // per-edge value doesn't drift on each reapply.
    const area: WorkArea = {
      position: { x: 0, y: 25 },
      size: { width: 1440, height: 875 },
      scaleFactor: 1,
    };
    const rect = drawerRect(area, "left", 400);
    expect(extentFromSize("left", { width: rect.width, height: rect.height })).toBe(
      400,
    );
  });
});
