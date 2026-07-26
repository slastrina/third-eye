import { test, expect, type Page } from "@playwright/test";

// The Watch Screen diagnostics section (?view=settings) in a plain browser.
//
// Two modes:
//  - No mock: every invoke() rejects (no Tauri runtime), so the section must
//    degrade into its named unavailable state — same contract settings.spec.ts
//    proves for the other sections.
//  - Mocked IPC: an init script installs a minimal window.__TAURI_INTERNALS__
//    (invoke + transformCallback, the two entry points @tauri-apps/api v2
//    actually uses) that answers watcher_status/set_watcher_enabled and lets
//    tests emit watcher://state and watcher://observation, so the live
//    diagnostics flow — toggle, run-state labels, snippet buffer, typed tick
//    errors — is exercised through the real bundle in a real browser.

/** Serve a fake Tauri backend for the watcher IPC surface. Non-watcher
 *  commands still reject so the rest of Settings keeps its proven degrade
 *  states. Exposes window.__mockEmit(event, payload) to tests. */
async function installWatcherIpcMock(page: Page): Promise<void> {
  await page.addInitScript(() => {
    const status = {
      enabled: false,
      state: "idle",
      lastTickError: null as unknown,
      error: null as unknown,
    };
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
            // StrictMode double-mounts Settings in dev; honoring unlisten is
            // what keeps the first mount's handlers from double-delivering.
            listeners.get(args.event)?.delete(args.eventId);
            callbacks.delete(args.eventId);
            return Promise.resolve();
          }
          case "watcher_status":
            return Promise.resolve({ ...status });
          case "set_watcher_enabled": {
            status.enabled = args.enable;
            status.state = args.enable ? "watching" : "idle";
            return Promise.resolve({ ...status });
          }
          default:
            return Promise.reject(`mock: no such command ${cmd}`);
        }
      },
    };
  });
}

const section = (page: Page) =>
  page.locator("section", { has: page.locator("#settings-watcher-heading") });

test("watcher section degrades to a named unavailable state outside Tauri", async ({ page }) => {
  await page.goto("/?view=settings&section=watcher");
  const watcher = section(page);
  await expect(watcher.getByRole("heading", { name: "Watch Screen" })).toBeVisible();
  // watcher_status rejects → toggle disabled with a named note, not a crash.
  await expect(page.getByRole("switch", { name: "Watch Screen" })).toBeDisabled();
  await expect(watcher.locator(".settings-unavailable")).toContainText(
    "Watcher state is unavailable outside the app.",
  );
  // No status row, no snippet list, no error banners without backend truth.
  await expect(watcher.locator(".settings-status-value")).toHaveCount(0);
  await expect(watcher.locator(".watcher-snippets")).toHaveCount(0);
  await expect(watcher.locator(".settings-error")).toHaveCount(0);
});

test("live status renders and the toggle drives set_watcher_enabled", async ({ page }) => {
  await installWatcherIpcMock(page);
  await page.goto("/?view=settings&section=watcher");
  const watcher = section(page);
  const toggle = page.getByRole("switch", { name: "Watch Screen" });
  const state = watcher.locator(".settings-status-value");

  // Mount-time watcher_status resolves → live toggle, idle status.
  await expect(toggle).toBeEnabled();
  await expect(toggle).not.toBeChecked();
  await expect(state).toHaveText("Off");
  await expect(state).toHaveAttribute("data-watcher-state", "idle");

  // Toggle on: the invoke response is authoritative for the new status.
  await toggle.check();
  await expect(state).toHaveText("Watching");
  await expect(state).toHaveAttribute("data-watcher-state", "watching");
  await expect(watcher.locator(".settings-hint").last()).toHaveText(
    "Watching — no text extracted yet.",
  );

  // Toggle off again: back to idle.
  await toggle.uncheck();
  await expect(state).toHaveText("Off");
});

test("watcher://observation feeds the snippet list, newest first, capped at 5", async ({ page }) => {
  await installWatcherIpcMock(page);
  await page.goto("/?view=settings&section=watcher");
  const watcher = section(page);
  await expect(page.getByRole("switch", { name: "Watch Screen" })).toBeEnabled();

  // Six observations through the real event path — the list keeps the last 5.
  await page.evaluate(() => {
    for (let i = 1; i <= 6; i++) {
      (window as any).__mockEmit("watcher://observation", {
        text: `snippet ${i}\nsecond line`,
        appContext: i === 6 ? "Safari" : null,
        capturedAt: 1_700_000_000_000 + i * 5_000,
      });
    }
  });

  const snippets = watcher.locator(".watcher-snippet");
  await expect(snippets).toHaveCount(5);
  // Newest first, newlines collapsed to the · separator.
  await expect(snippets.first().locator(".watcher-snippet-text")).toHaveText(
    "snippet 6 · second line",
  );
  await expect(snippets.last().locator(".watcher-snippet-text")).toHaveText(
    "snippet 2 · second line",
  );
  // App context rides the meta line when the backend knew it.
  await expect(snippets.first().locator(".watcher-snippet-meta")).toContainText("Safari");

  // Long text is previewed with an ellipsis, never dumped raw.
  await page.evaluate(() => {
    (window as any).__mockEmit("watcher://observation", {
      text: "x".repeat(400),
      appContext: null,
      capturedAt: 1_700_000_999_000,
    });
  });
  await expect(snippets.first().locator(".watcher-snippet-text")).toHaveText(
    `${"x".repeat(160)}…`,
  );
});

test("watcher://state broadcasts drive privacy pause and typed tick errors", async ({ page }) => {
  await installWatcherIpcMock(page);
  await page.goto("/?view=settings&section=watcher");
  const watcher = section(page);
  const state = watcher.locator(".settings-status-value");
  await expect(page.getByRole("switch", { name: "Watch Screen" })).toBeEnabled();

  // Privacy pause arrives as a broadcast (e.g. toggled from the tray) and is
  // its own visible state, not a silent stop.
  await page.evaluate(() => {
    (window as any).__mockEmit("watcher://state", {
      enabled: true,
      state: "paused-privacy",
      lastTickError: null,
      error: null,
    });
  });
  await expect(state).toHaveText("Paused by Privacy Mode");
  await expect(state).toHaveAttribute("data-watcher-state", "paused-privacy");
  // The broadcast snapshot is authoritative for the toggle too.
  await expect(page.getByRole("switch", { name: "Watch Screen" })).toBeChecked();

  // A typed tick error surfaces as a role=alert banner with the human title.
  await page.evaluate(() => {
    (window as any).__mockEmit("watcher://state", {
      enabled: true,
      state: "watching",
      lastTickError: { kind: "permission-denied", detail: "screen recording not granted" },
      error: null,
    });
  });
  const alert = watcher.getByRole("alert");
  await expect(alert).toContainText("Screen Recording permission needed");
  await expect(alert).toContainText("screen recording not granted");

  // A persist failure names the toggle it failed to save.
  await page.evaluate(() => {
    (window as any).__mockEmit("watcher://state", {
      enabled: false,
      state: "idle",
      lastTickError: null,
      error: "settings.json is read-only",
    });
  });
  await expect(watcher.getByRole("alert")).toContainText("Watch Screen couldn't be saved");
  await expect(watcher.getByRole("alert")).toContainText("settings.json is read-only");
});
