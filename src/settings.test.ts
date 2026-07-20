// Negative-path coverage for the settings view state machine (S07): failed
// model-list fetches (typed offline naming the endpoint, and raw IPC
// rejections outside Tauri), rejected lane pins, privacy persist failures,
// and the picker's out-of-list current-pin handling. The reducer is pure, so
// no Tauri runtime or DOM is needed.

import { describe, expect, it } from "vitest";
import type { HidArmedStatus, ModelInfo, PrivacyStatus } from "./chat";
import {
  HID_RUN_MODE_OPTIONS,
  hidModeShowsAutoRunWarning,
  initialSettingsState,
  laneOptions,
  modelsErrorDetail,
  modelsErrorTitle,
  settingsReducer,
  toModelsError,
  type ModelsError,
  type SettingsState,
} from "./settings-state";

const ENDPOINT = "http://192.168.182.224:1234";

const routing: ModelInfo = {
  activeLane: "thin",
  endpoint: ENDPOINT,
  lanes: [
    { name: "thin", modelId: "thin-1b" },
    { name: "heavy", modelId: "heavy-7b" },
  ],
};

const offline: ModelsError = { kind: "offline", endpoint: ENDPOINT, detail: "connection refused" };

describe("settingsReducer model list", () => {
  it("marks a fetch in flight and clears the previous error", () => {
    let s = settingsReducer(initialSettingsState, { type: "models-error", error: offline });
    s = settingsReducer(s, { type: "models-loading" });
    expect(s.modelsLoading).toBe(true);
    expect(s.modelsError).toBeNull();
  });

  it("stores the fetched list and settles the loading state", () => {
    let s = settingsReducer(initialSettingsState, { type: "models-loading" });
    s = settingsReducer(s, { type: "models-loaded", models: ["thin-1b", "heavy-7b"] });
    expect(s.models).toEqual(["thin-1b", "heavy-7b"]);
    expect(s.modelsLoading).toBe(false);
    expect(s.modelsError).toBeNull();
  });

  it("a failed refresh keeps the stale list so the pickers still render", () => {
    let s = settingsReducer(initialSettingsState, { type: "models-loaded", models: ["thin-1b"] });
    s = settingsReducer(s, { type: "models-loading" });
    s = settingsReducer(s, { type: "models-error", error: offline });
    expect(s.models).toEqual(["thin-1b"]);
    expect(s.modelsError).toEqual(offline);
    expect(s.modelsLoading).toBe(false);
  });

  it("an empty model list is a valid result, not an error", () => {
    const s = settingsReducer(initialSettingsState, { type: "models-loaded", models: [] });
    expect(s.models).toEqual([]);
    expect(s.modelsError).toBeNull();
  });
});

describe("settingsReducer routing and lane pins", () => {
  it("stores the routing snapshot and supersedes a stale lane rejection", () => {
    let s = settingsReducer(initialSettingsState, {
      type: "lane-error",
      lane: "thin",
      detail: "unknown lane",
    });
    s = settingsReducer(s, { type: "model-info", info: routing });
    expect(s.modelInfo).toEqual(routing);
    expect(s.laneError).toBeNull();
  });

  it("a rejected pin names the lane and leaves routing untouched", () => {
    let s = settingsReducer(initialSettingsState, { type: "model-info", info: routing });
    s = settingsReducer(s, {
      type: "lane-error",
      lane: "heavy",
      detail: "failed to persist heavyModel to settings.json",
    });
    expect(s.laneError).toContain("heavy");
    expect(s.laneError).toContain("settings.json");
    expect(s.modelInfo).toEqual(routing);
  });
});

describe("settingsReducer privacy and status", () => {
  it("stores the privacy status, persist failure included as data", () => {
    const failed: PrivacyStatus = {
      enabled: false,
      error: "failed to persist privacyMode=true to settings.json",
    };
    const s = settingsReducer(initialSettingsState, { type: "privacy", status: failed });
    expect(s.privacy).toEqual(failed);
  });

  it("a later broadcast overwrites — the backend status is authoritative", () => {
    let s = settingsReducer(initialSettingsState, {
      type: "privacy",
      status: { enabled: true, error: null },
    });
    s = settingsReducer(s, { type: "privacy", status: { enabled: false, error: null } });
    expect(s.privacy?.enabled).toBe(false);
  });

  it("stores the HID arming status without touching model state", () => {
    const armed: HidArmedStatus = {
      armed: true,
      mode: "ask",
      permission: { granted: true, supported: true },
      error: null,
    };
    let s: SettingsState = settingsReducer(initialSettingsState, {
      type: "model-info",
      info: routing,
    });
    s = settingsReducer(s, { type: "hid", status: armed });
    expect(s.hid).toEqual(armed);
    expect(s.modelInfo).toEqual(routing);
  });

  it("a later HID broadcast is authoritative — a disarm from any surface overwrites", () => {
    const armed: HidArmedStatus = {
      armed: true,
      mode: "ask",
      permission: { granted: true, supported: true },
      error: null,
    };
    let s = settingsReducer(initialSettingsState, { type: "hid", status: armed });
    s = settingsReducer(s, {
      type: "hid",
      status: { armed: false, mode: "off", permission: { granted: true, supported: true }, error: null },
    });
    expect(s.hid?.armed).toBe(false);
    expect(s.hid?.mode).toBe("off");
  });

  it("a refused arm rides a typed permission-denied error the walkthrough keys on", () => {
    // D038: an ungranted arm stays disarmed and surfaces a typed error (R007).
    const refused: HidArmedStatus = {
      armed: false,
      mode: "off",
      permission: { granted: false, supported: true },
      error: { kind: "permission-denied", detail: "Accessibility not granted" },
    };
    const s = settingsReducer(initialSettingsState, { type: "hid", status: refused });
    expect(s.hid?.armed).toBe(false);
    expect(s.hid?.error?.kind).toBe("permission-denied");
  });

  it("renders the persisted run mode and warns only on Auto-run (S04/T05)", () => {
    // The selector reflects the mode carried on the authoritative hid://state
    // snapshot; the "dangerously allows all input" warning shows only for
    // auto-run, never for off/ask.
    const autoRun: HidArmedStatus = {
      armed: true,
      mode: "auto-run",
      permission: { granted: true, supported: true },
      error: null,
    };
    const s = settingsReducer(initialSettingsState, { type: "hid", status: autoRun });
    expect(s.hid?.mode).toBe("auto-run");
    expect(hidModeShowsAutoRunWarning("auto-run")).toBe(true);
    expect(hidModeShowsAutoRunWarning("ask")).toBe(false);
    expect(hidModeShowsAutoRunWarning("off")).toBe(false);

    // The selector offers exactly the three modes, Off first as the default.
    expect(HID_RUN_MODE_OPTIONS.map((o) => o.value)).toEqual(["off", "ask", "auto-run"]);
  });

  it("stores hotkey and autostart health without touching model state", () => {
    let s: SettingsState = settingsReducer(initialSettingsState, {
      type: "model-info",
      info: routing,
    });
    s = settingsReducer(s, {
      type: "hotkey",
      status: { shortcut: "super+shift+space", registered: false, error: "already taken" },
    });
    s = settingsReducer(s, { type: "autostart", status: { enabled: true, error: null } });
    expect(s.hotkey?.registered).toBe(false);
    expect(s.hotkey?.error).toBe("already taken");
    expect(s.autostart?.enabled).toBe(true);
    expect(s.modelInfo).toEqual(routing);
  });
});

describe("toModelsError", () => {
  it("passes typed kind-tagged errors through untouched", () => {
    expect(toModelsError(offline)).toBe(offline);
  });

  it("wraps a raw IPC rejection (plain browser) as kind ipc", () => {
    expect(toModelsError("window.__TAURI_INTERNALS__ is undefined")).toEqual({
      kind: "ipc",
      detail: "window.__TAURI_INTERNALS__ is undefined",
    });
    expect(toModelsError(new Error("boom")).kind).toBe("ipc");
  });
});

describe("laneOptions", () => {
  it("returns the fetched list when the current pin is served", () => {
    expect(laneOptions(["a", "b"], "a")).toEqual(["a", "b"]);
  });

  it("prepends a pinned model the endpoint no longer lists", () => {
    // The select must show the truth even for a model that went away.
    expect(laneOptions(["a", "b"], "gone-model")).toEqual(["gone-model", "a", "b"]);
  });

  it("handles an unpinned lane and a never-fetched list", () => {
    expect(laneOptions(["a"], null)).toEqual(["a"]);
    expect(laneOptions(null, null)).toEqual([]);
    expect(laneOptions(null, "pinned")).toEqual(["pinned"]);
  });
});

describe("models error copy", () => {
  it("names the endpoint for typed failures (R006)", () => {
    expect(modelsErrorTitle(offline)).toMatch(/endpoint|reach/i);
    expect(modelsErrorDetail(offline)).toContain(ENDPOINT);
  });

  it("gives IPC failures their own copy without a fake endpoint", () => {
    const ipc: ModelsError = { kind: "ipc", detail: "no Tauri runtime" };
    expect(modelsErrorTitle(ipc)).toMatch(/unavailable/i);
    expect(modelsErrorDetail(ipc)).toBe("no Tauri runtime");
  });

  it("surfaces guard-blocked with its kebab-case reason, not a detail field", () => {
    const blocked: ModelsError = {
      kind: "guard-blocked",
      endpoint: "http://192.0.2.1:9",
      reason: "redaction-failed",
    };
    expect(modelsErrorTitle(blocked)).toBe("Blocked by privacy guard");
    expect(modelsErrorDetail(blocked)).toBe("http://192.0.2.1:9 — redaction-failed");
  });
});
