import { describe, expect, it } from "vitest";
import type { MemoryRecord } from "./memory-state";
import {
  MEMORY_TABS,
  appLabel,
  byCategory,
  categoryFacets,
  durationLabel,
  filterRecords,
  learnedRecords,
} from "./memory-window-state";

const record = (over: Partial<MemoryRecord>): MemoryRecord => ({
  id: 1,
  summary: "Refactored the watcher poll loop",
  source: "watcher",
  apps: ["Zed"],
  spanStartMs: 0,
  spanEndMs: 0,
  createdAtMs: 0,
  updatedAtMs: 0,
  ...over,
});

describe("memory window helpers", () => {
  it("exposes the tabs in order (design three + Chats + Graph)", () => {
    expect(MEMORY_TABS.map((t) => t.id)).toEqual([
      "timeline",
      "learned",
      "recall",
      "chats",
      "graph",
    ]);
  });

  it("filters over summary and apps, case-insensitive; empty filter passes all", () => {
    const records = [
      record({ id: 1, summary: "Edited Quarterly Report", apps: ["Sheet"] }),
      record({ id: 2, summary: "Thread with Sam", apps: ["Slack"] }),
    ];
    expect(filterRecords(records, "")).toHaveLength(2);
    expect(filterRecords(records, "quarterly").map((r) => r.id)).toEqual([1]);
    expect(filterRecords(records, "SLACK").map((r) => r.id)).toEqual([2]);
    expect(filterRecords(records, "nothing")).toHaveLength(0);
  });

  it("learned is exactly the chat-distilled subset — no invented facts", () => {
    const records = [
      record({ id: 1, source: "watcher" }),
      record({ id: 2, source: "chat" }),
      record({ id: 3, source: "" }),
    ];
    expect(learnedRecords(records).map((r) => r.id)).toEqual([2]);
  });

  it("duration claims nothing under a minute and formats hours honestly", () => {
    expect(durationLabel(record({ spanStartMs: 0, spanEndMs: 30_000 }))).toBe("");
    expect(durationLabel(record({ spanStartMs: 0, spanEndMs: 42 * 60_000 }))).toBe("42 min");
    expect(durationLabel(record({ spanStartMs: 0, spanEndMs: 110 * 60_000 }))).toBe("1h 50m");
    expect(durationLabel(record({ spanStartMs: 0, spanEndMs: 120 * 60_000 }))).toBe("2h");
  });

  it("app column shows the primary app or an honest dash", () => {
    expect(appLabel(record({ apps: ["Zed", "Terminal"] }))).toBe("Zed");
    expect(appLabel(record({ apps: [] }))).toBe("—");
  });
});

describe("memory v2 browse helpers", () => {
  const rec = (id: number, summary: string, category?: string, tags?: string[]) => ({
    id,
    summary,
    source: "watcher",
    apps: [],
    spanStartMs: 0,
    spanEndMs: 0,
    createdAtMs: 0,
    updatedAtMs: 0,
    category,
    tags,
  });

  it("filter matches tags and category, not just summary and apps", () => {
    const records = [
      rec(1, "compared ragu techniques", "browsing", ["lasagna", "ragu"]),
      rec(2, "debugged the watcher loop", "development", ["tokio"]),
    ];
    expect(filterRecords(records, "lasagna").map((r) => r.id)).toEqual([1]);
    expect(filterRecords(records, "development").map((r) => r.id)).toEqual([2]);
    // Records without v2 fields (older mocks) never crash the filter.
    expect(filterRecords([rec(3, "bare row")], "anything")).toEqual([]);
  });

  it("facets count biggest-first and byCategory applies the chip", () => {
    const records = [
      rec(1, "a", "browsing"),
      rec(2, "b", "browsing"),
      rec(3, "c", "development"),
      rec(4, "d"),
    ];
    expect(categoryFacets(records)).toEqual([
      { category: "browsing", count: 2 },
      { category: "development", count: 1 },
      { category: "other", count: 1 },
    ]);
    expect(byCategory(records, "browsing").map((r) => r.id)).toEqual([1, 2]);
    expect(byCategory(records, null)).toHaveLength(4);
    // Absent category rows live under "other".
    expect(byCategory(records, "other").map((r) => r.id)).toEqual([4]);
  });
});
