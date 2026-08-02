# Spec: Native OS integrations — CLI/TUI, Finder, image paste, settings controls

Date: 2026-08-02. User request: "shell command like `thirdeye .`", a Rust
TUI, a Finder entry point that opens Third Eye with that directory as the
workspace, image pasting into the overlay (opencode/Claude Code style),
Settings-managed install/remove of the optional integrations, and working
controls for the hotkey + launch-at-login rows that are currently
status-only.

## Objective

Third Eye becomes reachable from where the user already is: a terminal
(`thirdeye .` targets the CWD; `thirdeye ask` one-shots a question;
`thirdeye tui` is a terminal chat), Finder (right-click a folder →
Quick Action → work here), and the clipboard (paste an image straight
into the prompt — the lane model does vision when it supports it). All
optional pieces are installed AND removed from Settings → Integrations;
nothing lands on the system without an explicit click there.

## Tech stack

- CLI/TUI: Rust binary `thirdeye` (new `cli/` crate in a cargo workspace
  with src-tauri). TUI via `ratatui` + `crossterm` (user-requested);
  bridge client via `tungstenite` (sync, small) reusing the S7 loopback
  bridge with its per-boot token — the CLI is just another authenticated
  bridge client. NO new server surface: bridge protocol grows a v2
  inbound command set.
- Finder: a macOS Quick Action (`~/Library/Services/Work here with Third
  Eye.workflow`) generated and installed by the app — a `.workflow`
  bundle whose shell step runs the installed `thirdeye` CLI on the
  selected folder. No Xcode, no extension signing.
- Image paste: frontend-only — the overlay input's paste handler feeds
  the EXISTING attachment pipeline (CapturedFrame → vision turn).
- Settings: Integrations pane (install/remove/status per integration);
  hotkey recorder + presets and autostart toggle wired to the EXISTING
  `set_hotkey` / `set_autostart` IPC.

## Slices

- **N1 — Image paste.** Overlay input paste handler: image clipboard item
  → downscale-if-huge → CapturedFrame → the existing attach chip/flow.
  Works alongside the screenshot button; non-image pastes unaffected.
- **N2 — Hotkey + autostart controls.** Status rows become controls:
  autostart checkbox (`set_autostart`), hotkey preset picker + free-text
  accelerator with apply/validation (`set_hotkey`, health-as-value
  errors surfaced). Both stay visible in Status.
- **N3 — Bridge protocol v2 (inbound commands).** After auth a client may
  send: `{"type":"add-workspace","path"}` (canonical-absolute, appended
  to workspace roots + persisted), `{"type":"show-overlay","prefill"?}`,
  `{"type":"ask","text"}` (submits a chat AS IF typed in the overlay —
  the run is visible there too; the asking client gets `token`/`done`/
  `error` frames forwarded for that request only). Version stays 1
  compatible for VS Code (additive).
- **N4 — `thirdeye` CLI + TUI.** `thirdeye [path]` → add-workspace +
  show-overlay; `thirdeye ask "…"` → streams the answer to stdout;
  `thirdeye tui` → ratatui chat (input line, scrollback, streaming,
  Esc/Ctrl-C quits). Reads `bridge.json` for port/token; clear error
  when the app is not running. Built as a cargo workspace member and
  bundled in the app's Resources.
- **N5 — Settings → Integrations.** New pane: CLI (symlink
  `/usr/local/bin/thirdeye` → bundled binary; fallback `~/.local/bin`
  with PATH hint) and Finder Quick Action — each with Installed/Not
  detection, Install and Remove buttons, and the exact paths shown.
  Remove deletes exactly what install created.
- **N6 — Docs + evals.** Bridge v2 protocol tests, CLI arg parsing
  tests, spec updates.

## Decisions made during implementation

- N3/N4: `ask` runs BACKEND-side (the bridge handler calls the chat
  pipeline directly) rather than round-tripping through the overlay
  webview — more robust (works with the overlay closed), and the webview
  event hop proved unreliable for bridge-initiated events. Consequence:
  a CLI-initiated exchange shows in the overlay as run activity (HUD,
  run state), not as transcript bubbles — the terminal is the
  conversation surface. `show-overlay` treats "already visible" as
  success.
- N4: the CLI is a second bin target in the src-tauri crate (no cargo
  workspace surgery); `make build-tauri` stages it into
  `src-tauri/binaries/` and the bundler ships it under
  Resources/binaries/. Deps ratatui/crossterm/tungstenite (sync).
- N5: Quick Action is a hand-authored Automator `.workflow`
  (plutil-linted in tests); CLI install is a symlink —
  /usr/local/bin first, ~/.local/bin fallback.

- 2026-08-02 (later, user-directed): CONTAINMENT REPLACED BY CONSENT.
  The coding tools now work anywhere: relative paths use the ACTIVE
  (first) working directory; none set → a native folder chooser pauses
  the run and the pick becomes the active directory. Writes/commands in
  un-blessed directories prompt per directory ("this session" blesses
  that subtree); tmp is always free. Reads are unrestricted (visible in
  the HUD trail). Bare `cd` refuses typed (no persistent shell). The
  overlay context row lists every directory explicitly with ✕ removal.
  Terminal-only runs no longer summon the mouse follower.

## Boundaries

- Always: bridge stays loopback + token-auth; installs/uninstalls touch
  ONLY the named paths and are idempotent; every failure surfaces as
  data in Settings.
- Ask first: anything requiring sudo/admin (never silently escalate).
- Never: install without an explicit Settings click; a Finder/CLI
  workspace add bypassing canonical-path checks; image bytes persisted
  to memory/db (attachments stay transient, R011).

## Success criteria

1. Paste a screenshot → chip appears → the model describes it (vision
   lane) or the request degrades exactly like the screenshot button.
2. Hotkey changed from Settings sticks across restart; conflict shows
   the error and keeps the old binding. Autostart toggles and persists.
3. `thirdeye .` in a terminal: the folder appears in Settings →
   Workspaces and the overlay opens. `thirdeye ask "2+2?"` prints the
   streamed answer. `thirdeye tui` holds a conversation.
4. Right-click a folder in Finder → Quick Action → overlay opens with
   that folder as a workspace root.
5. Settings → Integrations shows accurate installed-state and can
   remove everything it installed.
