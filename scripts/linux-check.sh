#!/usr/bin/env bash
# S06 cross-target proof: the Rust crate compiles cleanly for
# x86_64-unknown-linux-gnu (R020). The Linux Tauri stack needs gtk/webkit
# system headers that do not exist on macOS, so the check runs inside a
# linux/amd64 container where the toolchain is native to that target.
#
# Usage: bash scripts/linux-check.sh   (from the repo root; Docker must be running)
set -euo pipefail

cd "$(dirname "$0")/.."

echo "== phase 1/3: docker daemon reachable =="
docker info --format 'server {{.ServerVersion}}' >/dev/null

echo "== phase 2/3: containerized apt + cargo check (x86_64-unknown-linux-gnu) =="
# Named volumes cache the cargo registry and target dir across reruns.
# CARGO_TARGET_DIR stays off the bind mount so the macOS tree is never
# polluted with Linux build output.
docker run --rm --platform linux/amd64 \
  -v "$PWD":/work \
  -v third-eye-cargo-registry:/usr/local/cargo/registry \
  -v third-eye-linux-target:/linux-target \
  -e CARGO_TARGET_DIR=/linux-target \
  -w /work/src-tauri \
  rust:1-bookworm \
  bash -euo pipefail -c '
    echo "-- apt: tauri v2 linux build deps --"
    apt-get update -qq
    apt-get install -y -qq --no-install-recommends \
      libwebkit2gtk-4.1-dev libgtk-3-dev libayatana-appindicator3-dev \
      librsvg2-dev libxdo-dev libssl-dev pkg-config >/dev/null
    echo "-- cargo check (lib + bins) --"
    cargo check --locked
    echo "-- cargo check (tests) --"
    cargo check --locked --tests
  '

echo "== phase 3/3: PASS — x86_64-unknown-linux-gnu checks clean =="
