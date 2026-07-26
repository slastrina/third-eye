# Spec: Third Eye UI redesign + first-start tour

Source design: Claude Design project "Third Eye" (`Third Eye.dc.html`,
https://claude.ai/design/p/5ec29e97-03a4-49f4-a685-c415797f0e22).
Status: **implemented 2026-07-26** — see "Close-out" at the end for the
recorded deviations and follow-ups.

## Objective

Adopt the new Claude Design interface across every Third Eye surface, backed by
a reusable design system with Storybook, and ship the redesigned **first-start
tour** — the four-step startup wizard (Welcome → Permissions → Memory → Summon)
that welcomes the user, collects the two OS permissions, sets memory retention,
and teaches the summon hotkey. The tour does **not** drive the computer or
demo automation. The automation HUD / ghost indicator from the design ARE
wired to live runs in this milestone (surface 7 below): while a real HID run
executes, the user sees what Third Eye is doing and where.

Success looks like: a fresh install boots into the new tour; every existing
surface (summon palette, settings, tray, toasts, menubar eye) renders in the
new visual language; every UI component exists in Storybook with stories; all
existing unit + e2e tests pass (updated where the UI contract changed).

## Tech Stack

- Tauri v2 (Rust backend, existing crate in `src-tauri/`)
- React 18 + TypeScript ~5.6, Vite 6
- Vitest (unit), Playwright (e2e)
- **New:** Storybook (latest stable, `@storybook/react-vite`) — devDependency
  only, never part of the Tauri bundle
- Fonts: **system-ui stack only** (user decision 2026-07-26: no Lufga). The
  design's Lufga references are dropped; tokens define one
  `--te-font: -apple-system, BlinkMacSystemFont, "Segoe UI", system-ui, sans-serif`

## Design system (new)

Everything below is extracted from `Third Eye.dc.html` into `src/ui/`.

### Tokens (`src/ui/tokens.css`, CSS custom properties)

- Color: navy backgrounds `#040C22 / #071D49 / #0B2050 / #0F2C66`, accent green
  `#93DA49` (hover `#A7E465` / `#6BB22E`), acting amber `#E9A23B`, danger
  `#E85C5C` (`#FF9D9D` text), white text at opacity steps (.92/.85/.65/.55/.45/.4),
  light-surface palette for the tour card (`#FFFFFF`, ink `#071D49`/`#2A3242`,
  muted `#5B6474`/`#8A93A3`, borders `#E3E6EC`/`#C9CFDA`)
- Radii: 8/10/12/14/16 px panels, 99px pills
- Effects: panel shadows, `backdrop-filter` blur (14–20px), glassy borders
  `rgba(255,255,255,.07–.14)`
- Motion: `te-in`, `te-fade`, `te-blink`, `te-ripple`, `te-scan`, `te-pulse`
  keyframes; standard cubic-bezier(.3,.7,.3,1) glide

### Components (`src/ui/*.tsx`, each with a colocated `*.stories.tsx`)

| Component | From design | Used by |
|---|---|---|
| `EyeIcon` | eye SVG with states: watching (green iris), thinking/acting (scanning pupil, amber), closed (lid arc) | tour, tray panel, palette, HUD |
| `StepIndicator` | onboarding step dots/labels | tour |
| `Button` | pill CTA (green primary, outline secondary, subtle/text) | everywhere |
| `Toggle` | 38×22 pill switch | settings |
| `RadioCard` | bordered selectable card with ring dot | settings, tour |
| `Chip` / `ChoiceChips` | pill chips incl. removable (✕) and selectable retention chips | settings, tour, palette |
| `Panel` | glassy dark window chrome (border, shadow, blur) | palette, tray panel, memory, settings |
| `Toast` | bottom-center pill notice with green dot | all windows |
| `SectionLabel` | tracked-caps micro-heading | panels |
| `StatTriplet` | three-up stat row | tray panel |
| `TimelineRow` | time/dot/app/text/duration/forget row | memory |
| `FactCard` | fact + confidence bar + forget | memory |
| `HudPill` + `ActionTrail` | run status pill, ○/●/✓ action list | live HUD (surface 7) |
| `GhostIndicator` | labeled target ring + click ripple | live HUD (surface 7) |

Storybook runs against the same `tokens.css` + `styles.css`; stories are the
review surface for every component before it lands in an app surface.

## Surfaces in scope (full redesign)

1. **First-start tour** (the feature of this milestone — replaces the M006
   permission explainer panel):
   - Step 0 *Welcome*: eye mark, "Meet your Third Eye", Observes / Learns /
     Acts bullets.
   - Step 1 *Permissions*: Screen recording + Input control rows with
     Grant → Granted ✓ buttons, wired to the existing
     `onboarding-state.ts` lifecycle (`first_run_status`, per-permission
     request IPC). **Semantics unchanged** (D038/R019): Screen Recording
     blocks Continue when supported-and-ungranted; Accessibility never
     blocks and granting it never arms HID.
   - Step 2 *Memory*: retention chips 7 days / 30 days / 90 days / Forever →
     new persisted setting (see Backend deltas); privacy copy ("on this
     device", excluded apps note).
   - Step 3 *Summon*: shows the **actual configured hotkey** (from
     `hotkey.rs` presets, not hard-coded ⌥space), "try pressing it now — or
     click Finish". Pressing the live hotkey or Finish completes the tour
     (persists the existing first-run flag, shows the "Third Eye is
     watching" toast).
   - Skip tour available on every step; Back from step ≥1; one-shot per
     install exactly like today.
   - Renders in the overlay window as a focused modal (current M006
     behavior); the design's mock desktop/menubar backdrop is prototype
     staging and is **not** implemented.
2. **Summon palette** (overlay): glassy panel, eye + large input, context
   chips (screen attached / focused window / + window), memory suggestions
   list, lane footer (Auto / Thin / Heavy override). Rendered with DS
   components; existing chat/stream/recall behavior preserved.
3. **Settings window**: sectioned left nav (INTELLIGENCE / PRIVACY & DATA /
   AUTOMATION / SYSTEM), panes restyled with DS components. Existing panes
   keep their wiring; design-only panes that have no backend stay absent (no
   dead stubs).
4. **Tray**: menubar eye icon redrawn to the new eye states (watching /
   thinking / acting / closed) in `tray.rs`'s procedural renderer; the rich
   tray dropdown panel (status header, pause options, stats, latest, Summon /
   Memory / Settings buttons) becomes a new small webview window anchored
   near the tray icon. Native menu kept as right-click fallback (Quit,
   Autostart). *(Ask-first item — see Boundaries.)*
5. **Memory window**: new surface with Timeline / Learned / Recall tabs per
   the design, backed by the existing memory store IPC. Recall tab reuses the
   existing recall path.
6. **Toasts / tray notices**: `tray-notice.ts` surface restyled to the DS
   `Toast`.
7. **Live automation HUD** (new surface): while a run is executing HID
   actions, the user sees what Third Eye is doing and where.
   - **Event sources (existing IPC, no new loop plumbing):** the tool loop's
     `ToolEvent::Call` / `ToolEvent::Result` events (`llm://` contract in
     `chat.ts`) carry each action + its ActionReport; `hid://` state events
     carry arm/disarm and approval prompts.
   - **HudPill** (top-center, interactive): eye in acting state (amber),
     current action label derived from the live tool call (e.g.
     "click · Export", "type · Q3-report-final.pdf"), running count, and a
     **Stop** button wired to the existing run-cancel path.
   - **ActionTrail** (under the pill): honest reactive trail — actions appear
     as they are called (● current) and settle to ✓/✗ from their results.
     The prototype's upfront future-step checklist is NOT shown: the real
     loop is reactive and we don't invent future steps (no-fake-data rule).
   - **GhostIndicator**: labeled "Third Eye" ring + click ripple rendered at
     the action's target coordinates (from `input_action` args, screen-point
     space per the CaptureGeometry mapping) on a **full-screen transparent
     click-through canvas window**.
   - **Window architecture:** click-through is per-window, so the HUD is two
     windows — `hud-canvas` (full-screen, transparent, alwaysOnTop,
     click-through, per the overlay's existing setIgnoresMouseEvents
     pattern) and `hud-pill` (small, nonactivating, interactive — same
     posture as the overlay panel). Both hidden except while a run with HID
     actions is live. Multi-monitor: canvas covers the monitor the actions
     target; v1 may cover the primary monitor only (documented limitation).
   - The HUD must never steal focus from the app being driven (the
     nonactivating-panel lesson: a focused HUD would swallow synthesized
     keys mid-run).

## Commands

```
make install        # npm ci
make tauri-dev      # full desktop app, dev
make dev            # vite only
make build          # full bundle
make check          # tsc + cargo check
make test           # vitest + cargo test
make test-e2e       # playwright
make lint / fmt     # clippy / cargo fmt
npm run storybook   # NEW — component workbench
npm run build-storybook  # NEW — static build (CI artifact only)
```

## Project structure

```
src/ui/                  → design system: tokens.css, components, *.stories.tsx
src/tour-state.ts        → tour reducer (extends onboarding-state pattern)
src/Tour.tsx             → tour surface (steps composed from src/ui)
src/hud-state.ts         → live-HUD reducer (folds tool-call/result events)
src/Hud.tsx              → hud-pill + hud-canvas views (?view=hud routing)
src/App.tsx              → overlay glue (palette redesign lands here)
src/Settings.tsx         → settings glue (restyle)
src/*-state.ts           → pure reducers + colocated *.test.ts (unchanged pattern)
src-tauri/src/tray.rs    → eye icon frames; tray panel window plumbing
specs/                   → this spec and successors
e2e/                     → Playwright specs (updated for new UI contract)
.storybook/              → Storybook config
```

## Code style

Follow the existing repo idiom — pure state modules with exhaustive doc
comments and reducer tests; React components as thin glue. New UI components:

```tsx
// src/ui/Toggle.tsx — controlled, stateless, tokens only (no literal colors).
export function Toggle({ on, onChange, label }: ToggleProps) {
  return (
    <button role="switch" aria-checked={on} className="te-toggle" data-on={on}
      onClick={() => onChange(!on)}>
      <span className="te-toggle-knob" />
      {label && <span className="te-toggle-label">{label}</span>}
    </button>
  );
}
```

- All colors/radii/motion via `var(--te-*)` tokens; no inline hex in components.
- Class prefix `te-`; component CSS lives beside the component.
- Interactive elements are real `<button>`/`<input>` with ARIA (the design's
  clickable `<span>`s are upgraded).

## Testing strategy

- **Vitest**: `tour-state.test.ts` covers every transition (step advance/back,
  skip, permission lifecycle folded from onboarding-state, hotkey-completes,
  retention selection, persist-error surfacing). `hud-state.test.ts` covers
  the event fold: call→trail append, result→settle (✓/✗ from ActionReport
  verification), stop/done/idle transitions, target-coordinate extraction.
  Existing reducer tests stay green; `onboarding-state.test.ts` evolves with
  the tour.
- **Storybook**: every `src/ui` component has stories for each visual state
  (e.g. EyeIcon×4 states, Button×3 variants, tour steps 0–3). Stories are the
  design-review artifact; no snapshot runner in this milestone.
- **Playwright**: update `smoke.spec.ts` / affected specs for the new DOM;
  add `tour.spec.ts` driving all four steps (mock IPC as the existing e2e
  harness does).
- **Rust**: `cargo test` for tray icon state mapping + new settings key;
  `make lint` clean.

## Backend deltas (Rust)

- New persisted setting `memoryRetention` (`"7d" | "30d" | "90d" | "forever"`,
  default `"30d"`) + IPC get/set, written by tour step 2 and Settings.
  **Enforcement (pruning the store) is a follow-up milestone** — this
  milestone persists and displays it only.
- Tray: eye-state frames (watching/thinking/acting/closed) replacing current
  frames; new `tray-panel` webview window (hidden, skipTaskbar,
  nonactivating) + show/hide IPC anchored to tray click.
- HUD: `hud-canvas` + `hud-pill` windows (see surface 7) with show/hide
  driven by run lifecycle; tool-phase events additionally fanned out to the
  HUD windows (today they target the overlay's chat surface only). If no
  run-cancel IPC exists yet, add one that aborts the tool loop and disarms
  HID (Stop button + Esc both call it).
- Tour completion reuses the existing `complete_first_run` flag unchanged.

## Boundaries

- **Always:** run `make check && make test` before each commit; keep reducers
  pure and tested; use DS tokens/components for all new UI; keep the overlay
  nonactivating except in its existing focused modes; preserve permission
  semantics (Screen Recording required, Accessibility optional and never
  auto-arms HID — D038/R019).
- **Ask first:** adding any dependency beyond Storybook packages; changing
  `tauri.conf.json` window definitions (the new tray-panel and memory windows
  will need this — flagged here so plan review covers it); bundling Lufga
  fonts (license unconfirmed); replacing the native tray menu outright;
  changing any IPC contract shape.
- **Never:** ship Storybook in the app bundle; weaken or bypass the
  guard/approval gates (`check-guard`, `check-mcp-guard`); delete failing
  tests to go green; commit fonts or design assets with unknown licensing.

## Success criteria

1. Fresh profile → app launches into the new four-step tour; Continue is
   blocked until Screen Recording is granted (macOS); Skip works; finishing
   persists the flag and the tour never re-shows.
2. Step 3 completes via the real hotkey press as well as Finish.
3. Retention choice persists across relaunch and is visible in Settings.
4. Summon palette, Settings, tray icon, toasts render the new design; no
   surface still uses the old visual language.
5. `npm run storybook` shows every `src/ui` component with all states.
6. `make check`, `make test`, `make test-e2e`, `make lint` all pass.
7. Tray panel + memory window open from the tray with the designed content.
8. During a real HID run: the hud-pill shows the current action live, the
   trail settles ✓/✗ from verified ActionReports, the ghost indicator marks
   click targets on screen, Stop/Esc abort the run and disarm HID, and the
   driven app never loses focus to the HUD.

## Resolved decisions (2026-07-26, with the user)

1. **Fonts** — no Lufga anywhere; system-ui token stack.
2. **Tray dropdown** — webview panel on left-click, native menu kept as
   right-click fallback (Quit, Autostart); tray-anchored positioning with a
   centered fallback on platforms where anchoring is unreliable.
3. **No fake data** — sections render only when real data backs them; honest
   empty states; the prototype's demo numbers never ship. Consequence for the
   HUD: reactive action trail, no invented future-step checklist.
4. **HUD live wiring is IN this milestone** — not deferred.
5. **Tutorial scope** — the tour is the four-step wizard only; it never
   drives or simulates driving the computer.
6. **Milestone scope** — full redesign of all surfaces now, not tutorial-only.

## Open questions

None blocking — plan-phase items to confirm during review: multi-monitor HUD
coverage (v1 primary-monitor-only acceptable?), and whether a run-cancel IPC
already exists or must be added.

## Close-out (2026-07-26)

All eight phases landed (commits `71756f8..HEAD`). Deviations from the
prototype, each an application of a spec rule rather than a cut:

1. **No-fake-data omissions** — the palette's focused-window chip and
   memory-suggestion rows (no backend serves them), the tray panel's
   "observed today / new facts" stats (ditto), the Learned tab's confidence
   bars (no stored field), and the HUD's upfront future-step checklist (the
   real loop is reactive; the HUD shows an announced-only action trail).
   Each renders the moment a backend exists for it.
2. **Tray icon stays template-monochrome** — macOS template images cannot
   carry the design's green/amber iris; the existing procedural frames
   already express the four eye states by shape (closed / scanning /
   orbiting / spark), so color rides only the in-app surfaces.
3. **Esc kill-switch is conditional** — global Escape is grabbed only while
   a run is live AND HID is armed; stopping never flips the user's standing
   Input Control setting (D038) — loop termination is what returns input.
4. **Recall tab shows ranked matches, not a chat answer** — memory_search's
   true ranking mode (and keyword-fallback degradation) is surfaced; LLM
   recall remains the palette's job.
5. **Timed tray pause** is a webview timer in the persistent tray-panel
   window; after an app restart it degrades to a plain persisted pause and
   the sub-line stops claiming a resume time.

Follow-ups (specced separately when picked up):
- Retention **enforcement** (store pruning honoring `memoryRetention`).
- Multi-monitor HUD coverage (v1 canvas covers the primary monitor).
- Tray-panel dismiss-on-outside-click (v1: toggle via tray icon, ✕, or a
  navigating action).
- The three cloud-keystore unit tests hit the real macOS keychain and are
  flaky under full-suite parallelism (environment contention, pre-existing);
  consider serializing them.
