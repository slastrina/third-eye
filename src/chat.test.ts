// Negative-path coverage for the chat state machine (R006): stale events,
// races between events and the invoke resolving, every error kind, retry
// composition, and the backoff schedule. The reducer is pure, so no Tauri
// runtime or DOM is needed.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  bannerDetail,
  bannerTitle,
  captureErrorTitle,
  chatReducer,
  composeMessages,
  initialChatState,
  freshNudgePreload,
  NUDGE_PRELOAD_FRESH_MS,
  nudgeContextMessage,
  nextProbeDelay,
  showStopButton,
  startHealthProbe,
  stripFailedTail,
  toCaptureFlowError,
  HEALTH_PROBE_INITIAL_MS,
  HEALTH_PROBE_MAX_MS,
  MEMORY_SEARCH_TOOL,
  MODEL_INFO_EVENT,
  PRIVACY_EVENT,
  RUN_STATE_EVENT,
  HID_STATE_EVENT,
  HID_APPROVAL_EVENT,
  MCP_APPROVAL_EVENT,
  TOOL_CALL_EVENT,
  TOOL_RESULT_EVENT,
  TERMINAL_CHUNK_EVENT,
  phaseStatusLine,
  RUN_IN_WORKSPACE_TOOL,
  WORKSPACE_DIFF_TOOL,
  diffLineKind,
  type ActionKind,
  type ApprovalVerdict,
  type HidApprovalRequest,
  type McpApprovalRequest,
  type McpApprovalVerdict,
  type CaptureError,
  type CapturedFrame,
  type ChatState,
  type HidArmedStatus,
  type InputError,
  type LlmError,
  type LlmHealth,
  type ToolCallPayload,
  type ToolResultPayload,
  type ModelInfo,
  type NudgePayload,
  type PrivacyStatus,
  isLocalEndpoint,
} from "./chat";

describe("isLocalEndpoint", () => {
  it("claims on-device only for loopback hosts; unparseable never claims", () => {
    expect(isLocalEndpoint("http://localhost:1234")).toBe(true);
    expect(isLocalEndpoint("http://127.0.0.1:1234/v1")).toBe(true);
    expect(isLocalEndpoint("http://[::1]:8080")).toBe(true);
    expect(isLocalEndpoint("https://api.example.com/v1")).toBe(false);
    expect(isLocalEndpoint("http://192.168.1.10:1234")).toBe(false);
    expect(isLocalEndpoint("not a url")).toBe(false);
  });
});

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

describe("chatReducer reasoning (Thinking… stream)", () => {
  it("accumulates reasoning deltas into the message's reasoning field, not text", () => {
    let s = started("hi", 1);
    s = chatReducer(s, { type: "reasoning", payload: { requestId: 1, delta: "Let me " } });
    s = chatReducer(s, { type: "reasoning", payload: { requestId: 1, delta: "think.\n" } });
    s = chatReducer(s, { type: "token", payload: { requestId: 1, token: "Answer" } });
    expect(lastMessage(s)).toMatchObject({
      role: "assistant",
      text: "Answer",
      reasoning: "Let me think.\n",
      status: "streaming",
    });
  });

  it("ignores stale reasoning tagged with a superseded request id", () => {
    let s = started("hi", 2);
    s = chatReducer(s, { type: "reasoning", payload: { requestId: 1, delta: "zombie thought" } });
    expect(lastMessage(s).reasoning).toBeUndefined();
  });

  it("buffers reasoning that beats the request id resolving, then replays it", () => {
    let s = chatReducer(initialChatState, { type: "submit", question: "hi" });
    s = chatReducer(s, { type: "reasoning", payload: { requestId: 7, delta: "early" } });
    expect(lastMessage(s).reasoning).toBeUndefined();
    s = chatReducer(s, { type: "request-started", requestId: 7 });
    expect(lastMessage(s).reasoning).toBe("early");
  });

  it("reasoning survives the terminal done event (stays readable after the answer settles)", () => {
    let s = started("hi", 1);
    s = chatReducer(s, { type: "reasoning", payload: { requestId: 1, delta: "pondering" } });
    s = chatReducer(s, {
      type: "done",
      payload: { requestId: 1, text: "final", tokenCount: 1, firstTokenMs: 5, totalMs: 9 },
    });
    expect(lastMessage(s)).toMatchObject({ text: "final", reasoning: "pondering", status: "done" });
  });

  it("reasoning is never resent as wire history (transient, answer-only history)", () => {
    let s = started("hi", 1);
    s = chatReducer(s, { type: "reasoning", payload: { requestId: 1, delta: "secret thought" } });
    s = chatReducer(s, {
      type: "done",
      payload: { requestId: 1, text: "the answer", tokenCount: 1, firstTokenMs: 5, totalMs: 9 },
    });
    const history = composeMessages(s.messages, "next");
    expect(history.some((m) => m.content.includes("secret thought"))).toBe(false);
    expect(history.some((m) => m.content === "the answer")).toBe(true);
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
    auto: false,
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
      auto: false,
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
    s = chatReducer(s, { type: "model-info", info: { activeLane: "thin", auto: false, endpoint: ENDPOINT, lanes: [] } });
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

describe("HID arming IPC contract (S03/M005)", () => {
  it("keeps the broadcast event name in sync with the Rust contract", () => {
    // src-tauri/src/input/commands.rs pins HID_STATE_EVENT to this same string.
    expect(HID_STATE_EVENT).toBe("hid://state");
  });

  it("HidArmedStatus carries armed, the permission snapshot, and null error on success", () => {
    // The serde camelCase shape apply_hid_armed broadcasts and hid_armed_status
    // returns — armed off by default, a granted+supported permission, no error.
    const status: HidArmedStatus = {
      armed: false,
      mode: "off",
      permission: { granted: true, supported: true },
      error: null,
    };
    expect(status.armed).toBe(false);
    expect(status.mode).toBe("off");
    expect(status.permission).toEqual({ granted: true, supported: true });
    expect(status.error).toBeNull();
  });

  it("a refused arm rides a typed permission-denied error the walkthrough keys on", () => {
    // D038: an ungranted arm never claims armed; the error is typed, never a
    // silent no-op (R007), and the Settings walkthrough matches on `kind`.
    const denied: InputError = {
      kind: "permission-denied",
      detail: "Accessibility not granted; enable Third Eye in System Settings",
    };
    const status: HidArmedStatus = {
      armed: false,
      mode: "off",
      permission: { granted: false, supported: true },
      error: denied,
    };
    expect(status.armed).toBe(false);
    expect(status.error?.kind).toBe("permission-denied");
  });

  it("carries the three-way run mode the Settings selector reads (S04/T05)", () => {
    // The serde camelCase shape apply_hid_run_mode broadcasts on hid://state:
    // an active mode reports armed=true; Off is the inert default.
    const ask: HidArmedStatus = {
      armed: true,
      mode: "ask",
      permission: { granted: true, supported: true },
      error: null,
    };
    expect(ask.mode).toBe("ask");
    expect(ask.armed).toBe(true);
    const autoRun: HidArmedStatus = {
      armed: true,
      mode: "auto-run",
      permission: { granted: true, supported: true },
      error: null,
    };
    expect(autoRun.mode).toBe("auto-run");
  });

  it("a persist failure rides a typed input-failed error, never silence", () => {
    const persistFailed: InputError = {
      kind: "input-failed",
      detail: "failed to persist hidEnabled to settings.json",
    };
    const status: HidArmedStatus = {
      armed: false,
      mode: "off",
      permission: { granted: true, supported: true },
      error: persistFailed,
    };
    expect(status.error?.kind).toBe("input-failed");
    if (status.error?.kind === "input-failed") {
      expect(status.error.detail).toContain("settings.json");
    }
  });
});

describe("HID approval IPC contract (S04/M005)", () => {
  it("keeps the approval-request event name in sync with the Rust contract", () => {
    // src-tauri/src/llm/commands.rs pins HID_APPROVAL_EVENT to this same string.
    expect(HID_APPROVAL_EVENT).toBe("hid://approval-request");
  });

  it("ApprovalRequestPayload carries the correlation id, kind, and human summary", () => {
    // The serde camelCase shape the gate emits and the overlay reads.
    const request: HidApprovalRequest = {
      approvalId: 3,
      kind: "mouse-click",
      summary: "Click the left mouse button",
    };
    expect(request.approvalId).toBe(3);
    expect(request.kind).toBe("mouse-click");
    expect(request.summary).toContain("Click");
  });

  it("the verdict and kind wire strings match the Rust kebab-case serde tags", () => {
    // respond_hid_approval deserializes these exact ApprovalVerdict strings, and
    // ActionKind mirrors InputAction's `action` tag.
    const verdicts: ApprovalVerdict[] = ["allow-once", "allow-kind", "deny"];
    expect(verdicts).toEqual(["allow-once", "allow-kind", "deny"]);
    const kinds: ActionKind[] = [
      "mouse-move",
      "mouse-click",
      "type-text",
      "key-press",
      "focus-app",
    ];
    expect(kinds).toHaveLength(5);
  });
});

describe("MCP approval IPC contract (S04/M007)", () => {
  it("keeps the approval-request event name in sync with the Rust contract", () => {
    // src-tauri/src/llm/commands.rs pins MCP_APPROVAL_EVENT to this same string
    // (mcp_approval_event_name_is_the_ipc_contract) — the const-test pair lock.
    expect(MCP_APPROVAL_EVENT).toBe("mcp://approval-request");
  });

  it("McpApprovalRequest carries the correlation id, namespaced tool name, and summary", () => {
    // The serde camelCase shape the MCP gate emits and the overlay reads —
    // pixel-free: id + toolName + a bounded human summary (R011).
    const request: McpApprovalRequest = {
      approvalId: 7,
      toolName: "mcp__weather_forecast",
      summary: 'Call mcp__weather_forecast({"city":"Paris"})',
    };
    expect(request.approvalId).toBe(7);
    expect(request.toolName).toBe("mcp__weather_forecast");
    expect(request.summary).toContain("mcp__weather_forecast");
  });

  it("the verdict wire strings match the Rust kebab-case serde tags", () => {
    // respond_mcp_approval deserializes these exact McpApprovalVerdict strings.
    // Keyed on the tool NAME (allow-tool), unlike the HID twin's allow-kind.
    const verdicts: McpApprovalVerdict[] = ["allow-once", "allow-tool", "deny"];
    expect(verdicts).toEqual(["allow-once", "allow-tool", "deny"]);
  });
});

describe("chatReducer tool events (S03)", () => {
  const toolCall = (requestId: number, name = MEMORY_SEARCH_TOOL): ToolCallPayload => ({
    requestId,
    round: 0,
    call: { id: "call_0", name, arguments: `{"query":"this morning"}` },
  });
  const toolResult = (
    requestId: number,
    overrides: Partial<ToolResultPayload> = {},
  ): ToolResultPayload => ({
    requestId,
    round: 0,
    callId: "call_0",
    name: MEMORY_SEARCH_TOOL,
    ok: true,
    resultCount: 3,
    mode: "semantic",
    failure: null,
    ...overrides,
  });

  it("keeps the tool event names in sync with the Rust contract", () => {
    expect(TOOL_CALL_EVENT).toBe("llm://tool-call");
    expect(TOOL_RESULT_EVENT).toBe("llm://tool-result");
    expect(MEMORY_SEARCH_TOOL).toBe("memory_search");
  });

  it("a memory_search tool-call marks the streaming answer as searching", () => {
    let s = started("what was I working on?", 1);
    s = chatReducer(s, { type: "tool-call", payload: toolCall(1) });
    expect(lastMessage(s)).toMatchObject({ role: "assistant", memory: "searching" });
  });

  it("a successful result flips searching to consulted, and done keeps it", () => {
    let s = started("hi", 1);
    s = chatReducer(s, { type: "tool-call", payload: toolCall(1) });
    s = chatReducer(s, { type: "tool-result", payload: toolResult(1) });
    expect(lastMessage(s).memory).toBe("consulted");
    s = chatReducer(s, {
      type: "done",
      payload: { requestId: 1, text: "grounded answer", tokenCount: 3, firstTokenMs: 5, totalMs: 9 },
    });
    expect(lastMessage(s)).toMatchObject({ status: "done", memory: "consulted" });
  });

  it("a failed result clears searching without claiming consultation", () => {
    let s = started("hi", 1);
    s = chatReducer(s, { type: "tool-call", payload: toolCall(1) });
    s = chatReducer(s, {
      type: "tool-result",
      payload: toolResult(1, { ok: false, resultCount: null, mode: null, failure: "db-failure" }),
    });
    expect(lastMessage(s).memory).toBeUndefined();
  });

  it("a failed later round never downgrades a consulted earned earlier", () => {
    let s = started("hi", 1);
    s = chatReducer(s, { type: "tool-result", payload: toolResult(1) });
    s = chatReducer(s, {
      type: "tool-result",
      payload: toolResult(1, { round: 1, ok: false, resultCount: null, mode: null, failure: "db-failure" }),
    });
    expect(lastMessage(s).memory).toBe("consulted");
  });

  it("stale tool events from a superseded request cannot touch the active answer", () => {
    let s = started("hi", 2);
    s = chatReducer(s, { type: "tool-call", payload: toolCall(1) });
    s = chatReducer(s, { type: "tool-result", payload: toolResult(1) });
    expect(lastMessage(s).memory).toBeUndefined();
  });

  it("a non-memory tool result does not flip the memory indicator", () => {
    let s = started("hi", 1);
    s = chatReducer(s, { type: "tool-call", payload: toolCall(1, "web_search") });
    s = chatReducer(s, { type: "tool-result", payload: toolResult(1, { name: "web_search" }) });
    expect(lastMessage(s).memory).toBeUndefined();
  });

  it("tool events that beat the invoke's resolution are buffered, then replayed", () => {
    let s = chatReducer(initialChatState, { type: "submit", question: "hi" });
    s = chatReducer(s, { type: "tool-call", payload: toolCall(5) });
    s = chatReducer(s, { type: "tool-result", payload: toolResult(5) });
    s = chatReducer(s, { type: "tool-result", payload: toolResult(4) }); // stale sibling
    expect(lastMessage(s).memory).toBeUndefined();
    s = chatReducer(s, { type: "request-started", requestId: 5 });
    expect(lastMessage(s).memory).toBe("consulted");
    expect(s.buffered).toEqual([]);
  });

  it("an interruption settles a dangling searching phase — no live claim on a dead stream", () => {
    let s = started("hi", 1);
    s = chatReducer(s, { type: "token", payload: { requestId: 1, token: "part" } });
    s = chatReducer(s, { type: "tool-call", payload: toolCall(1) });
    s = chatReducer(s, { type: "error", payload: { requestId: 1, error: interrupted("part") } });
    expect(lastMessage(s)).toMatchObject({ status: "interrupted", memory: undefined });
  });

  it("resubmitting settles the superseded answer's searching phase", () => {
    let s = started("first", 1);
    s = chatReducer(s, { type: "token", payload: { requestId: 1, token: "part" } });
    s = chatReducer(s, { type: "tool-call", payload: toolCall(1) });
    s = chatReducer(s, { type: "submit", question: "second" });
    const superseded = s.messages[s.messages.length - 3];
    expect(superseded).toMatchObject({ status: "interrupted", memory: undefined });
  });

  it("tools-unsupported raises a distinct visible banner naming the endpoint", () => {
    const toolsUnsupported: LlmError = {
      kind: "tools-unsupported",
      endpoint: ENDPOINT,
      detail: "model does not support tools",
    };
    let s = started("hi", 1);
    s = chatReducer(s, { type: "error", payload: { requestId: 1, error: toolsUnsupported } });
    expect(s.banner?.error).toEqual(toolsUnsupported);
    expect(s.requestId).toBeNull();
    // Distinct copy: must not read as offline or no-model.
    expect(bannerTitle(toolsUnsupported)).toMatch(/memory|tool/i);
    expect(bannerTitle(toolsUnsupported)).not.toBe(bannerTitle(noModel));
  });

  it("guard-blocked raises a distinct privacy-guard banner naming endpoint and reason", () => {
    // The M003 S02 wire shape: `reason` is a kebab-case machine token, no
    // free-text `detail` field (never any request text).
    const guardBlocked: LlmError = {
      kind: "guard-blocked",
      endpoint: "http://192.0.2.1:9",
      reason: "low-confidence",
    };
    let s = started("hi", 1);
    s = chatReducer(s, { type: "error", payload: { requestId: 1, error: guardBlocked } });
    expect(s.banner?.error).toEqual(guardBlocked);
    expect(s.requestId).toBeNull();
    expect(bannerTitle(guardBlocked)).toBe("Blocked by privacy guard");
    expect(bannerTitle(guardBlocked)).not.toBe(bannerTitle(offline));
    // Detail names the blocked endpoint (R006) and the kebab-case reason.
    expect(bannerDetail(guardBlocked)).toBe("http://192.0.2.1:9 — low-confidence");
  });

  it("phase pings show while waiting and clear the moment anything streams", () => {
    const ping = {
      requestId: 1,
      phase: "loading-model",
      model: "qwen3-coder",
      waitedMs: 4200,
      detail: "model state: loading",
    };
    let s = started("write a script", 1);
    s = chatReducer(s, { type: "phase", payload: ping });
    expect(s.phase).toEqual(ping);
    // Stale request ids never land.
    const stale = chatReducer(s, { type: "phase", payload: { ...ping, requestId: 9 } });
    expect(stale.phase).toEqual(ping);
    // A token clears it (activity beats status).
    s = chatReducer(s, { type: "token", payload: { requestId: 1, token: "H" } });
    expect(s.phase).toBeNull();
    // Copy: basic is plain words; verbose carries model + timer + detail.
    expect(phaseStatusLine(ping, false)).toBe("Loading the model…");
    expect(phaseStatusLine(ping, true)).toBe(
      "Loading the model… · qwen3-coder · waiting 4s · model state: loading",
    );
    expect(
      phaseStatusLine({ ...ping, phase: "processing-prompt", detail: null }, false),
    ).toBe("Reading your request…");
  });

  it("an empty-completion surfaces as a named banner, never a silent bubble", () => {
    // A broken model that streams only newlines used to leave an empty
    // assistant message with no explanation (qwen3.5-27b-heretic incident).
    const empty: LlmError = {
      kind: "empty-completion",
      endpoint: "http://localhost:1234",
      detail: "3 token(s) streamed, none visible",
    };
    let s = started("write a python script", 1);
    s = chatReducer(s, { type: "error", payload: { requestId: 1, error: empty } });
    expect(s.banner?.error).toEqual(empty);
    expect(bannerTitle(empty)).toBe("The model returned nothing");
    expect(bannerDetail(empty)).toContain("localhost:1234");
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

// ---------------------------------------------------------------------------
// Nudge lifecycle (S05): banner state, summon preload, consume-once
// ---------------------------------------------------------------------------

const nudge: NudgePayload = {
  kind: "nudge",
  message: "Looks like a tricky stack trace — want help?",
  screenText: "TypeError: cannot read properties of undefined",
  appContext: "Terminal",
  capturedAtMs: 1_700_000_000_000,
  memoryContext: ["Working on the Third Eye overlay app"],
};

describe("chatReducer nudge lifecycle", () => {
  it("nudge-shown parks the payload behind the banner", () => {
    const s = chatReducer(initialChatState, { type: "nudge-shown", payload: nudge });
    expect(s.nudge).toEqual(nudge);
    expect(s.nudgePreload).toBeNull();
  });

  it("auto-timeout dismiss clears the banner but STAGES the preload", () => {
    // The user often summons chat after the 12s banner timeout — the nudge's
    // context must survive the dismissal (freshness is enforced at submit).
    let s = chatReducer(initialChatState, { type: "nudge-shown", payload: nudge });
    s = chatReducer(s, { type: "nudge-dismissed", reason: "auto-timeout" });
    expect(s.nudge).toBeNull();
    expect(s.nudgePreload).toEqual(nudge);
  });

  it("disabled dismiss clears the banner AND any staged preload", () => {
    let s = chatReducer(initialChatState, { type: "nudge-shown", payload: nudge });
    s = chatReducer(s, { type: "nudge-dismissed", reason: "auto-timeout" });
    s = chatReducer(s, { type: "nudge-shown", payload: nudge });
    s = chatReducer(s, { type: "nudge-dismissed", reason: "disabled" });
    expect(s.nudge).toBeNull();
    expect(s.nudgePreload).toBeNull();
  });

  it("freshNudgePreload passes a recent stage and drops a stale one", () => {
    const now = nudge.capturedAtMs + NUDGE_PRELOAD_FRESH_MS;
    expect(freshNudgePreload(nudge, now)).toEqual(nudge);
    expect(freshNudgePreload(nudge, now + 1)).toBeNull();
    expect(freshNudgePreload(null, now)).toBeNull();
  });

  it("summoned dismiss stages the banner's payload as the chat preload", () => {
    let s = chatReducer(initialChatState, { type: "nudge-shown", payload: nudge });
    s = chatReducer(s, { type: "nudge-dismissed", reason: "summoned" });
    expect(s.nudge).toBeNull();
    expect(s.nudgePreload).toEqual(nudge);
  });

  it("a dismiss with no nudge showing is a no-op and cannot wipe a staged preload", () => {
    let s = chatReducer(initialChatState, { type: "nudge-shown", payload: nudge });
    s = chatReducer(s, { type: "nudge-dismissed", reason: "summoned" });
    const again = chatReducer(s, { type: "nudge-dismissed", reason: "hidden" });
    expect(again).toBe(s);
    expect(again.nudgePreload).toEqual(nudge);
  });

  it("a new nudge supersedes a stale unconsumed preload", () => {
    let s = chatReducer(initialChatState, { type: "nudge-shown", payload: nudge });
    s = chatReducer(s, { type: "nudge-dismissed", reason: "summoned" });
    const fresh: NudgePayload = { ...nudge, message: "New context", memoryContext: [] };
    s = chatReducer(s, { type: "nudge-shown", payload: fresh });
    expect(s.nudge).toEqual(fresh);
    expect(s.nudgePreload).toBeNull();
  });

  it("submit consumes the preload so it cannot ride a later question", () => {
    let s = chatReducer(initialChatState, { type: "nudge-shown", payload: nudge });
    s = chatReducer(s, { type: "nudge-dismissed", reason: "summoned" });
    s = chatReducer(s, { type: "submit", question: "what does this error mean?" });
    expect(s.nudgePreload).toBeNull();
  });
});

describe("resume-chat seeding (2026-07-27)", () => {
  const modelInfo: ModelInfo = {
    activeLane: "thin",
    auto: false,
    endpoint: "http://localhost:1234",
    lanes: [{ name: "thin", modelId: "thin-1b" }],
  };

  it("seeds the stored transcript as settled bubbles and keeps environment state", () => {
    let s = chatReducer(initialChatState, { type: "model-info", info: modelInfo });
    s = chatReducer(s, { type: "submit", question: "in progress" });
    s = chatReducer(s, {
      type: "resume-chat",
      messages: [
        { role: "user", text: "find me a carbonara recipe" },
        { role: "assistant", text: "Here is one from RecipeTinEats." },
        { role: "system", text: "never rendered" },
      ],
    });
    expect(s.messages).toEqual([
      { role: "user", text: "find me a carbonara recipe", status: "done" },
      { role: "assistant", text: "Here is one from RecipeTinEats.", status: "done" },
    ]);
    // Environment snapshots survive; the in-flight ask does not.
    expect(s.modelInfo).toEqual(modelInfo);
    expect(s.awaitingId).toBe(false);
    expect(s.lastQuestion).toBeNull();
  });

  it("a resumed transcript rides into the next question's history", () => {
    let s = chatReducer(initialChatState, {
      type: "resume-chat",
      messages: [
        { role: "user", text: "earlier question" },
        { role: "assistant", text: "earlier answer" },
      ],
    });
    const wire = composeMessages(s.messages, "follow-up");
    expect(wire).toEqual([
      { role: "user", content: "earlier question" },
      { role: "assistant", content: "earlier answer" },
      { role: "user", content: "follow-up" },
    ]);
  });
});

describe("nudge context preload composition", () => {
  it("prepends exactly one system message carrying screen and memory context", () => {
    const wire = composeMessages([], "what does this error mean?", [], nudge);
    expect(wire).toHaveLength(2);
    expect(wire[0].role).toBe("system");
    expect(wire[0].content).toContain(nudge.screenText);
    expect(wire[0].content).toContain("Terminal");
    expect(wire[0].content).toContain(nudge.memoryContext[0]);
    expect(wire[1]).toEqual({ role: "user", content: "what does this error mean?" });
  });

  it("composes without a system message when no preload is staged", () => {
    const wire = composeMessages([], "hi", [], null);
    expect(wire).toEqual([{ role: "user", content: "hi" }]);
  });

  it("tells the model about the attached nudge-time screenshot only when one rides", () => {
    const withShot = nudgeContextMessage(nudge, true);
    expect(withShot.content).toContain("screenshot taken when the nudge appeared");
    const without = nudgeContextMessage(nudge, false);
    expect(without.content).not.toContain("screenshot");
    // composeMessages threads the flag through to the system message.
    const wire = composeMessages([], "what was that about?", [{ base64Png: "UE5H" }], nudge, true);
    expect(wire[0].content).toContain("screenshot");
    expect(wire[1].attachments).toEqual([{ base64Png: "UE5H" }]);
  });

  it("omits the memory block and app label when the payload has neither", () => {
    const bare = nudgeContextMessage({
      ...nudge,
      appContext: null,
      memoryContext: [],
    });
    expect(bare.role).toBe("system");
    expect(bare.content).not.toContain("Relevant stored memories");
    expect(bare.content).not.toContain("frontmost app");
    expect(bare.content).toContain(nudge.screenText);
  });
});

// ---------------------------------------------------------------------------
// Run-state + Stop control (S04 T04)
// ---------------------------------------------------------------------------

describe("chat run-state and the Stop control", () => {
  it("pins the run-state event string as the IPC contract", () => {
    // The Rust const test asserts the same string on the backend side.
    expect(RUN_STATE_EVENT).toBe("llm://run-state");
  });

  it("hides the Stop control while idle", () => {
    expect(showStopButton(initialChatState)).toBe(false);
  });

  it("shows the Stop control the moment a question is submitted", () => {
    // Submit flips runPhase to "running" before the backend broadcast lands, so
    // the control appears immediately.
    const submitted = chatReducer(initialChatState, { type: "submit", question: "do a task" });
    expect(submitted.runPhase).toBe("running");
    expect(showStopButton(submitted)).toBe(true);
  });

  it("keeps the Stop control up while the run streams, then clears it on the idle broadcast", () => {
    const running = started("do a task", 1);
    expect(showStopButton(running)).toBe(true);

    // A natural finish: the backend broadcasts idle.
    const idle = chatReducer(running, { type: "run-state", phase: "idle" });
    expect(idle.runPhase).toBe("idle");
    expect(showStopButton(idle)).toBe(false);
  });

  it("clears the Stop control when a run is stopped", () => {
    const running = started("do a task", 1);
    const stopped = chatReducer(running, { type: "run-state", phase: "stopped" });
    expect(stopped.runPhase).toBe("stopped");
    expect(showStopButton(stopped)).toBe(false);
  });

  it("clears the Stop control when the chat invoke itself fails (no backend run)", () => {
    // request-failed means no backend task started, so no run-state broadcast
    // will arrive — the reducer must clear runPhase itself.
    const submitted = chatReducer(initialChatState, { type: "submit", question: "do a task" });
    const failed = chatReducer(submitted, { type: "request-failed", detail: "ipc down" });
    expect(failed.runPhase).toBe("idle");
    expect(showStopButton(failed)).toBe(false);
  });
});


describe("terminal runs in the transcript (computer-control I2)", () => {
  const streamingState = () => {
    let state = chatReducer(initialChatState, { type: "submit", question: "what time is it?", retry: false });
    state = chatReducer(state, { type: "request-started", requestId: 7 });
    return state;
  };
  const runCall = (id: string, command: string) => ({
    type: "tool-call" as const,
    payload: {
      requestId: 7,
      round: 0,
      call: { id, name: "run_command", arguments: JSON.stringify({ command }) },
    },
  });
  const runResult = (callId: string, ok: boolean, preview: string | null, failure: string | null = null) => ({
    type: "tool-result" as const,
    payload: {
      requestId: 7,
      round: 0,
      callId,
      name: "run_command",
      ok,
      resultCount: null,
      mode: null,
      failure,
      preview,
    },
  });
  const lastAssistant = (s: ReturnType<typeof chatReducer>) =>
    s.messages[s.messages.length - 1];

  it("a run_command call appends a pending terminal block with the exact command", () => {
    const state = chatReducer(streamingState(), runCall("t1", "date"));
    const terminal = lastAssistant(state).terminal ?? [];
    expect(terminal).toHaveLength(1);
    expect(terminal[0]).toEqual({ callId: "t1", command: "date", ok: null, preview: null });
  });

  it("the result settles the matching block with its output preview", () => {
    let state = chatReducer(streamingState(), runCall("t1", "date"));
    state = chatReducer(state, runResult("t1", true, "exit code: 0\nstdout:\nSat 26 Jul"));
    const terminal = lastAssistant(state).terminal ?? [];
    expect(terminal[0].ok).toBe(true);
    expect(terminal[0].preview).toContain("Sat 26 Jul");
  });

  it("a failed run without a preview shows the typed failure kind", () => {
    let state = chatReducer(streamingState(), runCall("t1", "date"));
    state = chatReducer(state, runResult("t1", false, null, "approval-denied"));
    const terminal = lastAssistant(state).terminal ?? [];
    expect(terminal[0].ok).toBe(false);
    expect(terminal[0].preview).toBe("[approval-denied]");
  });

  it("malformed arguments still render raw so nothing runs invisibly", () => {
    const state = chatReducer(streamingState(), {
      type: "tool-call",
      payload: {
        requestId: 7,
        round: 0,
        call: { id: "t2", name: "run_command", arguments: "{not json" },
      },
    });
    const terminal = lastAssistant(state).terminal ?? [];
    expect(terminal[0].command).toBe("{not json");
  });

  it("run_in_workspace gets the same terminal block and streams chunks live (S4)", () => {
    expect(TERMINAL_CHUNK_EVENT).toBe("llm://terminal-chunk");
    let state = chatReducer(streamingState(), {
      type: "tool-call",
      payload: {
        requestId: 7,
        round: 0,
        call: {
          id: "w1",
          name: RUN_IN_WORKSPACE_TOOL,
          arguments: JSON.stringify({ command: "cargo build" }),
        },
      },
    });
    state = chatReducer(state, {
      type: "terminal-chunk",
      payload: { requestId: 7, callId: "w1", chunk: "   Compiling third-eye\n" },
    });
    state = chatReducer(state, {
      type: "terminal-chunk",
      payload: { requestId: 7, callId: "w1", chunk: "    Finished dev\n" },
    });
    let terminal = lastAssistant(state).terminal ?? [];
    expect(terminal[0].command).toBe("cargo build");
    expect(terminal[0].ok).toBeNull();
    expect(terminal[0].preview).toBe("   Compiling third-eye\n    Finished dev\n");
    // A stale chunk (wrong request) never lands.
    const stale = chatReducer(state, {
      type: "terminal-chunk",
      payload: { requestId: 6, callId: "w1", chunk: "ghost" },
    });
    expect((lastAssistant(stale).terminal ?? [])[0].preview).not.toContain("ghost");
    // The result's bounded report replaces the stream, and later chunks
    // cannot reopen a settled block.
    state = chatReducer(state, {
      type: "tool-result",
      payload: {
        requestId: 7,
        round: 0,
        callId: "w1",
        name: RUN_IN_WORKSPACE_TOOL,
        ok: true,
        resultCount: null,
        mode: null,
        failure: null,
        preview: "exit code: 0 (in 4.20s)",
      },
    });
    state = chatReducer(state, {
      type: "terminal-chunk",
      payload: { requestId: 7, callId: "w1", chunk: "late" },
    });
    terminal = lastAssistant(state).terminal ?? [];
    expect(terminal[0].ok).toBe(true);
    expect(terminal[0].preview).toBe("exit code: 0 (in 4.20s)");
  });

  it("workspace_diff renders a collapsible diff block from the result preview (S5)", () => {
    let state = chatReducer(streamingState(), {
      type: "tool-call",
      payload: {
        requestId: 7,
        round: 0,
        call: { id: "d1", name: WORKSPACE_DIFF_TOOL, arguments: "{}" },
      },
    });
    let diffs = lastAssistant(state).diffs ?? [];
    expect(diffs).toEqual([{ callId: "d1", ok: null, report: null }]);
    state = chatReducer(state, {
      type: "tool-result",
      payload: {
        requestId: 7,
        round: 0,
        callId: "d1",
        name: WORKSPACE_DIFF_TOOL,
        ok: true,
        resultCount: null,
        mode: null,
        failure: null,
        preview: "status:\n M main.rs\ndiff:\n+fn new() {}\n-fn old() {}",
      },
    });
    diffs = lastAssistant(state).diffs ?? [];
    expect(diffs[0].ok).toBe(true);
    expect(diffs[0].report).toContain("+fn new() {}");
    // The colorizer maps line prefixes, meta before add/del.
    expect(diffLineKind("+fn new() {}")).toBe("add");
    expect(diffLineKind("-fn old() {}")).toBe("del");
    expect(diffLineKind("+++ b/main.rs")).toBe("meta");
    expect(diffLineKind("--- a/main.rs")).toBe("meta");
    expect(diffLineKind("@@ -1 +1 @@")).toBe("hunk");
    expect(diffLineKind(" fn kept() {}")).toBe("context");
  });
});


describe("approval prompt queues (the stuck-run fix)", () => {
  const hid = (approvalId: number): HidApprovalRequest => ({
    approvalId,
    kind: "run-command",
    summary: "Run command: curl -s ifconfig.me",
  });

  it("a request folds once (replay-safe) and renders until answered", () => {
    let s = chatReducer(initialChatState, { type: "hid-approval", request: hid(1) });
    s = chatReducer(s, { type: "hid-approval", request: hid(1) });
    expect(s.hidApprovals).toHaveLength(1);
    s = chatReducer(s, { type: "hid-approval-answered", approvalId: 1 });
    expect(s.hidApprovals).toHaveLength(0);
  });

  it("pending approvals survive new-chat and a fresh submit (the run outlives both)", () => {
    let s = chatReducer(initialChatState, { type: "hid-approval", request: hid(7) });
    s = chatReducer(s, { type: "new-chat" });
    expect(s.hidApprovals).toHaveLength(1);
    s = chatReducer(s, { type: "submit", question: "again" });
    expect(s.hidApprovals).toHaveLength(1);
  });

  it("mcp approvals queue independently", () => {
    let s = chatReducer(initialChatState, {
      type: "mcp-approval",
      request: { approvalId: 2, toolName: "mcp__files__write", summary: "write ~/notes.txt" },
    });
    expect(s.mcpApprovals).toHaveLength(1);
    expect(s.hidApprovals).toHaveLength(0);
    s = chatReducer(s, { type: "mcp-approval-answered", approvalId: 2 });
    expect(s.mcpApprovals).toHaveLength(0);
  });
});

describe("transcript steps block (2026-08-01)", () => {
  const call = (id: string, name: string, args: string): ToolCallPayload => ({
    requestId: 1,
    round: 0,
    call: { id, name, arguments: args },
  });
  const result = (id: string, name: string, ok: boolean): ToolResultPayload => ({
    requestId: 1,
    round: 0,
    callId: id,
    name,
    ok,
    resultCount: null,
    mode: null,
    failure: ok ? null : "off-target",
  });

  it("folds every tool call into steps and settles them from results", () => {
    let s = chatReducer(initialChatState, { type: "submit", question: "find hl2" });
    s = chatReducer(s, { type: "request-started", requestId: 1 });
    s = chatReducer(s, { type: "tool-call", payload: call("c1", "focus_app", '{"app":"Chrome"}') });
    s = chatReducer(s, {
      type: "tool-call",
      payload: call("c2", "input_action", '{"action":"mouse-click","x":10,"y":20}'),
    });
    s = chatReducer(s, { type: "tool-result", payload: result("c1", "focus_app", true) });
    s = chatReducer(s, { type: "tool-result", payload: result("c2", "input_action", false) });
    const assistant = s.messages[s.messages.length - 1];
    expect(assistant.steps).toEqual([
      { callId: "c1", label: "focus · Chrome", ok: true },
      { callId: "c2", label: "click · 10, 20", ok: false },
    ]);
    // Replayed calls fold once.
    s = chatReducer(s, { type: "tool-call", payload: call("c1", "focus_app", "{}") });
    expect(s.messages[s.messages.length - 1].steps).toHaveLength(2);
  });
});

describe("markdown math-delimiter stripping", () => {
  it("unwraps $$..$$, \\[..\\] and \\(..\\) to plain text", async () => {
    const { stripMathDelimiters } = await import("./ui/Markdown");
    expect(stripMathDelimiters("$$2 + 2 = 4$$")).toBe("2 + 2 = 4");
    expect(stripMathDelimiters("so \\(x=1\\) and \\[y=2\\] hold")).toBe("so x=1 and y=2 hold");
    // Ordinary dollars survive: prices are not math.
    expect(stripMathDelimiters("costs $50 or $11.50 used")).toBe("costs $50 or $11.50 used");
  });
});
