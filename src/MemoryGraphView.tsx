// The Graph tab (2026-07-27): memories as an Obsidian-style force graph.
// Physics lives in graph-layout.ts (pure, tested); this component is glue —
// fetch, animate to rest, pan/zoom/drag, and a detail card on click.
import { useEffect, useMemo, useRef, useState } from "react";
import {
  ALPHA_DECAY,
  ALPHA_MIN,
  ALPHA_REHEAT,
  DEFAULT_PARAMS,
  seedLayout,
  stepLayout,
  type LayoutNode,
} from "./graph-layout";
import {
  memoryGraph,
  type MemoryGraphEdge,
  type MemoryGraphPayload,
} from "./memory-state";

/** Node radius from degree: hubs read bigger, capped so nothing dominates. */
function radius(degree: number): number {
  return 5 + Math.min(degree, 6) * 1.4;
}

const EDGE_COLOR: Record<MemoryGraphEdge["kind"], string> = {
  semantic: "rgba(233, 162, 59, 0.55)",
  keyword: "rgba(147, 218, 73, 0.45)",
  app: "rgba(255, 255, 255, 0.16)",
};

export function MemoryGraphView() {
  const [graph, setGraph] = useState<MemoryGraphPayload | null>(null);
  const [unavailable, setUnavailable] = useState(false);
  const [nodes, setNodes] = useState<LayoutNode[]>([]);
  const [selected, setSelected] = useState<number | null>(null);
  const [hovered, setHovered] = useState<number | null>(null);
  // viewBox pan/zoom: x, y, scale over a nominal 900x620 canvas.
  const [view, setView] = useState({ x: -450, y: -310, w: 900, h: 620 });
  const dragRef = useRef<
    | { kind: "pan"; startX: number; startY: number; view: typeof view }
    | { kind: "node"; id: number }
    | null
  >(null);
  const svgRef = useRef<SVGSVGElement | null>(null);

  useEffect(() => {
    memoryGraph(120).then(
      (payload) => {
        setGraph(payload);
        setNodes(seedLayout(payload.nodes.map((n) => n.id)));
      },
      (err) => {
        console.debug("memory-window: memory_graph unavailable:", err);
        setUnavailable(true);
      },
    );
  }, []);

  // Animate the simulation to rest. Temperature (alpha) decays every tick
  // so rest is GUARANTEED — the loop always terminates; a drag re-heats it
  // gently instead of restarting a hot simulation.
  const alphaRef = useRef(1);
  const anyPinned = nodes.some((n) => n.pinned);
  useEffect(() => {
    if (!graph || nodes.length === 0) return;
    if (anyPinned) alphaRef.current = Math.max(alphaRef.current, ALPHA_REHEAT);
    const edges = graph.edges.map((e) => ({ a: e.a, b: e.b, weight: e.weight }));
    let frame = 0;
    let current = nodes;
    const tick = () => {
      current = stepLayout(current, edges, DEFAULT_PARAMS, alphaRef.current);
      alphaRef.current *= ALPHA_DECAY;
      setNodes(current);
      if (alphaRef.current > ALPHA_MIN) {
        frame = requestAnimationFrame(tick);
      }
    };
    frame = requestAnimationFrame(tick);
    return () => cancelAnimationFrame(frame);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [graph, anyPinned]);

  const byId = useMemo(() => new Map(nodes.map((n) => [n.id, n])), [nodes]);
  const meta = useMemo(
    () => new Map((graph?.nodes ?? []).map((n) => [n.id, n])),
    [graph],
  );
  const degree = useMemo(() => {
    const d = new Map<number, number>();
    for (const e of graph?.edges ?? []) {
      d.set(e.a, (d.get(e.a) ?? 0) + 1);
      d.set(e.b, (d.get(e.b) ?? 0) + 1);
    }
    return d;
  }, [graph]);
  const neighbors = useMemo(() => {
    const n = new Map<number, Set<number>>();
    for (const e of graph?.edges ?? []) {
      if (!n.has(e.a)) n.set(e.a, new Set());
      if (!n.has(e.b)) n.set(e.b, new Set());
      n.get(e.a)!.add(e.b);
      n.get(e.b)!.add(e.a);
    }
    return n;
  }, [graph]);

  const toGraphPoint = (clientX: number, clientY: number) => {
    const rect = svgRef.current?.getBoundingClientRect();
    if (!rect) return { x: 0, y: 0 };
    return {
      x: view.x + ((clientX - rect.left) / rect.width) * view.w,
      y: view.y + ((clientY - rect.top) / rect.height) * view.h,
    };
  };

  if (unavailable) {
    return (
      <p className="memory-empty">The memory graph is unavailable outside the app.</p>
    );
  }
  if (!graph) return <p className="memory-empty">Building the graph…</p>;
  if (graph.nodes.length === 0) {
    return <p className="memory-empty">No memories yet — the graph grows as Third Eye learns.</p>;
  }

  const focus = hovered ?? selected;
  const selectedMeta = selected !== null ? meta.get(selected) : undefined;
  return (
    <div className="memory-graph">
      <svg
        ref={svgRef}
        className="memory-graph-canvas"
        viewBox={`${view.x} ${view.y} ${view.w} ${view.h}`}
        onWheel={(e) => {
          const factor = e.deltaY > 0 ? 1.12 : 1 / 1.12;
          const w = Math.min(3600, Math.max(220, view.w * factor));
          const h = w * (620 / 900);
          const p = toGraphPoint(e.clientX, e.clientY);
          // Zoom about the pointer: keep p at the same relative position.
          setView({
            x: p.x - ((p.x - view.x) / view.w) * w,
            y: p.y - ((p.y - view.y) / view.h) * h,
            w,
            h,
          });
        }}
        onPointerDown={(e) => {
          dragRef.current = { kind: "pan", startX: e.clientX, startY: e.clientY, view };
          (e.target as Element).setPointerCapture?.(e.pointerId);
        }}
        onPointerMove={(e) => {
          const drag = dragRef.current;
          if (!drag) return;
          if (drag.kind === "pan") {
            const rect = svgRef.current!.getBoundingClientRect();
            setView({
              ...drag.view,
              x: drag.view.x - ((e.clientX - drag.startX) / rect.width) * drag.view.w,
              y: drag.view.y - ((e.clientY - drag.startY) / rect.height) * drag.view.h,
            });
          } else {
            const p = toGraphPoint(e.clientX, e.clientY);
            setNodes((current) =>
              current.map((n) =>
                n.id === drag.id ? { ...n, x: p.x, y: p.y, vx: 0, vy: 0, pinned: true } : n,
              ),
            );
          }
        }}
        onPointerUp={() => {
          const drag = dragRef.current;
          dragRef.current = null;
          if (drag?.kind === "node") {
            // Release: the node rejoins the simulation.
            setNodes((current) =>
              current.map((n) => (n.id === drag.id ? { ...n, pinned: false } : n)),
            );
          }
        }}
      >
        {graph.edges.map((edge) => {
          const a = byId.get(edge.a);
          const b = byId.get(edge.b);
          if (!a || !b) return null;
          const dim =
            focus !== null && edge.a !== focus && edge.b !== focus;
          return (
            <line
              key={`${edge.a}-${edge.b}`}
              x1={a.x}
              y1={a.y}
              x2={b.x}
              y2={b.y}
              stroke={EDGE_COLOR[edge.kind]}
              strokeWidth={0.8 + edge.weight * 2.2}
              opacity={dim ? 0.15 : 1}
            />
          );
        })}
        {nodes.map((node) => {
          const m = meta.get(node.id);
          if (!m) return null;
          const deg = degree.get(node.id) ?? 0;
          const dim =
            focus !== null && node.id !== focus && !neighbors.get(focus)?.has(node.id);
          return (
            <g
              key={node.id}
              className="memory-graph-node"
              opacity={dim ? 0.25 : 1}
              onPointerDown={(e) => {
                e.stopPropagation();
                dragRef.current = { kind: "node", id: node.id };
                (e.target as Element).setPointerCapture?.(e.pointerId);
              }}
              onPointerEnter={() => setHovered(node.id)}
              onPointerLeave={() => setHovered(null)}
              onClick={() => setSelected(selected === node.id ? null : node.id)}
            >
              <circle
                cx={node.x}
                cy={node.y}
                r={radius(deg)}
                className="memory-graph-dot"
                data-source={m.source}
                data-selected={node.id === selected || undefined}
              />
              {(node.id === focus || deg >= 5) && (
                <text x={node.x} y={node.y - radius(deg) - 4} className="memory-graph-label">
                  {m.summary.length > 42 ? `${m.summary.slice(0, 42)}…` : m.summary}
                </text>
              )}
            </g>
          );
        })}
      </svg>
      {selectedMeta && (
        <aside className="memory-graph-detail">
          <p className="memory-graph-detail-summary">{selectedMeta.summary}</p>
          <p className="memory-graph-detail-meta">
            {new Date(selectedMeta.atMs).toLocaleString()}
            {selectedMeta.apps.length > 0 && ` · ${selectedMeta.apps.join(", ")}`}
            {` · ${selectedMeta.source === "chat" ? "from a chat" : "from watching"}`}
          </p>
          {(neighbors.get(selectedMeta.id)?.size ?? 0) > 0 && (
            <>
              <p className="memory-graph-detail-label">Connected</p>
              <ul className="memory-graph-detail-links">
                {[...(neighbors.get(selectedMeta.id) ?? [])].map((id) => (
                  <li key={id}>
                    <button type="button" onClick={() => setSelected(id)}>
                      {meta.get(id)?.summary.slice(0, 60) ?? `#${id}`}
                    </button>
                  </li>
                ))}
              </ul>
            </>
          )}
        </aside>
      )}
      <div className="memory-graph-legend" aria-hidden="true">
        <span data-kind="semantic">semantic</span>
        <span data-kind="keyword">keywords</span>
        <span data-kind="app">same app</span>
      </div>
    </div>
  );
}
