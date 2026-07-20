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
  await page.goto("/?view=settings");
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
  await page.goto("/?view=settings");

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
  await page.goto("/?view=settings");

  // The "dangerously allows all input" warning renders only for the auto-run
  // mode with loaded state. Off by default (state.hid === null here), so the
  // most dangerous posture is never surfaced without an explicit opt-in.
  await expect(page.locator("[data-hid-autorun-warning]")).toHaveCount(0);
});
