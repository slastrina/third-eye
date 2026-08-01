// Reducer + helper coverage for the Cloud Providers section (S04): opt-in
// status transitions (including the persist-error rollback and the
// disable-reverts-to-local path), the key presence snapshot / key-error
// channel, the heavy-lane selection, and the pure copy/selector helpers. The
// reducer is pure, so no Tauri runtime or DOM is needed. A dedicated test
// asserts the never-echo invariant: no key value can enter view state.

import { describe, expect, it } from "vitest";
import {
  CLOUD_OPTIN_EVENT,
  CLOUD_PROVIDERS,
  cloudReducer,
  initialCloudViewState,
  isCloudKeyError,
  keyErrorTitle,
  keyPresent,
  providerLabel,
  type CloudHeavyProviderStatus,
  type CloudKeyStatus,
  type CloudOptInStatus,
} from "./cloud-state";

const enabled: CloudOptInStatus = { enabled: true, persistError: null };
const disabled: CloudOptInStatus = { enabled: false, persistError: null };
const bothKeys: CloudKeyStatus = { openaiPresent: true, anthropicPresent: true };
const noKeys: CloudKeyStatus = { openaiPresent: false, anthropicPresent: false };

describe("event name", () => {
  it("matches the Rust-side IPC contract exactly", () => {
    // src-tauri/src/cloud/optin.rs pins the same string from its side.
    expect(CLOUD_OPTIN_EVENT).toBe("cloud://optin");
  });
});

describe("cloudReducer opt-in transitions", () => {
  it("starts unknown: nothing resolved", () => {
    expect(initialCloudViewState.optin).toBeNull();
    expect(initialCloudViewState.keys).toBeNull();
    expect(initialCloudViewState.heavy).toBeNull();
    expect(initialCloudViewState.keyError).toBeNull();
  });

  it("stores the backend opt-in snapshot as authoritative", () => {
    const s = cloudReducer(initialCloudViewState, { type: "optin", status: enabled });
    expect(s.optin).toEqual(enabled);
  });

  it("follows enable → disable (reverts to local-only)", () => {
    let s = cloudReducer(initialCloudViewState, { type: "optin", status: enabled });
    expect(s.optin?.enabled).toBe(true);
    s = cloudReducer(s, { type: "optin", status: disabled });
    expect(s.optin?.enabled).toBe(false);
  });

  it("surfaces a persist failure without losing the decided toggle state", () => {
    const s = cloudReducer(initialCloudViewState, {
      type: "optin",
      // Rollback contract: the toggle reverted to off, persistError says why.
      status: { enabled: false, persistError: "cloud: failed to persist cloudOptin=true" },
    });
    expect(s.optin?.enabled).toBe(false);
    expect(s.optin?.persistError).toContain("cloudOptin");
  });

  it("keeps keys and heavy selection when only opt-in changes", () => {
    let s = cloudReducer(initialCloudViewState, { type: "keys", status: bothKeys });
    s = cloudReducer(s, {
      type: "heavy",
      status: { provider: "openai", persistError: null },
    });
    s = cloudReducer(s, { type: "optin", status: disabled });
    // Disabling opt-in is UI-level local-only; the stored key presence and
    // remembered provider persist backend-side and must not be blanked here.
    expect(s.keys).toEqual(bothKeys);
    expect(s.heavy?.provider).toBe("openai");
  });

  it("tracks the coder-lane selection independently of heavy (S6)", () => {
    expect(initialCloudViewState.coder).toBeNull();
    let s = cloudReducer(initialCloudViewState, {
      type: "heavy",
      status: { provider: "openai", persistError: null },
    });
    s = cloudReducer(s, {
      type: "coder",
      status: { provider: "anthropic", persistError: null },
    });
    expect(s.heavy?.provider).toBe("openai");
    expect(s.coder?.provider).toBe("anthropic");
    // Clearing coder never touches heavy.
    s = cloudReducer(s, { type: "coder", status: { provider: null, persistError: null } });
    expect(s.coder?.provider).toBeNull();
    expect(s.heavy?.provider).toBe("openai");
  });
});

describe("cloudReducer key presence + error channel", () => {
  it("stores the presence snapshot (booleans only)", () => {
    const s = cloudReducer(initialCloudViewState, { type: "keys", status: bothKeys });
    expect(s.keys).toEqual(bothKeys);
    expect(keyPresent(s.keys, "openai")).toBe(true);
    expect(keyPresent(s.keys, "anthropic")).toBe(true);
  });

  it("records a key error without lying about current presence", () => {
    let s = cloudReducer(initialCloudViewState, { type: "keys", status: bothKeys });
    s = cloudReducer(s, {
      type: "key-error",
      error: { kind: "store-failed", detail: "keychain locked" },
    });
    expect(s.keyError).toEqual({ kind: "store-failed", detail: "keychain locked" });
    // The last known presence is untouched — the failed attempt changed nothing.
    expect(s.keys).toEqual(bothKeys);
  });

  it("a fresh presence snapshot clears a stale key error", () => {
    let s = cloudReducer(initialCloudViewState, {
      type: "key-error",
      error: { kind: "invalid-key", detail: "key is empty" },
    });
    expect(s.keyError).not.toBeNull();
    s = cloudReducer(s, { type: "keys", status: noKeys });
    expect(s.keyError).toBeNull();
  });

  it("NEVER lets a key value enter view state (never-echo invariant)", () => {
    // The only key-bearing action is inbound via setCloudApiKey (an invoke,
    // not a reducer action). No CloudViewAction carries a key, so a submitted
    // key can never round-trip back through the reducer into the DOM.
    const secret = "sk-super-secret-value-1234567890";
    let s = initialCloudViewState;
    s = cloudReducer(s, { type: "keys", status: bothKeys });
    s = cloudReducer(s, {
      type: "key-error",
      error: { kind: "store-failed", detail: "keychain locked" },
    });
    s = cloudReducer(s, { type: "optin", status: enabled });
    s = cloudReducer(s, { type: "heavy", status: { provider: "openai", persistError: null } });
    expect(JSON.stringify(s)).not.toContain(secret);
    expect(JSON.stringify(s)).not.toContain("sk-");
  });
});

describe("cloudReducer heavy-lane selection", () => {
  it("stores the selection and clears it back to null", () => {
    let s = cloudReducer(initialCloudViewState, {
      type: "heavy",
      status: { provider: "anthropic", persistError: null },
    });
    expect(s.heavy?.provider).toBe("anthropic");
    s = cloudReducer(s, { type: "heavy", status: { provider: null, persistError: null } });
    expect(s.heavy?.provider).toBeNull();
  });

  it("surfaces a heavy-provider persist failure as data", () => {
    const status: CloudHeavyProviderStatus = {
      provider: null,
      persistError: "cloud: failed to persist cloudHeavyProvider",
    };
    const s = cloudReducer(initialCloudViewState, { type: "heavy", status });
    expect(s.heavy?.persistError).toContain("cloudHeavyProvider");
  });
});

describe("error narrowing and copy helpers", () => {
  it("isCloudKeyError accepts exactly the kind-tagged contract", () => {
    expect(isCloudKeyError({ kind: "invalid-key", detail: "x" })).toBe(true);
    expect(isCloudKeyError({ kind: "store-failed", detail: "x" })).toBe(true);
    // Outside Tauri, invoke rejects with strings/Errors — not key errors.
    expect(isCloudKeyError("window.__TAURI_INTERNALS__ is undefined")).toBe(false);
    expect(isCloudKeyError(new Error("no runtime"))).toBe(false);
    expect(isCloudKeyError(null)).toBe(false);
    expect(isCloudKeyError({ kind: "offline" })).toBe(false);
  });

  it("providerLabel names every provider", () => {
    expect(providerLabel("openai")).toBe("OpenAI");
    expect(providerLabel("anthropic")).toBe("Anthropic");
    // CLOUD_PROVIDERS drives the rows/picker — one entry per provider.
    expect(CLOUD_PROVIDERS.map((p) => p.id)).toEqual(["openai", "anthropic"]);
  });

  it("keyPresent is false when the snapshot is unknown", () => {
    expect(keyPresent(null, "openai")).toBe(false);
    expect(keyPresent(noKeys, "openai")).toBe(false);
    expect(keyPresent({ openaiPresent: true, anthropicPresent: false }, "openai")).toBe(true);
    expect(keyPresent({ openaiPresent: true, anthropicPresent: false }, "anthropic")).toBe(false);
  });

  it("keyErrorTitle distinguishes every kind", () => {
    const titles = [
      keyErrorTitle({ kind: "invalid-key", detail: "" }),
      keyErrorTitle({ kind: "store-failed", detail: "" }),
    ];
    expect(new Set(titles).size).toBe(2);
  });
});
