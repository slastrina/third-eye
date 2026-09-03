# Spec: system tools — structure instead of pixels

Date: 2026-09-03. From the "what tools would better integrate with the
system" review; the user chose to build the whole set.

## Objective

Give the model deterministic, verifiable tools for the things it currently
improvises with pixels and shell idioms. Every tool here collapses a risky
freeform sequence into code (the `web_search` precedent), refuses typed,
carries a `verified` readback where the OS can answer, and is scoped to
the lanes that need it so the 9B's fixed token budget stays flat.

Doctrine: a tool earns its definition tokens by removing a class of
guessing. Prefer one discriminated tool (`action` enum, like
`input_action`) over many small ones.

## Tools

| tool | lanes | actions | backend seam |
|---|---|---|---|
| `ui_action` | thin, heavy (not teach) | `press`, `set_value`, `focus` on an AX element found by title/role in the focused app | `AxActions` (macOS: AXUIElement walk + AXPress / AXValue) |
| `browser` | thin, heavy (teach: read-only actions) | `tabs`, `switch`, `navigate`, `back`, `page_text`, `find`, `click`, `fill` | `BrowserBackend` (Chrome AppleScript + `execute javascript`; typed `javascript-disabled` with the fix when the View→Developer toggle is off) |
| `text_selection` | all | `get`, `replace`, `insert` in the focused app | `SelectionBackend` (AX AXSelectedText / range; clipboard fallback that restores the user's clipboard) |
| `wait_for_text` | thin, heavy | `{text, timeoutMs}` — poll window-scoped `screen_query` until the text appears; returns the element | existing `ScreenQuery` |
| `open` | all | `{url \| path \| app}` — URL through the one-tab browser module (same grounding as `run_command open`), path via `open`, app via `focus_app` | existing modules |
| `find_files` | all | `{query, kind?, in?, modifiedWithinDays?, limit?}` Spotlight | `FileSearch` (mdfind) |
| `processes` | heavy, coder | `list {sort, limit}`, `kill {pid \| name}` — kill is HID-class and ALWAYS asks (dangerous-verb parity) | `ProcessBackend` (ps / libproc) |
| `mac` | all | `notify {title, body}`, `speak {text}`, `system_info`, `run_shortcut {name, input?}`, `calendar_today`, `reminder_add {title, due?}`, `note_add {title, body}` | `MacServices` (osascript / shortcuts CLI / say / pmset+system_profiler) |

Teach mode strips `ui_action`, `browser` mutating actions, and
`text_selection.replace/insert` — the human way is visible keys and mouse.

## Contracts

- Every tool: typed refusals (`invalid-arguments`, `not-found`,
  `unsupported`, `permission-denied`, `approval-denied`), bounded output,
  a `verified` block where the OS can confirm (ui_action reads the element
  back; browser returns the tab's title+URL after every action;
  text_selection returns the new selection).
- HID-class (gated by run mode + approval): `ui_action`, `browser` mutating
  actions, `text_selection.replace/insert`, `processes.kill`, `open`.
- Prompt: one short paragraph per tool in the lane section that offers it;
  no new CORE prose. Descriptions ≤ 80 words.
- Action labels (HUD trail / transcript) for every new tool.

## Slices (one commit each, full battery)

- S1 `open` + `wait_for_text` (existing modules).
- S2 `ui_action` (AX actions seam + macOS impl + live probe example).
- S3 `browser` (Chrome seam + live probe example).
- S4 `text_selection`.
- S5 `find_files` + `processes`.
- S6 `mac`.
- S7 prompts + lane scoping + live-eval scenarios for the new tools.

## Acceptance

- Deterministic evals per tool over stub seams: offered in the right lanes,
  stripped in teach mode, refusals typed, grounding preserved for `open`.
- Live probes (`examples/*_probe.rs`) for ui_action and browser against
  the real OS, run once before shipping.
- `make evals-live` ≥ 80% with the new scenarios.
- Per-request fixed tokens on the browsing lane grow by ≤ 1.8k.

## Boundaries

- Always: full battery per commit; no Co-Authored-By; artifacts in
  ~/Desktop/third_eye_test_dir.
- Ask first: anything that needs a new TCC permission class at boot.
- Never: a mutating action without the approval gate; a tool that claims
  success without a readback.
