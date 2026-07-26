# Spec: AX targeting, multi-display, chat resume, nudge controls, evals

Date: 2026-07-27. Follow-on to computer-control + hid-extensions, chosen by
the user from the backlog: (1) AX-tree targeting, (2) multi-display
capture, (3) resume past chats, (4) nudge controls, (5) tool-use evals.

## Objective

Make computer control categorically more reliable (click real UI elements,
work on every monitor), close the recall loop (continue a stored chat),
make nudges tunable, and put a regression harness around the model's tool
behaviour so prompt/tool changes are measured, not vibes.

## Commands

Build: `make build-tauri` · Run: `make run-app` · Rust: `cargo test`,
`cargo clippy --all-targets -- -D warnings`, `cargo fmt` ·
JS: `npm test`, `npx playwright test` (grep the `N failed|passed` line) ·
Evals: `make evals` (new).

## Feature 1 — AX-tree targeting (accuracy v2)

Today every click target comes from OCR boxes (quantized, text-only). The
accessibility tree has the real interactive elements with exact frames in
global screen points — no pixel mapping at all.

- New seam `axquery` (macOS impl + typed-unsupported fallback, mirroring
  the screenquery seam): `AXUIElementCreateApplication(pid)` for the
  focused app, bounded walk (depth ≤ 12, ≤ 400 elements collected), collect
  elements whose role is interactive (AXButton, AXLink, AXTextField,
  AXTextArea, AXSearchField, AXCheckBox, AXRadioButton, AXPopUpButton,
  AXComboBox, AXMenuItem, AXTab), visible with a nonzero AXFrame.
- `ScreenElement` gains `role: Option<String>` (serde skip when None). AX
  elements carry role + title/label as `text`; OCR elements keep `role:
  None`. cx/cy computed the same way (full precision — AX frames are
  already points).
- Merge in the screen_query path AFTER the focused-app filter: AX elements
  first, then OCR elements whose box does not substantially overlap an AX
  element (IoU-style containment check, pure + unit-tested) — dedup keeps
  the authoritative AX frame.
- Degrade: AX walk failure or empty ⇒ OCR-only result exactly as today
  (never an error). Third Eye's own windows excluded by pid.
- Prompt/tool description: prefer elements with a `role` (they are the
  real buttons/links); OCR text is the fallback.

## Feature 2 — Multi-display capture

`capture_primary` means a focused app on a second monitor is invisible and
unclickable.

- Capture selects the display containing the focused app's frontmost
  window center (CGWindowList bounds for the pid); fallback primary.
- `CaptureGeometry` gains the display's global origin (`origin_x/origin_y`
  points); `to_screen_points` adds it — coordinates stay global, enigo
  clicks land on any monitor. HUD canvas already fits per-monitor.
- `take_screenshot` uses the same display selection (see what the focused
  app sees). Watcher keeps primary (cost).

## Feature 3 — Resume past chats

- Overlay gains a sessions affordance beside ＋New: list recent sessions
  (existing `chat_sessions` IPC), pick one → transcript seeds from
  `chat_session_messages`, and the run's session id is pinned so new
  exchanges append to the SAME stored session (backend: resume_session id
  on the logging state instead of create-on-first-exchange).
- Reducer-pure: `seedFromSession(messages)` + tests; e2e for open-list,
  seed, continue.

## Feature 4 — Nudge controls

- Settings (Nudges section): cooldown chips (1m/5m/15m/1h), auto-dismiss
  chips (8s/12s/20s). Persisted keys + applier with rollback (nudges
  toggle contract); detector reads live values each round.
- Recent nudges: bounded ring (last 20) on NudgeState — message, app,
  shown-at, dismiss reason; `nudge_history` IPC; rendered as a read-only
  list in the section.

## Feature 5 — Evals for tool use and behaviour

Deterministic harness first (no live model): scripted fake LLM turns drive
the REAL toolloop against fake backends, and assertions score behaviour.

- `src-tauri/tests/evals/` — table-driven scenarios, each: scripted model
  turns (tool calls + final text), fake tool backends (input/screen/
  capture/store seeded), and behavioural assertions:
  - grounding: no coordinate click without a prior screen_query (gate
    fires; scripted "bad model" scenario expects the typed refusal),
  - honesty: a refused/failed tool ⇒ final text must not claim success
    (string-level heuristics: refusal kind present ⇒ answer mentions
    failure / does not contain "done/success" claims — assertions written
    per scenario, not generic NLP),
  - recall: "what did I ask before" scenario ⇒ chat_history_search called,
  - verification: wrong-app focus readback ⇒ next call is screen_query,
  - toggles: disabled tool never appears in offered definitions.
- Live-model mode (opt-in, `THIRD_EYE_EVAL_LIVE=1` + LM Studio up): same
  scenarios sent to the real endpoint, report pass/fail per behaviour to
  stderr + a JSON report file; never part of `cargo test` default.
- `make evals` runs the deterministic suite; live mode documented in the
  Makefile comment.

## Boundaries

- Always: per-feature commits with the full verification battery; no
  co-author trailers; no fake data in UI.
- Ask first: new runtime deps beyond objc2 AX bindings already in tree.
- Never: relax D038 structural gates; store AX/OCR coordinates (R011).

## Success criteria

1. Clicking a Safari/Finder button works via an AX element with `role`,
   coordinates byte-identical between indicator and click.
2. With Chrome on a secondary monitor: focus_app → screen_query returns
   that monitor's elements and a click lands there.
3. A stored session reopened from the overlay continues appending to the
   same session row.
4. Cooldown/auto-dismiss changes persist, apply without restart, and the
   history list shows real nudges only.
5. `make evals` runs green deterministically; a deliberately-broken prompt
   (e.g. removing the recall paragraph) flips at least one eval red.
