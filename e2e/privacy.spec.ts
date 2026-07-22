import { test, expect, type Page } from "@playwright/test";

// The Privacy Guard sub-surface (?view=settings) in a plain browser.
//
// Two modes, same contract watcher.spec.ts proves for its section:
//  - No mock: every invoke() rejects (no Tauri runtime), so the sub-surface
//    degrades to its named unavailable line — no counter rows, no crash.
//  - Mocked IPC: an init script installs the minimal window.__TAURI_INTERNALS__
//    (invoke + transformCallback) that answers guard_status and lets tests
//    emit privacy://state, so live counters, the blocked count, and the
//    fail-closed banner are exercised through the real bundle.
//
// The wire contract carries detection kinds and counts only — never original
// or redacted text. SECRET below is a sentinel for text the guard would have
// redacted backend-side; every mocked test closes by asserting it never
// appears anywhere in the Settings DOM.

const SECRET = "hunter2-P@ssw0rd-4111111111111111-sk-live-abc123";

/** Serve a fake Tauri backend for the guard IPC surface. Non-guard commands
 *  still reject so the rest of Settings keeps its proven degrade states.
 *  Exposes window.__mockEmit(event, payload) to tests. */
async function installGuardIpcMock(page: Page): Promise<void> {
  await page.addInitScript(() => {
    const callbacks = new Map<number, (e: unknown) => void>();
    // event name -> callback ids registered via plugin:event|listen
    const listeners = new Map<string, Set<number>>();
    let nextId = 1;

    (window as any).__mockEmit = (event: string, payload: unknown) => {
      for (const id of listeners.get(event) ?? []) {
        callbacks.get(id)?.({ event, id, payload });
      }
    };

    // unlisten() calls this before the plugin:event|unlisten invoke; without
    // it cleanup throws and StrictMode's first mount keeps double-delivering.
    (window as any).__TAURI_EVENT_PLUGIN_INTERNALS__ = {
      unregisterListener(event: string, eventId: number) {
        listeners.get(event)?.delete(eventId);
        callbacks.delete(eventId);
      },
    };

    (window as any).__TAURI_INTERNALS__ = {
      transformCallback(cb: (e: unknown) => void) {
        const id = nextId++;
        callbacks.set(id, cb);
        return id;
      },
      unregisterCallback(id: number) {
        callbacks.delete(id);
      },
      invoke(cmd: string, args: any) {
        switch (cmd) {
          case "plugin:event|listen": {
            if (!listeners.has(args.event)) listeners.set(args.event, new Set());
            listeners.get(args.event)!.add(args.handler);
            return Promise.resolve(args.handler);
          }
          case "plugin:event|unlisten": {
            listeners.get(args.event)?.delete(args.eventId);
            callbacks.delete(args.eventId);
            return Promise.resolve();
          }
          // Never-rejecting health-as-value contract: a fresh guard has no
          // redactions yet (the wire omits zero-count kinds) and no blocks.
          case "guard_status":
            return Promise.resolve({ redactions: [], blockedCount: 0 });
          default:
            return Promise.reject(`mock: no such command ${cmd}`);
        }
      },
    };
  });
}

const guardSection = (page: Page) => page.locator(".guard-subsection");

test("guard sub-surface degrades to a named unavailable state outside Tauri", async ({ page }) => {
  await page.goto("/?view=settings&section=privacy");
  const guard = guardSection(page);
  await expect(guard.getByRole("heading", { name: "Privacy Guard" })).toBeVisible();
  // guard_status rejects → named unavailable line, not a crash.
  await expect(guard.locator(".settings-unavailable")).toHaveText(
    "Privacy guard state is unavailable outside the app.",
  );
  // No counter rows, no blocked row, no error banner without backend truth.
  await expect(guard.locator("[data-guard-active]")).toHaveCount(0);
  await expect(guard.locator("[data-guard-kind]")).toHaveCount(0);
  await expect(guard.locator("[data-guard-blocked]")).toHaveCount(0);
  await expect(guard.locator(".settings-error")).toHaveCount(0);
});

test("guard_status renders Active with zero-filled counters, and privacy://state increments per-kind counters exactly once", async ({ page }) => {
  await installGuardIpcMock(page);
  await page.goto("/?view=settings&section=privacy");
  const guard = guardSection(page);

  // Mount-time guard_status resolves → Active, all three known kinds
  // zero-filled ("0" is visible evidence, not absence), zero blocked.
  await expect(guard.locator("[data-guard-active]")).toHaveText("Active");
  await expect(guard.locator("[data-guard-kind]")).toHaveCount(3);
  for (const kind of ["password", "card", "api-key"]) {
    await expect(
      guard.locator(`[data-guard-kind="${kind}"] .settings-status-value`),
    ).toHaveText("0");
  }
  await expect(guard.locator("[data-guard-blocked] .settings-status-value")).toHaveText("0");
  await expect(guard.locator(".settings-unavailable")).toHaveCount(0);

  // A seeded redaction snapshot through the real event path: counters land
  // exactly once (unlisten honored → no StrictMode double-delivery).
  await page.evaluate(() => {
    (window as any).__mockEmit("privacy://state", {
      redactions: [
        { kind: "password", count: 2 },
        { kind: "card", count: 1 },
      ],
      blockedCount: 0,
    });
  });
  await expect(
    guard.locator('[data-guard-kind="password"] .settings-status-value'),
  ).toHaveText("2");
  await expect(
    guard.locator('[data-guard-kind="card"] .settings-status-value'),
  ).toHaveText("1");
  // Untouched kind stays a visible zero.
  await expect(
    guard.locator('[data-guard-kind="api-key"] .settings-status-value'),
  ).toHaveText("0");

  // A follow-up increment (the watcher-side redaction path deferred from
  // S02) lands as a fresh authoritative snapshot, not an accumulation bug.
  await page.evaluate(() => {
    (window as any).__mockEmit("privacy://state", {
      redactions: [
        { kind: "password", count: 3 },
        { kind: "card", count: 1 },
      ],
      blockedCount: 0,
    });
  });
  await expect(
    guard.locator('[data-guard-kind="password"] .settings-status-value'),
  ).toHaveText("3");

  // Kinds-and-counts-only: the sentinel secret never reaches the DOM.
  await expect(page.locator("body")).not.toContainText(SECRET);
});

test("a guard block surfaces the fail-closed state: blocked count, last-block reason, and the guard-blocked banner", async ({ page }) => {
  await installGuardIpcMock(page);
  await page.goto("/?view=settings&section=privacy");
  const guard = guardSection(page);
  await expect(guard.locator("[data-guard-active]")).toHaveText("Active");

  await page.evaluate(() => {
    (window as any).__mockEmit("privacy://state", {
      redactions: [{ kind: "api-key", count: 1 }],
      blockedCount: 1,
      lastBlockReason: "redaction-failed",
      lastError: {
        kind: "guard-blocked",
        endpoint: "http://192.168.1.50:1234",
        reason: "redaction-failed",
      },
    });
  });

  await expect(guard.locator("[data-guard-blocked] .settings-status-value")).toHaveText("1");
  await expect(guard.locator("[data-guard-last-block] .settings-status-value")).toHaveText(
    "Redaction failed",
  );
  // The typed guard-blocked lastError rides the existing banner copy.
  const alert = guard.getByRole("alert");
  await expect(alert).toContainText("Blocked by privacy guard");
  await expect(alert).toContainText("http://192.168.1.50:1234 — redaction-failed");

  // Guard stays visibly active while failing closed.
  await expect(guard.locator("[data-guard-active]")).toHaveText("Active");
  await expect(page.locator("body")).not.toContainText(SECRET);
});

test("an unknown future detection kind is appended verbatim with its real count, never dropped", async ({ page }) => {
  await installGuardIpcMock(page);
  await page.goto("/?view=settings&section=privacy");
  const guard = guardSection(page);
  await expect(guard.locator("[data-guard-active]")).toHaveText("Active");

  await page.evaluate(() => {
    (window as any).__mockEmit("privacy://state", {
      redactions: [{ kind: "ssh-key", count: 4 }],
      blockedCount: 0,
    });
  });

  // Three known zero-filled rows plus the unknown kind appended after them.
  await expect(guard.locator("[data-guard-kind]")).toHaveCount(4);
  const last = guard.locator("[data-guard-kind]").last();
  await expect(last).toHaveAttribute("data-guard-kind", "ssh-key");
  await expect(last.locator(".settings-status-value")).toHaveText("4");
  await expect(page.locator("body")).not.toContainText(SECRET);
});
