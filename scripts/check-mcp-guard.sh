#!/usr/bin/env bash
# S03 structural proof (M007, R016): no production MCP tool-action path can reach
# a server bypassing the approval gate. This is the MCP-tool analogue of
# scripts/check-guard-mounts.sh, and it pins two invariants with a machine check
# instead of review discipline (MEM090/MEM110):
#
#   1. Single choke point — the rmcp `.call_tool(` invocation may appear in
#      production ONLY inside src-tauri/src/llm/mcp.rs (x1, McpExecutor::execute).
#      If a second production call site appears anywhere, an MCP tool action can
#      reach a server without ever consulting McpApprovalGate — the exact bypass
#      this slice exists to forbid.
#
#   2. Guarded mount — the commands.rs chat-task mount pushes the McpExecutor
#      ONLY wrapped in McpApprovalGate (`executors.push(Box::new(McpApprovalGate::new(`),
#      and never pushes a raw McpExecutor. The count in (1) is only sound while
#      the executor behind that choke point is actually gated at its mount; this
#      positive/negative co-location lock notices if a refactor drops the wrap.
#
# Test code is exempt: everything from the first `#[cfg(test)]` in a file onward
# is stripped before matching, and src-tauri/tests/ is never scanned. Follows the
# negative-shell-assertion pattern: the check fails on ANY new production
# `.call_tool(` site, guarded or not — extending the allowlist is a deliberate
# review act, never an accident.
#
# Every run also executes a non-destructive negative self-test: it plants a rogue
# `.call_tool(` production file, asserts the core check exits non-zero, then
# removes it — proving the guard actually fires rather than trivially passing.
#
# Usage: bash scripts/check-mcp-guard.sh   (from the repo root)
set -uo pipefail

cd "$(dirname "$0")/.."

# Print `file:line` for every production occurrence of fixed string $1 under
# src-tauri/src. Fixed-string match (awk index), so no regex escaping games and
# comments mentioning the token in prose do not match the invocation form.
prod_hits() {
  find src-tauri/src -name '*.rs' -print0 | sort -z | xargs -0 awk -v pat="$1" '
    FNR == 1 { intest = 0 }
    index($0, "#[cfg(test)]") { intest = 1 }
    !intest && index($0, pat) { print FILENAME ":" FNR }
  '
}

# Reduce file:line hits to sorted "file xN" summary lines.
summarize() { cut -d: -f1 | sort | uniq -c | awk '{ print $2 " x" $1 }'; }

# The core structural assertions. Runs against whatever is currently on disk
# under src-tauri/src, so the self-test can re-run it with a rogue file planted.
# Uses a local `status`; the global fail() sees it via bash dynamic scoping.
check_tree() {
  local status=0
  fail() { echo "FAIL: $*" >&2; status=1; }

  echo "== phase 1/2: .call_tool( appears only at the single mcp.rs choke point =="
  local hits actual
  hits=$(prod_hits ".call_tool(")
  actual=$(printf '%s' "$hits" | summarize)
  local expected="src-tauri/src/llm/mcp.rs x1"
  if [[ "$actual" != "$expected" ]]; then
    fail ".call_tool( production sites drifted from the single choke point.
expected:
$expected
actual:
${actual:-<none>}
hits:
${hits:-<none>}
Every production MCP tool action must flow through McpExecutor::execute in
src-tauri/src/llm/mcp.rs, which is gated by McpApprovalGate at its mount. A new
.call_tool( site can reach a server bypassing the gate — route it through the
guarded executor instead of adding a second wire call."
  fi

  echo "== phase 2/2: the commands.rs mount wraps McpExecutor in McpApprovalGate =="
  # Positive co-location lock: the pushed executor is the gate, on one line.
  local gated raw
  gated=$(prod_hits "executors.push(Box::new(McpApprovalGate::new(" \
    | grep -c "^src-tauri/src/llm/commands.rs:" || true)
  [[ "$gated" -ge 1 ]] ||
    fail "commands.rs no longer pushes the MCP executor wrapped in McpApprovalGate
(need executors.push(Box::new(McpApprovalGate::new(...))) at the chat-task mount).
Without the wrap, the mcp.rs choke point is reachable unguarded at runtime."
  # Negative co-location lock: a raw McpExecutor must never be pushed unguarded.
  raw=$(prod_hits "executors.push(Box::new(McpExecutor" | grep -c ":" || true)
  [[ "$raw" -eq 0 ]] ||
    fail "commands.rs (or another mount) pushes a RAW McpExecutor without the
McpApprovalGate wrap — this bypasses the approval gate. Wrap it in
McpApprovalGate::new(...) before pushing."

  return $status
}

# ── Real tree check ──────────────────────────────────────────────────────────
if ! check_tree; then
  echo "== check-mcp-guard: FAIL ==" >&2
  exit 1
fi

# ── Negative self-test ───────────────────────────────────────────────────────
# Prove the check FIRES: plant a rogue production .call_tool( site, re-run the
# core check in a subshell, and assert it fails. Non-destructive — the rogue
# file is git-untracked and removed unconditionally via trap.
echo "== self-test: a rogue .call_tool( site must make the check fail =="
rogue="src-tauri/src/__mcp_guard_selftest_rogue.rs"
cleanup() { rm -f "$rogue"; }
trap cleanup EXIT
cat > "$rogue" <<'ROGUE'
// TEMPORARY self-test fixture for check-mcp-guard.sh — must never be committed.
// Simulates a rogue MCP tool-action path that reaches a server bypassing the gate.
fn __rogue_bypass(peer: &Peer) {
    let _ = peer.call_tool(params);
}
ROGUE
if check_tree >/dev/null 2>&1; then
  echo "FAIL: self-test rogue .call_tool( site did NOT trip the check — the guard is not actually enforcing." >&2
  echo "== check-mcp-guard: FAIL (self-test) ==" >&2
  exit 1
fi
cleanup
trap - EXIT
echo "== self-test: OK — rogue site correctly rejected =="

echo "== check-mcp-guard: PASS — single guarded MCP call_tool choke point, no bypass site =="
