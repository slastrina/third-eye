import { test, expect, type Page } from "@playwright/test";

// The Memory section (?view=settings) in a plain browser.
//
// Two modes, same contract watcher.spec.ts proves for the watcher section:
//  - No mock: every invoke() rejects (no Tauri runtime), so the section must
//    degrade into its named unavailable state and render nothing else.
//  - Mocked IPC: an init script installs a minimal window.__TAURI_INTERNALS__
//    backed by a stateful in-page record store (mutable array standing in for
//    the S02 SQLite store), so browse/paginate, inline edit, two-step delete,
//    two-step wipe, the "Cleared N" notice, memory_status health rows, and
//    every kind-tagged error path (db / not-found / invalid-input) run
//    through the real bundle in a real browser.

interface SeedRecord {
  id: number;
  summary: string;
  apps: string[];
  spanStartMs: number;
  spanEndMs: number;
  createdAtMs: number;
  updatedAtMs: number;
}

interface SeedIngest {
  buffered: number;
  distilledCount: number;
  lastDistillAtMs: number | null;
  lastError: unknown;
}

/** n seeded memories, ids 1..n ascending — the mock serves them newest-first
 *  (highest id first), matching the store's ORDER BY created_at DESC. */
function seedRecords(n: number): SeedRecord[] {
  const base = 1_700_000_000_000;
  return Array.from({ length: n }, (_, i) => {
    const id = i + 1;
    return {
      id,
      summary: `memory ${id}`,
      apps: id % 2 === 0 ? ["Safari"] : [],
      spanStartMs: base + id * 60_000,
      spanEndMs: base + id * 60_000 + 30_000,
      createdAtMs: base + id * 60_000 + 30_000,
      updatedAtMs: base + id * 60_000 + 30_000,
    };
  });
}

const defaultIngest: SeedIngest = {
  buffered: 0,
  distilledCount: 0,
  lastDistillAtMs: null,
  lastError: null,
};

/** Serve a fake Tauri backend for the S02 memory IPC surface. Non-memory
 *  commands still reject so the rest of Settings keeps its proven degrade
 *  states. Exposes window.__mockMemory with test hooks:
 *   - removeRaw(id): drop a row behind the UI's back (stale-list scenarios)
 *   - failNext(cmd, error): make the next call to cmd reject with a
 *     kind-tagged MemoryError instead of touching the store */
async function installMemoryIpcMock(
  page: Page,
  opts: { records?: SeedRecord[]; ingest?: SeedIngest } = {},
): Promise<void> {
  await page.addInitScript(
    (seed: { records: SeedRecord[]; ingest: SeedIngest }) => {
      let records: SeedRecord[] = [...seed.records];
      const failNext = new Map<string, unknown>();

      (window as any).__mockMemory = {
        removeRaw(id: number) {
          records = records.filter((r) => r.id !== id);
        },
        failNext(cmd: string, error: unknown) {
          failNext.set(cmd, error);
        },
      };

      const takeFailure = (cmd: string): unknown | undefined => {
        const err = failNext.get(cmd);
        if (err !== undefined) failNext.delete(cmd);
        return err;
      };

      (window as any).__TAURI_INTERNALS__ = {
        transformCallback: () => 0,
        unregisterCallback: () => {},
        invoke(cmd: string, args: any) {
          const injected = takeFailure(cmd);
          if (injected !== undefined) return Promise.reject(injected);
          switch (cmd) {
            case "memory_list": {
              const page = [...records]
                .sort((a, b) => b.createdAtMs - a.createdAtMs)
                .slice(args.offset, args.offset + args.limit);
              return Promise.resolve(page);
            }
            case "memory_update": {
              if (args.summary.trim().length === 0)
                return Promise.reject({ kind: "invalid-input", detail: "summary is empty" });
              const record = records.find((r) => r.id === args.id);
              if (!record) return Promise.reject({ kind: "not-found", id: args.id });
              record.summary = args.summary;
              record.updatedAtMs = record.createdAtMs + 3_600_000;
              return Promise.resolve({ ...record });
            }
            case "memory_delete": {
              if (!records.some((r) => r.id === args.id))
                return Promise.reject({ kind: "not-found", id: args.id });
              records = records.filter((r) => r.id !== args.id);
              return Promise.resolve(null);
            }
            case "memory_wipe": {
              const removed = records.length;
              records = [];
              return Promise.resolve(removed);
            }
            case "memory_status":
              return Promise.resolve({
                available: true,
                count: records.length,
                dbPath: "/mock/memory.db",
                ingest: seed.ingest,
              });
            default:
              return Promise.reject(`mock: no such command ${cmd}`);
          }
        },
      };
    },
    { records: opts.records ?? seedRecords(3), ingest: opts.ingest ?? defaultIngest },
  );
}

const section = (page: Page) =>
  page.locator("section", { has: page.locator("#settings-memory-heading") });

test("memory section degrades to a named unavailable state outside Tauri", async ({ page }) => {
  await page.goto("/?view=settings");
  const memory = section(page);
  await expect(memory.getByRole("heading", { name: "Memory" })).toBeVisible();
  // memory_list rejects with a plain string (no runtime) → the named message
  // and nothing else: no rows, no health rows, no wipe control, no banners.
  await expect(memory.locator(".settings-unavailable")).toHaveText(
    "Memory is unavailable outside the app",
  );
  await expect(memory.locator(".memory-row")).toHaveCount(0);
  await expect(memory.locator(".settings-status-row")).toHaveCount(0);
  await expect(memory.getByRole("button", { name: "Wipe all memories" })).toHaveCount(0);
  await expect(memory.locator(".settings-error")).toHaveCount(0);
});

test("stored memories render newest-first with health rows from memory_status", async ({ page }) => {
  await installMemoryIpcMock(page, {
    records: seedRecords(3),
    ingest: { buffered: 3, distilledCount: 7, lastDistillAtMs: null, lastError: null },
  });
  await page.goto("/?view=settings");
  const memory = section(page);

  // Newest first: id 3 on top, id 1 last.
  const rows = memory.locator(".memory-row");
  await expect(rows).toHaveCount(3);
  await expect(rows.first().locator(".memory-summary")).toHaveText("memory 3");
  await expect(rows.last().locator(".memory-summary")).toHaveText("memory 1");
  // App context on the meta line; a span with no known app gets the placeholder.
  await expect(rows.nth(1).locator(".memory-meta").first()).toContainText("Safari");
  await expect(rows.first().locator(".memory-meta").first()).toContainText("Unknown app");

  // memory_status health-as-value surfaces: count, ingest, last distill.
  await expect(memory.locator(".settings-status-row", { hasText: "Stored memories" })).toContainText("3");
  await expect(memory.locator(".settings-status-row", { hasText: "Ingest" })).toContainText(
    "3 buffered · 7 distilled",
  );
  await expect(memory.locator(".settings-status-row", { hasText: "Last distill" })).toContainText(
    "never",
  );
  // Healthy store: no alerts anywhere in the section.
  await expect(memory.getByRole("alert")).toHaveCount(0);
});

test("empty store shows the hint and hides pager and wipe controls", async ({ page }) => {
  await installMemoryIpcMock(page, { records: [] });
  await page.goto("/?view=settings");
  const memory = section(page);
  await expect(memory.locator(".settings-hint")).toHaveText("No memories stored yet");
  await expect(memory.locator(".memory-row")).toHaveCount(0);
  await expect(memory.locator(".memory-pager")).toHaveCount(0);
  await expect(memory.getByRole("button", { name: "Wipe all memories" })).toHaveCount(0);
});

test("an ingest LlmError from memory_status surfaces as an alert", async ({ page }) => {
  await installMemoryIpcMock(page, {
    records: seedRecords(1),
    ingest: {
      buffered: 2,
      distilledCount: 0,
      lastDistillAtMs: null,
      lastError: {
        kind: "offline",
        endpoint: "http://localhost:11434",
        detail: "connection refused",
      },
    },
  });
  await page.goto("/?view=settings");
  const memory = section(page);
  const alert = memory.getByRole("alert");
  await expect(alert).toContainText("Local AI offline");
  await expect(alert).toContainText("http://localhost:11434 — connection refused");
});

test("pagination pages newest-first and clamps at both ends", async ({ page }) => {
  await installMemoryIpcMock(page, { records: seedRecords(30) });
  await page.goto("/?view=settings");
  const memory = section(page);
  const rows = memory.locator(".memory-row");
  const prev = memory.getByRole("button", { name: "Prev" });
  const next = memory.getByRole("button", { name: "Next" });

  // Page 1: 25 newest (30..6), Prev clamped off, Next live (count=30 > 25).
  await expect(rows).toHaveCount(25);
  await expect(rows.first().locator(".memory-summary")).toHaveText("memory 30");
  await expect(rows.last().locator(".memory-summary")).toHaveText("memory 6");
  await expect(memory.locator(".memory-pager")).toContainText("Page 1");
  await expect(prev).toBeDisabled();
  await expect(next).toBeEnabled();

  // Page 2: the remaining 5 (5..1), Next clamped off.
  await next.click();
  await expect(rows).toHaveCount(5);
  await expect(rows.first().locator(".memory-summary")).toHaveText("memory 5");
  await expect(rows.last().locator(".memory-summary")).toHaveText("memory 1");
  await expect(memory.locator(".memory-pager")).toContainText("Page 2");
  await expect(next).toBeDisabled();
  await expect(prev).toBeEnabled();

  // Back to page 1.
  await prev.click();
  await expect(rows).toHaveCount(25);
  await expect(rows.first().locator(".memory-summary")).toHaveText("memory 30");
  await expect(prev).toBeDisabled();
});

test("inline edit persists through memory_update and survives a refetch", async ({ page }) => {
  await installMemoryIpcMock(page, { records: seedRecords(3) });
  await page.goto("/?view=settings");
  const memory = section(page);
  const topRow = memory.locator(".memory-row").first();

  // Click-to-edit opens a textarea prefilled with the current summary.
  await topRow.locator(".memory-summary").click();
  const input = memory.getByLabel("Edit memory summary");
  await expect(input).toHaveValue("memory 3");

  await input.fill("rewritten summary");
  await memory.getByRole("button", { name: "Save", exact: true }).click();

  // Edit mode closes and the row shows the new summary. The post-save
  // refetch re-reads the mock store, so this text is the persisted value
  // coming back over memory_list, not a local echo.
  await expect(memory.getByLabel("Edit memory summary")).toHaveCount(0);
  await expect(topRow.locator(".memory-summary")).toHaveText("rewritten summary");
  // The store bumped updatedAtMs, so the meta line now shows Updated too.
  await expect(topRow.locator(".memory-meta").last()).toContainText("Updated");

  // Cancel leaves the stored summary untouched.
  await topRow.locator(".memory-summary").click();
  await memory.getByLabel("Edit memory summary").fill("discard me");
  await memory.getByRole("button", { name: "Cancel", exact: true }).click();
  await expect(topRow.locator(".memory-summary")).toHaveText("rewritten summary");
});

test("a blank draft is rejected inline before any round-trip", async ({ page }) => {
  await installMemoryIpcMock(page, { records: seedRecords(1) });
  await page.goto("/?view=settings");
  const memory = section(page);

  await memory.locator(".memory-summary").first().click();
  const input = memory.getByLabel("Edit memory summary");
  await input.fill("   ");
  await memory.getByRole("button", { name: "Save", exact: true }).click();

  // Inline error, edit mode stays open, the draft stands, no banner.
  await expect(memory.locator(".memory-edit-error")).toHaveText("Summary can't be empty");
  await expect(input).toHaveValue("   ");
  await expect(memory.locator(".settings-error")).toHaveCount(0);
});

test("a backend invalid-input rejection keeps edit mode open with the detail", async ({ page }) => {
  await installMemoryIpcMock(page, { records: seedRecords(1) });
  await page.goto("/?view=settings");
  const memory = section(page);
  await expect(memory.locator(".memory-row")).toHaveCount(1);

  await page.evaluate(() => {
    (window as any).__mockMemory.failNext("memory_update", {
      kind: "invalid-input",
      detail: "summary is too long",
    });
  });
  await memory.locator(".memory-summary").first().click();
  const input = memory.getByLabel("Edit memory summary");
  await input.fill("a perfectly fine draft");
  await memory.getByRole("button", { name: "Save", exact: true }).click();

  await expect(memory.locator(".memory-edit-error")).toHaveText("summary is too long");
  await expect(input).toHaveValue("a perfectly fine draft");
  await expect(memory.locator(".memory-summary")).toHaveCount(0);
});

test("per-row delete is two-step: cancel disarms, confirm removes and re-polls status", async ({ page }) => {
  await installMemoryIpcMock(page, { records: seedRecords(3) });
  await page.goto("/?view=settings");
  const memory = section(page);
  const rows = memory.locator(".memory-row");
  await expect(rows).toHaveCount(3);
  await expect(memory.locator(".settings-status-row", { hasText: "Stored memories" })).toContainText("3");

  // Step one arms the confirm; Cancel disarms without touching the store.
  await memory.getByRole("button", { name: "Delete memory 3" }).click();
  await expect(memory.getByRole("button", { name: "Confirm delete" })).toBeVisible();
  await rows.first().getByRole("button", { name: "Cancel", exact: true }).click();
  await expect(memory.getByRole("button", { name: "Confirm delete" })).toHaveCount(0);
  await expect(rows).toHaveCount(3);

  // Arm again and confirm: the row goes, and the post-mutation status poll
  // brings the authoritative count down to 2.
  await memory.getByRole("button", { name: "Delete memory 3" }).click();
  await memory.getByRole("button", { name: "Confirm delete" }).click();
  await expect(rows).toHaveCount(2);
  await expect(memory.getByRole("button", { name: "Delete memory 3" })).toHaveCount(0);
  await expect(memory.locator(".settings-status-row", { hasText: "Stored memories" })).toContainText("2");
});

test("deleting a row that vanished shows the not-found banner and refreshes the list", async ({ page }) => {
  await installMemoryIpcMock(page, { records: seedRecords(2) });
  await page.goto("/?view=settings");
  const memory = section(page);
  await expect(memory.locator(".memory-row")).toHaveCount(2);

  // The row disappears behind the UI's back (deleted elsewhere), then the
  // user confirms a delete against the now-stale list.
  await page.evaluate(() => (window as any).__mockMemory.removeRaw(2));
  await memory.getByRole("button", { name: "Delete memory 2" }).click();
  await memory.getByRole("button", { name: "Confirm delete" }).click();

  const banner = memory.locator(".settings-error", { hasText: "no longer exists" });
  await expect(banner).toContainText("Memory #2 no longer exists");
  // not-found marks the list stale → refetch drops the phantom row.
  await expect(memory.locator(".memory-row")).toHaveCount(1);
  await banner.getByRole("button", { name: "Dismiss" }).click();
  await expect(memory.locator(".settings-error")).toHaveCount(0);
});

test("wipe-all is two-step and lands on the Cleared notice then the empty state", async ({ page }) => {
  await installMemoryIpcMock(page, { records: seedRecords(3) });
  await page.goto("/?view=settings");
  const memory = section(page);
  await expect(memory.locator(".memory-row")).toHaveCount(3);

  // Step one arms the warning; Cancel backs out with the store intact.
  await memory.getByRole("button", { name: "Wipe all memories" }).click();
  await expect(memory.locator(".memory-wipe-warning")).toContainText(
    "Delete all stored memories? This can't be undone.",
  );
  await memory.locator(".memory-wipe-row").getByRole("button", { name: "Cancel", exact: true }).click();
  await expect(memory.locator(".memory-wipe-warning")).toHaveCount(0);
  await expect(memory.locator(".memory-row")).toHaveCount(3);

  // Arm again and confirm: notice with the removed count, empty hint, count 0.
  await memory.getByRole("button", { name: "Wipe all memories" }).click();
  await memory.getByRole("button", { name: "Confirm wipe" }).click();
  const notice = memory.locator(".settings-notice");
  await expect(notice).toContainText("Cleared 3 memories");
  await expect(memory.locator(".memory-row")).toHaveCount(0);
  await expect(memory.locator(".settings-hint")).toHaveText("No memories stored yet");
  await expect(memory.locator(".settings-status-row", { hasText: "Stored memories" })).toContainText("0");
  await notice.getByRole("button", { name: "Dismiss" }).click();
  await expect(memory.locator(".settings-notice")).toHaveCount(0);
});

test("a db failure on wipe surfaces as a dismissible banner and keeps the store", async ({ page }) => {
  await installMemoryIpcMock(page, { records: seedRecords(2) });
  await page.goto("/?view=settings");
  const memory = section(page);
  await expect(memory.locator(".memory-row")).toHaveCount(2);

  await page.evaluate(() => {
    (window as any).__mockMemory.failNext("memory_wipe", {
      kind: "db",
      detail: "disk I/O error",
    });
  });
  await memory.getByRole("button", { name: "Wipe all memories" }).click();
  await memory.getByRole("button", { name: "Confirm wipe" }).click();

  const banner = memory.locator(".settings-error");
  await expect(banner).toContainText("Memory store error: disk I/O error");
  // The confirm disarmed but nothing was deleted.
  await expect(memory.locator(".memory-wipe-warning")).toHaveCount(0);
  await expect(memory.locator(".memory-row")).toHaveCount(2);
  await banner.getByRole("button", { name: "Dismiss" }).click();
  await expect(memory.locator(".settings-error")).toHaveCount(0);
});
