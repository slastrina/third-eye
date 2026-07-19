// UI side of the privacy-guard IPC surface (S03): the never-rejecting
// `guard_status` command and the `privacy://state` broadcast, plus the pure
// helpers behind the Privacy Guard sub-surface in Settings. The shapes here
// mirror the serde camelCase serialization of Rust's GuardTelemetry,
// Detection, and GuardBlockReason (src-tauri/src/llm/guard.rs,
// src-tauri/src/privacy/mod.rs) — a change on either side is a breaking IPC
// change, pinned by contract-lock tests on both sides.
//
// Payloads structurally carry detection kinds and counts only — never
// original or redacted text. All display logic is pure so the counter
// zero-fill, ordering, and copy are unit-testable without a Tauri runtime
// (src/privacy-state.test.ts); Settings.tsx is only glue. (Kebab-case name
// per MEM051, following the watcher-state.ts/settings-state.ts convention.)

import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import type { LlmError } from "./chat";

/** Guard-state broadcast: every guard mutation (redaction recorded on an
 *  external forward, guard block, watcher redaction increment) emits the
 *  resulting kinds-and-counts-only snapshot app-wide. */
export const PRIVACY_STATE_EVENT = "privacy://state";

/** What kind of sensitive value a detector matched — the kebab-case serde
 *  tags of Rust's DetectionKind, in its stable ALL order. */
export type DetectionKind = "password" | "card" | "api-key";

/** Stable display order, mirroring Rust's DetectionKind::ALL. */
export const DETECTION_KINDS: DetectionKind[] = ["password", "card", "api-key"];

/** One detector's aggregate result: kind and count only, never text. */
export interface Detection {
  kind: string;
  count: number;
}

/** Why the guard refused a send — kebab-case serde tags of Rust's
 *  GuardBlockReason. Kept as string-typed on the wire (`reason` also rides
 *  the guard-blocked LlmError as a plain string) so an unknown future
 *  reason degrades to verbatim display, never a crash. */
export type GuardBlockReason =
  | "attachment-unredactable"
  | "redaction-failed"
  | "low-confidence";

/** Queryable guard state (health-as-value): returned by `guard_status`,
 *  broadcast on `privacy://state`. `redactions` omits zero-count kinds
 *  (RedactionOutcome semantics); `lastBlockReason`/`lastError` are absent
 *  until a block happens (serde skip_serializing_if). */
export interface GuardTelemetry {
  redactions: Detection[];
  blockedCount: number;
  lastBlockReason?: GuardBlockReason;
  lastError?: LlmError;
}

/** Current guard telemetry (health-as-value, `watcher_status` precedent —
 *  never rejects backend-side; rejection outside a Tauri runtime is
 *  absorbed by the caller into the named unavailable state). */
export function guardStatus(): Promise<GuardTelemetry> {
  return invoke<GuardTelemetry>("guard_status");
}

/** Subscribe to the app-wide guard-state broadcast (`privacy://state`). */
export function onPrivacyState(cb: (telemetry: GuardTelemetry) => void): Promise<UnlistenFn> {
  return listen<GuardTelemetry>(PRIVACY_STATE_EVENT, (e) => cb(e.payload));
}

// ---------------------------------------------------------------------------
// Display helpers (pure)
// ---------------------------------------------------------------------------

/** One rendered counter row. `kind` stays the wire tag (stable e2e/data
 *  attribute); `label` is the human column. */
export interface RedactionRow {
  kind: string;
  label: string;
  count: number;
}

/** Human label for a detection kind; an unknown tag surfaces verbatim
 *  rather than being dropped — the counter must never silently lie. */
export function kindLabel(kind: string): string {
  switch (kind) {
    case "password":
      return "Passwords";
    case "card":
      return "Card numbers";
    case "api-key":
      return "API keys";
    default:
      return kind;
  }
}

/** Counter rows for the sub-surface: every known kind in DetectionKind::ALL
 *  order, zero-filled (the wire snapshot omits zero-count kinds, but the
 *  surface shows all three so "0" is visible evidence, not absence). Any
 *  unknown kind the backend ever adds is appended after the known ones with
 *  its real count — data is never silently dropped. */
export function redactionRows(telemetry: GuardTelemetry): RedactionRow[] {
  const counts = new Map(telemetry.redactions.map((d) => [d.kind, d.count]));
  const known: RedactionRow[] = DETECTION_KINDS.map((kind) => ({
    kind,
    label: kindLabel(kind),
    count: counts.get(kind) ?? 0,
  }));
  const unknown: RedactionRow[] = telemetry.redactions
    .filter((d) => !(DETECTION_KINDS as string[]).includes(d.kind))
    .map((d) => ({ kind: d.kind, label: kindLabel(d.kind), count: d.count }));
  return [...known, ...unknown];
}

/** Human label for the last-block reason; an unknown kebab-case reason
 *  surfaces verbatim (machine-readable beats blank). */
export function blockReasonLabel(reason: string): string {
  switch (reason) {
    case "attachment-unredactable":
      return "Attachment can't be redacted";
    case "redaction-failed":
      return "Redaction failed";
    case "low-confidence":
      return "Redaction couldn't be verified";
    default:
      return reason;
  }
}

/** Named unavailable copy for outside-Tauri rendering (vite dev,
 *  Playwright degraded mode). */
export const GUARD_UNAVAILABLE_MESSAGE =
  "Privacy guard state is unavailable outside the app.";
