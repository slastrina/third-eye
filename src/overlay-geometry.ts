// Pure geometry helpers for the movable/resizable overlay panel (M006/S01).
// Geometry is a platform side-effect, not an OverlayState transition (RESEARCH
// point 3), so this module holds no state — just the rect/size math App.tsx
// wires to Tauri window calls. Moving uses native startDragging; RESIZING is
// pointer-driven here (draggedExtent / draggedModalSize) because tao's
// drag_resize_window is NotSupported on macOS — native startResizeDragging
// silently no-ops there, so the overlay computes its own resize on every
// platform. Kebab-case module name per MEM051 (a Geometry.tsx companion would
// collide on the case-insensitive filesystem).

/** The smallest the overlay may shrink to — below this the chat chrome clips. */
export const OVERLAY_MIN_WIDTH = 360;
export const OVERLAY_MIN_HEIGHT = 120;

export interface OverlaySize {
  width: number;
  height: number;
}

/**
 * The display edge a drawer snaps flush against (M006/S02). Left/right drawers
 * take the full work-area HEIGHT at a set width; top/bottom take the full
 * work-area WIDTH at a set height. Typed as the literal union so callers and
 * the ?edge= dev harness can't pass an off-contract string.
 */
export type Edge = "top" | "bottom" | "left" | "right";

/**
 * A monitor's work area — the visible frame excluding menu bar / dock / notch —
 * in PHYSICAL pixels, as Tauri's JS `Monitor.workArea` surfaces it, paired with
 * the monitor's `scaleFactor`. Kept as a plain shape (not the Tauri class) so
 * `drawerRect` stays a pure function that unit tests can call with a literal.
 * `position` is the top-left relative to the virtual desktop and is NOT (0,0)
 * on a secondary or menu-bar-offset display (RESEARCH Constraints).
 */
export interface WorkArea {
  position: { x: number; y: number };
  size: { width: number; height: number };
  scaleFactor: number;
}

/** A snapped drawer window rect in LOGICAL pixels, ready for setPosition/setSize. */
export interface DrawerRect {
  x: number;
  y: number;
  width: number;
  height: number;
}

/**
 * Compute the drawer window rect for snapping the overlay flush to `edge` of the
 * active display's work area.
 *
 * Unit contract (the prior Retina click-miss class, MEM screen_query pixels-vs-
 * points): `workArea` is PHYSICAL pixels; `extent` and the returned rect are
 * LOGICAL pixels. The physical->logical conversion happens ONCE here, dividing
 * work-area origin and size by `scaleFactor`; `extent` is already logical and is
 * NOT scaled. The drawer always anchors off `workArea.position` (never assuming
 * origin 0,0), so a drawer on a menu-bar-offset or secondary monitor lands on
 * that monitor rather than off-screen.
 *
 * `extent` is the drawer's variable dimension — width for left/right, height for
 * top/bottom — floored at the overlay minimum on that axis so the chrome never
 * clips. The spanning dimension always fills the full work area.
 */
export function drawerRect(
  workArea: WorkArea,
  edge: Edge,
  extent: number,
): DrawerRect {
  const scale = workArea.scaleFactor;
  const originX = workArea.position.x / scale;
  const originY = workArea.position.y / scale;
  const areaWidth = workArea.size.width / scale;
  const areaHeight = workArea.size.height / scale;

  switch (edge) {
    case "left": {
      const width = Math.max(extent, OVERLAY_MIN_WIDTH);
      return { x: originX, y: originY, width, height: areaHeight };
    }
    case "right": {
      const width = Math.max(extent, OVERLAY_MIN_WIDTH);
      return {
        x: originX + areaWidth - width,
        y: originY,
        width,
        height: areaHeight,
      };
    }
    case "top": {
      const height = Math.max(extent, OVERLAY_MIN_HEIGHT);
      return { x: originX, y: originY, width: areaWidth, height };
    }
    case "bottom": {
      const height = Math.max(extent, OVERLAY_MIN_HEIGHT);
      return {
        x: originX,
        y: originY + areaHeight - height,
        width: areaWidth,
        height,
      };
    }
  }
}

/**
 * The new extent of a drawer implied by dragging its inner-edge bar from
 * `from` to `to` — both in LOGICAL screen points (MouseEvent.screenX/Y, which
 * stay stable while the window itself moves and resizes under the cursor;
 * clientX/Y would drift as the window origin shifts).
 *
 * The sign folds in which way the drawer's free edge faces — the drag must
 * grow the drawer INWARD, never fight the docked anchor: a left drawer grows
 * as the pointer moves right (+x), a right drawer as it moves left (−x), a
 * top drawer as it moves down (+y), a bottom drawer as it moves up (−y).
 * Clamped between the overlay minimum on the drawer's variable axis (the
 * chrome never clips) and `maxExtent` — the work-area span on that axis — so
 * a drag can never grow the drawer past its screen. A `maxExtent` below the
 * minimum (a pathological work area) resolves to the minimum: the floor wins.
 */
export function draggedExtent(
  edge: Edge,
  startExtent: number,
  from: OverlayPoint,
  to: OverlayPoint,
  maxExtent: number,
): number {
  let delta: number;
  switch (edge) {
    case "left":
      delta = to.x - from.x;
      break;
    case "right":
      delta = from.x - to.x;
      break;
    case "top":
      delta = to.y - from.y;
      break;
    case "bottom":
      delta = from.y - to.y;
      break;
  }
  const min =
    edge === "left" || edge === "right" ? OVERLAY_MIN_WIDTH : OVERLAY_MIN_HEIGHT;
  return Math.max(Math.min(startExtent + delta, maxExtent), min);
}

/**
 * The new modal (floating) size implied by dragging the bottom-right corner
 * grip from `from` to `to`: the window stays anchored at its top-left, so both
 * axes grow with the pointer. Same LOGICAL screen-point contract as
 * `draggedExtent`, floored per axis by `clampMinSize`.
 */
export function draggedModalSize(
  start: OverlaySize,
  from: OverlayPoint,
  to: OverlayPoint,
): OverlaySize {
  return clampMinSize({
    width: start.width + (to.x - from.x),
    height: start.height + (to.y - from.y),
  });
}

/**
 * Read a drawer's variable extent back out of a measured window size: width for
 * left/right drawers, height for top/bottom, floored at the overlay minimum on
 * that axis. This is the pure seam S04 consumes to persist the per-edge extent
 * after a live resize (it reads innerSize() and calls this to get the number to
 * store), so the flooring here matches drawerRect's so a persisted-then-reapplied
 * extent is stable.
 */
export function extentFromSize(edge: Edge, size: OverlaySize): number {
  switch (edge) {
    case "left":
    case "right":
      return Math.max(size.width, OVERLAY_MIN_WIDTH);
    case "top":
    case "bottom":
      return Math.max(size.height, OVERLAY_MIN_HEIGHT);
  }
}

/**
 * A window top-left in LOGICAL pixels — the shape S05 stores as `modalPosition`
 * and restores on relaunch. Distinct from a size: it carries no floor, because a
 * legal multi-monitor virtual desktop places monitors at NEGATIVE origins, so a
 * saved point may legitimately be negative (mirrors the Rust `stored_point`
 * interpreter, which repairs only the non-finite/non-object CORRUPT half).
 */
export interface OverlayPoint {
  x: number;
  y: number;
}

/**
 * A monitor's full bounds as Tauri's `availableMonitors()` surfaces each entry —
 * `position`/`size` in PHYSICAL pixels paired with the monitor's `scaleFactor`.
 * Kept as a plain shape (not the Tauri `Monitor` class, which also carries name/
 * workArea) so `isOnScreen` stays a pure function unit tests call with literals.
 */
export interface MonitorBounds {
  position: { x: number; y: number };
  size: { width: number; height: number };
  scaleFactor: number;
}

/**
 * True when a LOGICAL-pixel `point` (a saved modal top-left) lands inside the
 * bounds of at least one live monitor — the OFF-SCREEN-BUT-FINITE half of the
 * SC4 fallback the Rust interpreter can't see. A finite point stored against a
 * monitor that has since been unplugged fails here, so the caller centers
 * instead of restoring a lost off-screen window.
 *
 * Each monitor's PHYSICAL bounds are converted to logical by dividing by its OWN
 * `scaleFactor` (the same pixels-vs-points boundary `drawerRect` honours) so the
 * comparison happens entirely in the logical space the point lives in. The right/
 * bottom edges are half-open (`< right`, `< bottom`): a top-left exactly on the
 * far edge would put the window off the monitor, so it is treated as off-screen.
 * An empty monitor list (headless / detached) is off-screen for every point.
 */
export function isOnScreen(point: OverlayPoint, monitors: MonitorBounds[]): boolean {
  return monitors.some((monitor) => {
    const scale = monitor.scaleFactor;
    const left = monitor.position.x / scale;
    const top = monitor.position.y / scale;
    const right = left + monitor.size.width / scale;
    const bottom = top + monitor.size.height / scale;
    return (
      point.x >= left && point.x < right && point.y >= top && point.y < bottom
    );
  });
}

/**
 * The centered fallback modal rect in LOGICAL pixels: the `size` box centered
 * within `workArea`. Both the never-moved path (no stored position) and the
 * off-screen-fallback path (`isOnScreen` failed) route through this ONE centering
 * computation, so a modal that has never been dragged and one whose saved point
 * went off-screen land in the same sane on-screen spot.
 *
 * `workArea` is PHYSICAL pixels (converted once via `scaleFactor`, as `drawerRect`
 * does); `size` is already LOGICAL and is floored at the overlay minimum on each
 * axis so the chrome never clips. Anchors off `workArea.position` (never assuming
 * origin 0,0) so the centre lands on a menu-bar-offset or secondary monitor.
 */
export function centeredModalRect(
  workArea: WorkArea,
  size: OverlaySize,
): DrawerRect {
  const scale = workArea.scaleFactor;
  const originX = workArea.position.x / scale;
  const originY = workArea.position.y / scale;
  const areaWidth = workArea.size.width / scale;
  const areaHeight = workArea.size.height / scale;
  const width = Math.max(size.width, OVERLAY_MIN_WIDTH);
  const height = Math.max(size.height, OVERLAY_MIN_HEIGHT);
  return {
    x: originX + (areaWidth - width) / 2,
    y: originY + (areaHeight - height) / 2,
    width,
    height,
  };
}

/**
 * Clamp a proposed size up to the overlay minimum on each axis independently.
 * Resizing is JS-driven (see draggedExtent/draggedModalSize), so this floor IS
 * the min-size constraint — there is no native one backing it up.
 */
export function clampMinSize(
  size: OverlaySize,
  min: OverlaySize = { width: OVERLAY_MIN_WIDTH, height: OVERLAY_MIN_HEIGHT },
): OverlaySize {
  return {
    width: Math.max(size.width, min.width),
    height: Math.max(size.height, min.height),
  };
}
