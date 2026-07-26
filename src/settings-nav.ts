// Pure navigation model for the PyCharm-style settings window: grouped
// sections rendered as a left sidebar, one section shown at a time in the
// content pane. Kept Tauri-free and side-effect-free so vitest covers it
// without a DOM (the settings-state.ts convention). Kebab-case filename on
// purpose: a `settingsNav.ts` sibling of Settings.tsx would be fine, but a
// `settings-nav.ts` matches overlay-state.ts/tray-notice.ts and stays clear
// of the macOS case-insensitivity trap around Settings.tsx.

/** Stable id of one settings page — rides the `?section=` deep link (used by
 *  Playwright and any future "open settings at X" entry point), so renames
 *  here are a wire-format change. */
export type SectionId =
  | "models"
  | "cloud"
  | "privacy"
  | "watcher"
  | "memory"
  | "input"
  | "mcp"
  | "nudges"
  | "overlay"
  | "programs"
  | "status";

export interface SectionEntry {
  id: SectionId;
  /** Sidebar label AND the page's h2 heading — one string, one truth. */
  label: string;
  /** Extra lowercase terms the sidebar search matches besides the label. */
  keywords: string;
}

export interface SectionGroup {
  /** Small uppercase group caption in the sidebar (PyCharm's tree roots). */
  title: string;
  entries: SectionEntry[];
}

/** The whole sidebar, in render order. Grouped by concern: what the app
 *  thinks with, what it may see/keep, what it may do, how it looks, and the
 *  read-only system readouts. */
export const SECTION_GROUPS: SectionGroup[] = [
  {
    title: "Intelligence",
    entries: [
      { id: "models", label: "Models", keywords: "lane llm local lm studio endpoint url ollama" },
      { id: "cloud", label: "Cloud Providers", keywords: "api key openai anthropic remote" },
    ],
  },
  {
    title: "Privacy & Data",
    entries: [
      { id: "privacy", label: "Privacy", keywords: "guard redaction block capture" },
      { id: "watcher", label: "Watch Screen", keywords: "ocr observe snippets" },
      { id: "memory", label: "Memory", keywords: "stored distill ingest wipe" },
    ],
  },
  {
    title: "Automation",
    entries: [
      { id: "input", label: "Input Control", keywords: "hid mouse keyboard click type" },
      { id: "mcp", label: "MCP Servers", keywords: "tools external stdio http json" },
      { id: "nudges", label: "Nudges", keywords: "hint suggestion" },
    ],
  },
  {
    title: "Appearance",
    entries: [
      { id: "overlay", label: "Overlay", keywords: "drawer modal edge presentation" },
    ],
  },
  {
    title: "System",
    entries: [
      { id: "programs", label: "Programs", keywords: "inventory apps tools installed cli path scan" },
      { id: "status", label: "Status", keywords: "hotkey autostart login shortcut" },
    ],
  },
];

/** Every section id in sidebar order — the `?section=` validation set. */
const ALL_IDS: SectionId[] = SECTION_GROUPS.flatMap((g) => g.entries.map((e) => e.id));

/** The page shown when no (or an off-contract) deep link is present. */
export const DEFAULT_SECTION: SectionId = "models";

/** Resolve the initial page from a `window.location.search` string. An absent
 *  or unknown `?section=` falls back to the default — a stale deep link must
 *  never blank the window. */
export function sectionFromSearch(search: string): SectionId {
  const raw = new URLSearchParams(search).get("section");
  return (ALL_IDS as string[]).includes(raw ?? "") ? (raw as SectionId) : DEFAULT_SECTION;
}

/** Sidebar search: case-insensitive substring over label + keywords. An empty
 *  filter returns the full tree; a group with no surviving entries is dropped
 *  (never an empty caption). The active page stays reachable via its content
 *  even when filtered out of the sidebar — filtering hides nav items only. */
export function filterGroups(groups: SectionGroup[], filter: string): SectionGroup[] {
  const needle = filter.trim().toLowerCase();
  if (needle.length === 0) return groups;
  return groups
    .map((group) => ({
      ...group,
      entries: group.entries.filter((e) =>
        `${e.label} ${e.keywords}`.toLowerCase().includes(needle),
      ),
    }))
    .filter((group) => group.entries.length > 0);
}
