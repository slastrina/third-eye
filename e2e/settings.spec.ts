import { test, expect } from "@playwright/test";

// The settings view (?view=settings branch of the shared bundle) in a plain
// browser. Outside a Tauri runtime every invoke() rejects, so these tests
// double as proof that the view degrades into named unavailable states
// instead of crashing (the absorb-on-reject contract).

test("settings view renders from the shared bundle", async ({ page }) => {
  await page.goto("/?view=settings");
  await expect(page).toHaveTitle("Third Eye");

  await expect(page.locator(".settings-root")).toHaveCount(1);
  await expect(page.locator(".settings-panel")).toBeVisible();
  await expect(page.getByRole("heading", { name: "Third Eye Settings" })).toBeVisible();
  // The settings view must never mount the overlay (and vice versa).
  await expect(page.locator(".overlay-root")).toHaveCount(0);
});

test("model section degrades to a named unavailable state, with refresh", async ({ page }) => {
  await page.goto("/?view=settings");
  // model_info rejects → no lanes to render, a named note instead of a crash.
  await expect(page.locator(".settings-unavailable").first()).toContainText(
    "Model routing is unavailable",
  );
  // list_models rejects → visible error state, not silence.
  await expect(page.locator(".settings-error").first()).toContainText(
    "Model list unavailable",
  );
  // The refresh affordance exists and a click is absorbed, not a crash.
  const refresh = page.getByRole("button", { name: /refresh/i });
  await expect(refresh).toBeEnabled();
  await refresh.click();
  await expect(page.locator(".settings-panel")).toBeVisible();
});

test("privacy toggle and status readouts render their unavailable states", async ({ page }) => {
  await page.goto("/?view=settings");
  // privacy_status rejects → the toggle is disabled with a named note.
  const toggle = page.getByLabel("Privacy Mode");
  await expect(toggle).toBeDisabled();
  // Matched by text, not position — the watcher section renders its own
  // unavailable note after this one.
  await expect(
    page.locator(".settings-unavailable", { hasText: "Privacy state is unavailable" }),
  ).toBeVisible();
  // hotkey_status / autostart_status reject → read-only rows say so.
  await expect(page.locator(".settings-status-value").first()).toHaveText("unavailable");
  await expect(page.locator(".settings-status-value").last()).toHaveText("unavailable");
});

test("overlay presentation section renders its controls and unavailable state", async ({ page }) => {
  await page.goto("/?view=settings");
  // The Overlay section (M006/S04) exposes the mode select with no window
  // geometry ACLs — it only invokes the custom command and listens for the
  // authoritative overlay://presentation broadcast.
  await expect(page.getByRole("heading", { name: "Overlay" })).toBeVisible();
  const modeSelect = page.getByLabel("Overlay presentation mode");
  await expect(modeSelect).toHaveCount(1);
  // Every mode is offered; the default option is modal (floating).
  await expect(modeSelect.locator("option")).toHaveCount(5);
  // overlay_presentation rejects outside Tauri → the select is disabled with a
  // named note, and no drawer extent input renders (modal has no extent).
  await expect(modeSelect).toBeDisabled();
  await expect(modeSelect).toHaveValue("modal");
  await expect(
    page.locator(".settings-unavailable", { hasText: "Overlay presentation is unavailable" }),
  ).toBeVisible();
  await expect(page.locator("[data-overlay-extent]")).toHaveCount(0);
});

test("in-page close and Escape are absorbed outside Tauri", async ({ page }) => {
  await page.goto("/?view=settings");
  await page.getByRole("button", { name: "Close settings" }).click();
  await page.keyboard.press("Escape");
  // hide_settings_window rejects in a plain browser; the view must survive.
  await expect(page.locator(".settings-panel")).toBeVisible();
});
