# Security

Third Eye can move the mouse, type, run commands, and read the screen. Bugs in its gates are security bugs.

## Reporting

Please **do not** open a public issue for a vulnerability. Use GitHub's private vulnerability reporting on this repository (Security → Report a vulnerability). You will get an acknowledgement within a few days.

In scope, for example:

- a way to run a command, write a file, click, or navigate without the approval gate or grounding the design promises;
- a way for model output or page content to reach the webview as executable HTML/JS;
- secrets or screen content persisted un-redacted;
- a non-local endpoint reachable while cloud is opted out.

## Design notes for reviewers

- Run modes and gates: `src-tauri/src/input/commands.rs`, `src-tauri/src/llm/toolloop.rs` (`ApprovalGate`, `UrlGroundingExecutor`), `src-tauri/src/command_runner/`.
- Dangerous verbs: `command_runner::dangerous_command` — always prompts, never persists.
- Redaction: `src-tauri/src/privacy/`. Endpoint guard: `src-tauri/src/llm/guard.rs`.
- Evals that pin the gates: `src-tauri/tests/evals.rs`.
