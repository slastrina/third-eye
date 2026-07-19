// Reducer + helper coverage for the Memory section (S04): pagination
// clamping, the inline-edit lifecycle (draft → save → invalid-input /
// not-found / db failures), both two-step confirms (per-row delete,
// wipe-all), the named unavailable state, and the pure copy helpers. The
// reducer is pure, so no Tauri runtime or DOM is needed.

import { describe, expect, it } from "vitest";
import {
  appsLabel,
  canGoNext,
  canGoPrev,
  initialMemoryViewState,
  isMemoryError,
  lastDistillLabel,
  MEMORY_PAGE_SIZE,
  memoryErrorMessage,
  memoryReducer,
  spanLabel,
  validateSummaryDraft,
  type MemoryRecord,
  type MemoryStatus,
  type MemoryViewState,
} from "./memory-state";

function record(id: number, summary = `memory ${id}`): MemoryRecord {
  return {
    id,
    summary,
    apps: ["Zed"],
    spanStartMs: 1_752_800_000_000,
    spanEndMs: 1_752_800_060_000,
    createdAtMs: 1_752_800_060_000,
    updatedAtMs: 1_752_800_060_000,
  };
}

function status(count: number | null, overrides: Partial<MemoryStatus> = {}): MemoryStatus {
  return {
    available: true,
    count,
    dbPath: "/tmp/memory.db",
    ingest: { buffered: 0, distilledCount: 0, lastDistillAtMs: null, lastError: null },
    ...overrides,
  };
}

/** A ready state with one loaded page — the common test starting point. */
function loaded(records: MemoryRecord[], overrides: Partial<MemoryViewState> = {}): MemoryViewState {
  let s = memoryReducer(initialMemoryViewState, { type: "list", records, offset: 0 });
  s = memoryReducer(s, { type: "status", status: status(records.length) });
  return { ...s, ...overrides };
}

const fullPage = Array.from({ length: MEMORY_PAGE_SIZE }, (_, i) => record(i + 1));

describe("initial state and availability", () => {
  it("starts unknown and loading, with nothing armed", () => {
    expect(initialMemoryViewState.availability).toBe("unknown");
    expect(initialMemoryViewState.loading).toBe(true);
    expect(initialMemoryViewState.edit).toBeNull();
    expect(initialMemoryViewState.confirmDelete).toBeNull();
    expect(initialMemoryViewState.confirmWipe).toBe(false);
  });

  it("a loaded page makes the view ready and stops loading", () => {
    const s = memoryReducer(initialMemoryViewState, {
      type: "list",
      records: [record(1)],
      offset: 0,
    });
    expect(s.availability).toBe("ready");
    expect(s.loading).toBe(false);
    expect(s.records).toHaveLength(1);
  });

  it("unavailable resets everything to the named degraded state", () => {
    const busy = loaded([record(1)], {
      edit: { id: 1, draft: "x", error: null, saving: false },
      confirmWipe: true,
      banner: "old",
    });
    const s = memoryReducer(busy, { type: "unavailable" });
    expect(s.availability).toBe("unavailable");
    expect(s.loading).toBe(false);
    expect(s.records).toEqual([]);
    expect(s.edit).toBeNull();
    expect(s.confirmWipe).toBe(false);
    expect(s.banner).toBeNull();
  });

  it("a status snapshot alone also makes the view ready", () => {
    const s = memoryReducer(initialMemoryViewState, { type: "status", status: status(0) });
    expect(s.availability).toBe("ready");
    expect(s.status?.count).toBe(0);
  });

  it("a list db failure surfaces a banner but stays ready, not unavailable", () => {
    const s = memoryReducer(initialMemoryViewState, {
      type: "list-failed",
      error: { kind: "db", detail: "disk I/O error" },
    });
    expect(s.availability).toBe("ready");
    expect(s.loading).toBe(false);
    expect(s.banner).toContain("disk I/O error");
  });
});

describe("pagination", () => {
  it("next advances one page and starts loading when the count says more exist", () => {
    let s = loaded(fullPage);
    s = memoryReducer(s, { type: "status", status: status(MEMORY_PAGE_SIZE + 5) });
    expect(canGoNext(s)).toBe(true);
    s = memoryReducer(s, { type: "next-page" });
    expect(s.offset).toBe(MEMORY_PAGE_SIZE);
    expect(s.loading).toBe(true);
  });

  it("next is clamped when the count says this is the last page", () => {
    const s = loaded(fullPage); // count === MEMORY_PAGE_SIZE
    expect(canGoNext(s)).toBe(false);
    expect(memoryReducer(s, { type: "next-page" })).toBe(s);
  });

  it("without a count, next falls back to the full-page heuristic", () => {
    const noCount = { ...loaded(fullPage), status: null };
    expect(canGoNext(noCount)).toBe(true);
    const partial = { ...loaded([record(1)]), status: null };
    expect(canGoNext(partial)).toBe(false);
  });

  it("prev is clamped at offset 0", () => {
    const s = loaded([record(1)]);
    expect(canGoPrev(s)).toBe(false);
    expect(memoryReducer(s, { type: "prev-page" })).toBe(s);
  });

  it("prev steps back one page from a deeper offset", () => {
    let s = loaded(fullPage, { offset: MEMORY_PAGE_SIZE * 2 });
    expect(canGoPrev(s)).toBe(true);
    s = memoryReducer(s, { type: "prev-page" });
    expect(s.offset).toBe(MEMORY_PAGE_SIZE);
    expect(s.loading).toBe(true);
  });

  it("page turns disarm edits and confirms", () => {
    let s = loaded(fullPage, {
      status: status(MEMORY_PAGE_SIZE * 2),
      edit: { id: 1, draft: "x", error: null, saving: false },
      confirmDelete: 2,
    });
    s = memoryReducer(s, { type: "next-page" });
    expect(s.edit).toBeNull();
    expect(s.confirmDelete).toBeNull();
  });

  it("an empty page above offset 0 clamps back a page and refetches", () => {
    const deep = loaded([record(1)], { offset: MEMORY_PAGE_SIZE });
    const before = deep.refreshToken;
    const s = memoryReducer(deep, { type: "list", records: [], offset: MEMORY_PAGE_SIZE });
    expect(s.offset).toBe(0);
    expect(s.loading).toBe(true);
    expect(s.refreshToken).toBe(before + 1);
  });

  it("an empty page at offset 0 is just the empty state", () => {
    const s = memoryReducer(initialMemoryViewState, { type: "list", records: [], offset: 0 });
    expect(s.records).toEqual([]);
    expect(s.loading).toBe(false);
    expect(s.refreshToken).toBe(initialMemoryViewState.refreshToken);
  });
});

describe("inline edit lifecycle", () => {
  it("begin-edit seeds the draft from the record and disarms confirms", () => {
    let s = loaded([record(1, "original")], { confirmDelete: 1, confirmWipe: true });
    s = memoryReducer(s, { type: "begin-edit", id: 1 });
    expect(s.edit).toEqual({ id: 1, draft: "original", error: null, saving: false });
    expect(s.confirmDelete).toBeNull();
    expect(s.confirmWipe).toBe(false);
  });

  it("begin-edit on an unknown id is a no-op", () => {
    const s = loaded([record(1)]);
    expect(memoryReducer(s, { type: "begin-edit", id: 99 })).toBe(s);
  });

  it("typing updates the draft and clears a prior inline error", () => {
    let s = loaded([record(1)], {
      edit: { id: 1, draft: "", error: "Summary can't be empty", saving: false },
    });
    s = memoryReducer(s, { type: "edit-draft", draft: "better" });
    expect(s.edit?.draft).toBe("better");
    expect(s.edit?.error).toBeNull();
  });

  it("save-edit with a blank draft shows the inline error and stays in edit mode", () => {
    let s = loaded([record(1)], {
      edit: { id: 1, draft: "   ", error: null, saving: false },
    });
    s = memoryReducer(s, { type: "save-edit" });
    expect(s.edit?.error).toBe("Summary can't be empty");
    expect(s.edit?.saving).toBe(false);
  });

  it("save-edit with a valid draft flips saving on", () => {
    let s = loaded([record(1)], {
      edit: { id: 1, draft: "new text", error: null, saving: false },
    });
    s = memoryReducer(s, { type: "save-edit" });
    expect(s.edit?.saving).toBe(true);
  });

  it("edit-saved replaces the row, exits edit mode, and requests a refresh", () => {
    const before = loaded([record(1, "old"), record(2)], {
      edit: { id: 1, draft: "new", error: null, saving: true },
    });
    const s = memoryReducer(before, { type: "edit-saved", record: record(1, "new") });
    expect(s.records[0].summary).toBe("new");
    expect(s.records[1].summary).toBe("memory 2");
    expect(s.edit).toBeNull();
    expect(s.refreshToken).toBe(before.refreshToken + 1);
  });

  it("backend invalid-input keeps edit mode open with the inline detail", () => {
    let s = loaded([record(1)], {
      edit: { id: 1, draft: " ", error: null, saving: true },
    });
    s = memoryReducer(s, { type: "edit-failed", error: { kind: "invalid-input", detail: "summary must not be empty" } });
    expect(s.edit?.error).toBe("summary must not be empty");
    expect(s.edit?.saving).toBe(false);
    expect(s.banner).toBeNull();
  });

  it("not-found on save drops edit mode, banners, and requests a refresh", () => {
    const before = loaded([record(1)], {
      edit: { id: 1, draft: "x", error: null, saving: true },
    });
    const s = memoryReducer(before, { type: "edit-failed", error: { kind: "not-found", id: 1 } });
    expect(s.edit).toBeNull();
    expect(s.banner).toBe("Memory #1 no longer exists");
    expect(s.refreshToken).toBe(before.refreshToken + 1);
  });

  it("a db failure on save keeps the draft and shows a dismissible banner", () => {
    let s = loaded([record(1)], {
      edit: { id: 1, draft: "precious draft", error: null, saving: true },
    });
    s = memoryReducer(s, { type: "edit-failed", error: { kind: "db", detail: "locked" } });
    expect(s.edit?.draft).toBe("precious draft");
    expect(s.edit?.saving).toBe(false);
    expect(s.banner).toBe("Memory store error: locked");
    s = memoryReducer(s, { type: "dismiss-banner" });
    expect(s.banner).toBeNull();
  });

  it("cancel-edit discards the draft", () => {
    let s = loaded([record(1)], {
      edit: { id: 1, draft: "abandoned", error: null, saving: false },
    });
    s = memoryReducer(s, { type: "cancel-edit" });
    expect(s.edit).toBeNull();
  });
});

describe("two-step delete", () => {
  it("request-delete arms exactly one row and disarms a pending wipe", () => {
    let s = loaded([record(1), record(2)], { confirmWipe: true });
    s = memoryReducer(s, { type: "request-delete", id: 2 });
    expect(s.confirmDelete).toBe(2);
    expect(s.confirmWipe).toBe(false);
  });

  it("request-delete on an unknown id is a no-op", () => {
    const s = loaded([record(1)]);
    expect(memoryReducer(s, { type: "request-delete", id: 99 })).toBe(s);
  });

  it("cancel-delete disarms without touching the list", () => {
    let s = loaded([record(1)], { confirmDelete: 1 });
    s = memoryReducer(s, { type: "cancel-delete" });
    expect(s.confirmDelete).toBeNull();
    expect(s.records).toHaveLength(1);
  });

  it("deleted removes the row, disarms, and requests a refresh", () => {
    const before = loaded([record(1), record(2)], { confirmDelete: 1 });
    const s = memoryReducer(before, { type: "deleted", id: 1 });
    expect(s.records.map((r) => r.id)).toEqual([2]);
    expect(s.confirmDelete).toBeNull();
    expect(s.refreshToken).toBe(before.refreshToken + 1);
  });

  it("deleting the row currently being edited also drops edit mode", () => {
    let s = loaded([record(1)], {
      confirmDelete: 1,
      edit: { id: 1, draft: "x", error: null, saving: false },
    });
    s = memoryReducer(s, { type: "deleted", id: 1 });
    expect(s.edit).toBeNull();
  });

  it("a not-found delete banners and requests a refresh (list was stale)", () => {
    const before = loaded([record(1)], { confirmDelete: 1 });
    const s = memoryReducer(before, { type: "delete-failed", error: { kind: "not-found", id: 1 } });
    expect(s.confirmDelete).toBeNull();
    expect(s.banner).toBe("Memory #1 no longer exists");
    expect(s.refreshToken).toBe(before.refreshToken + 1);
  });

  it("a db delete failure banners without a refresh", () => {
    const before = loaded([record(1)], { confirmDelete: 1 });
    const s = memoryReducer(before, { type: "delete-failed", error: { kind: "db", detail: "busy" } });
    expect(s.banner).toBe("Memory store error: busy");
    expect(s.refreshToken).toBe(before.refreshToken);
  });
});

describe("two-step wipe", () => {
  it("request-wipe arms and disarms a pending row delete", () => {
    let s = loaded([record(1)], { confirmDelete: 1 });
    s = memoryReducer(s, { type: "request-wipe" });
    expect(s.confirmWipe).toBe(true);
    expect(s.confirmDelete).toBeNull();
  });

  it("cancel-wipe disarms without touching the store", () => {
    let s = loaded([record(1)], { confirmWipe: true });
    s = memoryReducer(s, { type: "cancel-wipe" });
    expect(s.confirmWipe).toBe(false);
    expect(s.records).toHaveLength(1);
  });

  it("wiped clears the page, resets the offset, notices the count, and refetches", () => {
    const before = loaded(fullPage, { offset: MEMORY_PAGE_SIZE, confirmWipe: true });
    const s = memoryReducer(before, { type: "wiped", removed: 27 });
    expect(s.records).toEqual([]);
    expect(s.offset).toBe(0);
    expect(s.confirmWipe).toBe(false);
    expect(s.notice).toBe("Cleared 27 memories");
    expect(s.refreshToken).toBe(before.refreshToken + 1);
  });

  it("wiping a single memory uses the singular notice", () => {
    const s = memoryReducer(loaded([record(1)], { confirmWipe: true }), {
      type: "wiped",
      removed: 1,
    });
    expect(s.notice).toBe("Cleared 1 memory");
    const dismissed = memoryReducer(s, { type: "dismiss-notice" });
    expect(dismissed.notice).toBeNull();
  });

  it("a wipe failure disarms and banners", () => {
    const s = memoryReducer(loaded([record(1)], { confirmWipe: true }), {
      type: "wipe-failed",
      error: { kind: "db", detail: "readonly" },
    });
    expect(s.confirmWipe).toBe(false);
    expect(s.banner).toBe("Memory store error: readonly");
    expect(s.records).toHaveLength(1);
  });
});

describe("error narrowing and copy helpers", () => {
  it("isMemoryError accepts exactly the kind-tagged contract", () => {
    expect(isMemoryError({ kind: "db", detail: "x" })).toBe(true);
    expect(isMemoryError({ kind: "not-found", id: 3 })).toBe(true);
    expect(isMemoryError({ kind: "invalid-input", detail: "x" })).toBe(true);
    // Outside Tauri, invoke rejects with strings/Errors — not memory errors.
    expect(isMemoryError("window.__TAURI_INTERNALS__ is undefined")).toBe(false);
    expect(isMemoryError(new Error("no runtime"))).toBe(false);
    expect(isMemoryError(null)).toBe(false);
    expect(isMemoryError({ kind: "offline" })).toBe(false);
  });

  it("memoryErrorMessage covers every kind", () => {
    expect(memoryErrorMessage({ kind: "db", detail: "corrupt" })).toBe(
      "Memory store error: corrupt",
    );
    expect(memoryErrorMessage({ kind: "not-found", id: 7 })).toBe("Memory #7 no longer exists");
    expect(memoryErrorMessage({ kind: "invalid-input", detail: "too long" })).toBe("too long");
  });

  it("validateSummaryDraft rejects blank and whitespace-only drafts", () => {
    expect(validateSummaryDraft("")).toBe("Summary can't be empty");
    expect(validateSummaryDraft("  \n\t ")).toBe("Summary can't be empty");
    expect(validateSummaryDraft("fine")).toBeNull();
  });

  it("appsLabel joins apps and names the unknown case", () => {
    expect(appsLabel(["Zed", "Safari"])).toBe("Zed, Safari");
    expect(appsLabel([])).toBe("Unknown app");
  });

  it("spanLabel collapses same-day spans and spells out cross-day spans", () => {
    const start = 1_752_800_000_000;
    const sameDay = spanLabel(start, start + 60_000);
    expect(sameDay).toContain("–");
    expect(sameDay.indexOf(new Date(start).toLocaleDateString())).toBe(
      sameDay.lastIndexOf(new Date(start).toLocaleDateString()),
    );
    const crossDay = spanLabel(start, start + 48 * 60 * 60 * 1000);
    expect(crossDay).toContain(new Date(start).toLocaleString());
    expect(crossDay).toContain(new Date(start + 48 * 60 * 60 * 1000).toLocaleString());
  });

  it("lastDistillLabel names the never case and formats timestamps", () => {
    expect(lastDistillLabel(null)).toBe("never");
    expect(lastDistillLabel(1_752_800_000_000)).toBe(
      new Date(1_752_800_000_000).toLocaleString(),
    );
  });
});
