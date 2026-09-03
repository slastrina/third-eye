# Spec: review follow-ups — observability, live evals, lane health, dangerous verbs

Date: 2026-09-03. From the 2026-09-03 review; the user chose items 1, 3, 5, 6.

## Objective

Make Third Eye diagnosable without a Claude session, measurable against the
live model, honest about broken model pins, and safe to leave in auto-run.

## Items

### 6. Dangerous verbs always ask (safety)

- `command_runner::dangerous_command(cmd) -> Option<&'static str>` — pure.
  Splits on `;`, `&&`, `||`, `|`, newlines; strips env assignments and
  wrappers (`sudo`, `env`, `nohup`, `time`, `exec`, `command`); flags the first
  token of any segment in the DANGEROUS set (`sudo rm kill pkill killall
  shutdown reboot halt dd diskutil launchctl chmod chown chflags osascript
  crontab defaults security tccutil csrutil spctl nvram mkfs*`), `git
  push|reset|clean|checkout --`, `curl`/`wget` with upload/data/method
  flags, and pipe-to-shell (`| sh|bash|zsh`).
- Structural: a dangerous command ALWAYS prompts — in `run_command` it
  ignores the persistent allowlist and the session RunCommand grant; in
  `run_in_workspace` it ignores tmp, the kind grant, and directory grants.
  The prompt summary is prefixed `⚠ <reason>:`. "Always allow" on a
  dangerous command never persists (persist_always_grant refuses).
- Settings → allowlist entries whose verb is dangerous show ⚠ "always asks".
- Acceptance: unit tests for the classifier and both tools' bypass paths;
  eval pin that `kill`, `rm -rf`, `curl -d`, `x | sh` prompt with allowlist +
  kind grant + tmp cwd present.

### 5. Lane health (model pins validated)

- `llm::lane_health` IPC → per lane `{lane, model, state: loaded|not-loaded|
  missing|unknown, toolUse: bool|null, warning}` from
  `lmstudio::model_rows` (loopback only; cloud lanes report `unknown`).
- Checked at boot (after settings load), on `settings://models` change, and
  on a 60s tick while the overlay is visible; broadcast `llm://lane-health`.
- Footer lane pill turns red with a title naming the problem; Settings →
  Models shows the same line per lane. No auto-repin.
- Acceptance: parser/classifier unit tests (row states → health), e2e that a
  broadcast turns the pill red and the title names the model.

### 1. Observability

- Always-on log file `~/Library/Logs/Third Eye/third-eye.log`, INFO level
  (RUST_LOG overrides), size-rotated at 5 MB keeping `.1` — implemented as a
  small `Write` target for env_logger; stderr kept in debug builds and when
  THIRD_EYE_LOG_FILE is set (that path wins).
- Run trace: `llm::trace::RunTrace` per chat request — ask, lane, model,
  every tool call (name, args ≤ 500 chars, ok/kind, ms, `verified` block),
  rounds, usage, end reason. Last 20 kept in `RunTraces` state.
- `run_report(request_id) -> String` IPC renders markdown; the transcript
  gets "Copy run report" on each finished assistant message (request id
  rides DoneEvent). Settings → Status shows the log path with "Reveal".
- Acceptance: rotation unit test, trace rendering unit test, e2e for the
  copy button, boot log file exists after a timed boot.

### 3. Live eval harness

- `tests/evals_live.rs`, all `#[ignore]`, run via `make evals-live`
  (`cargo test --test evals_live -- --ignored --test-threads=1 --nocapture`).
- Live LM Studio + DETERMINISTIC stub backends (the evals.rs stubs): scores
  the model's decisions, never touches the real desktop.
- ~10 scenarios × N=3 runs: ebay search, terminal command (newline/return),
  recall ("what's my name"), open-then-refine (no second open), teach-mode
  search (cmd+l, no web_search), pi script (no repeated call), workspace
  write (asks dir), honest refusal (disabled input), grounded click, read
  before open.
- Each scenario scores structural predicates (tool order/args, no
  `repeated-call`, one open, final answer non-empty). Prints a table:
  scenario | pass/N | ms avg | tokens; exit non-zero below the threshold.
- Acceptance: harness compiles in CI (ignored), runs green ≥ 8/10 on the
  pinned 9B locally.

## Boundaries

- Always: full battery per commit; no Co-Authored-By; artifacts in
  ~/Desktop/third_eye_test_dir.
- Ask first: changing default run modes; auto-repinning models.
- Never: silently widen what auto-run may execute.
