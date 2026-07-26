import { test, expect } from "@playwright/test";

// The Input Control (HID) section of the settings view (?view=settings branch
// of the shared bundle) in a plain browser. Outside a Tauri runtime every
// invoke() rejects, so hid_armed_status() never resolves and the section must
// render its off-by-default, named-unavailable posture instead of crashing or
// pretending armed (the absorb-on-reject contract; R007/R019). This is the
// browser-executable UAT the M005 milestone planned for the HID toggle.

test("HID section renders off-by-default and degrades to a named unavailable state", async ({
  page,
}) => {
  await page.goto("/?view=settings&section=input");
  await expect(page).toHaveTitle("Third Eye");

  // The Input Control section mounts with its heading.
  await expect(
    page.getByRole("heading", { name: "Input Control" }),
  ).toBeVisible();

  // hid_armed_status() rejects outside Tauri → state.hid === null → the mode
  // select is inert and pinned to "off" (off by default, never pretends armed).
  const modeSelect = page.getByLabel("Input Control mode");
  await expect(modeSelect).toBeVisible();
  await expect(modeSelect).toBeDisabled();
  await expect(modeSelect).toHaveValue("off");
  await expect(modeSelect).toHaveAttribute("data-hid-mode", "off");

  // The unavailability is named, not silent.
  await expect(
    page.locator(".settings-unavailable", {
      hasText: "Input Control state is unavailable outside the app.",
    }),
  ).toBeVisible();
});

test("HID mode select offers Off / Ask / Auto-run and never mounts the overlay", async ({
  page,
}) => {
  await page.goto("/?view=settings&section=input");

  // All three run modes are advertised as options, Off first (the default).
  const options = page.getByLabel("Input Control mode").locator("option");
  await expect(options).toHaveCount(3);
  await expect(options.nth(0)).toHaveText("Off — no input (default)");
  await expect(options.nth(1)).toHaveText("Ask — approve each action");
  await expect(options.nth(2)).toHaveText("Auto-run — no prompts");

  // The settings view must never mount the overlay (view isolation).
  await expect(page.locator(".overlay-root")).toHaveCount(0);
});

test("the auto-run danger warning is absent while HID is off by default", async ({
  page,
}) => {
  await page.goto("/?view=settings&section=input");

  // The "dangerously allows all input" warning renders only for the auto-run
  // mode with loaded state. Off by default (state.hid === null here), so the
  // most dangerous posture is never surfaced without an explicit opt-in.
  await expect(page.locator("[data-hid-autorun-warning]")).toHaveCount(0);
});

// ---------------------------------------------------------------------------
// Live automation HUD (2026-07 redesign, surface 7). The HUD windows fold
// llm:// broadcasts, which never arrive outside Tauri — so these specs drive
// the ?hud=seed TEST HOOK (Hud.tsx), which replays a scripted run through the
// SAME reducer the live events use.
// ---------------------------------------------------------------------------

test("hud-pill renders nothing without a run (absorb posture)", async ({ page }) => {
  await page.goto("/?view=hud-pill");
  await expect(page.locator(".te-hudpill")).toHaveCount(0);
  await expect(page.locator(".te-trail")).toHaveCount(0);
});

test("seeded run: pill narrates the current action and the trail settles honestly", async ({ page }) => {
  await page.goto("/?view=hud-pill&hud=seed");
  // The current (still-running) action is the headline; announced-only count.
  const pill = page.locator(".te-hudpill");
  await expect(pill).toBeVisible();
  await expect(pill).toHaveAttribute("data-tone", "acting");
  await expect(pill.locator(".te-hudpill__headline")).toHaveText("click · 226, 184");
  await expect(pill.locator(".te-hudpill__count")).toHaveText("3 / 3");
  // Stop control present while live.
  await expect(pill.getByRole("button", { name: /Stop/ })).toBeVisible();
  // Trail: settled ✓ / ✗ with the typed failure line, ● for the live action —
  // and nothing beyond what was announced (no invented future steps).
  const items = page.locator(".te-trail__item");
  await expect(items).toHaveCount(3);
  await expect(items.nth(0)).toHaveAttribute("data-status", "ok");
  await expect(items.nth(1)).toHaveAttribute("data-status", "failed");
  await expect(items.nth(1)).toContainText("verification-failed");
  await expect(items.nth(2)).toHaveAttribute("data-status", "running");
});

test("seeded run: the canvas draws the ghost ring at the live action's target", async ({ page }) => {
  await page.goto("/?view=hud-canvas&hud=seed");
  const ghost = page.locator(".te-ghost");
  await expect(ghost).toHaveCount(1);
  // The live click's coordinates, straight from the action's own arguments.
  await expect(ghost).toHaveCSS("left", "226px");
  await expect(ghost).toHaveCSS("top", "184px");
  // Click-through by construction: the marker never intercepts pointer events.
  await expect(ghost).toHaveCSS("pointer-events", "none");
});
