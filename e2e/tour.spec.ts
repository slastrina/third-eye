import { test, expect } from "@playwright/test";

// First-start tour (2026-07 redesign). Outside a Tauri runtime
// first_run_status rejects and the tour never shows, so these specs drive it
// through the documented ?tour= TEST HOOK (App.tsx, ?edge= precedent):
// `pending` seeds a fresh install (Screen Recording missing → hard block),
// `granted` seeds a grantable run so the full walkthrough is reachable.

test("no tour renders without the seed (invokes reject, absorb posture)", async ({ page }) => {
  await page.goto("/");
  await expect(page.locator(".tour-card")).toHaveCount(0);
});

test("fresh install: welcome renders and the hard block gates both Continue and Skip", async ({ page }) => {
  await page.goto("/?tour=pending");
  const card = page.locator(".tour-card");
  await expect(card).toBeVisible();

  // Welcome step content + step indicator at step 1.
  await expect(card.getByRole("heading", { name: "Meet your Third Eye" })).toBeVisible();
  await expect(card.locator(".te-steps__step[data-status='current']")).toHaveText(/Welcome/);

  // A hard-blocked install offers no Skip from ANY step (M006 posture).
  await expect(card.getByRole("button", { name: "Skip tour" })).toHaveCount(0);

  // Continue → Permissions, where the block is visible (R007), not silent.
  await card.getByRole("button", { name: "Continue" }).click();
  await expect(card.getByRole("heading", { name: "Two permissions, both revocable" })).toBeVisible();
  await expect(card.getByRole("alert")).toContainText("Screen Recording is required");
  await expect(card.getByRole("button", { name: "Continue" })).toBeDisabled();

  // Back returns to Welcome.
  await card.getByRole("button", { name: "Back" }).click();
  await expect(card.getByRole("heading", { name: "Meet your Third Eye" })).toBeVisible();
});

test("granted install: the four steps walk through and Finish dismisses", async ({ page }) => {
  await page.goto("/?tour=granted");
  const card = page.locator(".tour-card");
  await expect(card).toBeVisible();

  // Skip is offered when nothing required is missing.
  await expect(card.getByRole("button", { name: "Skip tour" })).toBeVisible();

  // Welcome → Permissions: capture already granted, no block, Continue live.
  await card.getByRole("button", { name: "Continue" }).click();
  await expect(card.locator(".tour-perm[data-permission], .tour-perm").first()).toContainText(
    "Screen recording",
  );
  await expect(card.locator(".tour-perm-granted")).toHaveText("Granted ✓");
  await expect(card.getByRole("button", { name: "Continue" })).toBeEnabled();

  // → Memory: retention chips select (persist rejects outside Tauri; the
  // optimistic selection stays until a backend echo, which never comes here).
  await card.getByRole("button", { name: "Continue" }).click();
  await expect(card.getByRole("heading", { name: "Your memory, your rules" })).toBeVisible();
  const ninety = card.getByRole("button", { name: "90 days" });
  await ninety.click();
  await expect(ninety).toHaveAttribute("aria-pressed", "true");

  // → Summon: outside Tauri hotkey_status rejects, so no keycaps are invented
  // (no-fake-data) — the step still renders with its hint.
  await card.getByRole("button", { name: "Continue" }).click();
  await expect(card.getByRole("heading", { name: "Summon it anywhere" })).toBeVisible();
  await expect(card.locator(".tour-keycap")).toHaveCount(0);

  // Finish: complete_first_run rejects outside Tauri; the card dismisses
  // anyway (never wedges) — the flag simply wasn't persisted.
  await card.getByRole("button", { name: "Finish" }).click();
  await expect(page.locator(".tour-card")).toHaveCount(0);
});

test("skip from the welcome step dismisses a granted install immediately", async ({ page }) => {
  await page.goto("/?tour=granted");
  const card = page.locator(".tour-card");
  await card.getByRole("button", { name: "Skip tour" }).click();
  await expect(page.locator(".tour-card")).toHaveCount(0);
});
