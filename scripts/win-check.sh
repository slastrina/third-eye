#!/usr/bin/env bash
# S01 (M007) cross-target proof: the Rust crate — now including the rmcp
# stdio MCP host code — still compiles cleanly for x86_64-pc-windows-msvc
# (R020). Unlike the Linux check this runs natively via rustup's Windows
# target on macOS: no container, but two host tools are hard requirements —
#   * the x86_64-pc-windows-msvc rustup target, and
#   * llvm-rc (Homebrew llvm, keg-only) for tauri-winres's resource step,
#     which also needs src-tauri/icons/icon.ico to exist.
#
# Usage: bash scripts/win-check.sh   (from the repo root)
set -euo pipefail

cd "$(dirname "$0")/.."

# gsd_exec / minimal shells strip ~/.cargo/bin; llvm is keg-only so its bin
# is never on PATH by default. Prepend both so cargo and llvm-rc resolve.
export PATH="$HOME/.cargo/bin:/opt/homebrew/opt/llvm/bin:$PATH"

TARGET="x86_64-pc-windows-msvc"

echo "== phase 1/3: toolchain preflight (rustup target + llvm-rc + icon.ico) =="
if ! rustup target list --installed | grep -qx "$TARGET"; then
  echo "FAIL: rustup target '$TARGET' not installed — run: rustup target add $TARGET" >&2
  exit 1
fi
if ! command -v llvm-rc >/dev/null 2>&1; then
  echo "FAIL: llvm-rc not found on PATH — install via 'brew install llvm' (keg-only)" >&2
  exit 1
fi
if [[ ! -f src-tauri/icons/icon.ico ]]; then
  echo "FAIL: src-tauri/icons/icon.ico missing — tauri-winres needs it for the resource step" >&2
  exit 1
fi

echo "== phase 2/3: cargo check --locked --tests (target $TARGET) =="
# --tests so the rmcp stdio integration test (mcp_stdio_live.rs) is also
# type-checked for Windows, not just the lib + bins.
cd src-tauri
cargo check --locked --target "$TARGET" --tests

echo "== phase 3/3: PASS — $TARGET checks clean (lib + bins + tests) =="
