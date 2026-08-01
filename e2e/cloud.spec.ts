import { test, expect, type Page } from "@playwright/test";

// The Cloud Providers section (?view=settings) in a plain browser — the S04
// opt-in UX proven end-to-end against the real src/cloud-state.ts + Settings
// bundle (playwright.config.ts runs `npm run dev`, plain vite, NO Tauri
// runtime).
//
// Two modes, the same contract watcher.spec.ts / privacy.spec.ts prove for
// their sections:
//  - No mock: every invoke() rejects (no Tauri runtime), so the section
//    degrades to its named unavailable line — no provider rows, no crash.
//  - Mocked IPC: an init script installs a *stateful* fake backend
//    (window.__TAURI_INTERNALS__ + the event plugin) answering the eight S04
//    commands and re-emitting cloud://optin on every set_cloud_optin, so the
//    full flow runs through the real reducer: enable opt-in → masked key
//    entry → presence → heavy-lane pick → disable → local-only.
//
// The never-echo property is asserted live: the API key is typed into a
// password field, submitKey clears the draft on its way to the store, and the
// entered secret must never reappear anywhere in the Settings DOM — the store
// only ever answers presence booleans.

const API_KEY = "sk-live-e2e-SECRET-4111111111111111-hunter2";

interface MockState {
  optinEnabled: boolean;
  optinPersistError: string | null;
  openaiPresent: boolean;
  anthropicPresent: boolean;
  heavyProvider: string | null;
  heavyPersistError: string | null;
  coderProvider?: string | null;
}

/** Serve a stateful fake Tauri backend for the S04 cloud IPC surface. Every
 *  set_cloud_optin re-broadcasts the resulting status on cloud://optin (the
 *  real backend contract). Non-cloud commands still reject so the rest of
 *  Settings keeps its proven degrade states. Tests may seed the starting
 *  state and drive an external cloud://optin via window.__mockEmit. */
async function installCloudIpcMock(page: Page, seed: Partial<MockState> = {}): Promise<void> {
  await page.addInitScript((seed: Partial<MockState>) => {
    const state: MockState = {
      optinEnabled: false,
      optinPersistError: null,
      openaiPresent: false,
      anthropicPresent: false,
      heavyProvider: null,
      heavyPersistError: null,
      coderProvider: null,
      ...seed,
    };

    const callbacks = new Map<number, (e: unknown) => void>();
    const listeners = new Map<string, Set<number>>();
    let nextId = 1;

    const emit = (event: string, payload: unknown) => {
      for (const id of listeners.get(event) ?? []) {
        callbacks.get(id)?.({ event, id, payload });
      }
    };
    (window as any).__mockEmit = emit;

    const optinStatus = () => ({
      enabled: state.optinEnabled,
      persistError: state.optinPersistError,
    });
    const keyStatus = () => ({
      openaiPresent: state.openaiPresent,
      anthropicPresent: state.anthropicPresent,
    });
    const heavyStatus = () => ({
      provider: state.heavyProvider,
      persistError: state.heavyPersistError,
    });
    const coderStatus = () => ({
      provider: state.coderProvider ?? null,
      persistError: null,
    });
    const setPresent = (provider: string, present: boolean) => {
      if (provider === "openai") state.openaiPresent = present;
      else if (provider === "anthropic") state.anthropicPresent = present;
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
          // Health-as-value: opt-in / heavy status never reject.
          case "cloud_optin_status":
            return Promise.resolve(optinStatus());
          case "set_cloud_optin": {
            state.optinEnabled = !!args.enable;
            const status = optinStatus();
            // Backend re-broadcasts every opt-in mutation app-wide.
            emit("cloud://optin", status);
            return Promise.resolve(status);
          }
          case "cloud_key_status":
            return Promise.resolve(keyStatus());
          // Presence-only: the store swallows the key and answers booleans;
          // the key material never rides a response.
          case "set_cloud_api_key":
            setPresent(args.provider, true);
            return Promise.resolve(keyStatus());
          case "delete_cloud_api_key":
            setPresent(args.provider, false);
            return Promise.resolve(keyStatus());
          case "cloud_heavy_provider":
            return Promise.resolve(heavyStatus());
          case "set_cloud_heavy_provider":
            state.heavyProvider = args.provider ?? null;
            return Promise.resolve(heavyStatus());
          // Coder-lane selection (coding-agent S6): same shape as heavy.
          case "cloud_coder_provider":
            return Promise.resolve(coderStatus());
          case "set_cloud_coder_provider":
            state.coderProvider = args.provider ?? null;
            return Promise.resolve(coderStatus());
          default:
            return Promise.reject(`mock: no such command ${cmd}`);
        }
      },
    };
  }, seed);
}

const cloudSection = (page: Page) =>
  page.locator("section", { has: page.getByRole("heading", { name: "Cloud Providers" }) });

test("cloud section degrades to a named unavailable state outside Tauri", async ({ page }) => {
  await page.goto("/?view=settings&section=cloud");
  const cloud = cloudSection(page);
  await expect(cloud.getByRole("heading", { name: "Cloud Providers" })).toBeVisible();

  // cloud_optin_status rejects → named unavailable line, not a crash.
  await expect(cloud.locator(".settings-unavailable")).toHaveText(
    "Cloud state is unavailable outside the app.",
  );
  // The toggle is disabled (no backend truth) and no provider rows exist.
  await expect(cloud.getByRole("switch", { name: "Use cloud providers" })).toBeDisabled();
  await expect(cloud.locator("[data-cloud-provider]")).toHaveCount(0);
  await expect(cloud.locator(".settings-error")).toHaveCount(0);
});

test("full opt-in flow: enable → masked key entry (never echoed) → presence → heavy-lane pick → disable reverts to local-only", async ({ page }) => {
  await installCloudIpcMock(page);
  await page.goto("/?view=settings&section=cloud");
  const cloud = cloudSection(page);
  const toggle = cloud.getByRole("switch", { name: "Use cloud providers" });

  // Mount-time cloud_optin_status resolves disabled → toggle enabled, off,
  // no unavailable line, and the opt-in-gated provider rows hidden.
  await expect(toggle).toBeEnabled();
  await expect(toggle).not.toBeChecked();
  await expect(cloud.locator(".settings-unavailable")).toHaveCount(0);
  await expect(cloud.locator("[data-cloud-provider]")).toHaveCount(0);

  // Enable opt-in → the two provider rows appear, both keys "Not stored".
  await toggle.check();
  await expect(toggle).toBeChecked();
  await expect(cloud.locator("[data-cloud-provider]")).toHaveCount(2);
  for (const p of ["openai", "anthropic"]) {
    await expect(
      cloud.locator(`[data-cloud-provider="${p}"] [data-cloud-key-present]`),
    ).toHaveText("Not stored");
  }

  // Enter an OpenAI key into the masked field; Save gates on non-empty draft.
  const openaiInput = cloud.locator('[data-cloud-key-input="openai"]');
  const openaiSave = cloud.locator('[data-cloud-key-save="openai"]');
  await expect(openaiInput).toHaveAttribute("type", "password");
  await expect(openaiSave).toBeDisabled();
  await openaiInput.fill(API_KEY);
  await expect(openaiSave).toBeEnabled();
  await openaiSave.click();

  // The store answered presence → "Stored", a Delete control appears, and the
  // draft field cleared on its way to the store (never-echo, step 1).
  await expect(
    cloud.locator('[data-cloud-provider="openai"] [data-cloud-key-present]'),
  ).toHaveText("Stored");
  await expect(openaiInput).toHaveValue("");
  await expect(cloud.locator('[data-cloud-key-delete="openai"]')).toBeVisible();
  // Anthropic untouched — presence is per-provider.
  await expect(
    cloud.locator('[data-cloud-provider="anthropic"] [data-cloud-key-present]'),
  ).toHaveText("Not stored");

  // Never-echo (step 2): the entered secret is nowhere in the Settings DOM,
  // neither rendered text nor any input value.
  await expect(page.locator("body")).not.toContainText(API_KEY);
  await expect(await page.content()).not.toContain(API_KEY);

  // Pick a heavy-lane provider — persisted and reflected by the select value.
  const heavy = cloud.locator("[data-cloud-heavy-provider]");
  await expect(heavy).toHaveValue("");
  await heavy.selectOption("openai");
  await expect(heavy).toHaveValue("openai");

  // Delete the stored key → back to "Not stored", Delete control gone.
  await cloud.locator('[data-cloud-key-delete="openai"]').click();
  await expect(
    cloud.locator('[data-cloud-provider="openai"] [data-cloud-key-present]'),
  ).toHaveText("Not stored");
  await expect(cloud.locator('[data-cloud-key-delete="openai"]')).toHaveCount(0);

  // Disable opt-in → the app reverts to local-only: provider rows and the
  // heavy-lane picker vanish, toggle unchecked.
  await toggle.uncheck();
  await expect(toggle).not.toBeChecked();
  await expect(cloud.locator("[data-cloud-provider]")).toHaveCount(0);
  await expect(cloud.locator("[data-cloud-heavy-provider]")).toHaveCount(0);
});

test("an external cloud://optin broadcast (another window's toggle) flips this section live", async ({ page }) => {
  await installCloudIpcMock(page);
  await page.goto("/?view=settings&section=cloud");
  const cloud = cloudSection(page);
  const toggle = cloud.getByRole("switch", { name: "Use cloud providers" });
  await expect(toggle).not.toBeChecked();
  await expect(cloud.locator("[data-cloud-provider]")).toHaveCount(0);

  // A different webview flipped opt-in; the backend broadcast lands here and
  // the section reflects it without this window touching the toggle.
  await page.evaluate(() => {
    (window as any).__mockEmit("cloud://optin", { enabled: true, persistError: null });
  });
  await expect(toggle).toBeChecked();
  await expect(cloud.locator("[data-cloud-provider]")).toHaveCount(2);

  // And the reverse broadcast reverts it to local-only, live.
  await page.evaluate(() => {
    (window as any).__mockEmit("cloud://optin", { enabled: false, persistError: null });
  });
  await expect(toggle).not.toBeChecked();
  await expect(cloud.locator("[data-cloud-provider]")).toHaveCount(0);
});

test("a persisted opt-in with a stored key rehydrates presence on mount", async ({ page }) => {
  // Seed the backend as if a prior session persisted opt-in + an Anthropic key
  // and an OpenAI heavy-lane choice — mount-time queries rehydrate the view.
  await installCloudIpcMock(page, {
    optinEnabled: true,
    anthropicPresent: true,
    heavyProvider: "openai",
  });
  await page.goto("/?view=settings&section=cloud");
  const cloud = cloudSection(page);

  await expect(cloud.getByRole("switch", { name: "Use cloud providers" })).toBeChecked();
  await expect(
    cloud.locator('[data-cloud-provider="anthropic"] [data-cloud-key-present]'),
  ).toHaveText("Stored");
  await expect(
    cloud.locator('[data-cloud-provider="openai"] [data-cloud-key-present]'),
  ).toHaveText("Not stored");
  await expect(cloud.locator('[data-cloud-key-delete="anthropic"]')).toBeVisible();
  await expect(cloud.locator("[data-cloud-heavy-provider]")).toHaveValue("openai");
});

test("a persist error on the opt-in toggle surfaces as data, not a rejection", async ({ page }) => {
  // The health-as-value contract: set_cloud_optin never rejects; a persist
  // failure rides persistError on the returned/broadcast status. Emit that
  // shape directly to prove the section renders the error banner.
  await installCloudIpcMock(page);
  await page.goto("/?view=settings&section=cloud");
  const cloud = cloudSection(page);
  await expect(cloud.getByRole("switch", { name: "Use cloud providers" })).toBeEnabled();

  await page.evaluate(() => {
    (window as any).__mockEmit("cloud://optin", {
      enabled: false,
      persistError: "keychain write denied",
    });
  });
  const alert = cloud.getByRole("alert");
  await expect(alert).toContainText("Cloud opt-in couldn't be saved");
  await expect(alert).toContainText("keychain write denied");
});
