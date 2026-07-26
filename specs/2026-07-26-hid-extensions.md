# Spec: HID vocabulary extensions — drag, scroll, multi-click, combos, clipboard, wait

Status: requested 2026-07-26 ("extend its hid input support for like select
and drag and other things"); implemented immediately.

## Objective
The model can only move/click/type/single-key today — half of real computer
use is unreachable (selection, drag-and-drop, scrolling, shortcuts,
reliable long-text entry). Extend `input_action` and add two small tools.

## New vocabulary
1. `mouse-drag {button, fromX, fromY, toX, toY}` — press at from, glide
   (interpolated moves so apps register motion), release at to. Selection,
   drag-and-drop, sliders. Grounding: BOTH endpoints must come from
   screen_query (same rule as clicks). New `ActionKind::MouseDrag`.
2. `mouse-click` gains `clicks: 1|2|3` — double-click opens/selects a word,
   triple selects a line. Same MouseClick kind (payload variance).
3. `scroll {x?, y?, deltaX?, deltaY?}` — optional aim first, then wheel
   lines (positive deltaY scrolls content down). New `ActionKind::Scroll`;
   an aimed scroll needs grounding, a coordless one does not.
4. `key-press` gains `modifiers: ["cmd"|"ctrl"|"alt"|"shift"]` — Cmd+C,
   Cmd+A, Cmd+T… pressed before, released after, reverse order.
5. `clipboard {op: "read" | "write", text?}` tool — NSPasteboard, no new
   deps. Write+Cmd+V replaces slow char-typing; copy+read extracts what the
   model selected. Gated on the SAME HidRunMode path with a new
   `ActionKind::Clipboard` (read is user data; prompt names the op).
6. `wait {ms ≤ 3000}` tool — ungated; lets UIs settle instead of churning
   verification failures.

## Unchanged invariants
Off stays structurally inert; Ask prompts per kind with session grants;
coordinates only from screen_query; verified evidence blocks on every
action (drag verifies the cursor at the destination); prompts/HUD labels
name the exact action. Wire compat: new fields are serde-defaulted, old
payloads parse unchanged.
