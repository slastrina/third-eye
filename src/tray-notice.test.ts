// De-stubbed in S07: the three former stub entries (settings,
// configure-models, privacy-mode) are real surfaces now, so the notice path
// only carries the named fallback for anything that still emits.

import { describe, expect, it } from "vitest";

import { noticeMessage, TRAY_NOTICE_EVENT } from "./tray-notice";

describe("tray-notice", () => {
  it("keeps the event name in sync with the Rust contract", () => {
    expect(TRAY_NOTICE_EVENT).toBe("tray://notice");
  });

  it("no longer carries S07 stub copy for the de-stubbed features", () => {
    // These entries open real surfaces now; if a notice still arrives for
    // one, it must not claim the feature "arrives with S07".
    for (const feature of ["settings", "configure-models", "privacy-mode"]) {
      const notice = noticeMessage(feature);
      expect(notice.detail).not.toContain("S07");
      expect(notice.title).toContain(feature);
    }
  });

  it("names any feature id in a visible banner (never silent)", () => {
    const notice = noticeMessage("telemetry");
    expect(notice.title).toContain("telemetry");
    expect(notice.detail.length).toBeGreaterThan(0);
  });

  it("falls back safely for an empty feature id", () => {
    const notice = noticeMessage("");
    expect(notice.title.length).toBeGreaterThan(0);
    expect(notice.detail.length).toBeGreaterThan(0);
  });
});
