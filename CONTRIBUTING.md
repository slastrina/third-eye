# Contributing to Third Eye

Thanks for helping. Third Eye is a small, opinionated codebase; the notes below keep it that way.

## Ground rules

1. **Structure over prose.** A behaviour that matters is enforced in code (a gate, a typed refusal, injected context) and pinned by a test — never only by a sentence in the system prompt. If you change a prompt, run `make evals-live` before and after and paste the tables in the PR.
2. **Every tool refuses typed and reports what the OS observed.** New tools follow the pattern in `src-tauri/src/llm/tools/`: a seam trait (so tests need no OS), typed failure kinds, a `verified` readback where one exists, an approval gate for mutations, HUD labels, and a live probe under `src-tauri/examples/` for anything OS-facing.
3. **No fake data in the UI.** Empty states are empty; degraded states are named.
4. **One commit, one change, full battery green** (below).

## The battery

```sh
cd src-tauri && cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
npm test && npx tsc --noEmit
npx playwright test | grep -E "[0-9]+ (failed|passed)"   # never tail -N — it hides the failed line
make evals
```

Plus a five-second timed boot of the debug binary when you touch state registration (`manage(...)` / `state::<T>()` mismatches panic at boot, not at compile time).

## Workflow

- Open an issue first for anything bigger than a fix; the `specs/` directory shows how features are designed here (objective, contracts, slices, acceptance).
- Branch from `main`, keep PRs focused, describe *what you verified* (tests, probes, live runs) in the PR body.
- Commit messages: `type: summary` (`feat`, `fix`, `perf`, `docs`, `build`, `chore`), with a body that says what broke and why the change is the right one.
- CI runs the Rust and TypeScript batteries on macOS. Playwright and the live evals run locally.

## Reporting bugs

Attach a **run report**: every finished answer in the overlay has a *copy run report* link that puts a markdown table of the run (each tool call, its result kind, timing, and the OS readback) on your clipboard. The always-on log is at `~/Library/Logs/Third Eye/third-eye.log` (Settings → Status → Reveal).

## Code of conduct

Be kind and direct. See [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md).

## License

By contributing you agree that your contributions are licensed under the [MIT License](LICENSE).
