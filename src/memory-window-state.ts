// Pure helpers for the memory window (2026-07 redesign, surface 5):
// Timeline / Learned / Recall over the real store. No-fake-data mappings:
// the Timeline is every stored memory newest-first, "Learned" is the
// chat-distilled subset (the store's source vocabulary — no invented
// confidence scores; the prototype's percentage bars have no backing field
// and never render), and Recall shows memory_search's ranked results with
// its true mode/degradation surfaced, not a fabricated chat answer.

import type { MemoryRecord } from "./memory-state";

export type MemoryTab = "timeline" | "learned" | "recall" | "chats";

export const MEMORY_TABS: readonly { id: MemoryTab; label: string }[] = [
  { id: "timeline", label: "Timeline" },
  { id: "learned", label: "Learned" },
  { id: "recall", label: "Recall" },
  { id: "chats", label: "Chats" },
];

/** Case-insensitive filter over summary + apps (the header filter input). */
export function filterRecords(records: readonly MemoryRecord[], filter: string): MemoryRecord[] {
  const needle = filter.trim().toLowerCase();
  if (needle.length === 0) return [...records];
  return records.filter((record) =>
    `${record.summary} ${record.apps.join(" ")}`.toLowerCase().includes(needle),
  );
}

/** The Learned subset: chat-distilled memories (source vocabulary is
 *  lenient-lowercase on the wire; anything not "chat" is watcher-class). */
export function learnedRecords(records: readonly MemoryRecord[]): MemoryRecord[] {
  return records.filter((record) => record.source === "chat");
}

/** "3:12 PM"-style label for a record's span end (when it was observed). */
export function timeLabel(record: MemoryRecord): string {
  return new Date(record.spanEndMs).toLocaleTimeString([], {
    hour: "numeric",
    minute: "2-digit",
  });
}

/** "42 min" / "1h 05m" duration of the observed span; empty under a minute
 *  (a point-in-time memory has no meaningful duration to claim). */
export function durationLabel(record: MemoryRecord): string {
  const minutes = Math.floor((record.spanEndMs - record.spanStartMs) / 60000);
  if (minutes < 1) return "";
  if (minutes < 60) return `${minutes} min`;
  const hours = Math.floor(minutes / 60);
  const rest = minutes % 60;
  return rest === 0 ? `${hours}h` : `${hours}h ${String(rest).padStart(2, "0")}m`;
}

/** The record's primary app for the Timeline's app column ("—" when the
 *  span recorded none). */
export function appLabel(record: MemoryRecord): string {
  return record.apps[0] ?? "—";
}
