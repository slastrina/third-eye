# Spec: Third Eye as a coding agent — coder lane, workspace tools, VS Code

Date: 2026-08-01. User decisions locked: the coder runs on Third Eye's
OWN tool loop (never delegated to external agent CLIs); execution gets a
NEW workspace-scoped exec tool; VS Code integration is a FULL extension
(its own slice, not CLI shelling).

## Objective

Third Eye orchestrates real coding work: the user asks for code in the
overlay, a dedicated coder model plans and edits files inside an
explicitly-designated workspace, compiles and runs the result, and the
user watches it happen — live in VS Code with diffs and debug control,
and in the transcript's steps/terminal blocks. Local-first
(qwen3-coder-next is already served); Claude/OpenAI slot in as coder
providers through the existing cloud opt-in when the user chooses.

Success looks like: "add a --json flag to my CLI in ~/code/mytool" →
coder lane routes automatically → files change (visible as they change in
VS Code), `cargo build && cargo test` runs in the workspace with output
in the transcript, the diff is reviewable in VS Code, nothing was touched
outside the workspace, and nothing was committed.

## Tech stack

- Backend: existing Rust/Tauri stack — `ModelRouter` gains a CODER lane;
  tools ride the existing `ToolExecutor` registry/toggles/approvals/evals.
- Coder models: local `qwen3-coder-next` (LM Studio) by default; cloud
  Claude/OpenAI via the existing keystore + opt-in + lane rerouting
  (the heavy-lane cloud pattern, applied to coder).
- VS Code extension: TypeScript, `vscode` API, packaged as `.vsix` in
  `vscode-extension/`; talks to Third Eye over a loopback-only,
  token-authenticated WebSocket bridge served by the Rust backend
  (R016 posture: never binds beyond 127.0.0.1; the token is generated
  per-boot and handed to the extension via a file in app-data).
- Diffs: git is the mechanism — workspaces are expected to be repos;
  Third Eye NEVER commits/pushes unless explicitly asked, never with a
  co-author trailer.

## Commands

Unchanged battery: `cargo test` / `clippy -D warnings` / `fmt`,
`npm test`, `npx playwright test` (grep the failed|passed line),
`make evals`, `make install-app`. New: `make vsix` (build the extension),
extension tests via `npm test` inside `vscode-extension/`.

## Project structure (new/changed)

```
src-tauri/src/llm/router.rs      → CODER_LANE joins thin/heavy
src-tauri/src/llm/routing.rs     → NEW: auto lane selection (pure + tests)
src-tauri/src/workspace/mod.rs   → NEW: workspace roots (Settings-managed,
                                   persisted), path containment checks
src-tauri/src/workspace/fs_tools.rs   → read_file / write_file / list_dir
src-tauri/src/workspace/exec_tool.rs  → run_in_workspace (build/run/test)
src-tauri/src/bridge/mod.rs      → NEW: loopback WS bridge for VS Code
vscode-extension/                → NEW: the extension (TS, own package.json)
src/…                            → Settings workspace pane; transcript
                                   diff/steps surfacing
specs/2026-08-01-coding-agent.md → this file
```

## Code style

Existing conventions verbatim — pure decision fns + thin glue, typed
kind-tagged errors, health-as-value IPC, structural gates (D038). Example
of the shape every new tool follows:

```rust
/// Write one file INSIDE a designated workspace root. Refuses typed
/// (`outside-workspace`) before any io when the resolved path escapes —
/// containment is checked on the CANONICAL path, so `../` and symlinks
/// cannot escape.
async fn execute(&self, call: &ToolCall) -> ToolOutcome { … }
```

## Slices

- **S1 — Coder lane + auto-routing.** `CODER_LANE` (default UNPINNED —
  probing the endpoint at boot was not worth the complexity; pin
  qwen3-coder-next in Settings → Models); footer chips become
  THIN/HEAVY/CODER overrides over a new default AUTO mode; pure
  `select_lane(request, history) -> Lane` heuristics (task-shaped → heavy,
  code-shaped → coder, chat → thin), sticky per conversation until ＋New;
  the footer always shows the real routed lane (auto → coder).
- **S2 — Workspaces.** Settings pane to add/remove workspace roots
  (persisted); `workspace` module with canonical-path containment; the
  no-roots state is structurally inert (fs tools not offered).
- **S3 — File tools.** `read_file`/`list_dir` (read-only, ungated beyond
  toggles), `write_file` (approval-gated: Allow once / session / Always
  per WriteFile kind; every write logged in steps; size caps). Registry +
  HUD labels + evals: containment refusal, write-approval flow, honest
  read results.
- **S4 — Workspace exec.** `run_in_workspace`: cwd locked to a root,
  configurable timeout up to 10 min, output streamed into the transcript
  terminal block, its own ActionKind + per-workspace session grants;
  killable by Esc/Stop (process group kill — a stuck build must die with
  the run).
- **S5 — Diffs in the loop.** After edits: `git diff` surfaced in the
  transcript (collapsible, syntax-highlighted) and pushed over the bridge;
  the coder is prompted to review its own diff before declaring done
  (evaluate-the-goal, coding flavor).
- **S6 — Cloud coder.** Provider picker for the coder lane (local /
  Claude / OpenAI) re-using keystore + opt-in + guard; prompt-contract
  evals run against whichever is pinned in live mode.
- **S7 — VS Code extension.** `vscode-extension/`: connects to the
  loopback bridge; live visibility (files open/reveal as the agent edits,
  inline "Third Eye edited this" decorations), diff view (virtual-doc
  before/after per edited file + workspace diff), run status bar, and
  debug control (launch configured debug sessions on agent request, with
  user approval). Packaged .vsix + `make vsix`; auto-detected and offered
  from Settings when VS Code is installed.
- **S8 — Coding evals.** Deterministic: containment, approval, exec
  timeout, diff-before-done, no-commit rule. Live (ignored): a real small
  task against qwen3-coder-next end-to-end in a scratch repo.

## Testing strategy

Per slice, the standing battery plus: pure tests for `select_lane`,
containment, timeout policy; integration evals driving the real loop with
scripted models over a temp git workspace; extension unit tests for the
bridge protocol; one live ignored end-to-end (scratch repo, real coder
model). Every prompt clause pinned in the prompt-contract eval.

## Boundaries

- **Always:** canonical-path containment before ANY fs io; approval on
  every write (until the user grants Always); process-group kill on stop;
  full battery per commit; spec updated when decisions change.
- **Ask first:** adding runtime deps (WS server crate for the bridge,
  syntax highlighter), running the first live end-to-end eval, anything
  that would touch files outside a designated workspace.
- **Never:** commit/push without an explicit ask (and never a co-author
  trailer); bind the bridge beyond loopback; delete/overwrite files
  outside a workspace root; store file contents in memory/db (R011
  applies — code stays on disk).

## Success criteria

1. Auto mode routes: a chat question → thin; "open ebay and…" → heavy;
   "write a function that…" → coder; footer shows the truth; chips
   override; sticky until ＋New. (Pure tests + evals.)
2. With a workspace configured, the coder can read/list/write inside it;
   a `../` or symlink escape is refused typed with zero io. (Evals.)
3. `run_in_workspace` builds a real project with streamed output; Esc
   kills the process group within 2s. (Live-ignored + deterministic.)
4. After an edit task, the transcript shows the diff and the final answer
   summarizes what changed and whether build/tests passed; nothing was
   committed. (Eval + live.)
5. Claude/OpenAI selectable as the coder; guard blocks any non-loopback
   call when cloud is off. (Existing guard tests extended.)
6. The extension, connected: files visibly open/change during an agent
   edit; a per-file diff and workspace diff are one click; a debug session
   can be launched on request after approval. (Extension tests + manual
   UAT script.)

## Decisions made during implementation

- S1–S6 + S8-deterministic shipped (edc3360…e6b5db6). S7 (extension +
  bridge) is pending the transport decision below; the live S8 eval
  exists as `#[ignore]` (`live_eval_coding_end_to_end`) and has not been
  run (ask-first boundary).
- S3: write cap 1 MB, read cap 24k chars (truncation marked), binary
  reads refused typed. Fixed a latent S2 bug: an existing path resolved
  with a trailing slash (`join("")` → ENOTDIR).
- S4: process-group kill uses `process_group(0)` + `/bin/kill -9 -- -pgid`
  — no libc dep. Session grants are per canonical root
  (`WorkspaceState::exec_grants`); a persisted Always covers the kind.
  Live output streams over `llm://terminal-chunk` into the terminal block.
- S5: diff coloring is dependency-free (+/−/@@ prefix → CSS classes on
  design tokens) — no syntax-highlighter dep needed. "Pushed over the
  bridge" waits for S7.
- S6: `apply_cloud_routing` routes heavy AND coder lanes independently,
  each from its own persisted provider selection; config persistence is
  lane-keyed (`cloudHeavyProvider` / `cloudCoderProvider`).

- S7 (shipped): transport is `tokio-tungstenite` (user-approved dep,
  2026-08-02). The bridge is a pure FORWARDER of the coding subset of
  existing app events (tool-call/result for write_file, run_in_workspace,
  workspace_diff; terminal chunks; run state) — no new emit sites, and
  screen/memory/chat content structurally never reaches the socket.
  Discovery via `bridge.json` (port + per-boot 32-byte urandom token,
  0600) in app-data; auth is the client's first frame on a 5s deadline.
  Debug control is a `vscode_debug` tool that only DELIVERS a request;
  the user approves inside VS Code (no app-side approval plumbing).
  Extension: `vscode-extension/` TS, `ws` client, protocol unit-tested
  with node:test; `make vsix` packages the side-loadable .vsix.

## Open questions

1. Bridge transport — RESOLVED: `tokio-tungstenite` (user-approved).
2. Extension marketplace publishing vs .vsix side-load only — v1 assumes
   side-load via `code --install-extension`.
3. Max write size / binary-file policy for `write_file` — RESOLVED: text
   only, 1 MB cap, refuse binaries typed (S3 as shipped).
