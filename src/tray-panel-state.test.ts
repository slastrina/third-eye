import { describe, expect, it } from "vitest";
import type { WatcherStatus } from "./watcher-state";
import type { MemoryStatus } from "./memory-state";
import {
  PAUSE_OPTIONS,
  initialTrayPanelState,
  pauseMs,
  trayEye,
  trayPanelReducer,
  traySub,
  trayTitle,
} from "./tray-panel-state";

const watcher = (enabled: boolean): WatcherStatus => ({
  enabled,
  state: enabled ? "watching" : "idle",
  lastTickError: null,
  error: null,
});

const memory = (count: number | null): MemoryStatus => ({
  available: count !== null,
  count,
  dbPath: null,
  ingest: { buffered: 0, distilledCount: 0, lastDistillAtMs: null, lastError: null },
  chatIngest: { buffered: 0, ingestedCount: 0, lastError: null, enabled: true },
});

describe("tray panel reducer", () => {
  it("starts unknown and never guesses", () => {
    expect(initialTrayPanelState.watching).toBeNull();
    expect(trayTitle(initialTrayPanelState)).toBe("Third Eye");
    expect(traySub(initialTrayPanelState, 0)).toBe("state unavailable");
    expect(trayEye(initialTrayPanelState)).toBe("closed");
  });

  it("folds watcher status into watching/paused", () => {
    const on = trayPanelReducer(initialTrayPanelState, { type: "watcher", status: watcher(true) });
    expect(trayTitle(on)).toBe("Watching");
    expect(trayEye(on)).toBe("watching");
    const off = trayPanelReducer(on, { type: "watcher", status: watcher(false) });
    expect(trayTitle(off)).toBe("Paused");
    expect(traySub(off, 0)).toBe("resumes when you say so");
  });

  it("a timed pause counts down; a watching fold clears it", () => {
    const now = 1_000_000;
    let state = trayPanelReducer(initialTrayPanelState, { type: "paused", choice: "15m", now });
    expect(state.watching).toBe(false);
    expect(state.pausedUntil).toBe(now + 15 * 60 * 1000);
    expect(traySub(state, now)).toBe("resumes in ~15 min");
    expect(traySub(state, now + 14 * 60 * 1000)).toBe("resumes in ~1 min");
    // Resume (timer, panel button, or Settings) folds watching → countdown gone.
    state = trayPanelReducer(state, { type: "watcher", status: watcher(true) });
    expect(state.pausedUntil).toBeNull();
  });

  it("manual pause carries no invented resume time", () => {
    const state = trayPanelReducer(initialTrayPanelState, { type: "paused", choice: "manual", now: 5 });
    expect(state.pausedUntil).toBeNull();
    expect(traySub(state, 5)).toBe("resumes when you say so");
  });

  it("pause durations match their labels", () => {
    expect(pauseMs("15m")).toBe(900_000);
    expect(pauseMs("1h")).toBe(3_600_000);
    expect(pauseMs("manual")).toBeNull();
    expect(PAUSE_OPTIONS.map((option) => option.value)).toEqual(["15m", "1h", "manual"]);
  });

  it("folds memory count and latest records; null count stays null", () => {
    let state = trayPanelReducer(initialTrayPanelState, { type: "memory", status: memory(38) });
    expect(state.memoriesStored).toBe(38);
    state = trayPanelReducer(state, { type: "memory", status: memory(null) });
    expect(state.memoriesStored).toBeNull();
    state = trayPanelReducer(state, {
      type: "latest",
      records: [
        {
          id: 1,
          summary: "Refactored the watcher poll loop",
          source: "watcher",
          apps: ["Zed"],
          spanStartMs: 0,
          spanEndMs: 1,
          createdAtMs: 1,
          updatedAtMs: 1,
        },
      ],
    });
    expect(state.latest).toHaveLength(1);
  });
});
