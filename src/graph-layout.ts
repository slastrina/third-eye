// Force-directed layout for the memory knowledge graph (2026-07-27) — a
// small, dependency-free simulation in the pure-module convention: every
// step is a plain function over plain data, so the physics is vitest-able
// and the Graph view is only glue + SVG.
//
// Determinism is deliberate: initial positions come from a golden-angle
// spiral (no Math.random), so the same graph always settles into the same
// shape and tests can assert real positions.

export interface LayoutNode {
  id: number;
  x: number;
  y: number;
  vx: number;
  vy: number;
  /** Pinned by a drag: forces skip it, the pointer owns it. */
  pinned: boolean;
}

export interface LayoutEdge {
  a: number;
  b: number;
  /** (0, 1] — stronger edges pull harder and sit closer. */
  weight: number;
}

export interface LayoutParams {
  /** Pairwise repulsion strength. */
  repulsion: number;
  /** Spring constant along edges. */
  spring: number;
  /** Preferred edge length at weight 1 (weaker edges sit farther). */
  springLength: number;
  /** Pull toward the origin keeping disconnected islands on screen. */
  gravity: number;
  /** Velocity retained per step (0..1). */
  damping: number;
  /** Hard per-step speed cap — no teleporting, no violent collisions. */
  maxSpeed: number;
}

export const DEFAULT_PARAMS: LayoutParams = {
  repulsion: 3600,
  spring: 0.08,
  springLength: 90,
  gravity: 0.015,
  damping: 0.6,
  maxSpeed: 9,
};

/** Temperature decay per step (d3's alpha pattern): forces fade every tick,
 *  so the simulation ALWAYS comes to rest — the first cut had no cooling and
 *  oscillated forever ("constantly moving fast and colliding"). */
export const ALPHA_DECAY = 0.985;

/** Below this temperature the simulation is at rest. */
export const ALPHA_MIN = 0.03;

/** Re-heat used when a drag perturbs a settled layout. */
export const ALPHA_REHEAT = 0.4;

/** Golden-angle spiral seeding: distinct, deterministic, roughly uniform. */
export function seedLayout(ids: readonly number[]): LayoutNode[] {
  const GOLDEN = Math.PI * (3 - Math.sqrt(5));
  return ids.map((id, i) => {
    const r = 14 * Math.sqrt(i + 1);
    const theta = i * GOLDEN;
    return { id, x: r * Math.cos(theta), y: r * Math.sin(theta), vx: 0, vy: 0, pinned: false };
  });
}

/** One simulation step. Mutates nothing — returns the next node array. */
export function stepLayout(
  nodes: readonly LayoutNode[],
  edges: readonly LayoutEdge[],
  params: LayoutParams = DEFAULT_PARAMS,
  alpha = 1,
): LayoutNode[] {
  const byId = new Map(nodes.map((n, i) => [n.id, i]));
  const fx = new Array(nodes.length).fill(0);
  const fy = new Array(nodes.length).fill(0);

  // Pairwise repulsion (n ≤ 200, so O(n²) is fine and stays simple).
  for (let i = 0; i < nodes.length; i++) {
    for (let j = i + 1; j < nodes.length; j++) {
      let dx = nodes[i].x - nodes[j].x;
      let dy = nodes[i].y - nodes[j].y;
      let d2 = dx * dx + dy * dy;
      if (d2 < 1) {
        // Coincident nodes get a deterministic nudge apart.
        dx = 1;
        dy = 1;
        d2 = 2;
      }
      const f = params.repulsion / d2;
      const d = Math.sqrt(d2);
      fx[i] += (dx / d) * f;
      fy[i] += (dy / d) * f;
      fx[j] -= (dx / d) * f;
      fy[j] -= (dy / d) * f;
    }
  }

  // Springs along edges: stronger weight → shorter rest length, harder pull.
  for (const edge of edges) {
    const i = byId.get(edge.a);
    const j = byId.get(edge.b);
    if (i === undefined || j === undefined || i === j) continue;
    const dx = nodes[j].x - nodes[i].x;
    const dy = nodes[j].y - nodes[i].y;
    const d = Math.max(1, Math.hypot(dx, dy));
    const rest = params.springLength * (1.6 - 0.8 * edge.weight);
    const f = params.spring * edge.weight * (d - rest);
    fx[i] += (dx / d) * f;
    fy[i] += (dy / d) * f;
    fx[j] -= (dx / d) * f;
    fy[j] -= (dy / d) * f;
  }

  return nodes.map((node, i) => {
    if (node.pinned) return { ...node, vx: 0, vy: 0 };
    let vx = (node.vx + (fx[i] - node.x * params.gravity) * alpha) * params.damping;
    let vy = (node.vy + (fy[i] - node.y * params.gravity) * alpha) * params.damping;
    const speed = Math.hypot(vx, vy);
    const cap = params.maxSpeed * alpha + 0.4;
    if (speed > cap) {
      vx = (vx / speed) * cap;
      vy = (vy / speed) * cap;
    }
    return { ...node, x: node.x + vx, y: node.y + vy, vx, vy };
  });
}

/** Total kinetic energy — the settle signal (stop iterating below ~n·0.02). */
export function layoutEnergy(nodes: readonly LayoutNode[]): number {
  return nodes.reduce((sum, n) => sum + n.vx * n.vx + n.vy * n.vy, 0);
}

/** Run the simulation to (near) rest. Bounded — never spins forever. */
export function settleLayout(
  nodes: LayoutNode[],
  edges: readonly LayoutEdge[],
  maxSteps = 400,
  params: LayoutParams = DEFAULT_PARAMS,
): LayoutNode[] {
  let current = nodes;
  let alpha = 1;
  for (let i = 0; i < maxSteps; i++) {
    current = stepLayout(current, edges, params, alpha);
    alpha *= ALPHA_DECAY;
    if (alpha < ALPHA_MIN) break;
  }
  return current;
}
