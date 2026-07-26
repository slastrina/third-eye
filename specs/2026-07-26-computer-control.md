# Spec: Computer control — inventory, commands, chat sessions

Status: approved verbally 2026-07-26 ("plan all of this and then start work
on them"); implementation follows immediately per phase.

## Objective

Third Eye can act on this machine like an informed operator: it **knows what
is installed** (GUI apps and terminal tools, cached and periodically
refreshed), can **run terminal commands** for the user — visibly, gated, and
logged — and can **open/launch apps**. Simple machine questions ("what time
is it", "what's my IP") resolve through immediate read-only commands rather
than screen-driving. Conversations are **persisted as sessions**: the user
can start a fresh chat, browse past sessions, and the assistant recalls
prior-session facts (via the existing chat-memory distillation).

## Existing architecture this builds on (verified)

- Tool loop + composite executor (`llm/toolloop.rs`): tools advertise
  unconditionally; every call/result already broadcasts app-wide
  (`llm://tool-call` / `llm://tool-result`) — chat and HUD render them.
- HID approval plumbing (`ApprovalGate`, `hid://approval-request`,
  `respond_hid_approval`, session whitelist keyed by `ActionKind`,
  `HidRunMode` plan/auto): the same UX gates command execution.
- `focus_app` already launches-or-focuses GUI apps with verified success —
  "open apps" needs no new actuator, only discoverability (inventory).
- Chat-memory distillation (`memory/chat_ingest.rs`) already lands
  chat-derived memories in the store — cross-session **recall** exists; this
  milestone adds raw transcript persistence and session navigation.
- The memory store (`memory/store.rs`) is one SQLite file behind a mutexed
  connection; session tables join it (one file, one lock, one backup story).

## Phase I1 — Machine inventory

- New `src-tauri/src/inventory/`: scanner + cache + IPC + tool.
- **Discovery (macOS):** GUI apps = `*.app` bundles at depth ≤2 under
  `/Applications`, `~/Applications`, `/System/Applications`; name = bundle
  stem. CLI tools = executable regular files in each `$PATH` dir, deduped
  first-wins in PATH order. Off-macOS: PATH scan only (typed, not stubbed).
- **Cache:** `inventory` table in the existing memory.db (name, path, kind
  `app|cli`, refreshed_at_ms). Refresh = atomic wipe+refill in one
  transaction. Non-fatal on error (log + keep stale cache).
- **Refresh policy:** once at startup (async, off the main thread), every
  24 h from the same spawned loop, and on the `refresh_inventory` IPC.
- **IPC:** `inventory_status` → `{apps, tools, lastRefreshMs}` (health-as-
  value); `inventory_search(query, limit)` → matches (name substring,
  case-insensitive, apps ranked before tools).
- **LLM tool `find_programs`:** `{query}` → matching entries. Tool
  description directs the model to check before claiming something is or
  is not installed, and to pair with `focus_app` (GUI) / `run_command`
  (CLI).

## Phase I2 — run_command tool

- **Execution:** `/bin/sh -lc <command>`, cwd = user home, no stdin, hard
  timeout (15 s default, tool arg up to 60 s), stdout+stderr captured and
  truncated (16 KB each) with truncation marked. Result carries exit code,
  duration, both streams — the model sees exactly what ran.
- **Gate (D038 posture):** a persisted `commandsEnabled` setting, default
  OFF, in Settings → Automation. Disabled ⇒ the tool refuses with the typed
  `disabled` kind (structural inertness — the HID precedent). When enabled,
  every call flows through the SAME approval plumbing as HID actions: a new
  `RunCommand` action kind, approval prompt showing the exact command
  string, session whitelist honored, `HidRunMode` plan/auto semantics
  unchanged. The Esc kill-switch / Stop path already covers in-flight runs
  (cooperative stop between actions; a running command still honors its own
  timeout).
- **Visibility:** the existing `llm://tool-call`/`tool-result` broadcasts
  carry the command and outcome; chat renders run_command results as a
  monospace terminal block (command, exit code, output preview); the HUD
  trail labels it `run · <command…>`. Nothing executes silently.
- **Quick lookups:** the tool-loop system prompt gains: for simple machine
  facts (time, IP, hostname, disk, battery) prefer one short read-only
  `run_command` over screen-driving; examples included in the tool
  description. No separate "lookup" tool — same gate, same visibility.

## Phase I3 — Chat sessions

- **Store:** `chat_sessions(id, started_at_ms, last_at_ms)` and
  `chat_session_messages(id, session_id, role user|assistant, text, at_ms)`
  as MemoryStore methods (same file/lock). No embeddings here — recall is
  the distillation pipeline's job (already shipped).
- **Write path:** the same completed-exchange hook that feeds chat_ingest
  appends the user+assistant pair under the current session (managed
  CurrentSession id; lazily created on first exchange).
- **IPC:** `chat_new_session` (starts fresh; returns the new id),
  `chat_sessions(limit)` → newest-first `{id, startedAtMs, lastAtMs,
  title (first user line, truncated), messageCount}`,
  `chat_session_messages(id)` → ordered transcript.
- **UI:** overlay palette gains a New-chat control (clears the reducer +
  `chat_new_session`); the Memory window gains a **Chats** tab — sessions
  list → read-only transcript view. Recall needs no UI: distilled chat
  memories already surface in Recall/search.

## Boundaries

- Always: commandsEnabled defaults OFF; every command approval names the
  exact command; truncation is marked, never silent; `make check && test`
  before each commit; no co-author trailers.
- Never: bypass the approval gate for any executing surface; auto-enable
  commands during onboarding; persist command output into the memory store
  (transcripts hold it; the store stays distilled-text-only).

## Success criteria

1. `inventory_status` reports real counts after startup; `find_programs`
   answers "is X installed" truthfully from cache; `refresh_inventory`
   re-scans on demand; the 24 h loop refreshes unattended.
2. With commands OFF, run_command refuses typed-disabled; ON + plan mode
   prompts with the exact command; approved commands stream visibly into
   chat + HUD; timeout and truncation both surface.
3. "what time is it" / "what's my IP" style asks resolve via one read-only
   command when the user has commands enabled.
4. New chat starts an empty transcript; past sessions are listed and
   readable; facts from old sessions surface via existing recall.
5. Full gates green (unit, e2e, Rust, clippy, fmt).

## Close-out (2026-07-26)

All three phases shipped (`c8cc359`, `1106d9c`, `d35cf10`), plus the four
redesign follow-ups beforehand (retention enforcement, multi-monitor HUD,
tray outside-click dismiss, keychain-test serialization). Notes:

- Transcript logging shares the `chatMemoryEnabled` gate: off means nothing
  chat-derived is stored, raw or distilled (one privacy switch, no surprise
  logging).
- run_command has NO auto-run: `HidRunMode::Auto` does not extend to
  commands; only an explicit per-session grant skips the prompt.
- Deferred: a Settings surface listing the inventory (IPC exists —
  inventory_status/inventory_search); per-command allowlists/denylists;
  Windows/Linux GUI-app discovery (PATH-only there today); searching across
  stored transcripts (recall covers the distilled layer).
- Pre-existing environmental flake: cloud keystore tests hit the real
  macOS keychain; under heavy parallel load the OS service occasionally
  refuses ("No default store") — serialized now, passes in isolation.
