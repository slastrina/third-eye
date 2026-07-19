// Pure-helper coverage for the Privacy Guard sub-surface (S03): the TS side
// of the privacy://state contract lock, counter zero-fill/ordering, and the
// copy helpers. All logic is pure, so no Tauri runtime or DOM is needed.

import { describe, expect, it } from "vitest";
import {
  blockReasonLabel,
  DETECTION_KINDS,
  GUARD_UNAVAILABLE_MESSAGE,
  kindLabel,
  PRIVACY_STATE_EVENT,
  redactionRows,
  type GuardTelemetry,
} from "./privacy-state";
import type { LlmError } from "./chat";
import { bannerDetail, bannerTitle } from "./chat";

/** A snapshot as `guard_status` returns it before any mutation. */
const pristine: GuardTelemetry = { redactions: [], blockedCount: 0 };

describe("event name", () => {
  it("matches the Rust-side IPC contract exactly", () => {
    // src-tauri/src/llm/commands.rs pins the same string from its side.
    expect(PRIVACY_STATE_EVENT).toBe("privacy://state");
  });
});

describe("detection kinds", () => {
  it("mirror Rust's DetectionKind::ALL kebab-case tags in stable order", () => {
    expect(DETECTION_KINDS).toEqual(["password", "card", "api-key"]);
  });

  it("every known kind has a human label distinct from its tag", () => {
    for (const kind of DETECTION_KINDS) {
      expect(kindLabel(kind)).not.toBe(kind);
      expect(kindLabel(kind)).not.toHaveLength(0);
    }
  });

  it("an unknown kind tag surfaces verbatim, never blank", () => {
    expect(kindLabel("ssh-key")).toBe("ssh-key");
  });
});

describe("redactionRows", () => {
  it("zero-fills every known kind when the wire snapshot is empty", () => {
    const rows = redactionRows(pristine);
    expect(rows.map((r) => r.kind)).toEqual(["password", "card", "api-key"]);
    expect(rows.map((r) => r.count)).toEqual([0, 0, 0]);
  });

  it("merges wire counts into ALL order regardless of payload order", () => {
    const rows = redactionRows({
      redactions: [
        { kind: "api-key", count: 3 },
        { kind: "password", count: 2 },
      ],
      blockedCount: 0,
    });
    expect(rows).toEqual([
      { kind: "password", label: "Passwords", count: 2 },
      { kind: "card", label: "Card numbers", count: 0 },
      { kind: "api-key", label: "API keys", count: 3 },
    ]);
  });

  it("appends an unknown future kind with its real count instead of dropping it", () => {
    const rows = redactionRows({
      redactions: [{ kind: "ssh-key", count: 1 }],
      blockedCount: 0,
    });
    expect(rows).toHaveLength(4);
    expect(rows[3]).toEqual({ kind: "ssh-key", label: "ssh-key", count: 1 });
  });

  it("rows carry kinds and counts only — no text-bearing fields", () => {
    const rows = redactionRows({
      redactions: [{ kind: "password", count: 5 }],
      blockedCount: 2,
    });
    for (const row of rows) {
      expect(Object.keys(row).sort()).toEqual(["count", "kind", "label"]);
    }
  });
});

describe("blockReasonLabel", () => {
  it("names every Rust GuardBlockReason variant", () => {
    expect(blockReasonLabel("attachment-unredactable")).toBe(
      "Attachment can't be redacted",
    );
    expect(blockReasonLabel("redaction-failed")).toBe("Redaction failed");
    expect(blockReasonLabel("low-confidence")).toBe(
      "Redaction couldn't be verified",
    );
  });

  it("an unknown kebab-case reason surfaces verbatim, never blank", () => {
    expect(blockReasonLabel("future-reason")).toBe("future-reason");
  });
});

describe("fail-closed lastError surface", () => {
  it("a guard-blocked lastError renders through the existing banner copy", () => {
    // The sub-surface reuses chat.ts's bannerTitle/bannerDetail — the same
    // "Blocked by privacy guard" copy the chat banner shows (slice
    // must-have: typed last error via existing banner helpers).
    const lastError: LlmError = {
      kind: "guard-blocked",
      endpoint: "http://192.0.2.1:9",
      reason: "low-confidence",
    };
    const telemetry: GuardTelemetry = {
      redactions: [],
      blockedCount: 1,
      lastBlockReason: "low-confidence",
      lastError,
    };
    expect(bannerTitle(telemetry.lastError!)).toBe("Blocked by privacy guard");
    expect(bannerDetail(telemetry.lastError!)).toBe(
      "http://192.0.2.1:9 — low-confidence",
    );
  });
});

describe("degraded-mode copy", () => {
  it("names the unavailable state for outside-Tauri rendering", () => {
    expect(GUARD_UNAVAILABLE_MESSAGE).toBe(
      "Privacy guard state is unavailable outside the app.",
    );
  });
});
