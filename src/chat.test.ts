// Negative-path coverage for the chat state machine (R006): stale events,
// races between events and the invoke resolving, every error kind, retry
// composition, and the backoff schedule. The reducer is pure, so no Tauri
// runtime or DOM is needed.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  captureErrorTitle,
  chatReducer,
  composeMessages,
  initialChatState,
  nextProbeDelay,
  startHealthProbe,
  stripFailedTail,
  toCaptureFlowError,
  HEALTH_PROBE_INITIAL_MS,
  HEALTH_PROBE_MAX_MS,
  MODEL_INFO_EVENT,
  PRIVACY_EVENT,
  type CaptureError,
  type CapturedFrame,
  type ChatState,
  type LlmError,
  type LlmHealth,
  type ModelInfo,
  type PrivacyStatus,
} from "./chat";

const ENDPOINT = "http://192.168.182.224:1234";

const offline: LlmError = { kind: "offline", endpoint: ENDPOINT, detail: "connection refused" };
const noModel: LlmError = { kind: "no-model", endpoint: ENDPOINT, detail: "HTTP 400" };
const interrupted = (partialText: string): LlmError => ({
  kind: "interrupted",
  endpoint: ENDPOINT,
  partialText,
  detail: "connection reset",
});

/** Submit a question and resolve its request id — the normal happy prefix. */
function started(question: string, requestId: number, from: ChatState = initialChatState): ChatState {
  const submitted = chatReducer(from, { type: "submit", question });
  return chatReducer(submitted, { type: "request-started", requestId });
}

function lastMessage(state: ChatState) {
  return state.messages[state.messages.length - 1];
}

describe("chatReducer streaming", () => {
  it("appends tokens for the active request to the streaming placeholder", () => {
    let s = started("hi", 1);
    s = chatReducer(s, { type: "token", payload: { requestId: 1, token: "Hel" } });
    s = chatReducer(s, { type: "token", payload: { requestId: 1, token: "lo" } });
    expect(lastMessage(s)).toMatchObject({ role: "assistant", text: "Hello", status: "streaming" });
  });

  it("ignores stale tokens tagged with a superseded request id", () => {
    let s = started("hi", 2);
    s = chatReducer(s, { type: "token", payload: { requestId: 1, token: "zombie" } });
    expect(lastMessage(s).text).toBe("");
  });

  it("ignores a stale done event and keeps the stream open", () => {
    let s = started("hi", 2);
    s = chatReducer(s, {
      type: "done",
      payload: { requestId: 1, text: "old answer", tokenCount: 2, firstTokenMs: 5, totalMs: 9 },
    });
    expect(lastMessage(s).status).toBe("streaming");
    expect(s.requestId).toBe(2);
  });

  it("done replaces coalesced text with the authoritative backend text", () => {
    let s = started("hi", 1);
    s = chatReducer(s, { type: "token", payload: { requestId: 1, token: "partial tai" } });
    s = chatReducer(s, {
      type: "done",
      payload: { requestId: 1, text: "full answer", tokenCount: 3, firstTokenMs: 100, totalMs: 900 },
    });
    expect(lastMessage(s)).toMatchObject({ text: "full answer", status: "done" });
    expect(s.requestId).toBeNull();
  });
});

describe("chatReducer pre-resolve buffering", () => {
  it("buffers events that arrive before the request id resolves, then replays matches", () => {
    let s = chatReducer(initialChatState, { type: "submit", question: "hi" });
    s = chatReducer(s, { type: "token", payload: { requestId: 5, token: "fast" } });
    s = chatReducer(s, { type: "token", payload: { requestId: 4, token: "stale" } });
    expect(lastMessage(s).text).toBe("");

    s = chatReducer(s, { type: "request-started", requestId: 5 });
    expect(lastMessage(s).text).toBe("fast");
    expect(s.buffered).toEqual([]);
  });

  it("delivers an instant offline error that beat the invoke's resolution", () => {
    let s = chatReducer(initialChatState, { type: "submit", question: "hi" });
    s = chatReducer(s, { type: "error", payload: { requestId: 3, error: offline } });
    s = chatReducer(s, { type: "request-started", requestId: 3 });
    expect(s.banner).toEqual({ error: offline, online: false });
  });
});

describe("chatReducer failure paths", () => {
  it("offline: raises a banner naming the endpoint and drops the empty placeholder", () => {
    let s = started("hi", 1);
    s = chatReducer(s, { type: "error", payload: { requestId: 1, error: offline } });
    expect(s.banner?.error).toEqual(offline);
    expect(lastMessage(s).role).toBe("user");
    expect(s.requestId).toBeNull();
  });

  it("no-model: raises a banner and clears the in-flight request", () => {
    let s = started("hi", 1);
    s = chatReducer(s, { type: "error", payload: { requestId: 1, error: noModel } });
    expect(s.banner?.error).toEqual(noModel);
    expect(s.requestId).toBeNull();
  });

  it("interrupted: preserves the partial text on screen, marked interrupted", () => {
    let s = started("hi", 1);
    s = chatReducer(s, { type: "token", payload: { requestId: 1, token: "half an ans" } });
    s = chatReducer(s, { type: "error", payload: { requestId: 1, error: interrupted("half an answer") } });
    expect(lastMessage(s)).toMatchObject({ text: "half an answer", status: "interrupted" });
    expect(s.banner?.error.kind).toBe("interrupted");
  });

  it("a rejected chat invoke becomes a visible ipc banner, never a silent hang", () => {
    let s = chatReducer(initialChatState, { type: "submit", question: "hi" });
    s = chatReducer(s, { type: "request-failed", detail: "ipc channel closed" });
    expect(s.banner?.error).toEqual({ kind: "ipc", detail: "ipc channel closed" });
    expect(s.awaitingId).toBe(false);
    expect(lastMessage(s).role).toBe("user");
  });

  it("a stale error from an aborted request cannot clobber the active stream", () => {
    let s = started("hi", 2);
    s = chatReducer(s, { type: "error", payload: { requestId: 1, error: offline } });
    expect(s.banner).toBeNull();
    expect(s.requestId).toBe(2);
  });
});

describe("chatReducer resubmit and retry", () => {
  it("resubmitting marks the still-streaming answer interrupted (single-flight mirror)", () => {
    let s = started("first", 1);
    s = chatReducer(s, { type: "token", payload: { requestId: 1, token: "partial" } });
    s = chatReducer(s, { type: "submit", question: "second" });
    const prior = s.messages[s.messages.length - 3];
    expect(prior).toMatchObject({ role: "assistant", text: "partial", status: "interrupted" });
    expect(s.lastQuestion).toBe("second");
  });

  it("retry does not stack a duplicate user turn", () => {
    let s = started("hi", 1);
    s = chatReducer(s, { type: "error", payload: { requestId: 1, error: offline } });
    s = chatReducer(s, { type: "submit", question: "hi", retry: true });
    const userTurns = s.messages.filter((m) => m.role === "user");
    expect(userTurns).toHaveLength(1);
    expect(s.banner).toBeNull();
  });

  it("stripFailedTail removes an interrupted exchange but keeps completed ones", () => {
    let s = started("q1", 1);
    s = chatReducer(s, {
      type: "done",
      payload: { requestId: 1, text: "a1", tokenCount: 1, firstTokenMs: 1, totalMs: 2 },
    });
    s = chatReducer(s, { type: "submit", question: "q2" });
    s = chatReducer(s, { type: "request-started", requestId: 2 });
    s = chatReducer(s, { type: "error", payload: { requestId: 2, error: interrupted("part") } });

    const stripped = stripFailedTail(s.messages);
    expect(stripped).toHaveLength(2);
    expect(stripped[1]).toMatchObject({ role: "assistant", text: "a1", status: "done" });
  });

  it("composeMessages excludes interrupted partials from the wire history", () => {
    let s = started("q1", 1);
    s = chatReducer(s, { type: "error", payload: { requestId: 1, error: interrupted("part") } });
    const history = composeMessages(s.messages, "q2");
    expect(history).toEqual([
      { role: "user", content: "q1" },
      { role: "user", content: "q2" },
    ]);
  });
});

describe("chatReducer model info", () => {
  const routing: ModelInfo = {
    activeLane: "thin",
    endpoint: ENDPOINT,
    lanes: [
      { name: "thin", modelId: "thin-1b" },
      { name: "heavy", modelId: "heavy-7b" },
    ],
  };
  const heavyRouting: ModelInfo = { ...routing, activeLane: "heavy" };

  it("stores routing state from a model-info action without touching the chat", () => {
    let s = started("hi", 1);
    s = chatReducer(s, { type: "token", payload: { requestId: 1, token: "Hel" } });
    const before = s;
    s = chatReducer(s, { type: "model-info", info: routing });
    expect(s.modelInfo).toEqual(routing);
    expect(s.messages).toBe(before.messages);
    expect(s.requestId).toBe(1);
    expect(s.banner).toBeNull();
  });

  it("a lane switch replaces the stored info with the backend's snapshot", () => {
    let s = chatReducer(initialChatState, { type: "model-info", info: routing });
    s = chatReducer(s, { type: "model-info", info: heavyRouting });
    expect(s.modelInfo?.activeLane).toBe("heavy");
  });

  it("accepts unpinned lanes (single-model fallback shape)", () => {
    const single: ModelInfo = {
      activeLane: "thin",
      endpoint: ENDPOINT,
      lanes: [
        { name: "thin", modelId: null },
        { name: "heavy", modelId: null },
      ],
    };
    const s = chatReducer(initialChatState, { type: "model-info", info: single });
    expect(s.modelInfo?.lanes.every((lane) => lane.modelId === null)).toBe(true);
  });

  it("submit preserves modelInfo while resetting the exchange state", () => {
    let s = chatReducer(initialChatState, { type: "model-info", info: routing });
    s = chatReducer(s, { type: "submit", question: "hi" });
    expect(s.modelInfo).toEqual(routing);
    expect(s.awaitingId).toBe(true);
  });

  it("every streaming and failure action preserves modelInfo", () => {
    let s = chatReducer(initialChatState, { type: "model-info", info: routing });
    s = chatReducer(s, { type: "submit", question: "hi" });
    s = chatReducer(s, { type: "request-started", requestId: 1 });
    s = chatReducer(s, { type: "token", payload: { requestId: 1, token: "a" } });
    expect(s.modelInfo).toEqual(routing);
    s = chatReducer(s, { type: "error", payload: { requestId: 1, error: offline } });
    expect(s.modelInfo).toEqual(routing);
    s = chatReducer(s, { type: "health", online: true });
    expect(s.modelInfo).toEqual(routing);
    s = chatReducer(s, { type: "submit", question: "again" });
    s = chatReducer(s, { type: "request-failed", detail: "ipc channel closed" });
    expect(s.modelInfo).toEqual(routing);
    s = chatReducer(s, { type: "submit", question: "once more" });
    s = chatReducer(s, { type: "request-started", requestId: 2 });
    s = chatReducer(s, {
      type: "done",
      payload: { requestId: 2, text: "ok", tokenCount: 1, firstTokenMs: 1, totalMs: 2 },
    });
    expect(s.modelInfo).toEqual(routing);
  });
});

describe("chatReducer health", () => {
  it("flips the banner to online when the probe reports recovery", () => {
    let s = started("hi", 1);
    s = chatReducer(s, { type: "error", payload: { requestId: 1, error: offline } });
    s = chatReducer(s, { type: "health", online: true });
    expect(s.banner?.online).toBe(true);
  });

  it("is a no-op without a banner and returns the same state for repeats", () => {
    expect(chatReducer(initialChatState, { type: "health", online: true })).toBe(initialChatState);
    let s = started("hi", 1);
    s = chatReducer(s, { type: "error", payload: { requestId: 1, error: offline } });
    expect(chatReducer(s, { type: "health", online: false })).toBe(s);
  });
});

describe("chatReducer attach lifecycle (R004/R007)", () => {
  const frame: CapturedFrame = { width: 2048, height: 1152, base64Png: "QUJD" };
  const denied: CaptureError = {
    kind: "permission-denied",
    detail: "Screen Recording not granted",
  };

  /** Run the happy attach prefix: start pending, resolve with a frame. */
  function attached(from: ChatState = initialChatState): ChatState {
    const pending = chatReducer(from, { type: "attach-start" });
    return chatReducer(pending, { type: "attach-done", frame });
  }

  it("attach-start marks the capture pending and clears a prior capture error", () => {
    let s = chatReducer(initialChatState, { type: "attach-start" });
    s = chatReducer(s, { type: "attach-error", error: denied });
    expect(s.captureError).toEqual(denied);
    s = chatReducer(s, { type: "attach-start" });
    expect(s.attachPending).toBe(true);
    expect(s.captureError).toBeNull();
  });

  it("attach-done stages the frame for the next submit", () => {
    const s = attached();
    expect(s.attachment).toEqual(frame);
    expect(s.attachPending).toBe(false);
    expect(s.captureError).toBeNull();
  });

  it("a settlement without a pending capture is dropped as stale", () => {
    // Covers both a double-resolve and a frame landing after submit consumed
    // the flow — a stale frame must never ride a future message unnoticed.
    expect(chatReducer(initialChatState, { type: "attach-done", frame })).toBe(initialChatState);
    expect(chatReducer(initialChatState, { type: "attach-error", error: denied })).toBe(
      initialChatState,
    );
  });

  it("permission-denied surfaces the walkthrough error and stages nothing", () => {
    let s = chatReducer(initialChatState, { type: "attach-start" });
    s = chatReducer(s, { type: "attach-error", error: denied });
    expect(s.captureError).toEqual(denied);
    expect(s.attachment).toBeNull();
    expect(s.attachPending).toBe(false);
  });

  it("every capture error kind lands in captureError, never silence", () => {
    const kinds: CaptureError[] = [
      denied,
      { kind: "no-display", detail: "asleep" },
      { kind: "capture-failed", detail: "stream error" },
      { kind: "unsupported", platform: "linux", detail: "no backend" },
      { kind: "privacy-mode", detail: "Privacy Mode is on — capture blocked" },
    ];
    for (const error of kinds) {
      let s = chatReducer(initialChatState, { type: "attach-start" });
      s = chatReducer(s, { type: "attach-error", error });
      expect(s.captureError).toEqual(error);
    }
  });

  it("attach-clear removes the staged frame and any capture error", () => {
    let s = attached();
    s = chatReducer(s, { type: "attach-clear" });
    expect(s.attachment).toBeNull();
    expect(s.captureError).toBeNull();
  });

  it("submit consumes the attachment: the user turn is marked, the staging cleared", () => {
    let s = attached();
    s = chatReducer(s, { type: "submit", question: "what is on my screen?" });
    const userTurn = s.messages[s.messages.length - 2];
    expect(userTurn).toMatchObject({ role: "user", attached: true });
    expect(s.attachment).toBeNull();
    expect(s.captureError).toBeNull();
  });

  it("submit without an attachment leaves the user turn unmarked", () => {
    const s = chatReducer(initialChatState, { type: "submit", question: "hi" });
    const userTurn = s.messages[s.messages.length - 2];
    expect(userTurn.attached).toBe(false);
  });

  it("submit while a capture is pending drops the late frame", () => {
    let s = chatReducer(initialChatState, { type: "attach-start" });
    s = chatReducer(s, { type: "submit", question: "hi" });
    expect(s.attachPending).toBe(false);
    s = chatReducer(s, { type: "attach-done", frame });
    expect(s.attachment).toBeNull();
  });

  it("capture-permission stores the queryable snapshot", () => {
    const s = chatReducer(initialChatState, {
      type: "capture-permission",
      permission: { granted: false, supported: true },
    });
    expect(s.capturePermission).toEqual({ granted: false, supported: true });
  });

  it("streaming and failure actions preserve the staged attachment", () => {
    let s = attached();
    s = chatReducer(s, { type: "capture-permission", permission: { granted: true, supported: true } });
    s = chatReducer(s, { type: "model-info", info: { activeLane: "thin", endpoint: ENDPOINT, lanes: [] } });
    expect(s.attachment).toEqual(frame);
    expect(s.capturePermission).toEqual({ granted: true, supported: true });
  });
});

describe("composeMessages attachments", () => {
  it("puts attachments on the outgoing user turn only", () => {
    let s = chatReducer(initialChatState, { type: "submit", question: "q1" });
    s = chatReducer(s, { type: "request-started", requestId: 1 });
    s = chatReducer(s, {
      type: "done",
      payload: { requestId: 1, text: "a1", tokenCount: 1, firstTokenMs: 1, totalMs: 2 },
    });
    const history = composeMessages(s.messages, "look", [{ base64Png: "QUJD" }]);
    expect(history).toEqual([
      { role: "user", content: "q1" },
      { role: "assistant", content: "a1" },
      { role: "user", content: "look", attachments: [{ base64Png: "QUJD" }] },
    ]);
  });

  it("omits the attachments key entirely when there is nothing attached", () => {
    const history = composeMessages([], "hi");
    expect(history).toEqual([{ role: "user", content: "hi" }]);
    expect("attachments" in history[0]).toBe(false);
  });
});

describe("capture error copy", () => {
  it("passes typed kind-tagged errors through and wraps anything else as ipc", () => {
    const typed: CaptureError = { kind: "no-display", detail: "asleep" };
    expect(toCaptureFlowError(typed)).toBe(typed);
    expect(toCaptureFlowError("invoke failed")).toEqual({
      kind: "ipc",
      detail: "invoke failed",
    });
  });

  it("names every failure kind with a human title", () => {
    expect(captureErrorTitle({ kind: "permission-denied", detail: "" })).toMatch(/permission/i);
    expect(captureErrorTitle({ kind: "no-display", detail: "" })).toMatch(/display/i);
    expect(captureErrorTitle({ kind: "capture-failed", detail: "" })).toMatch(/failed/i);
    expect(captureErrorTitle({ kind: "unsupported", platform: "linux", detail: "" })).toMatch(
      /not supported|unavailable/i,
    );
    expect(captureErrorTitle({ kind: "privacy-mode", detail: "" })).toMatch(/privacy/i);
    expect(captureErrorTitle({ kind: "ipc", detail: "" })).toMatch(/unavailable|capture/i);
  });
});

describe("chatReducer privacy mode (S07)", () => {
  const on: PrivacyStatus = { enabled: true, error: null };
  const off: PrivacyStatus = { enabled: false, error: null };

  it("keeps the broadcast event names in sync with the Rust contract", () => {
    expect(MODEL_INFO_EVENT).toBe("llm://model-info");
    expect(PRIVACY_EVENT).toBe("capture://privacy");
  });

  it("stores the privacy status without touching the chat", () => {
    let s = started("hi", 1);
    const before = s;
    s = chatReducer(s, { type: "privacy", status: on });
    expect(s.privacy).toEqual(on);
    expect(s.messages).toBe(before.messages);
    expect(s.requestId).toBe(1);
  });

  it("every later broadcast is authoritative — a tray toggle overwrites", () => {
    let s = chatReducer(initialChatState, { type: "privacy", status: on });
    s = chatReducer(s, { type: "privacy", status: off });
    expect(s.privacy).toEqual(off);
  });

  it("a persist failure rides the status as data, never silence", () => {
    const failed: PrivacyStatus = {
      enabled: false,
      error: "failed to persist privacyMode=true to settings.json",
    };
    const s = chatReducer(initialChatState, { type: "privacy", status: failed });
    expect(s.privacy?.error).toContain("settings.json");
  });

  it("submit and stream lifecycle preserve the privacy snapshot", () => {
    let s = chatReducer(initialChatState, { type: "privacy", status: on });
    s = chatReducer(s, { type: "submit", question: "hi" });
    expect(s.privacy).toEqual(on);
    s = chatReducer(s, { type: "request-started", requestId: 1 });
    s = chatReducer(s, { type: "token", payload: { requestId: 1, token: "a" } });
    s = chatReducer(s, { type: "error", payload: { requestId: 1, error: offline } });
    expect(s.privacy).toEqual(on);
  });

  it("a blocked capture surfaces the typed privacy-mode error in captureError", () => {
    let s = chatReducer(initialChatState, { type: "privacy", status: on });
    s = chatReducer(s, { type: "attach-start" });
    s = chatReducer(s, {
      type: "attach-error",
      error: { kind: "privacy-mode", detail: "Privacy Mode is on — capture blocked" },
    });
    expect(s.captureError?.kind).toBe("privacy-mode");
    expect(s.attachment).toBeNull();
  });
});

describe("health probe backoff", () => {
  beforeEach(() => {
    vi.useFakeTimers();
  });
  afterEach(() => {
    vi.useRealTimers();
  });

  it("doubles 2s → 30s and stays capped", () => {
    const delays = [HEALTH_PROBE_INITIAL_MS];
    for (let i = 0; i < 5; i++) delays.push(nextProbeDelay(delays[delays.length - 1]));
    expect(delays).toEqual([2000, 4000, 8000, 16000, 30000, 30000]);
    expect(nextProbeDelay(HEALTH_PROBE_MAX_MS)).toBe(HEALTH_PROBE_MAX_MS);
  });

  it("keeps probing on failures and stops once online", async () => {
    const results: LlmHealth[] = [];
    const probe = vi
      .fn<() => Promise<LlmHealth>>()
      .mockRejectedValueOnce(new Error("ipc down"))
      .mockResolvedValueOnce({ online: false, endpoint: ENDPOINT })
      .mockResolvedValue({ online: true, endpoint: ENDPOINT });

    startHealthProbe((h) => results.push(h), probe);

    await vi.advanceTimersByTimeAsync(2000); // rejected probe: no result, keeps going
    expect(probe).toHaveBeenCalledTimes(1);
    expect(results).toEqual([]);

    await vi.advanceTimersByTimeAsync(4000); // offline result forwarded
    expect(results).toEqual([{ online: false, endpoint: ENDPOINT }]);

    await vi.advanceTimersByTimeAsync(8000); // online result forwarded, probe stops
    expect(results).toEqual([
      { online: false, endpoint: ENDPOINT },
      { online: true, endpoint: ENDPOINT },
    ]);

    await vi.advanceTimersByTimeAsync(120000);
    expect(probe).toHaveBeenCalledTimes(3);
  });

  it("stop() cancels the pending probe and suppresses late results", async () => {
    const results: LlmHealth[] = [];
    let release: (h: LlmHealth) => void = () => {};
    const probe = vi.fn(
      () => new Promise<LlmHealth>((resolve) => { release = resolve; }),
    );

    const stop = startHealthProbe((h) => results.push(h), probe);
    await vi.advanceTimersByTimeAsync(2000);
    expect(probe).toHaveBeenCalledTimes(1);

    stop();
    release({ online: true, endpoint: ENDPOINT });
    await vi.advanceTimersByTimeAsync(120000);
    expect(results).toEqual([]);
    expect(probe).toHaveBeenCalledTimes(1);
  });
});
