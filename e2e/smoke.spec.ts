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

test("drag handle and resize grip render inside the overlay panel", async ({ page }) => {
  await page.goto("/");

  // Geometry affordances (M006/S01): the header drags the window, the corner
  // grip resizes it. Both must live INSIDE .overlay-panel — its pointer-events
  // is auto only in visible-focused, so an idle click-through overlay stays
  // non-draggable (the correct security posture). Assert DOM presence + nesting;
  // the native drag/resize behaviour itself is manual-only (live NSPanel).
  const panel = page.locator(".overlay-panel");
  await expect(panel.locator(".overlay-drag-handle")).toHaveCount(1);
  await expect(panel.locator(".overlay-resize-grip")).toHaveCount(1);
});

test("the ?edge= test hook seeds a drawer presentation", async ({ page }) => {
  // Production geometry is driven by the persisted overlay-presentation config
  // (overlay_presentation on mount + the overlay://presentation broadcast, S04).
  // Outside a Tauri runtime that invoke rejects and no config can load, so the
  // ?edge= query is retained ONLY as a documented test hook that seeds the
  // INITIAL presentation — letting Playwright render the drawer DOM variant.
  // currentMonitor() still rejects here, so the native snap is a benign no-op,
  // but the DOM must carry the data-edge attribute that selects the drawer CSS.
  await page.goto("/?edge=left");
  const root = page.locator(".overlay-root");
  await expect(root).toHaveAttribute("data-edge", "left");
  // The floating panel still renders; drawer mode is a CSS/layout variant, not a
  // different component tree.
  await expect(page.locator(".overlay-panel")).toBeVisible();
});

test("an absent or off-contract ?edge= leaves the modal (floating) panel intact", async ({ page }) => {
  // A bad edge value seeds no drawer — the presentation stays modal (null seed),
  // so the root carries no data-edge and the centered floating panel renders.
  await page.goto("/?edge=sideways");
  const root = page.locator(".overlay-root");
  await expect(root).not.toHaveAttribute("data-edge", /.*/);
});

test("drawer mode swaps the corner grip for an inner-edge resize handle", async ({ page }) => {
  // Resize affordances are mutually exclusive by mode (M006/S03). In drawer
  // mode the INNER edge grows the drawer's variable dimension, so the
  // SouthEast corner grip must NOT render (it would resize a full-span
  // left/right drawer's height and fight the anchor). Both live inside
  // .overlay-panel so pointer-events tracks overlay state (MEM148), never
  // data-edge. The pointer-drag resize itself needs the live Tauri window
  // (manual-only); here we assert the DOM contract.
  await page.goto("/?edge=left");
  const panel = page.locator(".overlay-panel");
  const edgeHandle = panel.locator(".overlay-drawer-resize-edge");
  await expect(edgeHandle).toHaveCount(1);
  // The handle carries data-edge so the T03 CSS positions it on the drawer's
  // inner edge with the correct axis cursor.
  await expect(edgeHandle).toHaveAttribute("data-edge", "left");
  // The floating-mode corner grip is gone — mutual exclusion.
  await expect(panel.locator(".overlay-resize-grip")).toHaveCount(0);
});

test("modal (floating) mode shows the corner grip and no drawer edge handle", async ({ page }) => {
  // The inverse of the mutual-exclusion contract: with no drawer seeded the
  // presentation is modal, so the centered floating panel renders the SouthEast
  // corner grip and NOT the drawer inner-edge handle.
  await page.goto("/");
  const panel = page.locator(".overlay-panel");
  await expect(panel.locator(".overlay-resize-grip")).toHaveCount(1);
  await expect(panel.locator(".overlay-drawer-resize-edge")).toHaveCount(0);
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
