import { describe, expect, it } from "vitest";
import {
  DEFAULT_PARAMS,
  layoutEnergy,
  seedLayout,
  settleLayout,
  stepLayout,
} from "./graph-layout";

const dist = (a: { x: number; y: number }, b: { x: number; y: number }) =>
  Math.hypot(a.x - b.x, a.y - b.y);

describe("graph layout simulation", () => {
  it("seeds deterministic, distinct positions", () => {
    const a = seedLayout([1, 2, 3]);
    const b = seedLayout([1, 2, 3]);
    expect(a).toEqual(b);
    expect(dist(a[0], a[1])).toBeGreaterThan(1);
    expect(dist(a[1], a[2])).toBeGreaterThan(1);
  });

  it("pulls connected nodes together relative to unconnected ones", () => {
    const nodes = seedLayout([1, 2, 3]);
    const settled = settleLayout(nodes, [{ a: 1, b: 2, weight: 1 }]);
    const connected = dist(settled[0], settled[1]);
    const lonely = Math.min(dist(settled[0], settled[2]), dist(settled[1], settled[2]));
    expect(connected).toBeLessThan(lonely);
    // Springs rest near springLength — connected nodes neither collapse nor fly.
    expect(connected).toBeGreaterThan(10);
    expect(connected).toBeLessThan(DEFAULT_PARAMS.springLength * 3);
  });

  it("settles: energy decays and positions stay finite", () => {
    const ids = Array.from({ length: 30 }, (_, i) => i);
    const edges = ids.slice(1).map((id) => ({ a: 0, b: id, weight: 0.5 }));
    const settled = settleLayout(seedLayout(ids), edges);
    expect(layoutEnergy(settled)).toBeLessThan(30 * 0.5);
    for (const n of settled) {
      expect(Number.isFinite(n.x)).toBe(true);
      expect(Number.isFinite(n.y)).toBe(true);
      // Gravity keeps islands bounded.
      expect(Math.hypot(n.x, n.y)).toBeLessThan(4000);
    }
  });

  it("pinned nodes never move (the drag contract)", () => {
    const nodes = seedLayout([1, 2]);
    nodes[0] = { ...nodes[0], pinned: true };
    const before = { x: nodes[0].x, y: nodes[0].y };
    const after = stepLayout(nodes, [{ a: 1, b: 2, weight: 1 }]);
    expect(after[0].x).toBe(before.x);
    expect(after[0].y).toBe(before.y);
    expect(after[1].x).not.toBe(nodes[1].x);
  });
});
