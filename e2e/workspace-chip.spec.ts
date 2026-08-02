import { test, expect, type Page } from "@playwright/test";

// The overlay's workspace chip (2026-08-02): with workspace roots configured
// and model routing loaded, the footer names the ACTIVE (first) workspace.
// A mocked Tauri backend serves exactly model_info + workspace_roots; every
// other invoke rejects like the rest of the degrade-proof suite.

async function installOverlayIpcMock(page: Page): Promise<void> {
  await page.addInitScript(() => {
    const callbacks = new Map<number, (e: unknown) => void>();
    const listeners = new Map<string, Set<number>>();
    let nextId = 1;

    (window as any).__mockEmit = (event: string, payload: unknown) => {
      for (const id of listeners.get(event) ?? []) {
        callbacks.get(id)?.({ event, id, payload });
      }
    };

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
          case "model_info":
            return Promise.resolve({
              activeLane: "thin",
              endpoint: "http://localhost:1234",
              auto: true,
              lanes: [
                { name: "thin", modelId: "qwen-9b" },
                { name: "heavy", modelId: "qwen-9b" },
                { name: "coder", modelId: "qwen-9b" },
              ],
            });
          case "workspace_roots":
            return Promise.resolve({
              roots: ["/Users/alex/code/other", "/Users/alex/code/active-project"],
              persistError: null,
            });
          default:
            return Promise.reject(`mock: no such command ${cmd}`);
        }
      },
    };
  });
}

test("the context row names the ACTIVE (first) workspace root", async ({ page }) => {
  await installOverlayIpcMock(page);
  await page.goto("/");
  const chip = page.locator("[data-workspace-chip]");
  await expect(chip).toBeVisible();
  // Every directory is EXPLICIT (user direction 2026-08-02): the first is
  // the active one, the rest read "also …", home abbreviated to ~.
  await expect(chip).toContainText("working in");
  await expect(chip).toContainText("~/code/other");
  await expect(page.locator(".attach-chip--ambient")).toHaveCount(2);
  await expect(page.locator(".attach-chip--ambient").nth(1)).toContainText(
    "also ~/code/active-project",
  );
  // The row's ＋ Attach menu opens with the three context sources. The
  // hidden overlay is click-through (pointer-events: none — the security
  // posture), so first push it to visible-focused exactly like the real
  // backend broadcast would.
  await page.evaluate(() => {
    (window as any).__mockEmit("overlay://state-changed", "visible-focused");
  });
  await expect(page.locator(".overlay-root")).toHaveAttribute("data-state", "visible-focused");
  await page.getByRole("button", { name: "＋ Attach" }).click();
  await expect(page.getByRole("menuitem", { name: "Screenshot now" })).toBeVisible();
  await expect(page.getByRole("menuitem", { name: "File from disk…" })).toBeVisible();
  await expect(
    page.getByRole("menuitem", { name: "Screen with every message" }),
  ).toBeVisible();
});
