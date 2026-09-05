# Changelog

All notable changes are recorded here. The project follows a spec-driven workflow; the specs under `specs/` are the long-form record.

## Unreleased

### Added
- System tools: `open`, `wait_for_text`, `ui_action`, `browser`, `text_selection`, `find_files`, `processes`, `mac`.
- Live eval harness (`make evals-live`), always-on log file, copyable run reports, lane health.
- Teach Me mode; token spend per answer and per session.

### Changed
- `screen_query` reads the focused app's window by default (whole screen on request).
- Every navigation reuses one browser tab; `focus_app` settles and restores windows.

### Fixed
- Newlines in typed text press a real Return; dangerous verbs always ask; loop breakers for repeated and re-phrased calls.

## 0.1.0 — 2026-08

First working release: overlay chat over a local model, screen reading, verified input control with approval gates, memory with redaction, MCP servers and skill packs, the coding agent with VS Code bridge, native integrations (CLI/TUI, Finder, paste, hotkey).
