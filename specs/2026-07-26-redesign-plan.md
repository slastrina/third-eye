# Plan: Third Eye UI redesign + first-start tour

Implements `specs/2026-07-26-redesign-and-first-start-tour.md` (approved).
Status: **draft — awaiting human review** (Phases 2+3 of the gated workflow).

## Implementation order and rationale

Everything hangs off the design system, so it lands first. The tour is the
milestone's headline feature and exercises the DS's light-surface half; the
HUD exercises the dark/overlay half and the only new Rust window plumbing
besides the tray panel. The four surface restyles (palette, settings, tray,
memory) are mutually independent once the DS exists.

```
A. Design system + Storybook          (foundation — everything depends on it)
   ├─ B. First-start tour             (headline feature)
   ├─ C. Live automation HUD          (new windows + event fan-out)
   ├─ D. Summon palette restyle
   ├─ E. Settings restyle
   ├─ F. Tray: eye states + panel window
   └─ G. Memory window
H. Toast/notice restyle + old-style sweep   (after D–G)
```

B–G are independent of each other; sequential execution order is B, C, D, E,
F, G (feature value first), but any of D–G can be reordered freely.

## Verified integration facts (checked against the code, not assumed)

- Tool-phase events: `ToolEvent::Call` / `ToolEvent::Result` flow through the
  injected `ToolEventSink` and reach the UI via the `llm://` event names in
  `chat.ts` — the HUD folds these; no tool-loop changes.
- Run cancel: `stop_chat` IPC exists (single-flight abort via `Aborter`).
  HUD Stop/Esc call it. **Plan check during C3:** confirm `stop_chat` also
  disarms HID / returns input; if not, extend it there.
- Click-through: `overlay::set_click_through` wraps
  `set_ignore_cursor_events` with macOS + fallback backends — the
  `hud-canvas` window reuses this pattern (generalized to take a window
  label, not hard-coded to the overlay).
- First-run: `first_run_status` / per-permission request / `complete_first_run`
  IPC + `onboarding-state.ts` reducer already implement the permission
  lifecycle the tour's step 1 needs; the tour reducer wraps, not replaces, it.
- Both existing windows route by `index.html?view=` — new surfaces (hud-pill,
  hud-canvas, tray-panel, memory) extend that routing in `main.tsx`.

## Risks and mitigations

| Risk | Mitigation |
|---|---|
| Settings.tsx is 2082 lines; a big-bang restyle invites regressions | Restyle pane-by-pane (one task each isn't needed, but commits are per-pane); reducer/IPC wiring untouched; settings.test.ts + settings e2e must stay green after each pane |
| HUD steals focus mid-run → synthesized keys land in our own window | hud-pill uses the overlay's nonactivating panel posture; hud-canvas is click-through always; e2e/manual check types into a third-party app during a run |
| Ghost indicator coordinate space (pixels vs points, Retina) | Reuse the `CaptureGeometry → to_screen_points` mapping already proven for clicks; indicator positions derive from the same screen-point values the click uses |
| Tray-panel anchoring is flaky on Windows/Linux | Anchor from tray-click position when the platform provides it; else center-top fallback (spec'd) |
| Multi-monitor HUD | v1: canvas covers the primary monitor; documented limitation, revisit after milestone |
| Storybook 9 / Vite 6 interop | Pin the current stable `@storybook/react-vite`; Storybook is dev-only so a downgrade is cheap if the builder misbehaves |
| Old/new visual language coexisting mid-milestone | Acceptable while phases land; phase H does the sweep and deletes dead styles |

## Task breakdown

Task IDs are commit-sized; each lists acceptance, verification, files
(≤5 per task; \* = new file).

### Phase A — Design system + Storybook

- [ ] **A1 Storybook scaffold**
  - Acceptance: `npm run storybook` serves; a smoke story renders with
    `tokens.css` + `styles.css` loaded; `npm run build-storybook` outputs
    static build; nothing Storybook-related ships in `dist/` or the bundle.
  - Verify: run both commands; `make build-web` still green.
  - Files: `package.json`, `.storybook/main.ts`\*, `.storybook/preview.ts`\*
- [ ] **A2 Tokens**
  - Acceptance: `src/ui/tokens.css`\* defines the full `--te-*` set from the
    spec (colors incl. light-surface set, radii, shadows, blur, motion
    keyframes, font stack — system-ui, no Lufga); imported once app-wide.
  - Verify: tokens story renders swatches; `make check`.
  - Files: `src/ui/tokens.css`\*, `src/ui/tokens.stories.tsx`\*, `src/main.tsx`
- [ ] **A3 Primitives: Button, Chip/ChoiceChips, SectionLabel**
  - Acceptance: variants per spec table; real `<button>` semantics; tokens
    only, no literal colors.
  - Verify: stories for every variant/state; `make check && make test-unit`.
  - Files: `src/ui/Button.tsx`\*, `src/ui/Chip.tsx`\*,
    `src/ui/SectionLabel.tsx`\*, `src/ui/primitives.stories.tsx`\*, `src/ui/ui.css`\*
- [ ] **A4 Form controls: Toggle, RadioCard**
  - Acceptance: controlled, stateless, `role="switch"`/`role="radio"` ARIA.
  - Verify: stories on/off + selected/unselected; `make check`.
  - Files: `src/ui/Toggle.tsx`\*, `src/ui/RadioCard.tsx`\*,
    `src/ui/controls.stories.tsx`\*
- [ ] **A5 EyeIcon + StepIndicator + Toast + Panel**
  - Acceptance: EyeIcon renders watching/thinking/acting/closed (iris color,
    scan animation, lid arc) at arbitrary size; StepIndicator takes
    current/total/labels; Toast is the bottom-center pill; Panel gives the
    glassy chrome.
  - Verify: stories ×4 eye states, step positions, toast, panel; `make check`.
  - Files: `src/ui/EyeIcon.tsx`\*, `src/ui/StepIndicator.tsx`\*,
    `src/ui/Toast.tsx`\*, `src/ui/Panel.tsx`\*, `src/ui/chrome.stories.tsx`\*

### Phase B — First-start tour

- [ ] **B1 Tour reducer**
  - Acceptance: `tour-state.ts`\* wraps the existing onboarding permission
    lifecycle and adds: step 0–3 navigation (next/back/skip), Continue
    blocked on step 1 exactly per `onboardingBlocked`, retention selection,
    hotkey-press-completes-on-step-3, finish/skip → completed, persist-error
    surfaced. Every transition unit-tested.
  - Verify: `make test-unit` (new `tour-state.test.ts`\*).
  - Files: `src/tour-state.ts`\*, `src/tour-state.test.ts`\*,
    `src/onboarding-state.ts` (export reuse only)
- [ ] **B2 memoryRetention setting (Rust)**
  - Acceptance: persisted `memoryRetention` key (`7d|30d|90d|forever`,
    default `30d`), `get`/`set` IPC registered, survives relaunch; no
    pruning behavior.
  - Verify: `cargo test` covers default, set, persist round-trip;
    `make check-rust && make lint`.
  - Files: `src-tauri/src/config.rs`, `src-tauri/src/memory/commands.rs`,
    `src-tauri/src/lib.rs`
- [ ] **B3 Tour surface**
  - Acceptance: `Tour.tsx`\* renders all four designed steps from DS
    components (light card, StepIndicator, permission rows with
    Grant→Granted ✓, retention ChoiceChips, real hotkey display); replaces
    the M006 explainer render path in `App.tsx`; finish shows the designed
    toast; overlay modal sized for the 560px card; one-shot flag behavior
    unchanged.
  - Verify: `make test-unit`; manual `make tauri-dev` with cleared first-run
    flag walks all steps; hotkey press on step 3 completes.
  - Files: `src/Tour.tsx`\*, `src/tour.css`\*, `src/App.tsx`, `src/chat.ts`
    (retention IPC binding)
- [ ] **B4 Tour e2e**
  - Acceptance: `tour.spec.ts`\* drives welcome→permissions (mock grant)→
    retention→summon→finish, asserts block-until-capture-granted, skip, and
    no-reshow; `smoke.spec.ts` updated for the new DOM.
  - Verify: `make test-e2e`.
  - Files: `e2e/tour.spec.ts`\*, `e2e/smoke.spec.ts`

### Phase C — Live automation HUD

- [ ] **C1 HUD reducer**
  - Acceptance: `hud-state.ts`\* folds ToolEvent call/result payloads:
    call → trail append (● current, label like "click · Export"), result →
    settle ✓/✗ from the ActionReport verification, target screen-point
    extraction for input actions, run start/stop/done/idle lifecycle.
  - Verify: `make test-unit` (new `hud-state.test.ts`\*).
  - Files: `src/hud-state.ts`\*, `src/hud-state.test.ts`\*
- [ ] **C2 HUD components**
  - Acceptance: `HudPill` (eye acting-state, action label, count, Stop),
    `ActionTrail` (settling list), `GhostIndicator` (labeled ring + ripple at
    x,y) — pure presentational, stories for live/done/stopped/failed.
  - Verify: stories; `make check`.
  - Files: `src/ui/HudPill.tsx`\*, `src/ui/ActionTrail.tsx`\*,
    `src/ui/GhostIndicator.tsx`\*, `src/ui/hud.stories.tsx`\*
- [ ] **C3 HUD windows (Rust)**
  - Acceptance: `hud-pill` (nonactivating, interactive, top-center) and
    `hud-canvas` (primary-monitor full-screen, transparent, click-through
    via the generalized `set_click_through`) declared hidden in
    `tauri.conf.json`; shown while a run executes HID actions, hidden on
    done/stop; tool-phase + hid events reach both windows; `stop_chat`
    confirmed (or extended) to disarm HID.
  - Verify: `cargo test` for show/hide lifecycle mapping; `make lint`;
    manual run drives a real HID action with HUD visible, driven app keeps
    focus.
  - Files: `src-tauri/tauri.conf.json`, `src-tauri/src/overlay/mod.rs`,
    `src-tauri/src/hud.rs`\*, `src-tauri/src/lib.rs`,
    `src-tauri/src/llm/commands.rs`
- [ ] **C4 HUD views + wiring**
  - Acceptance: `?view=hud-pill` / `?view=hud-canvas` routes in `main.tsx`
    render `Hud.tsx`\* views off `hud-state`; Stop and Esc call `stop_chat`;
    ghost indicator draws at the same screen points the click used.
  - Verify: `make test-unit`; `e2e/hid.spec.ts` extended to assert HUD
    events; manual end-to-end run.
  - Files: `src/Hud.tsx`\*, `src/main.tsx`, `src/chat.ts`, `e2e/hid.spec.ts`

### Phase D — Summon palette restyle

- [ ] **D1 Palette panel + input + footer**
  - Acceptance: overlay chat surface rebuilt with DS (glassy Panel, eye +
    large input, esc chip; lane footer with Auto/Thin/Heavy override bound
    to the real router lane; on-device badge only when true). Chat/stream/
    reasoning behavior byte-identical (reducers untouched).
  - Verify: `make test-unit`; overlay e2e specs green; manual summon.
  - Files: `src/App.tsx`, `src/styles.css`
- [ ] **D2 Context chips + suggestions**
  - Acceptance: screen-attached chip and focused-window chip bound to real
    watcher/appfocus state; memory-suggestion rows render only when the
    memory store returns suggestions (no-fake-data); absent otherwise.
  - Verify: `make test-unit`; manual with/without memory data.
  - Files: `src/App.tsx`, `src/watcher-state.ts`, `src/chat.ts`

### Phase E — Settings restyle

- [ ] **E1 Nav + chrome**
  - Acceptance: sectioned left nav (INTELLIGENCE/PRIVACY & DATA/AUTOMATION/
    SYSTEM) per design, DS Panel chrome; only panes with real backends
    listed; pane routing unchanged (`settings-nav.ts`).
  - Verify: `settings.test.ts` + settings e2e green.
  - Files: `src/Settings.tsx`, `src/settings-nav.ts`, `src/styles.css`
- [ ] **E2 Panes restyle (batched, one commit per pane)**
  - Acceptance: every existing pane rebuilt from DS controls (Toggle,
    RadioCard, Chip, Button); Memory pane gains the retention ChoiceChips
    bound to B2's setting; wiring/reducers untouched.
  - Verify: `settings.test.ts`, `e2e/settings.spec.ts`, manual pass through
    every pane.
  - Files: `src/Settings.tsx`, `src/settings-state.ts` (retention read only)

### Phase F — Tray

- [ ] **F1 Eye-state icon frames**
  - Acceptance: procedural tray icon draws the new eye in
    watching/thinking/acting/closed; state transitions driven by existing
    watcher/run state; pure frame-selection logic unit-tested.
  - Verify: `cargo test`; visual check on macOS menubar.
  - Files: `src-tauri/src/tray.rs`
- [ ] **F2 Tray panel window**
  - Acceptance: left-click opens `tray-panel` webview (nonactivating, near
    the tray icon, centered-top fallback); shows status header, pause
    15m/1h/until-resume + resume (wired to watcher), real stats only
    (observed time/moments/facts from the memory store — sections omitted if
    the store can't serve them), Summon/Memory/Settings buttons; right-click
    keeps the native menu (Quit, Autostart).
  - Verify: `cargo test` for menu-action mapping; manual tray interaction.
  - Files: `src-tauri/tauri.conf.json`, `src-tauri/src/tray.rs`,
    `src/TrayPanel.tsx`\*, `src/main.tsx`, `src/chat.ts`
- [ ] **F3 Tray-panel state + tests**
  - Acceptance: `tray-panel-state.ts`\* reducer (pause options, stats
    presence, watching/paused) fully tested.
  - Verify: `make test-unit`.
  - Files: `src/tray-panel-state.ts`\*, `src/tray-panel-state.test.ts`\*

### Phase G — Memory window

- [ ] **G1 Memory window shell + Timeline**
  - Acceptance: `memory` window (`?view=memory`) with designed chrome,
    filter input, Timeline tab listing real moments (TimelineRow), forget
    action wired to the store; paused-gap row only when a real gap exists.
  - Verify: `memory-state` tests extended; `e2e/memory.spec.ts` updated.
  - Files: `src-tauri/tauri.conf.json`, `src/Memory.tsx`\*, `src/main.tsx`,
    `src/memory-state.ts`, `e2e/memory.spec.ts`
- [ ] **G2 Learned + Recall tabs**
  - Acceptance: Learned grid of FactCards (confidence bar, forget) from the
    real fact store; Recall tab reuses the existing recall path with the
    designed chat bubbles + busy row.
  - Verify: `make test-unit`; manual recall round-trip.
  - Files: `src/Memory.tsx`, `src/memory-state.ts`, `src/chat.ts`

### Phase H — Sweep

- [ ] **H1 Toast/notice unification + old-style removal**
  - Acceptance: `tray-notice.ts` surface renders the DS Toast; every
    remaining old-language style deleted from `styles.css`; no component
    outside `src/ui` declares literal colors.
  - Verify: grep gate (no stray hex outside tokens), full
    `make check && make test && make test-e2e && make lint`.
  - Files: `src/tray-notice.ts`, `src/App.tsx`, `src/styles.css`
- [ ] **H2 Spec/docs close-out**
  - Acceptance: spec updated to "implemented" with any drift recorded
    (living-document rule); follow-up specs stubbed (retention enforcement,
    multi-monitor HUD).
  - Verify: review.
  - Files: `specs/2026-07-26-redesign-and-first-start-tour.md`, this file

## Verification checkpoints (between phases)

After each phase: `make check && make test-unit && make lint`; e2e after B,
C, and H at minimum; manual `make tauri-dev` walkthrough after B (tour), C
(live run), F (tray). Commits reference the spec section they implement.
