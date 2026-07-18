import { test, expect } from "@playwright/test";

// Smoke assertions against the overlay shell in a plain browser. Outside a
// Tauri runtime every invoke() rejects, so these tests double as proof that
// the UI degrades instead of crashing (the T03 contract).

test("overlay shell renders with the hidden-state DOM", async ({ page }) => {
  await page.goto("/");
  await expect(page).toHaveTitle("Third Eye");

  const root = page.locator(".overlay-root");
  await expect(root).toHaveCount(1);
  // No Rust pushes overlay://state-changed here, so the initial reducer
  // state must hold.
  await expect(root).toHaveAttribute("data-state", "hidden");

  await expect(page.locator(".overlay-panel")).toBeVisible();

  const input = page.getByLabel("Overlay input");
  await expect(input).toBeVisible();
  await expect(input).toHaveAttribute("placeholder", "Third Eye");
});

test("overlay input accepts keyboard text", async ({ page }) => {
  await page.goto("/");
  // fill() drives focus + input events directly, so it works despite the
  // pointer-events: none click-through applied in the hidden state.
  const input = page.getByLabel("Overlay input");
  await input.fill("smoke");
  await expect(input).toHaveValue("smoke");
});

test("failed backend calls degrade silently instead of crashing", async ({ page }) => {
  await page.goto("/");
  await expect(page.locator(".overlay-panel")).toBeVisible();
  // model_info rejects outside Tauri → indicator absent, not broken.
  await expect(page.locator(".model-indicator")).toHaveCount(0);
  // No error banner and no stray chat messages on a cold load either.
  await expect(page.locator(".chat-banner")).toHaveCount(0);
  await expect(page.locator(".chat-messages")).toHaveCount(0);
});
