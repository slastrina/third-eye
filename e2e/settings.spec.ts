import { test, expect } from "@playwright/test";

// The settings view (?view=settings branch of the shared bundle) in a plain
// browser. Outside a Tauri runtime every invoke() rejects, so these tests
// double as proof that the view degrades into named unavailable states
// instead of crashing (the absorb-on-reject contract).
//
// The window is a PyCharm-style two-pane layout: a grouped sidebar on the
// left, one page (section) at a time on the right. `?section=` deep-links
// to a page; section-focused specs (watcher/privacy/cloud/…) use it to land
// directly on the page they exercise.

test("settings view renders from the shared bundle", async ({ page }) => {
  await page.goto("/?view=settings");
  await expect(page).toHaveTitle("Third Eye");

  await expect(page.locator(".settings-root")).toHaveCount(1);
  await expect(page.locator(".settings-panel")).toBeVisible();
  await expect(page.getByRole("heading", { name: "Third Eye Settings" })).toBeVisible();
  // The sidebar renders with its search box, and the default page (Models)
  // is marked current.
  await expect(page.getByRole("navigation", { name: "Settings sections" })).toBeVisible();
  await expect(page.getByLabel("Search settings")).toBeVisible();
  await expect(page.locator('[data-section="models"]')).toHaveAttribute("aria-current", "page");
  // One page at a time: exactly one section is mounted.
  await expect(page.locator(".settings-section")).toHaveCount(1);
  // Outside Tauri the endpoint query rejects, so the Models page degrades to
  // its named unavailable note instead of rendering a dead input.
  await expect(
    page.getByText("Endpoint configuration is unavailable outside the app."),
  ).toBeVisible();
  await expect(page.locator("[data-endpoint-input]")).toHaveCount(0);
  // The settings view must never mount the overlay (and vice versa).
  await expect(page.locator(".overlay-root")).toHaveCount(0);
});

test("sidebar navigation switches pages and ?section= deep-links land directly", async ({ page }) => {
  // Deep link: the window opens on the requested page, not the default.
  await page.goto("/?view=settings&section=mcp");
  await expect(page.getByRole("heading", { name: "MCP Servers" })).toBeVisible();
  await expect(page.locator('[data-section="mcp"]')).toHaveAttribute("aria-current", "page");
  // Outside Tauri there is no authoritative server list, so the JSON editor
  // entry point stays inert rather than seeding an editor from nothing.
  await expect(page.getByRole("button", { name: "Edit as JSON" })).toBeDisabled();

  // Clicking a nav item swaps the page — Watch Screen replaces MCP Servers.
  await page.locator('[data-section="watcher"]').click();
  await expect(page.getByRole("heading", { name: "Watch Screen" })).toBeVisible();
  await expect(page.getByRole("heading", { name: "MCP Servers" })).toHaveCount(0);
  await expect(page.locator('[data-section="watcher"]')).toHaveAttribute("aria-current", "page");

  // An off-contract deep link falls back to the default page, never a blank.
  await page.goto("/?view=settings&section=bogus");
  await expect(page.locator('[data-section="models"]')).toHaveAttribute("aria-current", "page");
});

test("sidebar search filters nav items by label and keywords", async ({ page }) => {
  await page.goto("/?view=settings");
  const nav = page.getByRole("navigation", { name: "Settings sections" });
  await expect(nav.locator(".settings-nav-item")).toHaveCount(10);

  // A label match narrows the tree to the one hit and drops emptied groups.
  await page.getByLabel("Search settings").fill("mcp");
  await expect(nav.locator(".settings-nav-item")).toHaveCount(1);
  await expect(nav.locator('[data-section="mcp"]')).toBeVisible();
  await expect(nav.locator(".settings-nav-group-title")).toHaveCount(1);

  // A keyword match: "api key" finds Cloud Providers without the literal label.
  await page.getByLabel("Search settings").fill("api key");
  await expect(nav.locator('[data-section="cloud"]')).toBeVisible();

  // Clearing restores the full tree.
  await page.getByLabel("Search settings").fill("");
  await expect(nav.locator(".settings-nav-item")).toHaveCount(10);
});

test("model section degrades to a named unavailable state, with refresh", async ({ page }) => {
  await page.goto("/?view=settings");
  // model_info rejects → no lanes to render, a named note instead of a crash.
  await expect(
    page.locator(".settings-unavailable", { hasText: "Model routing is unavailable" }),
  ).toBeVisible();
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

test("privacy toggle renders its unavailable state", async ({ page }) => {
  await page.goto("/?view=settings&section=privacy");
  // privacy_status rejects → the toggle is disabled with a named note.
  const toggle = page.getByLabel("Privacy Mode");
  await expect(toggle).toBeDisabled();
  await expect(
    page.locator(".settings-unavailable", { hasText: "Privacy state is unavailable" }),
  ).toBeVisible();
});

test("status page renders the read-only hotkey/autostart readouts", async ({ page }) => {
  await page.goto("/?view=settings&section=status");
  await expect(page.getByRole("heading", { name: "Status" })).toBeVisible();
  // hotkey_status / autostart_status reject → read-only rows say so.
  await expect(page.locator(".settings-status-value").first()).toHaveText("unavailable");
  await expect(page.locator(".settings-status-value").last()).toHaveText("unavailable");
});

test("overlay presentation section renders its controls and unavailable state", async ({ page }) => {
  await page.goto("/?view=settings&section=overlay");
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
