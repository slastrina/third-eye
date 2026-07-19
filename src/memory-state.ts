// UI side of the memory IPC surface (S02 → S04): the `memory_list`,
// `memory_update`, `memory_delete`, `memory_wipe`, and `memory_status`
// commands, plus the pure `memoryReducer` behind the Memory section in
// Settings. The shapes here mirror the serde camelCase serialization of
// Rust's MemoryRecord, MemoryError, MemoryStatus, and IngestStatus
// (src-tauri/src/memory) — a change on either side is a breaking IPC
// change.
//
// The reducer is pure so pagination clamping, the inline-edit lifecycle,
// both two-step confirms (per-row delete and wipe-all), and every error
// path are unit-testable without a Tauri runtime (src/memory-state.test.ts);
// Settings.tsx is only glue. (Kebab-case name per MEM051: pure-module
// companions follow watcher-state.ts/settings-state.ts convention.)

import { invoke } from "@tauri-apps/api/core";
import type { LlmError } from "./chat";

/** One stored memory as returned by `memory_list`/`memory_update` — the
 *  serde camelCase serialization of Rust's MemoryRecord (store.rs). The
 *  embedding is deliberately absent: search-internal, not record contract. */
export interface MemoryRecord {
  id: number;
  summary: string;
  /** App names active during the observed span. */
  apps: string[];
  spanStartMs: number;
  spanEndMs: number;
  createdAtMs: number;
  updatedAtMs: number;
}

/** Kind-tagged rejection JSON every fallible memory command shares — the
 *  serde serialization of Rust's MemoryError (mod.rs). Consumers match on
 *  `kind`, same contract as LlmError/OcrError. */
export type MemoryError =
  | { kind: "db"; detail: string }
  | { kind: "not-found"; id: number }
  | { kind: "invalid-input"; detail: string };

/** The ingest half of `memory_status` — Rust's IngestStatus (ingest.rs). */
export interface IngestStatus {
  buffered: number;
  distilledCount: number;
  lastDistillAtMs: number | null;
  lastError: LlmError | null;
}

/** `memory_status` payload — health-as-value, never rejects backend-side
 *  (R006). `available: false` means the store never opened this run;
 *  `storeError` carries a count failure on an otherwise open store (the
 *  serde skip means the key can be absent entirely). */
export interface MemoryStatus {
  available: boolean;
  count: number | null;
  dbPath: string | null;
  storeError?: MemoryError;
  ingest: IngestStatus;
}

/** Narrow an invoke rejection to the kind-tagged MemoryError contract.
 *  Outside a Tauri runtime, invoke rejects with a plain string/Error — that
 *  falls through here and the caller treats it as "unavailable". */
export function isMemoryError(e: unknown): e is MemoryError {
  if (typeof e !== "object" || e === null) return false;
  const kind = (e as { kind?: unknown }).kind;
  return kind === "db" || kind === "not-found" || kind === "invalid-input";
}

/** Newest-first page of stored memories. Limits are clamped server-side. */
export function memoryList(limit: number, offset: number): Promise<MemoryRecord[]> {
  return invoke<MemoryRecord[]>("memory_list", { limit, offset });
}

/** Replace one memory's summary; resolves with the updated record. */
export function memoryUpdate(id: number, summary: string): Promise<MemoryRecord> {
  return invoke<MemoryRecord>("memory_update", { id, summary });
}

/** Delete one memory by id (`not-found` when the id misses). */
export function memoryDelete(id: number): Promise<void> {
  return invoke<void>("memory_delete", { id });
}

/** Delete every stored memory; resolves with how many rows were removed. */
export function memoryWipe(): Promise<number> {
  return invoke<number>("memory_wipe");
}

/** Memory health snapshot — never rejects inside a Tauri runtime. */
export function memoryStatus(): Promise<MemoryStatus> {
  return invoke<MemoryStatus>("memory_status");
}

// ---------------------------------------------------------------------------
// Memory view state machine (pure)
// ---------------------------------------------------------------------------

/** Page size for the browse view. Small enough to render instantly, big
 *  enough that most stores fit on one page (server clamps at 500). */
export const MEMORY_PAGE_SIZE = 25;

/** Copy for the named degraded state outside a Tauri runtime (and shown
 *  with the backend detail when the store itself never opened). */
export const MEMORY_UNAVAILABLE_MESSAGE = "Memory is unavailable outside the app";

/** Copy for the empty store (distinct from unavailable — the store is fine,
 *  there's just nothing in it yet). */
export const MEMORY_EMPTY_HINT = "No memories stored yet";

/** Whether the memory IPC surface has answered yet, and how.
 *  - `unknown`: mount-time fetches still in flight.
 *  - `ready`: the surface answered (even if the answer was an error banner).
 *  - `unavailable`: an invoke rejected with a non-MemoryError shape — we are
 *    outside a Tauri runtime; the section renders the named message and
 *    nothing else. */
export type MemoryAvailability = "unknown" | "ready" | "unavailable";

/** Inline-edit lifecycle for one row. `saving: true` is the signal for the
 *  glue effect to fire `memory_update`; `error` keeps edit mode open with an
 *  inline message (blank draft, backend invalid-input). */
export interface MemoryEditState {
  id: number;
  draft: string;
  error: string | null;
  saving: boolean;
}

export interface MemoryViewState {
  availability: MemoryAvailability;
  /** Latest `memory_status` snapshot; null until the first poll lands. */
  status: MemoryStatus | null;
  /** Current page of records, newest first. */
  records: MemoryRecord[];
  /** Offset of the current page (multiple of MEMORY_PAGE_SIZE). */
  offset: number;
  /** True while a page fetch is in flight (mount, page turn, refresh). */
  loading: boolean;
  /** Bumped whenever the glue must refetch list + status (mutations,
   *  not-found staleness, empty-page clamp). The effect keys on it. */
  refreshToken: number;
  edit: MemoryEditState | null;
  /** Row id armed for the two-step delete confirm; null when disarmed. */
  confirmDelete: number | null;
  /** True when wipe-all is armed for its two-step confirm. */
  confirmWipe: boolean;
  /** Dismissible error banner (db failures, stale-row refreshes). */
  banner: string | null;
  /** Dismissible success notice ("Cleared N memories"). */
  notice: string | null;
}

export const initialMemoryViewState: MemoryViewState = {
  availability: "unknown",
  status: null,
  records: [],
  offset: 0,
  loading: true,
  refreshToken: 0,
  edit: null,
  confirmDelete: null,
  confirmWipe: false,
  banner: null,
  notice: null,
};

export type MemoryViewAction =
  | { type: "list"; records: MemoryRecord[]; offset: number }
  | { type: "list-failed"; error: MemoryError }
  | { type: "status"; status: MemoryStatus }
  | { type: "unavailable" }
  | { type: "next-page" }
  | { type: "prev-page" }
  | { type: "begin-edit"; id: number }
  | { type: "edit-draft"; draft: string }
  | { type: "cancel-edit" }
  | { type: "save-edit" }
  | { type: "edit-saved"; record: MemoryRecord }
  | { type: "edit-failed"; error: MemoryError }
  | { type: "request-delete"; id: number }
  | { type: "cancel-delete" }
  | { type: "deleted"; id: number }
  | { type: "delete-failed"; error: MemoryError }
  | { type: "request-wipe" }
  | { type: "cancel-wipe" }
  | { type: "wiped"; removed: number }
  | { type: "wipe-failed"; error: MemoryError }
  | { type: "dismiss-banner" }
  | { type: "dismiss-notice" };

/** Whether a next page plausibly exists. Uses the authoritative count when
 *  `memory_status` has reported one; otherwise falls back to "the current
 *  page is full". Clamped so the Next control disables at the end. */
export function canGoNext(state: MemoryViewState): boolean {
  const count = state.status?.count;
  if (typeof count === "number") return state.offset + MEMORY_PAGE_SIZE < count;
  return state.records.length === MEMORY_PAGE_SIZE;
}

/** Whether a previous page exists (clamped at offset 0). */
export function canGoPrev(state: MemoryViewState): boolean {
  return state.offset > 0;
}

/** Human message for a kind-tagged memory failure. */
export function memoryErrorMessage(error: MemoryError): string {
  switch (error.kind) {
    case "db":
      return `Memory store error: ${error.detail}`;
    case "not-found":
      return `Memory #${error.id} no longer exists`;
    case "invalid-input":
      return error.detail;
  }
}

/** Inline validation mirroring the store's invalid-input rule for blank
 *  summaries — catches it before a round-trip. */
export function validateSummaryDraft(draft: string): string | null {
  return draft.trim().length === 0 ? "Summary can't be empty" : null;
}

export function memoryReducer(
  state: MemoryViewState,
  action: MemoryViewAction,
): MemoryViewState {
  switch (action.type) {
    case "list": {
      // A page that came back empty above offset 0 means rows vanished
      // under us (deletes, a wipe from elsewhere): clamp back one page and
      // refetch instead of rendering a lying empty state.
      if (action.records.length === 0 && action.offset > 0) {
        return {
          ...state,
          availability: "ready",
          offset: Math.max(0, action.offset - MEMORY_PAGE_SIZE),
          loading: true,
          refreshToken: state.refreshToken + 1,
        };
      }
      return {
        ...state,
        availability: "ready",
        records: action.records,
        offset: action.offset,
        loading: false,
      };
    }
    case "list-failed":
      // The surface answered — with a db error. Ready + banner, not
      // unavailable: unavailable is reserved for the no-runtime case.
      return {
        ...state,
        availability: "ready",
        loading: false,
        banner: memoryErrorMessage(action.error),
      };
    case "status":
      return { ...state, availability: "ready", status: action.status };
    case "unavailable":
      return {
        ...initialMemoryViewState,
        availability: "unavailable",
        loading: false,
        refreshToken: state.refreshToken,
      };
    case "next-page":
      if (!canGoNext(state)) return state;
      return {
        ...state,
        offset: state.offset + MEMORY_PAGE_SIZE,
        loading: true,
        edit: null,
        confirmDelete: null,
        confirmWipe: false,
      };
    case "prev-page":
      if (!canGoPrev(state)) return state;
      return {
        ...state,
        offset: Math.max(0, state.offset - MEMORY_PAGE_SIZE),
        loading: true,
        edit: null,
        confirmDelete: null,
        confirmWipe: false,
      };
    case "begin-edit": {
      const record = state.records.find((r) => r.id === action.id);
      if (!record) return state;
      return {
        ...state,
        edit: { id: record.id, draft: record.summary, error: null, saving: false },
        confirmDelete: null,
        confirmWipe: false,
        notice: null,
      };
    }
    case "edit-draft":
      if (!state.edit || state.edit.saving) return state;
      return { ...state, edit: { ...state.edit, draft: action.draft, error: null } };
    case "cancel-edit":
      if (!state.edit) return state;
      return { ...state, edit: null };
    case "save-edit": {
      if (!state.edit || state.edit.saving) return state;
      const error = validateSummaryDraft(state.edit.draft);
      if (error) return { ...state, edit: { ...state.edit, error } };
      return { ...state, edit: { ...state.edit, error: null, saving: true } };
    }
    case "edit-saved":
      return {
        ...state,
        records: state.records.map((r) => (r.id === action.record.id ? action.record : r)),
        edit: null,
        refreshToken: state.refreshToken + 1,
      };
    case "edit-failed": {
      if (!state.edit) return state;
      switch (action.error.kind) {
        case "invalid-input":
          // Keep edit mode open with the inline message; the draft stands.
          return { ...state, edit: { ...state.edit, saving: false, error: action.error.detail } };
        case "not-found":
          // The row vanished under the editor: drop edit mode and refetch.
          return {
            ...state,
            edit: null,
            banner: memoryErrorMessage(action.error),
            refreshToken: state.refreshToken + 1,
          };
        case "db":
          // Keep the draft so nothing is lost; surface the failure app-wide.
          return {
            ...state,
            edit: { ...state.edit, saving: false },
            banner: memoryErrorMessage(action.error),
          };
      }
      return state;
    }
    case "request-delete":
      if (!state.records.some((r) => r.id === action.id)) return state;
      return { ...state, confirmDelete: action.id, confirmWipe: false, notice: null };
    case "cancel-delete":
      if (state.confirmDelete === null) return state;
      return { ...state, confirmDelete: null };
    case "deleted":
      return {
        ...state,
        records: state.records.filter((r) => r.id !== action.id),
        confirmDelete: null,
        edit: state.edit?.id === action.id ? null : state.edit,
        refreshToken: state.refreshToken + 1,
      };
    case "delete-failed":
      return {
        ...state,
        confirmDelete: null,
        banner: memoryErrorMessage(action.error),
        // A not-found delete means the list is stale — refetch it.
        refreshToken:
          action.error.kind === "not-found" ? state.refreshToken + 1 : state.refreshToken,
      };
    case "request-wipe":
      return { ...state, confirmWipe: true, confirmDelete: null, notice: null };
    case "cancel-wipe":
      if (!state.confirmWipe) return state;
      return { ...state, confirmWipe: false };
    case "wiped":
      return {
        ...state,
        records: [],
        offset: 0,
        edit: null,
        confirmDelete: null,
        confirmWipe: false,
        notice: `Cleared ${action.removed} ${action.removed === 1 ? "memory" : "memories"}`,
        refreshToken: state.refreshToken + 1,
      };
    case "wipe-failed":
      return { ...state, confirmWipe: false, banner: memoryErrorMessage(action.error) };
    case "dismiss-banner":
      if (state.banner === null) return state;
      return { ...state, banner: null };
    case "dismiss-notice":
      if (state.notice === null) return state;
      return { ...state, notice: null };
  }
}

// ---------------------------------------------------------------------------
// Copy helpers (pure)
// ---------------------------------------------------------------------------

/** "Safari, Zed" or a placeholder when the span had no known frontmost app. */
export function appsLabel(apps: string[]): string {
  return apps.length === 0 ? "Unknown app" : apps.join(", ");
}

/** Human label for a memory's observed span: same-day spans collapse to one
 *  date with a time range; cross-day spans show both endpoints in full. */
export function spanLabel(spanStartMs: number, spanEndMs: number): string {
  const start = new Date(spanStartMs);
  const end = new Date(spanEndMs);
  if (start.toDateString() === end.toDateString()) {
    return `${start.toLocaleDateString()} ${start.toLocaleTimeString()} – ${end.toLocaleTimeString()}`;
  }
  return `${start.toLocaleString()} – ${end.toLocaleString()}`;
}

/** Wall-clock label for created/updated timestamps (date + time — memories
 *  span days, unlike the watcher's last-few-ticks snippet list). */
export function memoryTimeLabel(ms: number): string {
  return new Date(ms).toLocaleString();
}

/** Human label for the ingest distiller's last run. */
export function lastDistillLabel(lastDistillAtMs: number | null): string {
  return lastDistillAtMs === null ? "never" : memoryTimeLabel(lastDistillAtMs);
}
