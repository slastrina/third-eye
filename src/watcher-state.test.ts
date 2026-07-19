// Reducer + helper coverage for the Watch Screen diagnostics section (S01):
// status transitions (including paused-privacy and both error channels),
// the rolling last-N snippet buffer, and the pure copy helpers. The reducer
// is pure, so no Tauri runtime or DOM is needed.

import { describe, expect, it } from "vitest";
import {
  capturedAtLabel,
  initialWatcherViewState,
  MAX_SNIPPETS,
  runStateLabel,
  snippetPreview,
  SNIPPET_PREVIEW_CHARS,
  tickErrorDetail,
  tickErrorTitle,
  watcherReducer,
  WATCHER_OBSERVATION_EVENT,
  WATCHER_STATE_EVENT,
  type OcrError,
  type TextObservation,
  type WatcherStatus,
} from "./watcher-state";

const watching: WatcherStatus = {
  enabled: true,
  state: "watching",
  lastTickError: null,
  error: null,
};

function observation(text: string, capturedAt = 1_752_800_000_000): TextObservation {
  return { text, appContext: "Safari", capturedAt };
}

describe("event names", () => {
  it("match the Rust-side IPC contract exactly", () => {
    // src-tauri/src/watcher/commands.rs pins the same strings from its side.
    expect(WATCHER_STATE_EVENT).toBe("watcher://state");
    expect(WATCHER_OBSERVATION_EVENT).toBe("watcher://observation");
  });
});

describe("watcherReducer status transitions", () => {
  it("starts unknown: no status, no snippets", () => {
    expect(initialWatcherViewState.status).toBeNull();
    expect(initialWatcherViewState.observations).toEqual([]);
  });

  it("stores the backend snapshot as authoritative", () => {
    const s = watcherReducer(initialWatcherViewState, { type: "status", status: watching });
    expect(s.status).toEqual(watching);
  });

  it("follows enable → paused-privacy → disable transitions", () => {
    let s = watcherReducer(initialWatcherViewState, { type: "status", status: watching });
    s = watcherReducer(s, {
      type: "status",
      status: { ...watching, state: "paused-privacy" },
    });
    expect(s.status?.state).toBe("paused-privacy");
    expect(s.status?.enabled).toBe(true);

    s = watcherReducer(s, {
      type: "status",
      status: { enabled: false, state: "idle", lastTickError: null, error: null },
    });
    expect(s.status?.state).toBe("idle");
    expect(s.status?.enabled).toBe(false);
  });

  it("keeps the snippet buffer across a disable — timestamps show staleness", () => {
    let s = watcherReducer(initialWatcherViewState, {
      type: "observation",
      observation: observation("on screen"),
    });
    s = watcherReducer(s, {
      type: "status",
      status: { enabled: false, state: "idle", lastTickError: null, error: null },
    });
    expect(s.observations).toHaveLength(1);
  });
});

describe("watcherReducer error states", () => {
  it("surfaces a typed tick error riding the status", () => {
    const err: OcrError = { kind: "permission-denied", detail: "TCC denied" };
    const s = watcherReducer(initialWatcherViewState, {
      type: "status",
      status: { ...watching, lastTickError: err },
    });
    expect(s.status?.lastTickError).toEqual(err);
  });

  it("a tick success clears the previous tick error via the next status", () => {
    let s = watcherReducer(initialWatcherViewState, {
      type: "status",
      status: {
        ...watching,
        lastTickError: { kind: "capture-failed", detail: "no display" },
      },
    });
    s = watcherReducer(s, { type: "status", status: watching });
    expect(s.status?.lastTickError).toBeNull();
  });

  it("surfaces a persist failure without losing the decided run state", () => {
    const s = watcherReducer(initialWatcherViewState, {
      type: "status",
      status: {
        enabled: false,
        state: "idle",
        lastTickError: null,
        error: "watcher: failed to persist watcherEnabled=true",
      },
    });
    // Rollback contract: the toggle reverted, the error says why.
    expect(s.status?.enabled).toBe(false);
    expect(s.status?.error).toContain("watcherEnabled");
  });
});

describe("watcherReducer rolling snippet buffer", () => {
  it("prepends: newest observation first", () => {
    let s = watcherReducer(initialWatcherViewState, {
      type: "observation",
      observation: observation("first", 1),
    });
    s = watcherReducer(s, { type: "observation", observation: observation("second", 2) });
    expect(s.observations.map((o) => o.text)).toEqual(["second", "first"]);
  });

  it("caps at MAX_SNIPPETS, dropping the oldest", () => {
    let s = initialWatcherViewState;
    for (let i = 1; i <= MAX_SNIPPETS + 2; i++) {
      s = watcherReducer(s, { type: "observation", observation: observation(`obs ${i}`, i) });
    }
    expect(s.observations).toHaveLength(MAX_SNIPPETS);
    expect(s.observations[0].text).toBe(`obs ${MAX_SNIPPETS + 2}`);
    expect(s.observations[MAX_SNIPPETS - 1].text).toBe("obs 3");
  });

  it("keeps a null appContext as-is (login window edge case)", () => {
    const s = watcherReducer(initialWatcherViewState, {
      type: "observation",
      observation: { text: "x", appContext: null, capturedAt: 1 },
    });
    expect(s.observations[0].appContext).toBeNull();
  });
});

describe("copy helpers", () => {
  it("labels every run state, paused-privacy visibly naming privacy", () => {
    expect(runStateLabel("idle")).toBe("Off");
    expect(runStateLabel("watching")).toBe("Watching");
    expect(runStateLabel("paused-privacy")).toBe("Paused by Privacy Mode");
  });

  it("titles every OcrError kind", () => {
    const titles = [
      tickErrorTitle({ kind: "permission-denied", detail: "" }),
      tickErrorTitle({ kind: "capture-failed", detail: "" }),
      tickErrorTitle({ kind: "recognition-failed", detail: "" }),
      tickErrorTitle({ kind: "unsupported", platform: "windows", detail: "" }),
    ];
    expect(new Set(titles).size).toBe(4);
    expect(titles[0]).toContain("permission");
  });

  it("detail names the platform only for unsupported", () => {
    expect(
      tickErrorDetail({ kind: "unsupported", platform: "windows", detail: "no backend" }),
    ).toBe("windows — no backend");
    expect(tickErrorDetail({ kind: "capture-failed", detail: "no display" })).toBe("no display");
  });

  it("collapses snippet newlines to one line", () => {
    expect(snippetPreview("hello\nworld")).toBe("hello · world");
  });

  it("truncates only past the preview cap, with an ellipsis", () => {
    const exact = "a".repeat(SNIPPET_PREVIEW_CHARS);
    expect(snippetPreview(exact)).toBe(exact);
    const over = "a".repeat(SNIPPET_PREVIEW_CHARS + 1);
    expect(snippetPreview(over)).toBe(`${"a".repeat(SNIPPET_PREVIEW_CHARS)}…`);
  });

  it("renders a capture timestamp as a non-empty local time", () => {
    expect(capturedAtLabel(1_752_800_000_000).length).toBeGreaterThan(0);
  });
});
