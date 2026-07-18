// UI side of the chat IPC surface defined in src-tauri/src/llm/commands.rs.
// The event names and payload shapes are the contract; keep them in sync with
// TOKEN_EVENT/DONE_EVENT/ERROR_EVENT and their serde camelCase serialization.
//
// All chat state transitions live in the pure `chatReducer` so stale-event
// filtering, pre-resolve buffering, and every failure path are unit-testable
// without a Tauri runtime (src/chat.test.ts). App.tsx is only glue.

import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

export const TOKEN_EVENT = "llm://token";
export const DONE_EVENT = "llm://done";
export const ERROR_EVENT = "llm://error";
/** Routing-state broadcast (S07): mutation responses only reach the calling
 *  window, so the backend emits the updated ModelInfo app-wide after every
 *  successful set_model / set_lane_model. The overlay consumes this to stay
 *  truthful when the settings window changes routing. */
export const MODEL_INFO_EVENT = "llm://model-info";

export type Role = "system" | "user" | "assistant";

/** One image riding a chat turn: the `base64Png` of a CapturedFrame, echoed
 *  back over IPC. The backend turns it into the OpenAI vision content part. */
export interface Attachment {
  base64Png: string;
}

/** One chat turn on the wire — serializes into the Rust ChatMessage. The
 *  `attachments` key is additive: omitted entirely when empty so the pre-S04
 *  wire shape is preserved byte-for-byte. */
export interface ChatMessage {
  role: Role;
  content: string;
  attachments?: Attachment[];
}

export interface TokenPayload {
  requestId: number;
  token: string;
}

export interface DonePayload {
  requestId: number;
  text: string;
  tokenCount: number;
  firstTokenMs: number | null;
  totalMs: number;
}

/** Kind-tagged error JSON — the serde serialization of Rust's LlmError. */
export type LlmError =
  | { kind: "offline"; endpoint: string; detail: string }
  | { kind: "no-model"; endpoint: string; detail: string }
  | { kind: "interrupted"; endpoint: string; partialText: string; detail: string };

export interface ErrorPayload {
  requestId: number;
  error: LlmError;
}

export interface LlmHealth {
  online: boolean;
  endpoint: string;
}

/** One routing lane — the serde camelCase serialization of Rust's LaneInfo. */
export interface ModelLane {
  name: string;
  /** null when the lane is unpinned (single-model fallback: requests omit
   *  the model key and the endpoint serves whatever it has loaded). */
  modelId: string | null;
}

/** Routing state snapshot from `model_info` / `set_model` (R003). */
export interface ModelInfo {
  activeLane: string;
  endpoint: string;
  lanes: ModelLane[];
}

/** Start a streaming completion; resolves to the request id whose llm://*
 *  events to accept. The backend aborts any prior in-flight request. */
export function sendChat(messages: ChatMessage[]): Promise<number> {
  return invoke<number>("chat", { messages });
}

export function llmHealth(): Promise<LlmHealth> {
  return invoke<LlmHealth>("llm_health");
}

/** Switch the active routing lane; resolves to the updated routing state so
 *  the UI can render the switch without a second round-trip. Unknown lanes
 *  reject with an error naming the lane and the known set, leaving routing
 *  unchanged backend-side. */
export function setModel(lane: string): Promise<ModelInfo> {
  return invoke<ModelInfo>("set_model", { lane });
}

/** Queryable routing state (health-as-value pattern, like `llm_health`). */
export function modelInfo(): Promise<ModelInfo> {
  return invoke<ModelInfo>("model_info");
}

/** Subscribe to the app-wide routing broadcast (`llm://model-info`). */
export function onModelInfoBroadcast(cb: (info: ModelInfo) => void): Promise<UnlistenFn> {
  return listen<ModelInfo>(MODEL_INFO_EVENT, (e) => cb(e.payload));
}

export function onLlmToken(cb: (payload: TokenPayload) => void): Promise<UnlistenFn> {
  return listen<TokenPayload>(TOKEN_EVENT, (e) => cb(e.payload));
}

export function onLlmDone(cb: (payload: DonePayload) => void): Promise<UnlistenFn> {
  return listen<DonePayload>(DONE_EVENT, (e) => cb(e.payload));
}

export function onLlmError(cb: (payload: ErrorPayload) => void): Promise<UnlistenFn> {
  return listen<ErrorPayload>(ERROR_EVENT, (e) => cb(e.payload));
}

// ---------------------------------------------------------------------------
// Screen capture IPC (S04) — contract defined in src-tauri/src/capture
// ---------------------------------------------------------------------------

/** Kind-tagged capture failure taxonomy — the serde serialization of Rust's
 *  CaptureError (R007: every failure is typed, never a bare string). */
export type CaptureError =
  | { kind: "permission-denied"; detail: string }
  | { kind: "no-display"; detail: string }
  | { kind: "capture-failed"; detail: string }
  | { kind: "unsupported"; platform: string; detail: string }
  | { kind: "privacy-mode"; detail: string };

/** One captured frame of the primary display, PNG-encoded and base64'd. */
export interface CapturedFrame {
  width: number;
  height: number;
  base64Png: string;
}

/** Screen Recording permission snapshot — health-as-value, never an error.
 *  `supported: false` means this platform has no capture backend at all. */
export interface CapturePermission {
  granted: boolean;
  supported: boolean;
}

/** Capture one frame of the primary display with every Third Eye window
 *  excluded (R008). Rejects with a typed CaptureError; the first ask on a
 *  supported platform may show the OS permission prompt. */
export function captureScreen(): Promise<CapturedFrame> {
  return invoke<CapturedFrame>("capture_screen");
}

/** Queryable permission state (health-as-value, like `llm_health`). Safe to
 *  poll while the walkthrough waits for the Settings toggle. */
export function capturePermissionStatus(): Promise<CapturePermission> {
  return invoke<CapturePermission>("capture_permission_status");
}

/** Deep-link to System Settings → Privacy & Security → Screen Recording —
 *  the walkthrough's "Open System Settings" action. */
export function openCaptureSettings(): Promise<void> {
  return invoke("open_capture_settings");
}

/** Privacy-state broadcast (S07): every privacy toggle — tray check item or
 *  `set_privacy_mode` IPC — emits the resulting PrivacyStatus app-wide, so
 *  the overlay's attach affordance stays truthful when the toggle flips in
 *  the settings window or tray. */
export const PRIVACY_EVENT = "capture://privacy";

/** Queryable privacy-mode state `{ enabled, error }` — health-as-value
 *  beside `hotkey_status`/`autostart_status` (R007). `error` carries the
 *  most recent persist failure, naming the settings path. */
export interface PrivacyStatus {
  enabled: boolean;
  error: string | null;
}

/** Current privacy-mode state (health-as-value, like `llm_health`). */
export function privacyStatus(): Promise<PrivacyStatus> {
  return invoke<PrivacyStatus>("privacy_status");
}

/** Subscribe to the app-wide privacy broadcast (`capture://privacy`). */
export function onPrivacyChanged(cb: (status: PrivacyStatus) => void): Promise<UnlistenFn> {
  return listen<PrivacyStatus>(PRIVACY_EVENT, (e) => cb(e.payload));
}

/** A typed capture error from the backend, or an IPC-level failure where the
 *  invoke itself rejected with no typed shape (mirrors BannerError's "ipc"). */
export type CaptureFlowError = CaptureError | { kind: "ipc"; detail: string };

/** Normalize a capture_screen rejection: typed kind-tagged errors pass
 *  through untouched; anything else (IPC string, Error) becomes "ipc". */
export function toCaptureFlowError(err: unknown): CaptureFlowError {
  if (typeof err === "object" && err !== null && "kind" in err) {
    return err as CaptureError;
  }
  return { kind: "ipc", detail: String(err) };
}

// ---------------------------------------------------------------------------
// Chat state machine (pure)
// ---------------------------------------------------------------------------

export type AssistantStatus = "streaming" | "done" | "interrupted";

export interface UiMessage {
  role: "user" | "assistant";
  text: string;
  /** Always "done" for user messages; lifecycle state for assistant ones. */
  status: AssistantStatus;
  /** True when a screen attachment rode this user turn (renders the chip). */
  attached?: boolean;
}

/** A typed backend error, or an IPC-level failure where the chat invoke
 *  itself rejected and no endpoint is known. */
export type BannerError = LlmError | { kind: "ipc"; detail: string };

export interface Banner {
  error: BannerError;
  /** Flipped by the health probe once the endpoint answers again. */
  online: boolean;
}

export interface ChatState {
  messages: UiMessage[];
  /** Resolved id of the in-flight request; null when idle. */
  requestId: number | null;
  /** True between the chat invoke and its request id resolving. */
  awaitingId: boolean;
  /** Events that raced ahead of the invoke resolving. */
  buffered: LlmEvent[];
  banner: Banner | null;
  /** Last submitted question, backing the Retry affordance. */
  lastQuestion: string | null;
  /** Routing state behind the model indicator; null until the first
   *  `model_info` query resolves (or forever, outside a Tauri runtime). */
  modelInfo: ModelInfo | null;
  /** Captured frame staged to ride the next submitted user message. */
  attachment: CapturedFrame | null;
  /** True between the capture_screen invoke and its settlement. */
  attachPending: boolean;
  /** Capture failure surfaced to the user (R007 — never silence);
   *  kind "permission-denied" renders the guided walkthrough. */
  captureError: CaptureFlowError | null;
  /** Screen Recording permission snapshot; null until the mount-time query
   *  resolves (or forever, outside a Tauri runtime). */
  capturePermission: CapturePermission | null;
  /** Privacy-mode snapshot behind the attach affordance's hint; fed by the
   *  mount-time `privacy_status` query and the `capture://privacy` broadcast
   *  (null forever outside a Tauri runtime). */
  privacy: PrivacyStatus | null;
}

export const initialChatState: ChatState = {
  messages: [],
  requestId: null,
  awaitingId: false,
  buffered: [],
  banner: null,
  lastQuestion: null,
  modelInfo: null,
  attachment: null,
  attachPending: false,
  captureError: null,
  capturePermission: null,
  privacy: null,
};

export type LlmEvent =
  | { type: "token"; payload: TokenPayload }
  | { type: "done"; payload: DonePayload }
  | { type: "error"; payload: ErrorPayload };

export type ChatAction =
  | { type: "submit"; question: string; retry?: boolean }
  | { type: "request-started"; requestId: number }
  | { type: "request-failed"; detail: string }
  | { type: "health"; online: boolean }
  | { type: "model-info"; info: ModelInfo }
  | { type: "attach-start" }
  | { type: "attach-done"; frame: CapturedFrame }
  | { type: "attach-error"; error: CaptureFlowError }
  | { type: "attach-clear" }
  | { type: "capture-permission"; permission: CapturePermission }
  | { type: "privacy"; status: PrivacyStatus }
  | LlmEvent;

function withLastAssistant(
  messages: UiMessage[],
  update: (msg: UiMessage) => UiMessage,
): UiMessage[] {
  const idx = messages.length - 1;
  if (idx < 0 || messages[idx].role !== "assistant") return messages;
  return [...messages.slice(0, idx), update(messages[idx])];
}

/** An offline/no-model error arrives before any token, so the streaming
 *  placeholder is empty — drop it. If tokens did land (shouldn't happen for
 *  these kinds), keep them marked interrupted rather than discard user-visible
 *  text (R006: partial text is never thrown away). */
function settleStreamingTail(messages: UiMessage[]): UiMessage[] {
  const last = messages[messages.length - 1];
  if (last && last.role === "assistant" && last.status === "streaming" && last.text === "") {
    return messages.slice(0, -1);
  }
  return withLastAssistant(messages, (m) =>
    m.status === "streaming" ? { ...m, status: "interrupted" } : m,
  );
}

/** Strip a trailing failed exchange (user turn plus missing or interrupted
 *  answer) so Retry re-runs the question instead of stacking a duplicate. */
export function stripFailedTail(messages: UiMessage[]): UiMessage[] {
  let end = messages.length;
  const last = messages[end - 1];
  if (last && last.role === "assistant" && last.status !== "done") end -= 1;
  const beforeLast = messages[end - 1];
  if (beforeLast && beforeLast.role === "user") end -= 1;
  return messages.slice(0, end);
}

/** Build the wire history for a new question from completed turns only —
 *  interrupted partials and the streaming placeholder are excluded. Only the
 *  outgoing turn carries attachments: past screenshots are not resent as
 *  history (the answer they grounded already is), keeping requests small. */
export function composeMessages(
  messages: UiMessage[],
  question: string,
  attachments: Attachment[] = [],
): ChatMessage[] {
  const history: ChatMessage[] = [];
  for (const m of messages) {
    if (m.role === "user") history.push({ role: "user", content: m.text });
    else if (m.status === "done") history.push({ role: "assistant", content: m.text });
  }
  const turn: ChatMessage = { role: "user", content: question };
  if (attachments.length > 0) turn.attachments = attachments;
  history.push(turn);
  return history;
}

export function chatReducer(state: ChatState, action: ChatAction): ChatState {
  switch (action.type) {
    case "submit": {
      // A resubmit aborts the in-flight stream backend-side (single-flight);
      // mirror that by settling a still-streaming answer as interrupted.
      const base = action.retry ? stripFailedTail(state.messages) : state.messages;
      const settled = withLastAssistant(base, (m) =>
        m.status === "streaming" ? { ...m, status: "interrupted" } : m,
      );
      return {
        messages: [
          ...settled,
          // The staged attachment is consumed here: marked on this turn and
          // cleared below, so it can never ride a later message by accident.
          { role: "user", text: action.question, status: "done", attached: state.attachment !== null },
          { role: "assistant", text: "", status: "streaming" },
        ],
        requestId: null,
        awaitingId: true,
        buffered: [],
        banner: null,
        lastQuestion: action.question,
        modelInfo: state.modelInfo,
        attachment: null,
        // Dropping the pending flag makes a capture that settles after this
        // submit stale — its frame is discarded, not silently re-staged.
        attachPending: false,
        captureError: null,
        capturePermission: state.capturePermission,
        privacy: state.privacy,
      };
    }
    case "request-started": {
      // Replay events that beat the invoke's resolution; anything tagged with
      // a different id belongs to an aborted predecessor and is dropped.
      let next: ChatState = {
        ...state,
        requestId: action.requestId,
        awaitingId: false,
        buffered: [],
      };
      for (const event of state.buffered) {
        if (event.payload.requestId === action.requestId) {
          next = chatReducer(next, event);
        }
      }
      return next;
    }
    case "request-failed":
      return {
        ...state,
        requestId: null,
        awaitingId: false,
        buffered: [],
        messages: settleStreamingTail(state.messages),
        banner: { error: { kind: "ipc", detail: action.detail }, online: false },
      };
    case "token": {
      if (state.awaitingId) return { ...state, buffered: [...state.buffered, action] };
      if (action.payload.requestId !== state.requestId) return state; // stale
      return {
        ...state,
        messages: withLastAssistant(state.messages, (m) => ({
          ...m,
          text: m.text + action.payload.token,
        })),
      };
    }
    case "done": {
      if (state.awaitingId) return { ...state, buffered: [...state.buffered, action] };
      if (action.payload.requestId !== state.requestId) return state;
      return {
        ...state,
        requestId: null,
        messages: withLastAssistant(state.messages, (m) => ({
          ...m,
          // The backend's accumulated text is authoritative — it replaces any
          // frame-coalesced tail still buffered UI-side.
          text: action.payload.text,
          status: "done",
        })),
      };
    }
    case "error": {
      if (state.awaitingId) return { ...state, buffered: [...state.buffered, action] };
      if (action.payload.requestId !== state.requestId) return state;
      const error = action.payload.error;
      const messages =
        error.kind === "interrupted"
          ? withLastAssistant(state.messages, (m) => ({
              ...m,
              // Preserve everything streamed before the drop (R006).
              text: error.partialText || m.text,
              status: "interrupted",
            }))
          : settleStreamingTail(state.messages);
      return { ...state, requestId: null, messages, banner: { error, online: false } };
    }
    case "health": {
      if (!state.banner || state.banner.online === action.online) return state;
      return { ...state, banner: { ...state.banner, online: action.online } };
    }
    case "model-info":
      // Both the mount-time query and every set_model response land here —
      // the backend's returned snapshot is authoritative.
      return { ...state, modelInfo: action.info };
    case "attach-start":
      return { ...state, attachPending: true, captureError: null };
    case "attach-done":
      // A settlement with no capture pending is stale — a double resolve, or
      // a frame landing after submit/clear consumed the flow. Dropping it
      // keeps a stale screenshot from riding a future message unnoticed.
      if (!state.attachPending) return state;
      return { ...state, attachPending: false, attachment: action.frame, captureError: null };
    case "attach-error":
      if (!state.attachPending) return state;
      return { ...state, attachPending: false, captureError: action.error };
    case "attach-clear":
      return { ...state, attachment: null, captureError: null };
    case "capture-permission":
      return { ...state, capturePermission: action.permission };
    case "privacy":
      // Both the mount-time query and every capture://privacy broadcast land
      // here — the backend's status is authoritative (cross-window sync).
      return { ...state, privacy: action.status };
  }
}

// ---------------------------------------------------------------------------
// Banner copy
// ---------------------------------------------------------------------------

/** Short human title naming the failure type. */
export function bannerTitle(error: BannerError): string {
  switch (error.kind) {
    case "offline":
      return "Local AI offline";
    case "no-model":
      return "No model loaded";
    case "interrupted":
      return "Answer interrupted";
    case "ipc":
      return "Chat unavailable";
  }
}

/** Detail line naming the endpoint that was tried (R006). */
export function bannerDetail(error: BannerError): string {
  return error.kind === "ipc" ? error.detail : `${error.endpoint} — ${error.detail}`;
}

/** Short human title for a capture failure (R007 — every kind gets a name). */
export function captureErrorTitle(error: CaptureFlowError): string {
  switch (error.kind) {
    case "permission-denied":
      return "Screen Recording permission needed";
    case "no-display":
      return "No display to capture";
    case "capture-failed":
      return "Screen capture failed";
    case "unsupported":
      return `Screen capture is not supported on ${error.platform}`;
    case "privacy-mode":
      return "Privacy Mode is on";
    case "ipc":
      return "Screen capture unavailable";
  }
}

/** Detail line for a capture failure banner. */
export function captureErrorDetail(error: CaptureFlowError): string {
  return error.detail;
}

// ---------------------------------------------------------------------------
// Health probe with exponential backoff (2s → 30s cap)
// ---------------------------------------------------------------------------

export const HEALTH_PROBE_INITIAL_MS = 2000;
export const HEALTH_PROBE_MAX_MS = 30000;

export function nextProbeDelay(previousMs: number): number {
  return Math.min(previousMs * 2, HEALTH_PROBE_MAX_MS);
}

/** Poll `llm_health` with exponential backoff until the endpoint reports
 *  online, then stop. Every result is forwarded to `onResult`. Returns a stop
 *  function; a rejected probe invoke counts as still-offline and keeps
 *  backing off — never a silent stall (R006). */
export function startHealthProbe(
  onResult: (health: LlmHealth) => void,
  probe: () => Promise<LlmHealth> = llmHealth,
): () => void {
  let stopped = false;
  let timer: ReturnType<typeof setTimeout> | undefined;
  let delay = HEALTH_PROBE_INITIAL_MS;

  const tick = async () => {
    let health: LlmHealth | null = null;
    try {
      health = await probe();
    } catch (err) {
      console.debug("llm: health probe invoke failed:", err);
    }
    if (stopped) return;
    if (health) {
      onResult(health);
      if (health.online) return;
    }
    delay = nextProbeDelay(delay);
    timer = setTimeout(tick, delay);
  };

  timer = setTimeout(tick, delay);
  return () => {
    stopped = true;
    if (timer !== undefined) clearTimeout(timer);
  };
}
