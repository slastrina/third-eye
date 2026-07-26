# Third Eye — build & dev commands
#
# Frontend: Vite + React + TypeScript (npm)
# Backend:  Tauri v2 (Rust crate in src-tauri/)
#
# Run `make` or `make help` to list targets.

# Use bash so the recipes behave consistently across machines.
SHELL := /usr/bin/env bash

# npm passes extra flags after `--`; TAURI holds the tauri CLI entrypoint.
TAURI := npm run tauri --

.DEFAULT_GOAL := help

.PHONY: help install dev tauri-dev build build-web build-tauri run-app preview \
        test test-unit test-e2e test-all check check-rust check-guard \
        check-mcp-guard linux-check win-check fmt fmt-check lint clean \
        clean-web clean-rust

## help: List available targets
help:
	@echo "Third Eye — make targets:"
	@echo ""
	@grep -E '^## ' $(MAKEFILE_LIST) | sed 's/^## /  /'

# ── Setup ───────────────────────────────────────────────────────────────

## install: Install frontend dependencies (npm ci from lockfile)
install:
	npm ci

# ── Dev ─────────────────────────────────────────────────────────────────

## dev: Start the Vite dev server (frontend only)
dev:
	npm run dev

## tauri-dev: Run the full Tauri desktop app in dev mode (native window)
tauri-dev:
	$(TAURI) dev

## preview: Preview the production frontend build locally
preview:
	npm run preview

# ── Build ───────────────────────────────────────────────────────────────

## build: Build the full Tauri app bundle (frontend + native)
build: build-tauri

# The bundled app the OS permission grants attach to. Grant Screen Recording
# and Accessibility to THIS app (System Settings → Privacy & Security); the
# grant sticks across launches, and only drops when the app is REBUILT
# (ad-hoc signing changes the binary hash — re-toggle the grant after a
# rebuild, or set a real signingIdentity in tauri.conf.json to make it
# survive rebuilds too).
APP_BUNDLE := src-tauri/target/release/bundle/macos/Third Eye.app

## run-app: Build the release app bundle and launch it (stable binary for OS permission grants)
run-app: build-tauri
	open "$(APP_BUNDLE)"

## build-web: Type-check and build the frontend (tsc && vite build)
build-web:
	npm run build

## build-tauri: Build the Tauri desktop app bundle for the current OS
build-tauri:
	$(TAURI) build

# ── Test ────────────────────────────────────────────────────────────────

## test: Run frontend + Rust unit tests
test: test-unit check-rust
	cd src-tauri && cargo test --locked

## test-unit: Run frontend unit tests (vitest, single run)
test-unit:
	npm run test

## test-e2e: Run Playwright end-to-end tests
test-e2e:
	npm run test:e2e

## test-all: Run unit, Rust, and e2e tests
test-all: test-unit test-e2e
	cd src-tauri && cargo test --locked

# ── Checks / lint ───────────────────────────────────────────────────────

## check: Type-check frontend and cargo-check the Rust crate
check: check-rust
	npx tsc --noEmit

## check-rust: cargo check the Tauri crate (lib, bins, and tests)
check-rust:
	cd src-tauri && cargo check --locked --tests

## check-guard: Verify no unguarded LLM client construction sites exist
check-guard:
	bash scripts/check-guard-mounts.sh

## check-mcp-guard: Verify no MCP tool-action path bypasses the approval gate
check-mcp-guard:
	bash scripts/check-mcp-guard.sh

## linux-check: Cross-compile check for x86_64-unknown-linux-gnu (needs Docker)
linux-check:
	bash scripts/linux-check.sh

## win-check: Cross-compile check for x86_64-pc-windows-msvc (needs rustup target + llvm-rc)
win-check:
	bash scripts/win-check.sh

## fmt: Format the Rust crate (cargo fmt)
fmt:
	cd src-tauri && cargo fmt

## fmt-check: Verify Rust formatting without writing changes
fmt-check:
	cd src-tauri && cargo fmt --check

## lint: Run clippy on the Rust crate (warnings denied)
lint:
	cd src-tauri && cargo clippy --locked --tests -- -D warnings

# ── Clean ───────────────────────────────────────────────────────────────

## clean: Remove all build artifacts (frontend + Rust)
clean: clean-web clean-rust

## clean-web: Remove the frontend build output (dist/)
clean-web:
	rm -rf dist

## clean-rust: Remove the Rust build output (src-tauri/target/)
clean-rust:
	cd src-tauri && cargo clean
