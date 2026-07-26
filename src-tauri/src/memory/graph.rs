//! Memory knowledge graph (user request 2026-07-27): the Obsidian-style
//! view — memories as nodes, joined by what they share.
//!
//! Pure construction over what the store already holds; nothing new is
//! persisted. Three edge signals, strongest wins per pair:
//!
//! - **semantic** — cosine similarity of the stored embeddings (computed
//!   HERE so raw embeddings never cross IPC, R-rule);
//! - **keyword** — shared distinctive summary words (stopworded, ≥ 4
//!   chars), two or more shared;
//! - **app** — a shared app context (the weakest join: half the graph is
//!   "Chrome", so it only connects otherwise-related pairs loosely).
//!
//! The graph is deliberately sparse: per-node edges are capped to the
//! strongest few, because a hairball is not browsable — Obsidian's charm
//! is the clusters, not the clutter.

use serde::Serialize;

use super::store::{MemoryRecord, MemorySource};

/// Node cap the IPC enforces (rendering and O(n²) pair scoring both stay
/// comfortable at this size).
pub const GRAPH_MAX_NODES: usize = 200;

/// Strongest edges kept per node.
const MAX_EDGES_PER_NODE: usize = 6;

/// Cosine floor for a semantic edge — below this, embeddings of unrelated
/// screen summaries still hover ~0.7 on small embedders, so the bar is high.
const SEMANTIC_MIN: f32 = 0.82;

/// Shared distinctive keywords needed for a keyword edge.
const KEYWORD_MIN_SHARED: usize = 2;

/// Words too common to signal a relationship. Lowercase; summaries are
/// distilled English one-liners, so a compact list carries most of the
/// weight (this is a relevance heuristic, not NLP).
const STOPWORDS: &[&str] = &[
    "about",
    "after",
    "again",
    "along",
    "around",
    "back",
    "been",
    "before",
    "being",
    "between",
    "browsing",
    "changes",
    "chat",
    "checked",
    "code",
    "computer",
    "conversation",
    "discussed",
    "doing",
    "file",
    "files",
    "from",
    "have",
    "info",
    "information",
    "into",
    "just",
    "looked",
    "looking",
    "more",
    "new",
    "online",
    "opened",
    "over",
    "page",
    "pages",
    "read",
    "reading",
    "reviewed",
    "screen",
    "searched",
    "searching",
    "session",
    "some",
    "that",
    "their",
    "then",
    "there",
    "these",
    "they",
    "this",
    "time",
    "used",
    "user",
    "using",
    "viewed",
    "view",
    "were",
    "window",
    "with",
    "work",
    "worked",
    "working",
];

/// One memory as a graph node — the record's browsable surface, camelCase
/// for the memory window.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GraphNode {
    pub id: i64,
    pub summary: String,
    pub apps: Vec<String>,
    pub source: MemorySource,
    pub at_ms: i64,
}

/// Why two nodes join.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum EdgeKind {
    Semantic,
    Keyword,
    App,
}

/// One undirected edge (`a` < `b` by id), weight in (0, 1].
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GraphEdge {
    pub a: i64,
    pub b: i64,
    pub weight: f32,
    pub kind: EdgeKind,
}

/// The `memory_graph` IPC payload.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryGraph {
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<GraphEdge>,
}

/// Distinctive lowercase keywords of one summary.
fn keywords(summary: &str) -> Vec<String> {
    let mut words: Vec<String> = summary
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.chars().count() >= 4 && !STOPWORDS.contains(w))
        .map(str::to_string)
        .collect();
    words.sort();
    words.dedup();
    words
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let (mut dot, mut na, mut nb) = (0.0f32, 0.0f32, 0.0f32);
    for (x, y) in a.iter().zip(b) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        dot / (na.sqrt() * nb.sqrt())
    }
}

/// Build the graph from records (+ their private embeddings). Pure and
/// deterministic — fully unit-testable without a store.
pub fn build_graph(records: &[(MemoryRecord, Option<Vec<f32>>)]) -> MemoryGraph {
    let records = &records[..records.len().min(GRAPH_MAX_NODES)];
    let nodes: Vec<GraphNode> = records
        .iter()
        .map(|(r, _)| GraphNode {
            id: r.id,
            summary: r.summary.clone(),
            apps: r.apps.clone(),
            source: r.source,
            at_ms: r.span_end_ms,
        })
        .collect();
    let keyword_sets: Vec<Vec<String>> =
        records.iter().map(|(r, _)| keywords(&r.summary)).collect();

    // Score every pair once; keep the strongest signal per pair.
    let mut candidates: Vec<GraphEdge> = Vec::new();
    for i in 0..records.len() {
        for j in (i + 1)..records.len() {
            let (ra, ea) = &records[i];
            let (rb, eb) = &records[j];
            let mut best: Option<(f32, EdgeKind)> = None;
            if let (Some(ea), Some(eb)) = (ea, eb) {
                let sim = cosine(ea, eb);
                if sim >= SEMANTIC_MIN {
                    best = Some((sim, EdgeKind::Semantic));
                }
            }
            let shared_kw = keyword_sets[i]
                .iter()
                .filter(|w| keyword_sets[j].binary_search(w).is_ok())
                .count();
            if shared_kw >= KEYWORD_MIN_SHARED {
                // 2 shared → 0.5, saturating toward 1.0 at 6+.
                let w = (shared_kw as f32 / 6.0).clamp(0.5, 1.0) * 0.9;
                if best.map(|(bw, _)| w > bw).unwrap_or(true) {
                    best = Some((w, EdgeKind::Keyword));
                }
            }
            if best.is_none() && ra.apps.iter().any(|app| rb.apps.contains(app)) {
                best = Some((0.25, EdgeKind::App));
            }
            if let Some((weight, kind)) = best {
                candidates.push(GraphEdge {
                    a: ra.id.min(rb.id),
                    b: ra.id.max(rb.id),
                    weight,
                    kind,
                });
            }
        }
    }

    // Sparsify: strongest first; an edge lands only while BOTH endpoints
    // have degree budget left. Keeps clusters, kills the hairball.
    candidates.sort_by(|x, y| y.weight.total_cmp(&x.weight));
    let mut degree: std::collections::HashMap<i64, usize> = std::collections::HashMap::new();
    let mut edges: Vec<GraphEdge> = Vec::new();
    for edge in candidates {
        let da = *degree.get(&edge.a).unwrap_or(&0);
        let db = *degree.get(&edge.b).unwrap_or(&0);
        if da < MAX_EDGES_PER_NODE && db < MAX_EDGES_PER_NODE {
            *degree.entry(edge.a).or_insert(0) += 1;
            *degree.entry(edge.b).or_insert(0) += 1;
            edges.push(edge);
        }
    }
    MemoryGraph { nodes, edges }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(id: i64, summary: &str, apps: &[&str]) -> (MemoryRecord, Option<Vec<f32>>) {
        (
            MemoryRecord {
                id,
                summary: summary.into(),
                apps: apps.iter().map(|a| a.to_string()).collect(),
                span_start_ms: id * 1_000,
                span_end_ms: id * 1_000 + 500,
                created_at_ms: id * 1_000,
                updated_at_ms: id * 1_000,
                source: MemorySource::Watcher,
            },
            None,
        )
    }

    #[test]
    fn keyword_overlap_joins_and_stopwords_do_not() {
        let graph = build_graph(&[
            record(
                1,
                "Researched lasagna recipe ingredients on RecipeTin Eats",
                &["Chrome"],
            ),
            record(
                2,
                "Compared lasagna ingredients and béchamel technique",
                &["Safari"],
            ),
            record(
                3,
                "Debugged kubernetes pod restarts in the prod cluster",
                &["Terminal"],
            ),
        ]);
        assert_eq!(graph.nodes.len(), 3);
        // 1↔2 share "lasagna"+"ingredients" → keyword edge; 3 is an island.
        assert_eq!(graph.edges.len(), 1);
        let edge = &graph.edges[0];
        assert_eq!((edge.a, edge.b, edge.kind), (1, 2, EdgeKind::Keyword));
        assert!(edge.weight > 0.4);
    }

    #[test]
    fn shared_app_is_the_weak_fallback_join() {
        let graph = build_graph(&[
            record(1, "Watched the quarterly earnings video", &["Chrome"]),
            record(2, "Ordered replacement keyboard switches", &["Chrome"]),
        ]);
        assert_eq!(graph.edges.len(), 1);
        assert_eq!(graph.edges[0].kind, EdgeKind::App);
        assert!(graph.edges[0].weight < 0.5);
    }

    #[test]
    fn semantic_edges_use_embeddings_and_beat_weaker_signals() {
        let mut a = record(1, "Alpha topic entry", &[]);
        let mut b = record(2, "Beta subject entry", &[]);
        a.1 = Some(vec![1.0, 0.0, 0.1]);
        b.1 = Some(vec![0.9, 0.05, 0.1]);
        let c = record(3, "Gamma unrelated", &[]);
        let graph = build_graph(&[a, b, c]);
        assert_eq!(graph.edges.len(), 1);
        assert_eq!(graph.edges[0].kind, EdgeKind::Semantic);
        assert!(graph.edges[0].weight >= SEMANTIC_MIN);
    }

    #[test]
    fn degree_cap_keeps_the_graph_sparse() {
        // A hub sharing keywords with 10 others keeps only its strongest 6.
        let mut records = vec![record(0, "central lasagna ragu besciamella notes", &[])];
        for i in 1..=10 {
            records.push(record(i, &format!("lasagna ragu variation number{i}"), &[]));
        }
        let graph = build_graph(&records);
        let hub_degree = graph.edges.iter().filter(|e| e.a == 0 || e.b == 0).count();
        assert!(hub_degree <= 6, "hub degree {hub_degree}");
    }

    #[test]
    fn node_cap_bounds_the_payload() {
        let records: Vec<_> = (0..(GRAPH_MAX_NODES as i64 + 50))
            .map(|i| record(i, &format!("unique{i} entry"), &[]))
            .collect();
        assert_eq!(build_graph(&records).nodes.len(), GRAPH_MAX_NODES);
    }
}
