import { describe, expect, it } from "vitest";
import type { MemoryRecord } from "./memory-state";
import {
  MEMORY_TABS,
  appLabel,
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
