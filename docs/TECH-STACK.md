# Third Eye — Tech Stack

One page on what the app is built with and why, from the overlay pixels down
to synthesized keystrokes. Updated 2026-07-27.

## Shape

A Tauri v2 desktop app: one Rust process (`src-tauri/`) owning every OS
capability, model call, and byte of stored data; several small React
webviews (`src/`) that render state they are handed over IPC. All
intelligence is local-first — an LM Studio endpoint at `localhost:1234`
serves a thin lane (fast 9B model: nudge classification, memory
distillation) and a heavy lane (27B: chat + tool calling), with optional
cloud providers behind an explicit opt-in.

## Frontend (src/)

- **React 18 + TypeScript 5.6, built by Vite 6.** No router, no state
  library: each window is one component tree (`App.tsx` overlay chat,
  `Settings.tsx`, `Hud.tsx` pill/canvas, `TrayPanel.tsx`, memory window).
- **Pure reducer modules** (`chat.ts`, `hud-state.ts`, `overlay-geometry.ts`,
  `settings-nav.ts`, …): every state transition is a plain function unit-
  tested by **Vitest 3** (~395 tests) with zero Tauri runtime; components
  are glue that subscribes to events and dispatches.
- **Design system** in `src/ui/` (Toggle, Chip, Panel, ApprovalCard,
  GhostIndicator, Markdown…), documented in **Storybook 10**. System fonts
  only; hand-written CSS in `styles.css`/`ui/*.css` (no CSS framework).
- **react-markdown + remark-gfm** renders assistant messages (no raw-HTML
  path, so model output can't script the webview).
- **Playwright 1.61** (~65 e2e specs) runs the real production bundle in a
  browser against a mocked `window.__TAURI_INTERNALS__`, proving render +
  degrade states without the Rust backend.

## Shell & windows

- **Tauri 2** (`macos-private-api`, tray-icon) with per-window ACL
  capability files. Windows: overlay, settings, memory, hud-canvas,
  hud-pill, tray-panel.
- **tauri-nspanel** converts windows to non-activating `NSPanel`s: the HUD
  never steals focus; the overlay can take keyboard focus and yield it
  before synthesized typing. Native `NSWindow` alpha stepping does the
  fade animation (CSS transforms don't repaint reliably in occluded
  WebKit windows).
- Plugins: global-shortcut (summon hotkey), autostart (LaunchAgent),
  store (`settings.json` persistence). Signed with a Developer ID so TCC
  grants survive rebuilds; `make run-app` builds and launches the bundle.

## Backend (src-tauri/, Rust)

- **tokio** multi-thread runtime; **async-trait** seams everywhere: every
  OS capability is an object-safe trait (`ScreenCapture`, `ScreenQuery`,
  `InputControl`, `AppFocus`) with a macOS impl and a typed-`unsupported`
  fallback, held as `Arc<dyn …>` in Tauri managed state.
- **Contracts**: health-as-value IPC (status queries never reject; errors
  are data), kind-tagged serde errors (`camelCase` fields, `kebab-case`
  kinds), structural inertness for anything gated off (a disabled tool is
  never advertised and refuses typed), no fake data in any UI.
- **rusqlite 0.32** (bundled SQLite): memories, chat sessions/transcripts,
  program inventory. **reqwest 0.12** streams SSE from the OpenAI-shaped
  endpoint; **chrono** for local timestamps; **base64** for frames.

## Seeing the screen (macOS)

- **screencapturekit 8** (`SCScreenshotManager`) captures the display —
  the one containing the frontmost window for on-demand eyes, primary for
  the periodic watcher — with Third Eye's own windows excluded by PID.
- **Apple Vision** (`objc2-vision`) OCRs captures into text boxes;
  `CaptureGeometry` maps captured pixels back to global logical points
  (per-axis scale + display origin) so a box is clickable on any monitor.
- **Accessibility tree** (raw HIServices FFI): `screen_query` also
  harvests the focused app's real controls — buttons/links/fields with
  exact frames and roles — merged ahead of OCR text, which is deduped
  when it sits inside an AX control. Server-side `cx,cy` centers are the
  click targets; the model does no coordinate arithmetic.

## Acting on the machine

- **enigo 0.6** synthesizes mouse/keyboard. Constructed transiently per
  action (it's `!Send`); mouse runs on a tokio blocking thread with eased
  glides and a cursor-commit wait; **keyboard synthesis hops to the main
  thread** via `dispatch_async_f` — Text Services Manager (keyboard layout
  lookup for letter shortcuts) asserts the main queue on modern macOS and
  SIGTRAPs off it. Typing is paced per-character (≤200 chars) for a
  visible rhythm; the sleeps stay off-main.
- **Verification loop**: every action returns a `verified` block read back
  from the OS — cursor position (CGEvent), keyboard-focused element and
  click-point hit-test (AX), `textEntered` polling — and a wrong-app
  readback structurally flips the result to `verification-failed`.
- **Gates**: HID Off/Ask/AutoRun mode, per-kind session whitelists, a
  per-tool Settings switchboard, per-command allowlists for the terminal
  tool, approval prompts mirrored into the HUD, and a `ScreenSeen` gate
  that refuses any click at coordinates the model never read from
  `screen_query`.

## Intelligence & tools

- `run_tool_loop` drives OpenAI-shape tool calling over SSE. Built-in
  tools: `focus_app`, `screen_query`, `input_action`, `take_screenshot`
  (vision turn + opt-in save), `memory_search`, `chat_history_search`,
  `find_programs`, `run_command`, `clipboard`, `wait` — plus external MCP
  servers. The composite executor routes by a `claims()` hook so gated
  tools own their refusals.
- Memory: watcher observations and chat exchanges are distilled by the
  thin lane into one-line memories (privacy-redacted before storage);
  nudges classify watcher batches on the same lane.

## Testing & evals

- **cargo test** (~840 unit/integration tests) including scripted-SSE
  integration tests that drive the real loop against a mock model server.
- **`make evals`**: behavioural evals asserting the *contract* — grounding
  (blind clicks refused, recovery lands on the served center), toggle
  inertness, wrong-app verification flips, recall usage, and a
  prompt-contract eval that pins every load-bearing system-prompt clause.
  A `#[ignore]` live twin runs the same scenarios against real LM Studio.
- Gate for every commit: `cargo clippy -D warnings`, `cargo fmt`, Vitest,
  Playwright (grepping the `N failed|passed` line), and a 5-second timed
  boot of the debug binary.
