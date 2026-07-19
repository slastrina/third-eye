#!/usr/bin/env bash
# S02 structural proof (M003, R016): no production call path can construct an
# unguarded LLM client or embedder. `OpenAiClient::new` / `OpenAiEmbedder::new`
# may appear ONLY at the allowlisted guarded construction choke points:
#
#   src-tauri/src/llm/router.rs    x4 — thin_heavy (2) + set_lane_model (2),
#                                       every hit wrapped in GuardedClient
#   src-tauri/src/llm/commands.rs  x1 — the /v1/models probe client (GET-only,
#                                       carries no user content; allowlisted
#                                       by the S02 slice plan)
#   src-tauri/src/cloud/client.rs  x1 — the M004 S03 guarded cloud-client
#                                       construction choke point, wrapped in
#                                       GuardedClient at an External endpoint
#   src-tauri/src/memory/mod.rs    x1 — the single embedder site, wrapped in
#                                       GuardedEmbedder
#
# Test code is exempt: everything from the first `#[cfg(test)]` in a file
# onward is stripped before matching, and src-tauri/tests/ is never scanned.
# Follows the M002 negative-shell-assertion pattern: the check fails on ANY
# new construction site, guarded or not — extending the allowlist is a
# deliberate review act, never an accident.
#
# Usage: bash scripts/check-guard-mounts.sh   (from the repo root)
set -euo pipefail

cd "$(dirname "$0")/.."

status=0
fail() { echo "FAIL: $*" >&2; status=1; }

# Print `file:line` for every production occurrence of fixed string $1 under
# src-tauri/src. Fixed-string match (awk index), so no regex escaping games.
prod_hits() {
  find src-tauri/src -name '*.rs' -print0 | sort -z | xargs -0 awk -v pat="$1" '
    FNR == 1 { intest = 0 }
    index($0, "#[cfg(test)]") { intest = 1 }
    !intest && index($0, pat) { print FILENAME ":" FNR }
  '
}

# Reduce file:line hits to sorted "file xN" summary lines.
summarize() { cut -d: -f1 | sort | uniq -c | awk '{ print $2 " x" $1 }'; }

check_allowlist() { # $1 = pattern, $2 = expected summary (newline-separated)
  local pattern=$1 expected=$2 hits actual
  hits=$(prod_hits "$pattern")
  actual=$(printf '%s' "$hits" | summarize)
  if [[ "$actual" != "$expected" ]]; then
    fail "$pattern construction sites drifted from the allowlist.
expected:
$expected
actual:
${actual:-<none>}
hits:
${hits:-<none>}
Every production LLM/embedding client must be built inside a guard wrap —
if a hit is new, mount it through GuardedClient/GuardedEmbedder and only
then extend the allowlist above."
  fi
}

echo "== phase 1/3: OpenAiClient::new appears only at guarded choke points =="
check_allowlist "OpenAiClient::new" \
"src-tauri/src/cloud/client.rs x1
src-tauri/src/llm/commands.rs x1
src-tauri/src/llm/router.rs x4"

echo "== phase 2/3: OpenAiEmbedder::new appears only at the guarded embedder site =="
check_allowlist "OpenAiEmbedder::new" \
"src-tauri/src/memory/mod.rs x1"

echo "== phase 3/3: the guard wraps still exist at the allowlisted sites =="
# Positive co-location locks: the allowlist above is only sound while the
# named files still perform the wrap. If a refactor drops the wrapper, the
# counts alone would not notice — these do.
[[ $(prod_hits "GuardedClient::new" | grep -c "^src-tauri/src/llm/router.rs:") -ge 2 ]] ||
  fail "router.rs no longer wraps lane clients in GuardedClient::new (need one wrap in thin_heavy and one in set_lane_model)"
[[ $(prod_hits "GuardedClient::new" | grep -c "^src-tauri/src/cloud/client.rs:") -ge 1 ]] ||
  fail "cloud/client.rs no longer wraps the cloud client in GuardedClient::new"
[[ $(prod_hits "GuardedEmbedder::new" | grep -c "^src-tauri/src/memory/mod.rs:") -ge 1 ]] ||
  fail "memory/mod.rs no longer wraps the embedder in GuardedEmbedder::new"

if [[ $status -ne 0 ]]; then
  echo "== check-guard-mounts: FAIL ==" >&2
  exit 1
fi
echo "== check-guard-mounts: PASS — every production construction site is guarded or allowlisted =="
