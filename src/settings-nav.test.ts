// Coverage for the settings sidebar model (settings-nav.ts): the ?section=
// deep-link contract, the grouped tree shape, and the search filter. All pure
// — no DOM, no Tauri runtime.

import { describe, expect, it } from "vitest";
import {
  DEFAULT_SECTION,
  SECTION_GROUPS,
  filterGroups,
  sectionFromSearch,
  type SectionId,
} from "./settings-nav";

describe("SECTION_GROUPS", () => {
  it("covers every section exactly once (ids are a partition, not a list)", () => {
    const ids = SECTION_GROUPS.flatMap((g) => g.entries.map((e) => e.id));
    expect(new Set(ids).size).toBe(ids.length);
    // The full page set the settings window renders — adding a section means
    // extending this expectation alongside the Settings.tsx page.
    expect([...ids].sort()).toEqual(
      [
        "cloud",
        "input",
        "mcp",
        "memory",
        "models",
        "nudges",
        "overlay",
        "privacy",
        "programs",
        "status",
        "watcher",
      ].sort(),
    );
  });

  it("never renders an empty group caption", () => {
    for (const group of SECTION_GROUPS) {
      expect(group.entries.length).toBeGreaterThan(0);
      expect(group.title.length).toBeGreaterThan(0);
    }
  });
});

describe("sectionFromSearch", () => {
  it("resolves a valid deep link", () => {
    expect(sectionFromSearch("?view=settings&section=privacy")).toBe("privacy");
    expect(sectionFromSearch("?section=mcp")).toBe("mcp");
  });

  it("falls back to the default page on an absent or unknown section", () => {
    expect(sectionFromSearch("")).toBe(DEFAULT_SECTION);
    expect(sectionFromSearch("?view=settings")).toBe(DEFAULT_SECTION);
    expect(sectionFromSearch("?section=does-not-exist")).toBe(DEFAULT_SECTION);
    expect(sectionFromSearch("?section=")).toBe(DEFAULT_SECTION);
  });
});

describe("filterGroups", () => {
  it("returns the full tree for an empty or whitespace filter", () => {
    expect(filterGroups(SECTION_GROUPS, "")).toEqual(SECTION_GROUPS);
    expect(filterGroups(SECTION_GROUPS, "   ")).toEqual(SECTION_GROUPS);
  });

  it("matches case-insensitively on the label", () => {
    const hits = filterGroups(SECTION_GROUPS, "cLoUd").flatMap((g) => g.entries);
    expect(hits.map((e) => e.id)).toEqual(["cloud"]);
  });

  it("matches on keywords too, so 'api key' finds Cloud Providers", () => {
    const ids = new Set(
      filterGroups(SECTION_GROUPS, "api key").flatMap((g) => g.entries.map((e) => e.id)),
    );
    expect(ids.has("cloud")).toBe(true);
  });

  it("drops a group whose entries all miss — no empty captions", () => {
    for (const group of filterGroups(SECTION_GROUPS, "hotkey")) {
      expect(group.entries.length).toBeGreaterThan(0);
    }
    const ids: SectionId[] = filterGroups(SECTION_GROUPS, "hotkey").flatMap((g) =>
      g.entries.map((e) => e.id),
    );
    expect(ids).toEqual(["status"]);
  });

  it("an unmatched filter yields an empty tree, not a crash", () => {
    expect(filterGroups(SECTION_GROUPS, "zzz-no-such-page")).toEqual([]);
  });
});
