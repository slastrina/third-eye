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
  await expect(input).toHaveAttribute("placeholder", "Ask, act, or recall anything…");
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

test("a docked drawer's chat transcript flex-fills the panel height", async ({ page }) => {
  // The floating panel caps .chat-messages at 40vh; a full-height drawer must
  // NOT inherit that cap — docked to an edge, the leftover vertical space IS
  // the chat (the "docked right but chat stops two-thirds down" regression).
  await page.goto("/?edge=right");
  const input = page.getByLabel("Overlay input");
  await input.fill("fill the drawer");
  await input.press("Enter");

  const messages = page.locator(".chat-messages");
  await expect(messages).toBeVisible();
  const panelBox = await page.locator(".overlay-panel").boundingBox();
  const messagesBox = await messages.boundingBox();
  const inputBox = await page.locator(".overlay-input-row").boundingBox();
  if (!panelBox || !messagesBox || !inputBox) throw new Error("panel/messages not laid out");
  // Grew past the floating-mode cap (40vh of the default 720px viewport = 288).
  expect(messagesBox.height).toBeGreaterThan(0.4 * 720);
  // Bottom-anchored input layout: the transcript ends just above the input
  // row, and the input row (plus the model footer under it) hugs the panel
  // bottom — the leftover vertical space is still all chat.
  // (Banners/attach affordances may legitimately sit between them, so only
  // the ordering is asserted, not a tight gap.)
  expect(messagesBox.y + messagesBox.height).toBeLessThanOrEqual(inputBox.y + 1);
  const bottomGap = panelBox.y + panelBox.height - (inputBox.y + inputBox.height);
  expect(bottomGap).toBeLessThan(80);
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

test("an empty chat centers the composer; the first message bottom-anchors it", async ({ page }) => {
  // Spotlight posture: before any message the input cluster floats at the
  // vertical center of the full-height drawer; submitting moves it to the
  // bottom (covered by the flex-fill test above).
  await page.goto("/?edge=right");
  const panelBox = await page.locator(".overlay-panel").boundingBox();
  const composerBox = await page.locator(".overlay-composer").boundingBox();
  if (!panelBox || !composerBox) throw new Error("panel/composer not laid out");
  const panelCenter = panelBox.y + panelBox.height / 2;
  const composerCenter = composerBox.y + composerBox.height / 2;
  expect(Math.abs(composerCenter - panelCenter)).toBeLessThan(panelBox.height * 0.2);

  // First message: the composer drops to the bottom region of the panel.
  const input = page.getByLabel("Overlay input");
  await input.fill("anchor me");
  await input.press("Enter");
  const anchored = await page.locator(".overlay-composer").boundingBox();
  if (!anchored) throw new Error("composer vanished after submit");
  expect(anchored.y + anchored.height).toBeGreaterThan(panelBox.y + panelBox.height * 0.6);
});
