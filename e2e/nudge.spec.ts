import { test, expect, type Page } from "@playwright/test";

// The S05 nudge surface in a plain browser: the overlay edge banner
// (idle-with-nudge early return in App.tsx) and the Settings Nudges toggle.
//
// Two modes, same contract watcher.spec.ts/memory.spec.ts prove for theirs:
//  - No mock: every invoke() rejects (no Tauri runtime), so the Nudges
//    section degrades into its named unavailable state.
//  - Mocked IPC: an init script installs a minimal window.__TAURI_INTERNALS__
//    (invoke + transformCallback + the event plugin plumbing) that answers
//    nudge_status/set_nudges_enabled and records `chat` calls, and lets tests
//    emit overlay://state-changed, nudge://show, nudge://dismiss, and
//    nudge://state — so the banner render, self-dismiss, summon preload, and
//    toggle flows run through the real production bundle in a real browser.

interface MockNudgeStatus {
  enabled: boolean;
  active: boolean;
  lastNudgeAtMs: number | null;
  lastError: unknown;
  suppressed: {
    disabled: number;
    overlayVisible: number;
    coolingDown: number;
    emptyBatch: number;
  };
  persistError: string | null;
}

const healthyStatus: MockNudgeStatus = {
  enabled: true,
  active: false,
  lastNudgeAtMs: null,
  lastError: null,
  suppressed: { disabled: 0, overlayVisible: 0, coolingDown: 0, emptyBatch: 0 },
  persistError: null,
};

/** A representative pixel-free nudge://show payload (camelCase serde shape). */
const nudgePayload = {
  kind: "nudge",
  message: "Looks like a stack trace — want a hand?",
  screenText: "TypeError: cannot read properties of undefined (reading 'map')",
  appContext: "VS Code",
  capturedAtMs: 1_700_000_000_000,
  memoryContext: ["You fixed a similar TypeError in chat.ts last week"],
};

/** Serve a fake Tauri backend for the nudge IPC surface. Non-nudge commands
 *  (except `chat`, recorded for the preload assertions) still reject so the
 *  rest of the UI keeps its proven degrade states. Exposes window.__mockNudge:
 *   - emit(event, payload): deliver an event through the real listen() path
 *   - listenerCount(event): readiness probe for waitForFunction
 *   - chatCalls(): every messages[] the `chat` command received
 *   - failNextPersist(detail): next set_nudges_enabled rolls back with
 *     persistError instead of applying (never rejects — health-as-value) */
async function installNudgeIpcMock(
  page: Page,
  opts: { status?: MockNudgeStatus } = {},
): Promise<void> {
  await page.addInitScript(
    (seed: { status: MockNudgeStatus }) => {
      const status: MockNudgeStatus = { ...seed.status };
      const chatCalls: unknown[] = [];
      let failPersist: string | null = null;
      let nextRequestId = 1;

      const callbacks = new Map<number, (e: unknown) => void>();
      const listeners = new Map<string, Set<number>>();
      let nextCallbackId = 1;

      (window as any).__mockNudge = {
        emit(event: string, payload: unknown) {
          for (const id of listeners.get(event) ?? []) {
            callbacks.get(id)?.({ event, id, payload });
          }
        },
        listenerCount(event: string) {
          return listeners.get(event)?.size ?? 0;
        },
        chatCalls() {
          return chatCalls;
        },
        failNextPersist(detail: string) {
          failPersist = detail;
        },
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
          const id = nextCallbackId++;
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
            case "nudge_status":
              return Promise.resolve({ ...status });
            case "set_nudges_enabled": {
              // Never rejects: a persist failure rolls the toggle back and
              // rides persistError on the authoritative snapshot.
              if (failPersist !== null) {
                const detail = failPersist;
                failPersist = null;
                return Promise.resolve({ ...status, persistError: detail });
              }
              status.enabled = args.enable;
              return Promise.resolve({ ...status });
            }
            case "chat": {
              chatCalls.push(args.messages);
              return Promise.resolve(nextRequestId++);
            }
            default:
              return Promise.reject(`mock: no such command ${cmd}`);
          }
        },
      };
    },
    { status: opts.status ?? healthyStatus },
  );
}

/** Wait until App.tsx's mount-time listen() calls registered for an event —
 *  emitting before that would silently drop the payload. */
async function waitForListener(page: Page, event: string): Promise<void> {
  await page.waitForFunction(
    (name) => (window as any).__mockNudge.listenerCount(name) > 0,
    event,
  );
}

const emit = (page: Page, event: string, payload: unknown) =>
  page.evaluate(
    ({ event, payload }) => (window as any).__mockNudge.emit(event, payload),
    { event, payload },
  );

const nudgesSection = (page: Page) =>
  page.locator("section", { has: page.locator("#settings-nudges-heading") });

// ---------------------------------------------------------------------------
// Overlay edge banner
// ---------------------------------------------------------------------------

test("idle overlay with an active nudge renders only the edge banner", async ({ page }) => {
  await installNudgeIpcMock(page);
  await page.goto("/");
  await waitForListener(page, "nudge://show");

  await emit(page, "nudge://show", nudgePayload);
  await emit(page, "overlay://state-changed", "visible-idle");

  const root = page.locator(".overlay-root");
  await expect(root).toHaveAttribute("data-state", "visible-idle");
  await expect(root).toHaveAttribute("data-nudge", "true");

  // Only the small banner: the .overlay-panel chat chrome must not mount at
  // all (structural ghost-panel prevention, not CSS hiding).
  const banner = page.locator(".nudge-banner");
  await expect(banner).toBeVisible();
  await expect(banner.locator(".nudge-message")).toHaveText(nudgePayload.message);
  await expect(banner.locator(".nudge-hint")).toHaveText("press the hotkey to ask");
  await expect(banner).toHaveAttribute("role", "status");
  await expect(page.locator(".overlay-panel")).toHaveCount(0);
  await expect(page.getByLabel("Overlay input")).toHaveCount(0);
});

test("the banner self-dismisses on nudge://dismiss", async ({ page }) => {
  await installNudgeIpcMock(page);
  await page.goto("/");
  await waitForListener(page, "nudge://dismiss");

  await emit(page, "nudge://show", nudgePayload);
  await emit(page, "overlay://state-changed", "visible-idle");
  await expect(page.locator(".nudge-banner")).toBeVisible();

  // Auto-dismiss: the backend emits nudge://dismiss and hides the window.
  await emit(page, "nudge://dismiss", "auto-timeout");
  await emit(page, "overlay://state-changed", "hidden");

  await expect(page.locator(".nudge-banner")).toHaveCount(0);
  await expect(page.locator(".overlay-root")).toHaveAttribute("data-state", "hidden");
  // The regular chrome is back — ignoring a nudge cost nothing.
  await expect(page.locator(".overlay-panel")).toHaveCount(1);
});

test("a nudge never replaces the chrome while chat is focused", async ({ page }) => {
  await installNudgeIpcMock(page);
  await page.goto("/");
  await waitForListener(page, "nudge://show");

  await emit(page, "overlay://state-changed", "visible-focused");
  await emit(page, "nudge://show", nudgePayload);

  // The banner is an idle-only surface: focused chat keeps its full panel.
  await expect(page.locator(".overlay-panel")).toBeVisible();
  await expect(page.locator(".nudge-banner")).toHaveCount(0);
});

// ---------------------------------------------------------------------------
// Summon preload (hotkey on a visible nudge)
// ---------------------------------------------------------------------------

test("a summoned dismiss grounds exactly the next question, consume-once", async ({ page }) => {
  await installNudgeIpcMock(page);
  await page.goto("/");
  await waitForListener(page, "nudge://dismiss");

  // Park the nudge, then the hotkey summon: the backend emits a "summoned"
  // dismiss and focuses the overlay. No new IPC rides this path — the
  // context below came entirely from the cached nudge://show payload.
  await emit(page, "nudge://show", nudgePayload);
  await emit(page, "overlay://state-changed", "visible-idle");
  await expect(page.locator(".nudge-banner")).toBeVisible();
  await emit(page, "nudge://dismiss", "summoned");
  await emit(page, "overlay://state-changed", "visible-focused");
  await expect(page.locator(".overlay-panel")).toBeVisible();

  const input = page.getByLabel("Overlay input");
  await input.fill("help me fix this");
  await input.press("Enter");
  await expect(page.locator(".chat-message.chat-user")).toHaveText("help me fix this");

  // First wire call: the prepended system message carries the nudge message,
  // screen text, app context, and memory bullets from the show payload.
  const calls = await page.evaluate(() => (window as any).__mockNudge.chatCalls());
  expect(calls).toHaveLength(1);
  const first = calls[0] as Array<{ role: string; content: string }>;
  expect(first).toHaveLength(2);
  expect(first[0].role).toBe("system");
  expect(first[0].content).toContain('proactive nudge: "Looks like a stack trace — want a hand?"');
  expect(first[0].content).toContain("frontmost app: VS Code");
  expect(first[0].content).toContain("TypeError: cannot read properties of undefined");
  expect(first[0].content).toContain("- You fixed a similar TypeError in chat.ts last week");
  expect(first[1]).toEqual({ role: "user", content: "help me fix this" });

  // Consume-once: a second question goes out ungrounded — no system message.
  await input.fill("and a follow-up");
  await input.press("Enter");
  await expect(page.locator(".chat-message.chat-user")).toHaveCount(2);
  const calls2 = await page.evaluate(() => (window as any).__mockNudge.chatCalls());
  expect(calls2).toHaveLength(2);
  const second = calls2[1] as Array<{ role: string; content: string }>;
  expect(second.every((m) => m.role !== "system")).toBe(true);
  expect(second[second.length - 1]).toEqual({ role: "user", content: "and a follow-up" });
});

test("a non-summon dismiss stages no preload", async ({ page }) => {
  await installNudgeIpcMock(page);
  await page.goto("/");
  await waitForListener(page, "nudge://dismiss");

  await emit(page, "nudge://show", nudgePayload);
  await emit(page, "overlay://state-changed", "visible-idle");
  await expect(page.locator(".nudge-banner")).toBeVisible();
  // The nudge times out unanswered; the user opens chat later on their own.
  await emit(page, "nudge://dismiss", "auto-timeout");
  await emit(page, "overlay://state-changed", "hidden");
  await emit(page, "overlay://state-changed", "visible-focused");

  const input = page.getByLabel("Overlay input");
  await input.fill("unrelated question");
  await input.press("Enter");
  await expect(page.locator(".chat-message.chat-user")).toHaveText("unrelated question");

  const calls = await page.evaluate(() => (window as any).__mockNudge.chatCalls());
  expect(calls).toHaveLength(1);
  expect(calls[0]).toEqual([{ role: "user", content: "unrelated question" }]);
});

// ---------------------------------------------------------------------------
// Settings Nudges toggle
// ---------------------------------------------------------------------------

test("nudges section degrades to a named unavailable state outside Tauri", async ({ page }) => {
  await page.goto("/?view=settings");
  const nudges = nudgesSection(page);
  await expect(nudges.getByRole("heading", { name: "Nudges" })).toBeVisible();
  // nudge_status rejects (no runtime) → toggle disabled with the named note.
  await expect(page.getByRole("checkbox", { name: "Nudges" })).toBeDisabled();
  await expect(nudges.locator(".settings-unavailable")).toHaveText(
    "Nudge state is unavailable outside the app.",
  );
  await expect(nudges.locator(".settings-error")).toHaveCount(0);
});

test("the toggle drives set_nudges_enabled and renders the authoritative snapshot", async ({ page }) => {
  await installNudgeIpcMock(page);
  await page.goto("/?view=settings");
  const toggle = page.getByRole("checkbox", { name: "Nudges" });

  // Mount-time nudge_status resolves → live toggle, default-on.
  await expect(toggle).toBeEnabled();
  await expect(toggle).toBeChecked();

  await toggle.uncheck();
  await expect(toggle).not.toBeChecked();
  await toggle.check();
  await expect(toggle).toBeChecked();
  await expect(nudgesSection(page).locator(".settings-error")).toHaveCount(0);
});

test("a persist failure rolls the toggle back with a named alert", async ({ page }) => {
  await installNudgeIpcMock(page);
  await page.goto("/?view=settings");
  const toggle = page.getByRole("checkbox", { name: "Nudges" });
  await expect(toggle).toBeChecked();

  await page.evaluate(() =>
    (window as any).__mockNudge.failNextPersist("settings.json is read-only"),
  );
  // click(), not uncheck(): the authoritative snapshot rolls the state back
  // to checked, which uncheck() would (rightly) refuse to accept.
  await toggle.click();

  const alert = nudgesSection(page).getByRole("alert");
  await expect(alert).toContainText("Nudges couldn't be saved");
  await expect(alert).toContainText("settings.json is read-only");
  await expect(toggle).toBeChecked();
});

test("nudge://state broadcasts sync the toggle and surface classifier errors", async ({ page }) => {
  await installNudgeIpcMock(page);
  await page.goto("/?view=settings");
  const toggle = page.getByRole("checkbox", { name: "Nudges" });
  await expect(toggle).toBeChecked();
  await waitForListener(page, "nudge://state");

  // A toggle from another surface (overlay window, future tray item) arrives
  // as a broadcast — this window follows without polling.
  await emit(page, "nudge://state", {
    ...healthyStatus,
    enabled: false,
  });
  await expect(toggle).not.toBeChecked();

  // A classification failure rides lastError on the same broadcast shape and
  // renders through the shared kind-tagged banner copy.
  await emit(page, "nudge://state", {
    ...healthyStatus,
    lastError: {
      kind: "offline",
      endpoint: "http://localhost:1234",
      detail: "connection refused",
    },
  });
  await expect(toggle).toBeChecked();
  const alert = nudgesSection(page).getByRole("alert");
  await expect(alert).toContainText("Local AI offline");
  await expect(alert).toContainText("http://localhost:1234 — connection refused");
});
